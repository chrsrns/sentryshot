// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(clippy::module_name_repetitions)]

use crate::{
    ArcMsgLogger, ArcStreamerMuxer, DynError, Event, H264Data, MonitorId, MonitorName, StreamType,
    TrackParameters,
    recording::{FrameRateLimiter, FrameRateLimiterError},
    time::{Duration, MINUTE, UnixH264},
};
use async_trait::async_trait;
use sentryshot_ffmpeg_h264::{H264BuilderError, ReceiveFrameError, SendPacketError};
use sentryshot_util::Frame;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::{collections::HashMap, ops::Deref, path::PathBuf, str::FromStr, sync::Arc};
use thiserror::Error;
use tokio::{
    runtime::Handle,
    sync::{broadcast, mpsc},
};
use tokio_util::sync::CancellationToken;
use url::Url;

#[allow(clippy::module_name_repetitions)]
pub type MonitorConfigs = HashMap<MonitorId, MonitorConfig>;

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::module_name_repetitions)]
pub struct MonitorConfig {
    config: Config,
    source: SourceConfig,

    // Raw json used for unknown values.
    raw: serde_json::Value,
}

impl MonitorConfig {
    #[must_use]
    pub fn new(config: Config, source: SourceConfig, raw: serde_json::Value) -> Self {
        Self {
            config,
            source,
            raw,
        }
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }
    #[must_use]
    pub fn source(&self) -> &SourceConfig {
        &self.source
    }
    #[must_use]
    pub fn raw(&self) -> &serde_json::Value {
        &self.raw
    }
    pub fn raw_mut(&mut self) -> &mut serde_json::Value {
        &mut self.raw
    }

    /*
    // Get config value by key.
    func (c Config) Get(key string) string {
        return c.v[key]
    }
    */

    // Monitor ID.
    #[must_use]
    pub fn id(&self) -> &MonitorId {
        &self.config.id
    }

    // Name returns the monitor name.
    #[must_use]
    pub fn name(&self) -> &MonitorName {
        &self.config.name
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enable
    }

    #[must_use]
    pub fn has_sub_stream(&self) -> bool {
        match &self.source {
            SourceConfig::Rtsp(v) => v.sub_stream.is_some(),
        }
    }

    /*
        // video length is seconds.
        func (c Config) videoLength() string {
            return c.v["videoLength"]
        }
    */
    #[must_use]
    pub fn always_record(&self) -> bool {
        self.config.always_record
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    pub fn video_length(&self) -> Duration {
        Duration::from_f64(self.config.video_length * (MINUTE as f64))
    }

    /*
        // TimestampOffset returns the timestamp offset.
        func (c Config) TimestampOffset() string {
            return c.v["timestampOffset"]
        }

        // LogLevel returns the ffmpeg log level.
        func (c Config) LogLevel() string {
            return c.v["logLevel"]
        }
    */
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Config {
    pub id: MonitorId,
    pub name: MonitorName,
    pub enable: bool,
    pub source: SelectedSource,

    #[serde(rename = "alwaysRecord")]
    pub always_record: bool,

    #[serde(rename = "videoLength")]
    pub video_length: f64,

    #[serde(default)]
    pub ptz: PtzConfig,

    #[serde(default)]
    pub audio: AudioConfig,
}

/// PTZ section of a monitor config — persisted under `"ptz"`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PtzConfig {
    #[serde(default)]
    enable: bool,

    #[serde(default, rename = "onvifAddr")]
    onvif_addr: String,

    /// Command names the camera reported `not implemented` — persisted
    /// capability marks (V254). Cleared when the endpoint changes.
    #[serde(default)]
    unsupported: Vec<String>,
}

/// Two-way audio section of a monitor config — persisted under
/// `"audio"`. Absent = disabled; pre-feature configs deserialize
/// unchanged via `#[serde(default)]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct AudioConfig {
    #[serde(default)]
    enable: bool,

    /// Capability names the camera disproved — persisted marks
    /// written by audio ops on capability faults only; transport
    /// faults never land here.
    #[serde(default)]
    unsupported: Vec<String>,

    /// V380 numeric cloud device ID as a decimal string —
    /// lenient-parsed at use (malformed ⇒ unset, ⊥ deser fail).
    /// Overrides the ONVIF-serial derivation when set.
    #[serde(default, rename = "v380DeviceId", deserialize_with = "de_loose_scalar")]
    v380_device_id: String,

