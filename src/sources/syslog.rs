use super::util::{SocketListenAddr, TcpSource};
#[cfg(unix)]
use crate::sources::util::build_unix_source;
use crate::{
    config::{
        log_schema, DataType, GenerateConfig, GlobalOptions, Resource, SourceConfig,
        SourceDescription,
    },
    event::{Event, LookupBuf, SegmentBuf, Value},
    internal_events::{SyslogEventReceived, SyslogUdpReadError, SyslogUdpUtf8Error},
    shutdown::ShutdownSignal,
    tls::{MaybeTlsSettings, TlsConfig},
    Pipeline,
};
use bytes::{Buf, Bytes, BytesMut};
use chrono::{Datelike, Utc};
use derive_is_enum_variant::is_enum_variant;
use futures::{compat::Sink01CompatExt, StreamExt};
use futures01::Sink;
use serde::{Deserialize, Serialize};
use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::PathBuf;
use syslog_loose::{IncompleteDate, Message, ProcId, Protocol};
use tokio::net::UdpSocket;
use tokio_util::{
    codec::{BytesCodec, Decoder, LinesCodec, LinesCodecError},
    udp::UdpFramed,
};

#[derive(Deserialize, Serialize, Debug)]
// TODO: add back when serde-rs/serde#1358 is addressed
// #[serde(deny_unknown_fields)]
pub struct SyslogConfig {
    #[serde(flatten)]
    pub mode: Mode,
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    /// The host key of the log. (This differs from `hostname`)
    pub host_key: Option<LookupBuf>,
}

#[derive(Deserialize, Serialize, Debug, Clone, is_enum_variant)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Mode {
    Tcp {
        address: SocketListenAddr,
        tls: Option<TlsConfig>,
    },
    Udp {
        address: SocketAddr,
    },
    #[cfg(unix)]
    Unix {
        path: PathBuf,
    },
}

pub fn default_max_length() -> usize {
    bytesize::kib(100u64) as usize
}

impl SyslogConfig {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            host_key: None,
            max_length: default_max_length(),
        }
    }
}

inventory::submit! {
    SourceDescription::new::<SyslogConfig>("syslog")
}

lazy_static::lazy_static! {
    static ref HOSTNAME_LOOKUP: LookupBuf = LookupBuf::from("hostname");
    static ref SEVERITY_LOOKUP: LookupBuf = LookupBuf::from("severity");
    static ref FACILITY_LOOKUP: LookupBuf = LookupBuf::from("facility");
    static ref VERSION_LOOKUP: LookupBuf = LookupBuf::from("version");
    static ref APPNAME_LOOKUP: LookupBuf = LookupBuf::from("appname");
    static ref MSGID_LOOKUP: LookupBuf = LookupBuf::from("msgid");
    static ref PROCID_LOOKUP: LookupBuf = LookupBuf::from("procid");
    static ref SOURCE_IP_LOOKUP: LookupBuf = LookupBuf::from("source_ip");
}

impl GenerateConfig for SyslogConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            mode: Mode::Tcp {
                address: SocketListenAddr::SocketAddr("0.0.0.0:514".parse().unwrap()),
                tls: None,
            },
            host_key: None,
            max_length: default_max_length(),
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "syslog")]
impl SourceConfig for SyslogConfig {
    async fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        let host_key = self
            .host_key
            .clone()
            .unwrap_or_else(|| log_schema().host_key().clone());

        match self.mode.clone() {
            Mode::Tcp { address, tls } => {
                let source = SyslogTcpSource {
                    max_length: self.max_length,
                    host_key,
                };
                let shutdown_secs = 30;
                let tls = MaybeTlsSettings::from_config(&tls, true)?;
                source.run(address, shutdown_secs, tls, shutdown, out)
            }
            Mode::Udp { address } => Ok(udp(address, self.max_length, host_key, shutdown, out)),
            #[cfg(unix)]
            Mode::Unix { path } => Ok(build_unix_source(
                path,
                SyslogDecoder::new(self.max_length),
                host_key,
                shutdown,
                out,
                event_from_str,
            )),
        }
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "syslog"
    }

    fn resources(&self) -> Vec<Resource> {
        match self.mode.clone() {
            Mode::Tcp { address, .. } => vec![address.into()],
            Mode::Udp { address } => vec![address.into()],
            #[cfg(unix)]
            Mode::Unix { .. } => vec![],
        }
    }
}

#[derive(Debug, Clone)]
struct SyslogTcpSource {
    max_length: usize,
    host_key: LookupBuf,
}

impl TcpSource for SyslogTcpSource {
    type Error = LinesCodecError;
    type Decoder = SyslogDecoder;

