use super::{
    fanout::{self, Fanout},
    task::{Task, TaskOutput},
    BuiltBuffer, ConfigDiff,
};
use crate::{
    config::{
        ComponentKey, DataType, OutputId, ProxyConfig, SinkContext, SourceContext, TransformContext,
    },
    event::Event,
    internal_events::EventsReceived,
    shutdown::SourceShutdownCoordinator,
    transforms::Transform,
    Pipeline,
};
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use lazy_static::lazy_static;
use std::pin::Pin;
use std::{
    collections::HashMap,
    future::ready,
    sync::{Arc, Mutex},
    time::Instant,
};
use stream_cancel::{StreamExt as StreamCancelExt, Trigger, Tripwire};
use tokio::{
    select,
    time::{timeout, Duration},
};
use vector_core::{
    buffers::{BufferInputCloner, BufferStream, BufferType},
    internal_event::EventsSent,
    ByteSizeOf,
};

lazy_static! {
    static ref ENRICHMENT_TABLES: enrichment::TableRegistry = enrichment::TableRegistry::default();
}

pub async fn load_enrichment_tables<'a>(
    config: &'a super::Config,
    diff: &'a ConfigDiff,
) -> (&'static enrichment::TableRegistry, Vec<String>) {
    let mut enrichment_tables = HashMap::new();

    let mut errors = vec![];

    // Build enrichment tables
    'tables: for (name, table) in config.enrichment_tables.iter() {
        let table_name = name.to_string();
        if ENRICHMENT_TABLES.needs_reload(&table_name) {
            let indexes = if !diff.enrichment_tables.contains_new(name) {
                // If this is an existing enrichment table, we need to store the indexes to reapply
                // them again post load.
                Some(ENRICHMENT_TABLES.index_fields(&table_name))
            } else {
                None
            };

            let mut table = match table.inner.build(&config.global).await {
                Ok(table) => table,
                Err(error) => {
                    errors.push(format!("Enrichment Table \"{}\": {}", name, error));
                    continue;
                }
            };

            if let Some(indexes) = indexes {
                for (case, index) in indexes {
                    match table
                        .add_index(case, &index.iter().map(|s| s.as_ref()).collect::<Vec<_>>())
                    {
                        Ok(_) => (),
                        Err(error) => {
                            // If there is an error adding an index we do not want to use the reloaded
                            // data, the previously loaded data will still need to be used.
                            // Just report the error and continue.
                            error!(message = "Unable to add index to reloaded enrichment table.",
                                    table = ?name.to_string(),
                                    %error);
                            continue 'tables;
                        }
                    }
                }
            }

            enrichment_tables.insert(table_name, table);
        }
    }

    ENRICHMENT_TABLES.load(enrichment_tables);

    (&ENRICHMENT_TABLES, errors)
}

pub struct Pieces {
    pub inputs: HashMap<ComponentKey, (BufferInputCloner<Event>, Vec<OutputId>)>,
    pub outputs: HashMap<ComponentKey, HashMap<Option<String>, fanout::ControlChannel>>,
    pub tasks: HashMap<ComponentKey, Task>,
    pub source_tasks: HashMap<ComponentKey, Task>,
    pub healthchecks: HashMap<ComponentKey, Task>,
    pub shutdown_coordinator: SourceShutdownCoordinator,
    pub detach_triggers: HashMap<ComponentKey, Trigger>,
    pub enrichment_tables: enrichment::TableRegistry,
}