    /// V380 app-account username — case-sensitive (`Admin` on
    /// v3-era firmware). Wins the username candidate order.
    #[serde(default, rename = "v380User", deserialize_with = "de_loose_str")]
    v380_user: String,
}

/// Lenient scalar deser — a JSON string or number becomes its
/// string form; anything else ⇒ empty (unset). A malformed value
/// can never fail monitor-config load.
fn de_loose_scalar<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// Lenient string deser — a JSON string passes, anything else ⇒
/// empty (unset).
fn de_loose_str<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        _ => String::new(),
    })
}

impl AudioConfig {
    #[must_use]
    pub fn new(enable: bool) -> Self {
        Self {
            enable,
            unsupported: Vec::new(),
            v380_device_id: String::new(),
            v380_user: String::new(),
        }
    }

    #[must_use]
    pub fn enable(&self) -> bool {
        self.enable
    }

    #[must_use]
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    /// Configured V380 device ID — the decimal string parsed to
    /// `u32`; empty ∨ malformed ⇒ `None` (unset, never fatal).
    #[must_use]
    pub fn v380_device_id(&self) -> Option<u32> {
        self.v380_device_id.parse().ok()
    }

    /// Configured V380 username — `None` when unset.
    #[must_use]
    pub fn v380_user(&self) -> Option<&str> {
        (!self.v380_user.is_empty()).then_some(self.v380_user.as_str())
    }
}

impl PtzConfig {
    #[must_use]
    pub fn new(enable: bool, onvif_addr: String) -> Self {
        Self {
            enable,
            onvif_addr,
            unsupported: Vec::new(),
        }
    }

    #[must_use]
    pub fn enable(&self) -> bool {
        self.enable
    }

    #[must_use]
    pub fn onvif_addr(&self) -> &str {
        &self.onvif_addr
    }

    #[must_use]
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    /// Save-time validation: empty (derive at command time) or an
    /// absolute `http` URL with a host — no `https`, userinfo, query
    /// or fragment. Config-file load never fails on a bad value.
    #[must_use]
    pub fn onvif_addr_ok(&self) -> bool {
        if self.onvif_addr.is_empty() {
            return true;
        }
        let Ok(url) = url::Url::parse(&self.onvif_addr) else {
            return false;
        };
        url.scheme() == "http"
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    }
}

