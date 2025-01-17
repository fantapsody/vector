use criterion::criterion_main;

mod batch;
mod event;
mod files;
mod http;
mod lua;
mod metrics_snapshot;
mod regex;
mod template;

criterion_main!(
    batch::benches,
    event::benches,
    files::benches,
    http::benches,
    lua::benches,
    metrics_snapshot::benches,
    regex::benches,
    template::benches,
);