/// Builds only the new pieces, and doesn't check their topology.
pub async fn build_pieces(
    config: &super::Config,
    diff: &ConfigDiff,
    mut buffers: HashMap<ComponentKey, BuiltBuffer>,
) -> Result<Pieces, Vec<String>> {
    let mut inputs = HashMap::new();
    let mut outputs = HashMap::new();
    let mut tasks = HashMap::new();
    let mut source_tasks = HashMap::new();
    let mut healthchecks = HashMap::new();
    let mut shutdown_coordinator = SourceShutdownCoordinator::default();
    let mut detach_triggers = HashMap::new();

    let mut errors = vec![];

    let (enrichment_tables, enrichment_errors) = load_enrichment_tables(config, diff).await;
    errors.extend(enrichment_errors);

    // Build sources
    for (key, source) in config
        .sources
        .iter()
        .filter(|(key, _)| diff.sources.contains_new(key))
    {
        let (tx, rx) = futures::channel::mpsc::channel(1000);
        let pipeline = Pipeline::from_sender(tx, vec![]);

        let typetag = source.inner.source_type();

        let (shutdown_signal, force_shutdown_tripwire) = shutdown_coordinator.register_source(key);

        let context = SourceContext {
            key: key.clone(),
            globals: config.global.clone(),
            shutdown: shutdown_signal,
            out: pipeline,
            proxy: ProxyConfig::merge_with_env(&config.global.proxy, &source.proxy),
        };
        let server = match source.inner.build(context).await {
            Err(error) => {
                errors.push(format!("Source \"{}\": {}", key, error));
                continue;
            }
            Ok(server) => server,
        };

        let (output, control) = Fanout::new();
        let pump = rx.map(Ok).forward(output).map_ok(|_| TaskOutput::Source);
        let pump = Task::new(key.clone(), typetag, pump);

        // The force_shutdown_tripwire is a Future that when it resolves means that this source
        // has failed to shut down gracefully within its allotted time window and instead should be
        // forcibly shut down. We accomplish this by select()-ing on the server Task with the
        // force_shutdown_tripwire. That means that if the force_shutdown_tripwire resolves while
        // the server Task is still running the Task will simply be dropped on the floor.
        let server = async {
            let result = select! {
                biased;

                _ = force_shutdown_tripwire => {
                    Ok(())
                },
                result = server => result,
            };

            match result {
                Ok(()) => {
                    debug!("Finished.");
                    Ok(TaskOutput::Source)
                }
                Err(()) => Err(()),
            }
        };
        let server = Task::new(key.clone(), typetag, server);

        outputs.insert(OutputId::from(key), control);
        tasks.insert(key.clone(), pump);
        source_tasks.insert(key.clone(), server);
    }

    // Build transforms
    for (key, transform) in config
        .transforms
        .iter()
        .filter(|(key, _)| diff.transforms.contains_new(key))
    {
        let context = TransformContext {
            key: Some(key.clone()),
            globals: config.global.clone(),
            enrichment_tables: enrichment_tables.clone(),
        };

        let node = TransformNode {
            key: key.clone(),
            typetag: transform.inner.transform_type(),
            inputs: transform.inputs.clone(),
            input_type: transform.inner.input_type(),
            named_outputs: transform.inner.named_outputs(),
        };

        let transform = match transform.inner.build(&context).await {
            Err(error) => {
                errors.push(format!("Transform \"{}\": {}", key, error));
                continue;
            }
            Ok(transform) => transform,
        };

        let (input_tx, input_rx, _) = vector_core::buffers::build(
            vector_core::buffers::Variant::Memory {
                max_events: 100,
                when_full: vector_core::buffers::WhenFull::Block,
                instrument: false,
            },
            tracing::Span::none(),
        )
        .unwrap();

        inputs.insert(key.clone(), (input_tx, node.inputs.clone()));

        let (transform_task, transform_outputs) = build_transform(transform, node, input_rx);

        outputs.extend(transform_outputs);
        tasks.insert(key.clone(), transform_task);
    }

    // Build sinks
    for (key, sink) in config
        .sinks
        .iter()
        .filter(|(key, _)| diff.sinks.contains_new(key))
    {
        let sink_inputs = &sink.inputs;
        let healthcheck = sink.healthcheck();
        let enable_healthcheck = healthcheck.enabled && config.healthchecks.enabled;

        let typetag = sink.inner.sink_type();
        let input_type = sink.inner.input_type();

        let (tx, rx, acker) = if let Some(buffer) = buffers.remove(key) {
            buffer
        } else {
            let buffer_type = match sink.buffer.stages().first().expect("cant ever be empty") {
                BufferType::Memory { .. } => "memory",
                #[cfg(feature = "disk-buffer")]
                BufferType::Disk { .. } => "disk",
            };
            let buffer_span = error_span!(
                "sink",
                component_kind = "sink",
                component_id = %key.id(),
                component_scope = %key.scope(),
                component_type = typetag,
                component_name = %key.id(),
                buffer_type = buffer_type,
            );
            let buffer = sink
                .buffer
                .build(&config.global.data_dir, key.to_string(), buffer_span);
            match buffer {
                Err(error) => {
                    errors.push(format!("Sink \"{}\": {}", key, error));
                    continue;
                }
                Ok((tx, rx, acker)) => (tx, Arc::new(Mutex::new(Some(rx.into()))), acker),
            }
        };

        let cx = SinkContext {
            acker: acker.clone(),
            healthcheck,
            globals: config.global.clone(),
            proxy: ProxyConfig::merge_with_env(&config.global.proxy, sink.proxy()),
        };

        let (sink, healthcheck) = match sink.inner.build(cx).await {
            Err(error) => {
                errors.push(format!("Sink \"{}\": {}", key, error));
                continue;
            }
            Ok(built) => built,
        };

        let (trigger, tripwire) = Tripwire::new();

        let sink = async move {
            // Why is this Arc<Mutex<Option<_>>> needed you ask.
            // In case when this function build_pieces errors
            // this future won't be run so this rx won't be taken
            // which will enable us to reuse rx to rebuild
            // old configuration by passing this Arc<Mutex<Option<_>>>
            // yet again.
            let rx = rx
                .lock()
                .unwrap()
                .take()
                .expect("Task started but input has been taken.");

            let mut rx = crate::utilization::wrap(rx);

            sink.run(
                rx.by_ref()
                    .filter(|event| ready(filter_event_type(event, input_type)))
                    .inspect(|event| {
                        emit!(&EventsReceived {
                            count: 1,
                            byte_size: event.size_of(),
                        })
                    })
                    .take_until_if(tripwire),
            )
            .await
            .map(|_| {
                debug!("Finished.");
                TaskOutput::Sink(rx, acker)
            })
        };

        let task = Task::new(key.clone(), typetag, sink);

        let component_key = key.clone();
        let healthcheck_task = async move {
            if enable_healthcheck {
                let duration = Duration::from_secs(10);
                timeout(duration, healthcheck)
                    .map(|result| match result {
                        Ok(Ok(_)) => {
                            info!("Healthcheck: Passed.");
                            Ok(TaskOutput::Healthcheck)
                        }
                        Ok(Err(error)) => {
                            error!(
                                msg = "Healthcheck: Failed Reason.",
                                %error,
                                component_kind = "sink",
                                component_type = typetag,
                                component_id = %component_key.id(),
                                // maintained for compatibility
                                component_name = %component_key.id(),
                            );
                            Err(())
                        }
                        Err(_) => {
                            error!(
                                msg = "Healthcheck: timeout.",
                                component_kind = "sink",
                                component_type = typetag,
                                component_id = %component_key.id(),
                                // maintained for compatibility
                                component_name = %component_key.id(),
                            );
                            Err(())
                        }
                    })
                    .await
            } else {
                info!("Healthcheck: Disabled.");
                Ok(TaskOutput::Healthcheck)
            }
        };

        let healthcheck_task = Task::new(key.clone(), typetag, healthcheck_task);

        inputs.insert(key.clone(), (tx, sink_inputs.clone()));
        healthchecks.insert(key.clone(), healthcheck_task);
        tasks.insert(key.clone(), task);
        detach_triggers.insert(key.clone(), trigger);
    }

    // We should have all the data for the enrichment tables loaded now, so switch them over to
    // readonly.
    enrichment_tables.finish_load();

    let mut finalized_outputs = HashMap::new();
    for (id, output) in outputs {
        let entry = finalized_outputs
            .entry(id.component)
            .or_insert_with(HashMap::new);
        entry.insert(id.port, output);
    }

    if errors.is_empty() {
        let pieces = Pieces {
            inputs,
            outputs: finalized_outputs,
            tasks,
            source_tasks,
            healthchecks,
            shutdown_coordinator,
            detach_triggers,
            enrichment_tables: enrichment_tables.clone(),
        };

        Ok(pieces)
    } else {
        Err(errors)
    }
}