    fn decoder(&self) -> Self::Decoder {
        SyslogDecoder::new(self.max_length)
    }

    fn build_event(&self, frame: String, host: Bytes) -> Option<Event> {
        event_from_str(self.host_key.clone(), Some(host), &frame)
    }
}

/// Decodes according to `Octet Counting` in https://tools.ietf.org/html/rfc6587
#[derive(Clone, Debug)]
struct SyslogDecoder {
    other: LinesCodec,
}

impl SyslogDecoder {
    fn new(max_length: usize) -> Self {
        Self {
            other: LinesCodec::new_with_max_length(max_length),
        }
    }

    fn octet_decode(&self, src: &mut BytesMut) -> Result<Option<String>, LinesCodecError> {
        // Encoding scheme:
        //
        // len ' ' data
        // |    |  | len number of bytes that contain syslog message
        // |    |
        // |    | Separating whitespace
        // |
        // | ASCII decimal number of unknown length

        if let Some(i) = src.iter().position(|&b| b == b' ') {
            let len: usize = std::str::from_utf8(&src[..i])
                .map_err(|_| ())
                .and_then(|num| num.parse().map_err(|_| ()))
                .map_err(|_| {
                    LinesCodecError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unable to decode message len as number",
                    ))
                })?;

            let from = i + 1;
            let to = from + len;

            if let Some(msg) = src.get(from..to) {
                let s = std::str::from_utf8(msg)
                    .map_err(|_| {
                        LinesCodecError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Unable to decode message as UTF8",
                        ))
                    })?
                    .to_string();
                src.advance(to);
                Ok(Some(s))
            } else {
                Ok(None)
            }
        } else if src.len() < self.other.max_length() {
            Ok(None)
        } else {
            // This is certainly malformed, and there is no recovering from this.
            Err(LinesCodecError::Io(io::Error::new(
                io::ErrorKind::Other,
                "Frame length limit exceeded",
            )))
        }
    }

    /// None if this is not octet counting encoded
    fn checked_decode(
        &self,
        src: &mut BytesMut,
    ) -> Option<Result<Option<String>, LinesCodecError>> {
        if let Some(&first_byte) = src.get(0) {
            if 49 <= first_byte && first_byte <= 57 {
                // First character is non zero number so we can assume that
                // octet count framing is used.
                trace!("Octet counting encoded event detected.");
                return Some(self.octet_decode(src));
            }
        }
        None
    }
}

impl Decoder for SyslogDecoder {
    type Item = String;
    type Error = LinesCodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(ret) = self.checked_decode(src) {
            ret
        } else {
            // Octet counting isn't used so fallback to newline codec.
            self.other.decode(src)
        }
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(ret) = self.checked_decode(buf) {
            ret
        } else {
            // Octet counting isn't used so fallback to newline codec.
            self.other.decode_eof(buf)
        }
    }
}

pub fn udp(
    addr: SocketAddr,
    _max_length: usize,
    host_key: LookupBuf,
    shutdown: ShutdownSignal,
    out: Pipeline,
) -> super::Source {
    let out = out.sink_map_err(|error| error!(message = "Error sending line.", %error));

    Box::pin(async move {
        let socket = UdpSocket::bind(&addr)
            .await
            .expect("Failed to bind to UDP listener socket");
        info!(
            message = "Listening.",
            addr = %addr,
            r#type = "udp"
        );

        let _ = UdpFramed::new(socket, BytesCodec::new())
            .take_until(shutdown)
            .filter_map(|frame| {
                let host_key = host_key.clone();
                async move {
                    match frame {
                        Ok((bytes, received_from)) => {
                            let received_from = received_from.ip().to_string().into();

                            std::str::from_utf8(&bytes)
                                .map_err(|error| emit!(SyslogUdpUtf8Error { error }))
                                .ok()
                                .and_then(|s| {
                                    event_from_str(host_key, Some(received_from), s).map(Ok)
                                })
                        }
                        Err(error) => {
                            emit!(SyslogUdpReadError { error });
                            None
                        }
                    }
                }
            })
            .forward(out.sink_compat())
            .await;

        info!("Finished sending.");
        Ok(())
    })
}

/// Function used to resolve the year for syslog messages that don't include the year.
/// If the current month is January, and the syslog message is for December, it will take the previous year.
/// Otherwise, take the current year.
fn resolve_year((month, _date, _hour, _min, _sec): IncompleteDate) -> i32 {
    let now = Utc::now();
    if now.month() == 1 && month == 12 {
        now.year() - 1
    } else {
        now.year()
    }
}