impl Serialize for MonitorConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MonitorConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let config: Config = serde_json::from_value(value.clone()).map_err(Error::custom)?;

        let sources: SourcesDeserializer =
            serde_json::from_value(value.clone()).map_err(Error::custom)?;

        let source = match config.source {
            SelectedSource::Rtsp => SourceConfig::Rtsp(sources.source_rtsp.map_err(Error::custom)?),
        };

        Ok(Self {
            config,
            source,
            raw: value,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub enum SelectedSource {
    #[serde(rename = "rtsp")]
    Rtsp,
}

#[derive(Deserialize)]
pub struct SourcesDeserializer {
    #[serde(rename = "sourcertsp", deserialize_with = "try_deserialize")]
    pub source_rtsp: Result<SourceRtspConfig, serde_json::Error>,
}

fn try_deserialize<'de, D, T>(deserializer: D) -> Result<Result<T, serde_json::Error>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(v))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceConfig {
    Rtsp(SourceRtspConfig),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SourceRtspConfig {
    pub protocol: Protocol,

    #[serde(rename = "mainStream")]
    pub main_stream: RtspUrl,

    #[serde(rename = "subStream")]
    pub sub_stream: Option<RtspUrl>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub enum Protocol {
    #[serde(rename = "tcp")]
    Tcp,

    #[serde(rename = "udp")]
    Udp,
}

#[derive(Debug, Error)]
pub enum ParseRtspUrl {
    #[error("{0}")]
    ParseUrl(#[from] url::ParseError),

    #[error("bad url '{0}' only scheme rtsp supported")]
    BadScheme(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspUrl(Url);

impl TryFrom<Url> for RtspUrl {
    type Error = ParseRtspUrl;

    fn try_from(url: Url) -> Result<Self, Self::Error> {
        if url.scheme() != "rtsp" {
            return Err(ParseRtspUrl::BadScheme(url.to_string()));
        }
        Ok(Self(url))
    }
}

impl FromStr for RtspUrl {
    type Err = ParseRtspUrl;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::try_from(value)?;
        RtspUrl::try_from(url)
    }
}

impl<'de> Deserialize<'de> for RtspUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(serde::de::Error::custom)
    }
}

impl Deref for RtspUrl {
    type Target = Url;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Error)]
pub enum DecoderError {
    #[error("dropped frames")]
    DroppedFrames,

    #[error("{0}")]
    SendFrame(#[from] SendPacketError),

    #[error("receive frame: {0}")]
    ReceiveFrame(#[from] ReceiveFrameError),

    #[error("try from: {0}")]
    TryFrom(#[from] std::num::TryFromIntError),

    #[error("frame rate limiter: {0}")]
    FrameRateLimiter(#[from] FrameRateLimiterError),
}

#[derive(Debug, Error)]
pub enum SubscribeDecodedError {
    #[error("new video decoder: {0}")]
    NewVideoDecoder(#[from] H264BuilderError),

    #[error("unsupported codec: {0}")]
    UnsupportedCodec(String),
}

// A 'broadcast' channel is used instead of a 'watch' channel to detect dropped frames.
pub type Feed = broadcast::Receiver<H264Data>;
pub type FeedDecoded = mpsc::Receiver<Result<Frame, DecoderError>>;

pub type ArcSource = Arc<dyn Source + Send + Sync>;

#[async_trait]
pub trait Source {
    #[must_use]
    fn stream_type(&self) -> &StreamType;

    // Returns the HLS muxer for this source. Will block until the source has started.
    // Returns None if cancelled.
    async fn muxer(&self) -> Option<ArcStreamerMuxer>;

    // Subscribe to the raw feed. Will block until the source has started.
    async fn subscribe(&self) -> Option<Feed>;

    // Subscribe to a decoded feed. Currently creates a new decoder for each
    // call but this may change. Will block until the source has started.
    // Will close channel when cancelled.
    async fn subscribe_decoded(
        &self,
        rt_handle: Handle,
        logger: ArcMsgLogger,
        limiter: Option<FrameRateLimiter>,
    ) -> Option<Result<FeedDecoded, SubscribeDecodedError>>;

    // Non-blocking snapshot of the running muxer's RFC 6381 codec
    // string. None while the stream is down or absent.
    fn codec(&self) -> Option<String> {
        None
    }
}

pub type ArcMonitor = Arc<dyn MonitorImpl + Send + Sync>;

#[async_trait]
pub trait MonitorImpl {
    fn config(&self) -> &MonitorConfig;

    async fn stop(&self) {}

    // Return sub stream if it exists otherwise returns main stream.
    // Returns None if cancelled
    async fn get_smallest_source(&self) -> Option<ArcSource>;

    // Returns None if cancelled.
    async fn source_main(&self) -> Option<ArcSource>;

    // Returns None if cancelled and Some(None) if sub stream doesn't exist.
    async fn source_sub(&self) -> Option<Option<ArcSource>>;

    async fn trigger(
        &self,
        trigger_duration: Duration,
        event: Event,
    ) -> Result<(), CreateEventDbError>;
}

pub type ArcMonitorHooks = Arc<dyn MonitorHooks + Send + Sync>;

#[async_trait]
pub trait MonitorHooks {
    async fn on_monitor_start(&self, token: CancellationToken, monitor: ArcMonitor);
    // Blocking.
    fn on_thumb_save(&self, config: &MonitorConfig, frame: Frame) -> Frame;
    async fn on_event(&self, event: Event, config: MonitorConfig);
    fn migrate_monitor(&self, _config: &mut Value) -> Result<(), DynError>;
}

pub struct DummyMonitorHooks;

impl DummyMonitorHooks {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> ArcMonitorHooks {
        Arc::new(Self {})
    }
}

#[async_trait]
impl MonitorHooks for DummyMonitorHooks {
    async fn on_monitor_start(&self, _: CancellationToken, _: ArcMonitor) {}
    fn on_thumb_save(&self, _: &MonitorConfig, frame: Frame) -> Frame {
        frame
    }
    async fn on_event(&self, _: Event, _: MonitorConfig) {}
    fn migrate_monitor(&self, _config: &mut Value) -> Result<(), DynError> {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum MonitorRestartError {
    #[error("monitor does not exist '{0}'")]
    NotExist(MonitorId),
}

#[derive(Debug, Error)]
pub enum MonitorSetError {
    #[error("serialize config: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("write config to file: {0}")]
    WriteFile(std::io::Error),

    #[error("invalid ptz onvifAddr — empty or absolute http URL without userinfo/query/fragment")]
    InvalidPtzAddr,
}

#[derive(Debug, Error)]
pub enum MonitorSetAndRestartError {
    #[error(transparent)]
    Set(MonitorSetError),

    #[error(transparent)]
    Restart(MonitorRestartError),
}

#[derive(Debug, Error)]
pub enum MonitorDeleteError {
    #[error("monitor does not exist '{0}'")]
    NotExist(String),

    #[error("remove file: {0}")]
    RemoveFile(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum CreateEventDbError {
    #[error("create eventdb directory: {0} {1}")]
    CreateDir(PathBuf, std::io::Error),
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub name: MonitorName,
    pub enable: bool,

    #[serde(rename = "hasSubStream")]
    pub has_sub_stream: bool,

    // RFC 6381 codec of the running main/sub stream, e.g. "hvc1.1.6.L93.B0".
    // Absent while the stream is down or doesn't exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,

    #[serde(rename = "subCodec", skip_serializing_if = "Option::is_none")]
    pub sub_codec: Option<String>,
}

impl MonitorInfo {
    #[must_use]
    pub fn new(
        id: MonitorId,
        name: MonitorName,
        enable: bool,
        has_sub_stream: bool,
        codec: Option<String>,
        sub_codec: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            enable,
            has_sub_stream,
            codec,
            sub_codec,
        }
    }
}

pub type ArcMonitorManager = Arc<dyn IMonitorManager + Send + Sync>;

#[async_trait]
pub trait IMonitorManager {
    async fn monitor_restart(
        &self,
        monitor_id: MonitorId,
    ) -> Option<Result<(), MonitorRestartError>>;

    async fn monitor_set(&self, config: MonitorConfig) -> Option<Result<bool, MonitorSetError>>;
    async fn monitor_set_and_restart(
        &self,
        config: MonitorConfig,
    ) -> Option<Result<bool, MonitorSetAndRestartError>>;

    async fn monitor_delete(&self, id: MonitorId) -> Option<Result<(), MonitorDeleteError>>;
    async fn monitors_info(&self) -> Option<HashMap<MonitorId, MonitorInfo>>;
    async fn monitor_config(&self, monitor_id: MonitorId) -> Option<Option<MonitorConfig>>;
    async fn monitor_configs(&self) -> Option<MonitorConfigs>;

    /// Raw-patch removal of `names` from `audio.unsupported` —
    /// the server-owned unmark path for composed-direction healing.
    /// NOT a `monitor_set` RMW: `union_audio_marks` would resurrect
    /// a reduced set. Atomic-write + configs update, actor-serial.
    async fn monitor_unmark_audio(
        &self,
        id: MonitorId,
        names: Vec<String>,
    ) -> Option<Result<(), MonitorSetError>>;
}

#[async_trait]
pub trait H264WriterImpl {
    // TODO: replace &mut with &.
    async fn write_h264(&mut self, data: H264Data) -> Result<(), DynError>;
}

pub type DynH264Writer = Box<dyn H264WriterImpl + Send>;

#[async_trait]
pub trait StreamerImpl {
    // Creates muxer and returns a H264Writer to it.
    // Stops and replaces existing muxer if present.
    // Returns None if cancelled.
    async fn new_muxer(
        &self,
        token: CancellationToken,
        monitor_id: MonitorId,
        sub_stream: bool,
        params: TrackParameters,
        start_time: UnixH264,
        first_sample: H264Data,
    ) -> Result<Option<(ArcStreamerMuxer, DynH264Writer)>, DynError>;
}

pub type ArcStreamer = Arc<dyn StreamerImpl + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn minimal_monitor_json() -> serde_json::Value {
        serde_json::json!({
            "id": "m1",
            "name": "cam",
            "enable": true,
            "source": "rtsp",
            "sourcertsp": {
                "protocol": "tcp",
                "mainStream": "rtsp://x"
            },
            "alwaysRecord": false,
            "videoLength": 15
        })
    }

    #[test]
    fn ptz_defaults_when_absent() {
        let cfg: MonitorConfig =
            serde_json::from_value(minimal_monitor_json()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(PtzConfig::default(), cfg.config().ptz);
    }

    #[test]
    fn ptz_round_trips() {
        let mut v = minimal_monitor_json();
        v["ptz"] =
            serde_json::json!({"enable": true, "onvifAddr": "http://cam/onvif/device_service"});
        let cfg: MonitorConfig =
            serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert!(cfg.config().ptz.enable());
        assert_eq!(
            "http://cam/onvif/device_service",
            cfg.config().ptz.onvif_addr()
        );
        // Raw passthrough preserves the section verbatim.
        assert_eq!(v, *cfg.raw());
    }

    #[test]
    fn raw_round_trips_unknown_keys() {
        let mut v = minimal_monitor_json();
        v["unknownPluginField"] = serde_json::json!({"nested": [1, "two", null]});
        v["customFlag"] = serde_json::json!(true);

        let cfg: MonitorConfig =
            serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}"));

        // Unknown keys survive the deserialize → serialize round-trip verbatim.
        assert_eq!(v, *cfg.raw());
        assert_eq!(v, serde_json::to_value(&cfg).unwrap_or_else(|e| panic!("{e}")));
    }

    #[test]
    fn raw_round_trips_typed_fields() {
        let v = minimal_monitor_json();
        let cfg: MonitorConfig =
            serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}"));

        // Parsed fields agree with the raw document.
        assert_eq!("m1", &**cfg.id());
        assert_eq!("cam", cfg.name().to_string());
        assert!(cfg.enabled());
        assert!(!cfg.always_record());
        assert_eq!(v, serde_json::to_value(&cfg).unwrap_or_else(|e| panic!("{e}")));
    }

    #[test]
    fn onvif_addr_validation() {
        let ok = |a: &str| PtzConfig::new(false, a.to_owned()).onvif_addr_ok();
        assert!(ok(""));
        assert!(ok("http://cam"));
        assert!(ok("http://cam:8080/onvif/device_service"));
        assert!(!ok("https://cam"));
        assert!(!ok("http://u:p@cam"));
        assert!(!ok("http://cam/x?y=1"));
        assert!(!ok("http://cam/x#f"));
        assert!(!ok("rtsp://cam"));
        assert!(!ok("not a url"));
    }

    #[test]
    fn audio_v380_fields_lenient() {
        let parse = |audio: serde_json::Value| {
            let mut v = minimal_monitor_json();
            v["audio"] = audio;
            serde_json::from_value::<MonitorConfig>(v)
                .unwrap_or_else(|e| panic!("{e}"))
                .config()
                .audio
                .clone()
        };
        let a = parse(serde_json::json!(
            {"v380DeviceId": "46078507", "v380User": "bob"}
        ));
        assert_eq!(Some(46_078_507), a.v380_device_id());
        assert_eq!(Some("bob"), a.v380_user());
        // A JSON number is lenient-parsed too.
        assert_eq!(
            Some(46_078_507),
            parse(serde_json::json!({"v380DeviceId": 46_078_507})).v380_device_id()
        );
        // Malformed ⇒ unset, never a deser failure.
        assert_eq!(
            None,
            parse(serde_json::json!({"v380DeviceId": "nope"})).v380_device_id()
        );
        assert_eq!(
            None,
            parse(serde_json::json!({"v380DeviceId": true})).v380_device_id()
        );
        assert_eq!(
            None,
            parse(serde_json::json!({"v380DeviceId": "123abc"})).v380_device_id()
        );
        assert_eq!(None, parse(serde_json::json!({"v380User": 42})).v380_user());
        // Absent ⇒ unset.
        assert_eq!(None, parse(serde_json::json!({})).v380_device_id());
        assert_eq!(None, parse(serde_json::json!({})).v380_user());
        // Existing fields still parse alongside.
        let b =
            parse(serde_json::json!({"enable": true, "unsupported": ["talk"], "v380User": "x"}));
        assert!(b.enable());
        assert_eq!(&["talk"], b.unsupported());
        assert_eq!(Some("x"), b.v380_user());
    }
}