const fn filter_event_type(event: &Event, data_type: DataType) -> bool {
    match data_type {
        DataType::Any => true,
        DataType::Log => matches!(event, Event::Log(_)),
        DataType::Metric => matches!(event, Event::Metric(_)),
    }
}

use crate::transforms::{FallibleFunctionTransform, FunctionTransform, TaskTransform};

// 128 is an arbitrary, smallish constant
const TRANSFORM_BATCH_SIZE: usize = 128;

#[derive(Debug, Clone)]
struct TransformNode {
    key: ComponentKey,
    typetag: &'static str,
    inputs: Vec<OutputId>,
    input_type: DataType,
    named_outputs: Vec<String>,
}

fn build_transform(
    transform: Transform,
    node: TransformNode,
    input_rx: BufferStream<Event>,
) -> (Task, HashMap<OutputId, fanout::ControlChannel>) {
    match transform {
        Transform::Function(t) => build_sync_transform(
            Box::new(t),
            input_rx,
            node.input_type,
            node.typetag,
            &node.key,
            Vec::new(),
        ),
        Transform::FallibleFunction(t) => build_sync_transform(
            Box::new(t),
            input_rx,
            node.input_type,
            node.typetag,
            &node.key,
            node.named_outputs,
        ),
        Transform::Task(t) => {
            build_task_transform(t, input_rx, node.input_type, node.typetag, &node.key)
        }
    }
}