/**
* Function to pass to build_unix_source, specific to the Unix mode of the syslog source.
* Handles the logic of parsing and decoding the syslog message format.
**/
// TODO: many more cases to handle:
// octet framing (i.e. num bytes as ascii string prefix) with and without delimiters
// null byte delimiter in place of newline
fn event_from_str(host_key: LookupBuf, default_host: Option<Bytes>, line: &str) -> Option<Event> {
    let line = line.trim();
    let parsed = syslog_loose::parse_message_with_year(line, resolve_year);
    let mut event = Event::from(&parsed.msg[..]);

    // Add source type
    event.as_mut_log().insert(
        log_schema().source_type_key().clone(),
        Bytes::from("syslog"),
    );

    if let Some(default_host) = default_host.clone() {
        event
            .as_mut_log()
            .insert(SOURCE_IP_LOOKUP.clone(), default_host);
    }

    let parsed_hostname = parsed.hostname.map(|x| Bytes::from(x.to_owned()));
    if let Some(parsed_host) = parsed_hostname.or(default_host) {
        event.as_mut_log().insert(host_key, parsed_host);
    }

    let timestamp = parsed
        .timestamp
        .map(|ts| ts.into())
        .unwrap_or_else(Utc::now);
    event
        .as_mut_log()
        .insert(log_schema().timestamp_key().clone(), timestamp);

    insert_fields_from_syslog(&mut event, parsed);

    emit!(SyslogEventReceived {
        byte_size: line.len()
    });

    trace!(
        message = "Processing one event.",
        event = ?event
    );

    Some(event)
}