struct TransformOutputs {
    primary_buffer: Vec<Event>,
    named_buffers: HashMap<String, Vec<Event>>,
    primary_output: Fanout,
    named_outputs: HashMap<String, Fanout>,
}

impl TransformOutputs {
    fn new(
        named_outputs_in: Vec<String>,
    ) -> (Self, HashMap<Option<String>, fanout::ControlChannel>) {
        let mut named_buffers = HashMap::new();
        let mut named_outputs = HashMap::new();
        let mut controls = HashMap::new();

        for name in named_outputs_in {
            let (fanout, control) = Fanout::new();
            named_outputs.insert(name.clone(), fanout);
            controls.insert(Some(name.clone()), control);
            named_buffers.insert(name.clone(), Vec::new());
        }

        let (primary_output, control) = Fanout::new();
        let me = Self {
            primary_buffer: Vec::with_capacity(TRANSFORM_BATCH_SIZE),
            named_buffers,
            primary_output,
            named_outputs,
        };
        controls.insert(None, control);

        (me, controls)
    }

    fn append(&mut self, slice: &mut Vec<Event>) {
        self.primary_buffer.append(slice)
    }

    fn append_named(&mut self, name: &str, slice: &mut Vec<Event>) {
        self.named_buffers
            .get_mut(name)
            .expect("unknown output")
            .append(slice)
    }

    fn len(&self) -> usize {
        self.primary_buffer.len()
            + self
                .named_buffers
                .iter()
                .map(|(_, buf)| buf.len())
                .sum::<usize>()
    }

    async fn flush(&mut self) {
        flush_inner(&mut self.primary_buffer, &mut self.primary_output).await;
        for (key, buf) in self.named_buffers.iter_mut() {
            flush_inner(
                buf,
                self.named_outputs.get_mut(key).expect("unknown output"),
            )
            .await;
        }
    }
}

async fn flush_inner(buf: &mut Vec<Event>, output: &mut Fanout) {
    for event in buf.drain(..) {
        output.feed(event).await.expect("unit error")
    }
}

impl ByteSizeOf for TransformOutputs {
    fn allocated_bytes(&self) -> usize {
        self.primary_buffer.size_of()
            + self
                .named_buffers
                .iter()
                .map(|(_, buf)| buf.size_of())
                .sum::<usize>()
    }
}

trait SyncTransform: Send + Sync {
    fn run(&mut self, events: Vec<Event>, outputs: &mut TransformOutputs);
}

impl SyncTransform for Box<dyn FallibleFunctionTransform> {
    fn run(&mut self, events: Vec<Event>, outputs: &mut TransformOutputs) {
        let mut buf = Vec::with_capacity(1);
        let mut err_buf = Vec::with_capacity(1);

        for v in events {
            self.transform(&mut buf, &mut err_buf, v);
            outputs.append(&mut buf);
            outputs.append_named("dropped", &mut err_buf);
        }
    }
}

impl SyncTransform for Box<dyn FunctionTransform> {
    fn run(&mut self, events: Vec<Event>, outputs: &mut TransformOutputs) {
        let mut buf = Vec::with_capacity(4); // also an arbitrary,
                                             // smallish constant
        for v in events {
            self.transform(&mut buf, v);
            outputs.append(&mut buf);
        }
    }
}

fn build_task_transform(
    t: Box<dyn TaskTransform>,
    input_rx: BufferStream<Event>,
    input_type: DataType,
    typetag: &str,
    key: &ComponentKey,
) -> (Task, HashMap<OutputId, fanout::ControlChannel>) {
    let (output, control) = Fanout::new();

    let input_rx = crate::utilization::wrap(Pin::new(input_rx));

    let filtered = input_rx
        .filter(move |event| ready(filter_event_type(event, input_type)))
        .inspect(|event| {
            emit!(&EventsReceived {
                count: 1,
                byte_size: event.size_of(),
            })
        });
    let transform = t
        .transform(Box::pin(filtered))
        .map(Ok)
        .forward(output.with(|event: Event| async {
            emit!(&EventsSent {
                count: 1,
                byte_size: event.size_of(),
            });
            Ok(event)
        }))
        .boxed()
        .map_ok(|_| {
            debug!("Finished.");
            TaskOutput::Transform
        });

    let mut outputs = HashMap::new();
    outputs.insert(OutputId::from(key), control);

    let task = Task::new(key.clone(), typetag, transform);

    (task, outputs)
}

fn build_sync_transform(
    mut t: Box<dyn SyncTransform>,
    input_rx: BufferStream<Event>,
    input_type: DataType,
    typetag: &str,
    key: &ComponentKey,
    named_outputs: Vec<String>,
) -> (Task, HashMap<OutputId, fanout::ControlChannel>) {
    let (mut outputs, controls) = TransformOutputs::new(named_outputs);

    let mut input_rx = input_rx
        .filter(move |event| ready(filter_event_type(event, input_type)))
        .ready_chunks(TRANSFORM_BATCH_SIZE);

    let mut timer = crate::utilization::Timer::new();
    let mut last_report = Instant::now();

    let transform = async move {
        timer.start_wait();
        while let Some(events) = input_rx.next().await {
            let stopped = timer.stop_wait();
            if stopped.duration_since(last_report).as_secs() >= 5 {
                timer.report();
                last_report = stopped;
            }

            emit!(&EventsReceived {
                count: events.len(),
                byte_size: events.size_of(),
            });

            t.run(events, &mut outputs);

            // TODO: account for named outputs separately?
            let count = outputs.len();
            // TODO: do we only want allocated_bytes for events themselves?
            let byte_size = outputs.size_of();

            timer.start_wait();
            outputs.flush().await;

            emit!(&EventsSent { count, byte_size });
        }

        debug!("Finished.");
        Ok(TaskOutput::Transform)
    }
    .boxed();

    let mut outputs = HashMap::new();
    for (name, control) in controls {
        match name {
            None => {
                outputs.insert(OutputId::from(key), control);
            }
            Some(name) => {
                outputs.insert(OutputId::from((key, name)), control);
            }
        }
    }

    let task = Task::new(key.clone(), typetag, transform);

    (task, outputs)
}