fn insert_fields_from_syslog(event: &mut Event, parsed: Message<&str>) {
    let log = event.as_mut_log();

    if let Some(host) = parsed.hostname {
        log.insert(HOSTNAME_LOOKUP.clone(), host.to_string());
    }
    if let Some(severity) = parsed.severity {
        log.insert(SEVERITY_LOOKUP.clone(), severity.as_str().to_owned());
    }
    if let Some(facility) = parsed.facility {
        log.insert(FACILITY_LOOKUP.clone(), facility.as_str().to_owned());
    }
    if let Protocol::RFC5424(version) = parsed.protocol {
        log.insert(VERSION_LOOKUP.clone(), version as i64);
    }
    if let Some(app_name) = parsed.appname {
        log.insert(APPNAME_LOOKUP.clone(), app_name.to_owned());
    }
    if let Some(msg_id) = parsed.msgid {
        log.insert(MSGID_LOOKUP.clone(), msg_id.to_owned());
    }
    if let Some(procid) = parsed.procid {
        let value: Value = match procid {
            ProcId::PID(pid) => pid.into(),
            ProcId::Name(name) => name.to_string().into(),
        };
        log.insert(PROCID_LOOKUP.clone(), value);
    }

    for element in parsed.structured_data.into_iter() {
        let element_lookup =
            LookupBuf::from_str(element.id).unwrap_or_else(|_| LookupBuf::from(element.id));
        for (name, value) in element.params.into_iter() {
            let mut key_lookup = element_lookup.clone();
            key_lookup.push(SegmentBuf::from(name.to_string()));
            log.insert(key_lookup, value.to_string());
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        config::log_schema,
        event::{Event, Segment},
    };
    use chrono::prelude::*;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<SyslogConfig>();
    }

    #[test]
    fn config_tcp() {
        let config: SyslogConfig = toml::from_str(
            r#"
            mode = "tcp"
            address = "127.0.0.1:1235"
          "#,
        )
        .unwrap();
        assert!(config.mode.is_tcp());
    }

    #[test]
    fn config_udp() {
        let config: SyslogConfig = toml::from_str(
            r#"
            mode = "udp"
            address = "127.0.0.1:1235"
            max_length = 32187
          "#,
        )
        .unwrap();
        assert!(config.mode.is_udp());
    }

    #[cfg(unix)]
    #[test]
    fn config_unix() {
        let config: SyslogConfig = toml::from_str(
            r#"
            mode = "unix"
            path = "127.0.0.1:1235"
          "#,
        )
        .unwrap();
        assert!(config.mode.is_unix());
    }

    #[test]
    fn syslog_ng_network_syslog_protocol() {
        // this should also match rsyslog omfwd with template=RSYSLOG_SyslogProtocol23Format
        let msg = "i am foobar";
        let raw = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {}{} {}"#,
            r#"[meta sequenceId="1" sysUpTime="37" language="EN"]"#,
            r#"[origin ip="192.168.0.1" software="test"]"#,
            msg
        );

        let mut expected = Event::from(msg);

        {
            let expected = expected.as_mut_log();
            expected.insert(
                log_schema().timestamp_key().clone(),
                chrono::Utc.ymd(2019, 2, 13).and_hms(19, 48, 34),
            );
            expected.insert(log_schema().source_type_key().clone(), "syslog");
            expected.insert(LookupBuf::from_str("host").unwrap(), "74794bfb6795");
            expected.insert(LookupBuf::from_str("hostname").unwrap(), "74794bfb6795");

            expected.insert(LookupBuf::from_str("meta.sequenceId").unwrap(), "1");
            expected.insert(LookupBuf::from_str("meta.sysUpTime").unwrap(), "37");
            expected.insert(LookupBuf::from_str("meta.language").unwrap(), "EN");
            expected.insert(LookupBuf::from_str("origin.software").unwrap(), "test");
            expected.insert(LookupBuf::from_str("origin.ip").unwrap(), "192.168.0.1");

            expected.insert(LookupBuf::from_str("severity").unwrap(), "notice");
            expected.insert(LookupBuf::from_str("facility").unwrap(), "user");
            expected.insert(LookupBuf::from_str("version").unwrap(), 1);
            expected.insert(LookupBuf::from_str("appname").unwrap(), "root");
            expected.insert(LookupBuf::from_str("procid").unwrap(), 8449);
        }

        assert_eq!(
            event_from_str(LookupBuf::from("host"), None, &raw).unwrap(),
            expected
        );
    }

    #[test]
    fn handles_incorrect_sd_element() {
        crate::test_util::trace_init();
        let msg = "qwerty";
        let raw = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {} {}"#,
            r#"[incorrect x]"#, msg
        );

        let mut expected = Event::from(msg);
        {
            let expected = expected.as_mut_log();
            expected.insert(
                log_schema().timestamp_key().clone(),
                chrono::Utc.ymd(2019, 2, 13).and_hms(19, 48, 34),
            );
            expected.insert(log_schema().host_key().clone(), "74794bfb6795");
            expected.insert(LookupBuf::from("hostname"), "74794bfb6795");
            expected.insert(log_schema().source_type_key().clone(), "syslog");
            expected.insert(LookupBuf::from("severity"), "notice");
            expected.insert(LookupBuf::from("facility"), "user");
            expected.insert(LookupBuf::from("version"), 1);
            expected.insert(LookupBuf::from("appname"), "root");
            expected.insert(LookupBuf::from("procid"), 8449);
        }

        let event = event_from_str(LookupBuf::from("host"), None, &raw);
        assert_eq!(event, Some(expected.clone()));

        let raw = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {} {}"#,
            r#"[incorrect x=]"#, msg
        );

        let event = event_from_str(LookupBuf::from("host"), None, &raw);
        assert_eq!(event, Some(expected));
    }

    #[test]
    fn handles_empty_sd_element() {
        crate::test_util::trace_init();
        fn there_is_map_called_empty(event: Event) -> bool {
            event
                .as_log()
                .pairs(true)
                .find(|(key, _)| (&key[..]).starts_with([Segment::field("empty")].as_ref()))
                == None
        }

        let msg = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {} qwerty"#,
            r#"[empty]"#
        );

        let event = event_from_str(LookupBuf::from("host"), None, &msg).unwrap();
        assert!(there_is_map_called_empty(event));

        let msg = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {} qwerty"#,
            r#"[non_empty x="1"][empty]"#
        );

        let event = event_from_str(LookupBuf::from("host"), None, &msg).unwrap();
        assert!(there_is_map_called_empty(event));

        let msg = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {} qwerty"#,
            r#"[empty][non_empty x="1"]"#
        );

        let event = event_from_str(LookupBuf::from("host"), None, &msg).unwrap();
        assert!(there_is_map_called_empty(event));

        let msg = format!(
            r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - {} qwerty"#,
            r#"[empty not_really="testing the test"]"#
        );

        let event = event_from_str(LookupBuf::from("host"), None, &msg).unwrap();
        assert!(!there_is_map_called_empty(event));
    }

    #[test]
    fn handles_weird_whitespace() {
        crate::test_util::trace_init();
        // this should also match rsyslog omfwd with template=RSYSLOG_SyslogProtocol23Format
        let raw = r#"
            <13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - [meta sequenceId="1"] i am foobar
            "#;
        let cleaned = r#"<13>1 2019-02-13T19:48:34+00:00 74794bfb6795 root 8449 - [meta sequenceId="1"] i am foobar"#;

        assert_eq!(
            event_from_str(LookupBuf::from("host"), None, raw).unwrap(),
            event_from_str(LookupBuf::from("host"), None, cleaned).unwrap()
        );
    }

    #[test]
    fn syslog_ng_default_network() {
        crate::test_util::trace_init();
        let msg = "i am foobar";
        let raw = format!(r#"<13>Feb 13 20:07:26 74794bfb6795 root[8539]: {}"#, msg);

        let mut expected = Event::from(msg);
        {
            let expected = expected.as_mut_log();
            let expected_date: DateTime<Utc> =
                chrono::Local.ymd(2020, 2, 13).and_hms(20, 7, 26).into();
            expected.insert(log_schema().timestamp_key().clone(), expected_date);
            expected.insert(log_schema().host_key().clone(), "74794bfb6795");
            expected.insert(log_schema().source_type_key().clone(), "syslog");
            expected.insert(LookupBuf::from("hostname"), "74794bfb6795");
            expected.insert(LookupBuf::from("severity"), "notice");
            expected.insert(LookupBuf::from("facility"), "user");
            expected.insert(LookupBuf::from("appname"), "root");
            expected.insert(LookupBuf::from("procid"), 8539);
        }

        assert_eq!(
            event_from_str(LookupBuf::from("host"), None, &raw).unwrap(),
            expected
        );
    }

    #[test]
    fn rsyslog_omfwd_tcp_default() {
        crate::test_util::trace_init();
        let msg = "start";
        let raw = format!(
            r#"<190>Feb 13 21:31:56 74794bfb6795 liblogging-stdlog:  [origin software="rsyslogd" swVersion="8.24.0" x-pid="8979" x-info="http://www.rsyslog.com"] {}"#,
            msg
        );

        let mut expected = Event::from(msg);
        {
            let expected = expected.as_mut_log();
            let expected_date: DateTime<Utc> =
                chrono::Local.ymd(2020, 2, 13).and_hms(21, 31, 56).into();
            expected.insert(log_schema().timestamp_key().clone(), expected_date);
            expected.insert(log_schema().source_type_key().clone(), "syslog");
            expected.insert(LookupBuf::from_str("host").unwrap(), "74794bfb6795");
            expected.insert(LookupBuf::from_str("hostname").unwrap(), "74794bfb6795");
            expected.insert(LookupBuf::from_str("severity").unwrap(), "info");
            expected.insert(LookupBuf::from_str("facility").unwrap(), "local7");
            expected.insert(LookupBuf::from_str("appname").unwrap(), "liblogging-stdlog");
            expected.insert(LookupBuf::from_str("origin.software").unwrap(), "rsyslogd");
            expected.insert(LookupBuf::from_str("origin.swVersion").unwrap(), "8.24.0");
            expected.insert(LookupBuf::from_str("origin.x-pid").unwrap(), "8979");
            expected.insert(LookupBuf::from_str("origin.x-info").unwrap(), "http://www.rsyslog.com");
        }

        assert_eq!(
            event_from_str(LookupBuf::from("host"), None, &raw).unwrap(),
            expected
        );
    }

    #[test]
    fn rsyslog_omfwd_tcp_forward_format() {
        crate::test_util::trace_init();
        let msg = "start";
        let raw = format!(
            r#"<190>2019-02-13T21:53:30.605850+00:00 74794bfb6795 liblogging-stdlog:  [origin software="rsyslogd" swVersion="8.24.0" x-pid="9043" x-info="http://www.rsyslog.com"] {}"#,
            msg
        );

        let mut expected = Event::from(msg);
        {
            let expected = expected.as_mut_log();
            expected.insert(
                log_schema().timestamp_key().clone(),
                chrono::Utc
                    .ymd(2019, 2, 13)
                    .and_hms_micro(21, 53, 30, 605_850),
            );
            expected.insert(log_schema().source_type_key().clone(), "syslog");
            expected.insert(LookupBuf::from_str("host").unwrap(), "74794bfb6795");
            expected.insert(LookupBuf::from_str("hostname").unwrap(), "74794bfb6795");
            expected.insert(LookupBuf::from_str("severity").unwrap(), "info");
            expected.insert(LookupBuf::from_str("facility").unwrap(), "local7");
            expected.insert(LookupBuf::from_str("appname").unwrap(), "liblogging-stdlog");
            expected.insert(LookupBuf::from_str("origin.software").unwrap(), "rsyslogd");
            expected.insert(LookupBuf::from_str("origin.swVersion").unwrap(), "8.24.0");
            expected.insert(LookupBuf::from_str("origin.x-pid").unwrap(), "9043");
            expected.insert(LookupBuf::from_str("origin.x-info").unwrap(), "http://www.rsyslog.com");
        }

        assert_eq!(
            event_from_str(LookupBuf::from("host"), None, &raw).unwrap(),
            expected
        );
    }
}
