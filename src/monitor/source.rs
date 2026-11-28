// SPDX-License-Identifier: GPL-2.0-or-later

use crate::decoder::{self, VideoDecoder};
use crate::log_monitor;
use async_trait::async_trait;
use common::{
    ArcLogger, ArcMsgLogger, ArcStreamerMuxer, DynError, H264Data, LogEntry, LogLevel, MonitorId,
    MsgLogger, StreamType, TrackParameters,
    monitor::{
        ArcStreamer, DecoderError, DynH264Writer, Feed, FeedDecoded, Protocol, RtspUrl, Source,
        SourceRtspConfig, SubscribeDecodedError,
    },
    recording::FrameRateLimiter,
    time::{DtsOffset, H264_SECOND, UnixH264, UnixNano},
};
use futures_lite::StreamExt;
use retina::{
    client::{InitialTimestampPolicy, Stream},
    codec::{ParametersRef, VideoParameters},
};
use sentryshot_convert::Frame;
use sentryshot_ffmpeg_h264::{Packet, PaddedBytes, ReceiveFrameError, SendPacketError};
use std::sync::Arc;
use thiserror::Error;
use tokio::{
    runtime::Handle,
    sync::{broadcast, mpsc, oneshot, watch},
};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTrackerToken};
use url::Url;

#[allow(clippy::module_name_repetitions)]
pub struct MonitorSource {
    stream_type: StreamType,
    get_muxer_tx: mpsc::Sender<oneshot::Sender<ArcStreamerMuxer>>,
    subscribe_tx: mpsc::Sender<oneshot::Sender<Feed>>,
    codec_rx: watch::Receiver<Option<String>>,
}

impl MonitorSource {
    #[must_use]
    pub fn new(
        stream_type: StreamType,
        get_muxer_tx: mpsc::Sender<oneshot::Sender<ArcStreamerMuxer>>,
        subscribe_tx: mpsc::Sender<oneshot::Sender<Feed>>,
        codec_rx: watch::Receiver<Option<String>>,
    ) -> Self {
        Self {
            stream_type,
            get_muxer_tx,
            subscribe_tx,
            codec_rx,
        }
    }
}

#[async_trait]
impl Source for MonitorSource {
    #[must_use]
    fn stream_type(&self) -> &StreamType {
        &self.stream_type
    }

    // Returns the HLS muxer for this source. Will block until the source has started.
    // Returns None if cancelled.
    async fn muxer(&self) -> Option<ArcStreamerMuxer> {
        let (res_tx, res_rx) = oneshot::channel();
        if self.get_muxer_tx.send(res_tx).await.is_err() {
            return None;
        }
        let Ok(muxer) = res_rx.await else {
            return None;
        };
        Some(muxer)
    }

    // Non-blocking snapshot of the running muxer's codec.
    fn codec(&self) -> Option<String> {
        self.codec_rx.borrow().clone()
    }

    // Subscribe to the raw feed. Will block until the source has started.
    async fn subscribe(&self) -> Option<Feed> {
        let (res_tx, res_rx) = oneshot::channel();
        if self.subscribe_tx.send(res_tx).await.is_err() {
            return None;
        }
        let Ok(feed) = res_rx.await else {
            return None;
        };
        Some(feed)
    }

    // Subscribe to a decoded feed. Currently creates a new decoder for each
    // call but this may change. Will block until the source has started.
    // Will close channel when cancelled.
    async fn subscribe_decoded(
        &self,
        rt_handle: Handle,
        logger: ArcMsgLogger,
        limiter: Option<FrameRateLimiter>,
    ) -> Option<Result<FeedDecoded, SubscribeDecodedError>> {
        let feed = self.subscribe().await?;

        // We could grab the extradata strait from the source instead.
        let muxer = self.muxer().await?;
        let params = muxer.params();
        let codec = params.codec.clone();
        let extradata = params.extra_data.clone();

        let video_decoder = match decoder::build_video_decoder(&codec, PaddedBytes::new(extradata))
        {
            Ok(v) => v,
            Err(decoder::BuildVideoDecoderError::Builder(e)) => {
                return Some(Err(SubscribeDecodedError::NewVideoDecoder(e)));
            }
            Err(decoder::BuildVideoDecoderError::UnsupportedCodec(c)) => {
                return Some(Err(SubscribeDecodedError::UnsupportedCodec(c)));
            }
        };
        Some(Ok(new_decoder(
            rt_handle,
            logger,
            feed,
            video_decoder,
            limiter,
        )))
    }
}

struct SourceLogger {
    logger: ArcLogger,

    monitor_id: MonitorId,
    source_name: String,
    stream_type: StreamType,
}

impl SourceLogger {
    fn new(
        logger: ArcLogger,
        monitor_id: MonitorId,
        source_name: String,
        stream_type: StreamType,
    ) -> Self {
        Self {
            logger,
            monitor_id,
            source_name,
            stream_type,
        }
    }
}

impl MsgLogger for SourceLogger {
    fn log(&self, level: LogLevel, msg: &str) {
        self.logger.log(LogEntry::new(
            level,
            "monitor",
            &self.monitor_id,
            &format!(
                "({}) {} source: {}",
                self.stream_type.name(),
                self.source_name,
                msg
            ),
        ));
    }
}

#[allow(clippy::module_name_repetitions)]
pub struct SourceRtsp {
    msg_logger: ArcMsgLogger,
    streamer: ArcStreamer,

    monitor_id: MonitorId,
    config: SourceRtspConfig,
    stream_type: StreamType,
}

impl SourceRtsp {
    #[allow(clippy::new_ret_no_self, clippy::too_many_arguments)]
    pub fn new(
        token: CancellationToken,
        task_token: TaskTrackerToken,
        logger: ArcLogger,
        streamer: ArcStreamer,
        monitor_id: MonitorId,
        config: SourceRtspConfig,
        stream_type: StreamType,
    ) -> Option<MonitorSource> {
        if stream_type.is_sub() && config.sub_stream.is_none() {
            log_monitor(&logger, LogLevel::Debug, &monitor_id, "no sub stream");
            return None;
        }

        let msg_logger = Arc::new(SourceLogger::new(
            logger,
            monitor_id.clone(),
            "rtsp".to_owned(),
            stream_type,
        ));

        let source = Self {
            msg_logger,
            streamer,
            monitor_id,
            config,
            stream_type,
        };

        let (started_tx, mut started_rx) = mpsc::channel(1);
        let (codec_tx, codec_rx) = watch::channel(None);

        let task_token2 = task_token.clone();
        let token2 = token.clone();
        tokio::spawn(async move {
            let _task_token = task_token2;
            loop {
                if token2.is_cancelled() {
                    source.log(LogLevel::Info, "stopped");
                    return;
                }

                match source
                    .run(token2.child_token(), started_tx.clone(), codec_tx.clone())
                    .await
                {
                    Ok(()) => source.log(LogLevel::Debug, "cancelled"),
                    Err(e) => source.log(LogLevel::Error, &format!("crashed: {e}")),
                };
                codec_tx.send_replace(None);

                tokio::select! {
                    () = token2.cancelled() => {}
                    () = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {}
                }
            }
        });

        let (get_muxer_tx, mut get_muxer_rx) =
            mpsc::channel::<oneshot::Sender<ArcStreamerMuxer>>(1);
        let (subscribe_tx, mut subscribe_rx) = mpsc::channel::<oneshot::Sender<Feed>>(1);

        tokio::spawn(async move {
            let _task_token = task_token;
            let mut muxer = None;
            let mut feed_tx = None;
            let mut get_muxer_requests: Vec<oneshot::Sender<_>> = Vec::new();
            let mut subscribe_requests: Vec<oneshot::Sender<_>> = Vec::new();
            loop {
                tokio::select! {
                    () = token.cancelled() => return,
                   res = started_rx.recv() => {
                        let Some((m, f)) = res else {
                            return
                        };
                        while let Some(res) = get_muxer_requests.pop() {
                            _ = res.send(m.clone());
                        }
                        while let Some(res) = subscribe_requests.pop() {
                            _ = res.send(f.subscribe());
                        }
                        muxer = Some(m);
                        feed_tx = Some(f);
                    }

                    res = get_muxer_rx.recv() => {
                        let Some(res) = res else {
                            return
                        };
                        if let Some(muxer) = &muxer {
                            _ = res.send(muxer.clone());
                        } else {
                            get_muxer_requests.push(res);
                        }
                    }
                    res = subscribe_rx.recv() => {
                        let Some(res) = res else {
                            return
                        };
                        if let Some(feed_tx) = &feed_tx {
                            _ = res.send(feed_tx.subscribe());
                        } else {
                            subscribe_requests.push(res);
                        }
                    }
                }
            }
        });

        Some(MonitorSource::new(
            stream_type,
            get_muxer_tx,
            subscribe_tx,
            codec_rx,
        ))
    }

    fn log(&self, level: LogLevel, msg: &str) {
        self.msg_logger.log(level, msg);
    }

    #[allow(clippy::too_many_lines, clippy::similar_names)]
    async fn run(
        &self,
        token: CancellationToken,
        started_tx: mpsc::Sender<(ArcStreamerMuxer, broadcast::Sender<H264Data>)>,
        codec_tx: watch::Sender<Option<String>>,
    ) -> Result<(), SourceRtspRunError> {
        use SourceRtspRunError::*;

        let url: &Url = self.stream_url();
        let creds = creds_from_url(url);
        let url = remove_creds_from_url(url.to_owned())?;

        let session_group = Arc::new(retina::client::SessionGroup::default());
        let mut session = retina::client::Session::describe(
            url.clone(),
            retina::client::SessionOptions::default()
                .creds(creds)
                .session_group(session_group.clone())
                .teardown(retina::client::TeardownPolicy::Always),
        )
        .await
        .map_err(Describe)?;

        let video_stream_i = {
            for s in session.streams() {
                if s.media() == "video" && !matches!(s.encoding_name(), "h264" | "h265") {
                    self.log(
                        LogLevel::Debug,
                        &format!(
                            "ignoring {} video stream because it's unsupported",
                            s.encoding_name()
                        ),
                    );
                }
            }

            let picked = pick_video_stream(
                session
                    .streams()
                    .iter()
                    .map(|s| (s.media(), s.encoding_name())),
            );
            let Some(i) = picked else {
                return Err(NoVideoStreamFound(format_streams(session.streams())));
            };
            self.log(
                LogLevel::Debug,
                &format!(
                    "using {} video stream",
                    session.streams()[i].encoding_name()
                ),
            );
            i
        };

        let transport = match self.config.protocol {
            Protocol::Tcp => {
                retina::client::Transport::Tcp(retina::client::TcpTransportOptions::default())
            }
            Protocol::Udp => {
                retina::client::Transport::Udp(retina::client::UdpTransportOptions::default())
            }
        };

        session
            .setup(
                video_stream_i,
                retina::client::SetupOptions::default().transport(transport),
            )
            .await
            .map_err(SourceRtspRunError::Setup)?;

        let mut session = session
            .play(
                retina::client::PlayOptions::default()
                    .initial_timestamp(InitialTimestampPolicy::Permissive),
            )
            .await
            .map_err(Play)?
            .demuxed()
            .map_err(Demuxed)?;

        // Buffer 10 frame to reduce dropped frames.
        let (feed_tx, _) = broadcast::channel(10);

        let mut stream_started: Option<StreamStarted> = None;
        let params_wait_start = std::time::Instant::now();
        let mut params_warned = false;
        loop {
            tokio::select! {
                () = token.cancelled() => {
                    return Ok(());
                },
                pkt = session.next() => {
                    let Some(pkt) = pkt else {
                        return Err(Eof);
                    };
                    match pkt {
                        Ok(retina::codec::CodecItem::VideoFrame(frame)) => {
                            if let Some(stream_started) = &mut stream_started {
                                let data = parse_frame(
                                    frame.timestamp(),
                                    frame.is_random_access_point(),
                                    frame.into_data(),
                                    stream_started.start_time,
                                    stream_started.first_sample_pts,
                                )?;
                                check_clock_drift(data.pts)?;
                                stream_started.hls_writer.write_h264(data.clone()).await
                                    .map_err(SourceRtspRunError::WriteH264 )?;
                                _ = feed_tx.send(data);

                            } else {
                                // Frames flowing without a usable start means
                                // the camera isn't delivering stream
                                // parameters or a random-access point.
                                if !params_warned
                                    && params_wait_start.elapsed()
                                        > std::time::Duration::from_secs(30)
                                {
                                    params_warned = true;
                                    self.log(
                                        LogLevel::Warning,
                                        "30s without stream parameters or a random-access point",
                                    );
                                }
                                if !frame.is_random_access_point() {
                                    // Wait for IDR.
                                    continue
                                }

                                let stream = &session.streams()[frame.stream_id()];
                                if let Some(ParametersRef::Video(params)) = stream.parameters() {
                                    let start_time = UnixNano::now();
                                    let first_sample_pts = UnixH264::new(frame.timestamp().pts());
                                    let first_sample = parse_frame(
                                        frame.timestamp(),
                                        frame.is_random_access_point(),
                                        frame.into_data(),
                                        start_time,
                                        first_sample_pts,
                                    )?;
                                    let result = self.streamer.new_muxer(
                                        token.clone(),
                                        self.monitor_id.clone(),
                                        self.stream_type.is_sub(),
                                        track_params_from_video_params(params)?,
                                        start_time.into(),
                                        first_sample.clone(),
                                    )
                                        .await
                                        .map_err(SourceRtspRunError::NewMuxer)?;
                                    let Some((muxer, hls_writer)) = result else {
                                        // Cancelled.
                                        return Ok(());
                                    };
                                    codec_tx
                                        .send_replace(Some(muxer.params().codec.clone()));
                                    stream_started = Some(StreamStarted{ hls_writer, start_time, first_sample_pts });
                                    // Notify successful start.
                                    _ = started_tx.send((muxer, feed_tx.clone())).await;
                                };
                            }
                        },
                        Ok(_) => {},
                        Err(e) => return Err(Stream(e)),
                    }
                }
            }
        }
    }

    fn stream_url(&self) -> &RtspUrl {
        if self.stream_type.is_main() {
            &self.config.main_stream
        } else {
            self.config
                .sub_stream
                .as_ref()
                .expect("sub_stream to be `Some`")
        }
    }
}

struct StreamStarted {
    hls_writer: DynH264Writer,
    start_time: UnixNano,
    first_sample_pts: UnixH264,
}

#[derive(Debug, Error)]
enum ParseFrameError {
    #[error("subtract first sample pts")]
    SubFirstSample,

    #[error("add start time")]
    AddStartTime,
}

#[allow(clippy::similar_names)]
fn parse_frame(
    timestamp: retina::VideoTimestamp,
    is_random_access_point: bool,
    data: Vec<u8>,
    start_time: UnixNano,
    first_sample_time: UnixH264,
) -> Result<H264Data, ParseFrameError> {
    use ParseFrameError::*;

    let pts = timestamp.pts();
    let dts = timestamp.dts();
    let dts_offset = DtsOffset::new(i32::try_from(pts - dts).unwrap_or(0));

    let pts = UnixH264::new(pts)
        .checked_sub(first_sample_time)
        .ok_or(SubFirstSample)?
        .checked_add(UnixH264::from(start_time))
        .ok_or(AddStartTime)?;

    Ok(H264Data {
        pts,
        dts_offset,
        random_access_present: is_random_access_point,
        avcc: Arc::new(PaddedBytes::new(data)),
    })
}

fn check_clock_drift(pts: UnixH264) -> Result<(), SourceRtspRunError> {
    let now = UnixH264::now();
    let diff = (pts - now).abs();
    // This shouldnt be more than one or two frames, but we will try 30 sec for now.
    if diff > 30 * H264_SECOND {
        let diff_secs = diff / H264_SECOND;
        return if now < pts {
            Err(SourceRtspRunError::CameraClockAhead(diff_secs))
        } else {
            Err(SourceRtspRunError::CameraClockBehind(diff_secs))
        };
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ParseParamsError {
    #[error("{0}")]
    TryFromInt(#[from] std::num::TryFromIntError),
}

pub fn track_params_from_video_params(
    params: &VideoParameters,
) -> Result<TrackParameters, ParseParamsError> {
    let (width, height) = params.pixel_dimensions();
    Ok(TrackParameters {
        width: u16::try_from(width)?,
        height: u16::try_from(height)?,
        codec: params.rfc6381_codec().to_owned(),
        extra_data: params.extra_data().to_owned(),
    })
}

#[derive(Debug, Error)]
enum SourceRtspRunError {
    #[error("end of file")]
    Eof,

    #[error("describe: {0}")]
    Describe(retina::Error),

    #[error("remove credentials: {0}")]
    RemoveCreds(#[from] RemoveCreds),

    #[error("no suitable video stream found, streams:{0}")]
    NoVideoStreamFound(String),

    #[error("setup: {0}")]
    Setup(retina::Error),

    #[error("play: {0}")]
    Play(retina::Error),

    #[error("demuxed: {0}")]
    Demuxed(retina::Error),

    #[error("stream: {0}")]
    Stream(retina::Error),

    #[error("write h264: {0}")]
    WriteH264(DynError),

    #[error("convert params: {0}")]
    ConvertTrackParams(#[from] ParseParamsError),

    #[error("new muxer: {0}")]
    NewMuxer(DynError),

    #[error("parse frame: {0}")]
    ParseFrame(#[from] ParseFrameError),

    #[error("camera clock drifted {0} seconds ahead")]
    CameraClockAhead(i64),

    #[error("camera clock drifted {0} seconds behind")]
    CameraClockBehind(i64),
}

#[derive(Debug, Error)]
enum RemoveCreds {
    #[error("set username")]
    SetUsername,

    #[error("set password")]
    SetPassword,
}

fn remove_creds_from_url(mut url: Url) -> Result<Url, RemoveCreds> {
    url.set_username("")
        .map_err(|()| RemoveCreds::SetUsername)?;
    url.set_password(None)
        .map_err(|()| RemoveCreds::SetPassword)?;
    Ok(url)
}

fn creds_from_url(url: &Url) -> Option<retina::client::Credentials> {
    fn decode(s: &str) -> String {
        percent_encoding::percent_decode_str(s)
            .decode_utf8()
            .map_or_else(|_| s.to_owned(), |d| d.into_owned())
    }

    let username = decode(url.username());
    let password = url.password().map(decode);

    if let Some(password) = password {
        Some(retina::client::Credentials { username, password })
    } else if !username.is_empty() {
        Some(retina::client::Credentials {
            username,
            password: String::new(),
        })
    } else {
        None
    }
}

// Picks the index of the preferred video stream: the first h264 stream
// if any, otherwise the first h265 stream. h264 wins when the camera
// offers both because browser HEVC playback support is device-dependent.
fn pick_video_stream<'a>(streams: impl Iterator<Item = (&'a str, &'a str)>) -> Option<usize> {
    let mut first_h265 = None;
    for (i, (media, encoding)) in streams.enumerate() {
        if media != "video" {
            continue;
        }
        match encoding {
            "h264" => return Some(i),
            "h265" => {
                if first_h265.is_none() {
                    first_h265 = Some(i);
                }
            }
            _ => {}
        }
    }
    first_h265
}

fn format_streams(streams: &[Stream]) -> String {
    streams
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                " {}:{{media=\"{}\", name=\"{}\"}}",
                i,
                s.media(),
                s.encoding_name()
            )
        })
        .collect::<Vec<_>>()
        .concat()
}

fn new_decoder(
    rt_handle: Handle,
    logger: ArcMsgLogger,
    mut feed: Feed,
    mut video_decoder: VideoDecoder,
    mut frame_rate_limiter: Option<FrameRateLimiter>,
) -> FeedDecoded {
    let (frame_tx, frame_rx) = mpsc::channel(1);

    rt_handle.clone().spawn(async move {
        use DecoderError::*;
        loop {
            use broadcast::error::RecvError;
            let frame = match feed.recv().await {
                Ok(v) => v,
                Err(RecvError::Closed) => {
                    // Close receiver by dropping sender.
                    return;
                }
                Err(RecvError::Lagged(_)) => {
                    _ = frame_tx.send(Err(DroppedFrames)).await;
                    return;
                }
            };

            // State juggling to avoid lifetime issue.
            let avcc = frame.avcc.clone();

            let result: Result<(), SendPacketError>;
            (video_decoder, result) = rt_handle
                .spawn_blocking(move || {
                    let result =
                        video_decoder.send_packet(&Packet::new(&avcc).with_pts(*frame.pts));
                    (video_decoder, result)
                })
                .await
                .expect("join");
            if let Err(e) = result {
                if let SendPacketError::Invaliddata = e {
                    logger.log(
                        LogLevel::Warning,
                        "video decoder: send_packet: invalid data",
                    );
                    continue;
                }
                _ = frame_tx.send(Err(SendFrame(e))).await;
                return;
            };

            loop {
                let mut frame_decoded = Frame::new();
                match video_decoder.receive_frame(&mut frame_decoded) {
                    Ok(()) => {}
                    Err(ReceiveFrameError::Eagain) => break,
                    Err(e) => {
                        _ = frame_tx.send(Err(ReceiveFrame(e))).await;
                        return;
                    }
                };
                let pts = match u64::try_from(frame_decoded.pts()) {
                    Ok(v) => v,
                    Err(e) => {
                        _ = frame_tx.send(Err(TryFrom(e))).await;
                        return;
                    }
                };

                let discard = if let Some(limiter) = &mut frame_rate_limiter {
                    match limiter.discard(pts) {
                        Ok(v) => v,
                        Err(e) => {
                            _ = frame_tx.send(Err(FrameRateLimiter(e))).await;
                            return;
                        }
                    }
                } else {
                    false
                };
                if !discard {
                    _ = frame_tx.send(Ok(frame_decoded)).await;
                }
            }
        }
    });

    frame_rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creds_from_url_decodes_percent_encoded() {
        let url = Url::parse("rtsp://user:p%40ss%2Fword@192.168.1.1/stream").unwrap();
        let creds = creds_from_url(&url).unwrap();
        assert_eq!("user", creds.username);
        assert_eq!("p@ss/word", creds.password);
    }

    #[test]
    fn creds_from_url_plain_unchanged() {
        let url = Url::parse("rtsp://admin:letmein@192.168.1.1/stream").unwrap();
        let creds = creds_from_url(&url).unwrap();
        assert_eq!("admin", creds.username);
        assert_eq!("letmein", creds.password);
    }

    #[test]
    fn creds_from_url_username_only() {
        let url = Url::parse("rtsp://admin@192.168.1.1/stream").unwrap();
        let creds = creds_from_url(&url).unwrap();
        assert_eq!("admin", creds.username);
        assert_eq!("", creds.password);
    }

    #[test]
    fn creds_from_url_none() {
        let url = Url::parse("rtsp://192.168.1.1/stream").unwrap();
        assert!(creds_from_url(&url).is_none());
    }

    fn pick(streams: &[(&str, &str)]) -> Option<usize> {
        pick_video_stream(streams.iter().copied())
    }

    #[test]
    fn pick_video_stream_h264_only() {
        assert_eq!(
            Some(1),
            pick(&[
                ("audio", "mpeg4-generic"),
                ("video", "h264"),
                ("video", "h264"),
            ])
        );
    }

    #[test]
    fn pick_video_stream_h265_only() {
        assert_eq!(
            Some(0),
            pick(&[("video", "h265"), ("audio", "mpeg4-generic")])
        );
    }

    #[test]
    fn remove_creds_from_url_strips() {
        let url = Url::parse("rtsp://user:p%40ss@192.168.1.1/stream").unwrap();
        let stripped = remove_creds_from_url(url).unwrap();
        assert_eq!("", stripped.username());
        assert_eq!(None, stripped.password());
        assert_eq!("rtsp://192.168.1.1/stream", stripped.as_str());
    }

    #[test]
    fn remove_creds_from_url_passthrough() {
        let url = Url::parse("rtsp://192.168.1.1/stream").unwrap();
        let stripped = remove_creds_from_url(url.clone()).unwrap();
        assert_eq!(url, stripped);
    }

    fn video_ts(pts: i64, dts: Option<i64>) -> retina::VideoTimestamp {
        retina::VideoTimestamp::new(
            pts,
            dts,
            std::num::NonZeroU32::new(90_000).unwrap(),
            0,
        )
        .unwrap()
    }

    #[test]
    fn parse_frame_rebases_to_start_time() {
        let start_time = UnixNano::new(1_000_000_000);
        let first = UnixH264::new(90_000);

        let d = parse_frame(
            video_ts(180_000, Some(170_000)),
            true,
            b"data".to_vec(),
            start_time,
            first,
        )
        .unwrap();

        // 1 second elapsed in 90kHz ticks, rebased onto start_time.
        let want = UnixH264::new(90_000)
            .checked_add(UnixH264::from(start_time))
            .unwrap();
        assert_eq!(want, d.pts);
        assert_eq!(DtsOffset::new(10_000), d.dts_offset);
        assert!(d.random_access_present);
        assert_eq!(b"data".as_slice(), &**d.avcc);
    }

    #[test]
    fn parse_frame_pts_underflow_error() {
        // i64 underflow on pts - first_sample_time, not mere negativity.
        let d = parse_frame(
            video_ts(i64::MIN, None),
            false,
            Vec::new(),
            UnixNano::new(0),
            UnixH264::new(1),
        );
        assert!(matches!(d, Err(ParseFrameError::SubFirstSample)));
    }

    #[test]
    fn parse_frame_huge_dts_offset_saturates_zero() {
        // pts - dts beyond i32 collapses to a zero offset rather than wrapping.
        let d = parse_frame(
            video_ts(i64::from(i32::MAX) + 10, Some(0)),
            false,
            Vec::new(),
            UnixNano::new(0),
            UnixH264::new(0),
        )
        .unwrap();
        assert_eq!(DtsOffset::new(0), d.dts_offset);
    }

    #[test]
    fn check_clock_drift_within_bounds() {
        check_clock_drift(UnixH264::now()).unwrap();
    }

    #[test]
    fn check_clock_drift_ahead() {
        let ahead = UnixH264::now()
            .checked_add(UnixH264::new(31 * H264_SECOND))
            .unwrap();
        assert!(matches!(
            check_clock_drift(ahead),
            Err(SourceRtspRunError::CameraClockAhead(..))
        ));
    }

    #[test]
    fn check_clock_drift_behind() {
        let behind = UnixH264::now()
            .checked_sub(UnixH264::new(31 * H264_SECOND))
            .unwrap();
        assert!(matches!(
            check_clock_drift(behind),
            Err(SourceRtspRunError::CameraClockBehind(..))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_decoder_exits_on_lagged_feed() {
        use crate::decoder::VideoDecoder;
        use sentryshot_ffmpeg_h264::H264DecoderBuilder;

        let (feed_tx, feed_rx) = broadcast::channel(1);
        // Overflow the receiver before the task runs: deterministic Lagged.
        let frame = || H264Data {
            pts: UnixH264::new(0),
            dts_offset: DtsOffset::new(0),
            random_access_present: false,
            avcc: Arc::new(PaddedBytes::new(Vec::new())),
        };
        feed_tx.send(frame()).unwrap();
        feed_tx.send(frame()).unwrap();

        let decoder =
            VideoDecoder::H264(H264DecoderBuilder::new().annex_b().unwrap());
        let mut decoded = new_decoder(
            Handle::current(),
            common::DummyLogger::new(),
            feed_rx,
            decoder,
            None,
        );

        assert!(matches!(
            decoded.recv().await,
            Some(Err(DecoderError::DroppedFrames))
        ));
        // Task exited: sender dropped, channel closed.
        assert!(decoded.recv().await.is_none());
    }

    #[test]
    fn pick_video_stream_prefers_h264_over_h265() {
        // h265 first: h264 still wins.
        assert_eq!(Some(1), pick(&[("video", "h265"), ("video", "h264")]));
        // h264 first: h264 wins.
        assert_eq!(Some(0), pick(&[("video", "h264"), ("video", "h265")]));
    }

    #[test]
    fn pick_video_stream_first_of_kind() {
        assert_eq!(Some(0), pick(&[("video", "h265"), ("video", "h265")]));
    }

    #[test]
    fn pick_video_stream_unsupported_codec() {
        assert_eq!(None, pick(&[("video", "mjpeg")]));
    }

    #[test]
    fn pick_video_stream_no_video() {
        assert_eq!(None, pick(&[("audio", "mpeg4-generic")]));
        assert_eq!(None, pick(&[]));
    }

    #[test]
    fn monitor_source_codec_snapshot() {
        let (codec_tx, codec_rx) = watch::channel(None);
        let (get_muxer_tx, _get_muxer_rx) = mpsc::channel(1);
        let (subscribe_tx, _subscribe_rx) = mpsc::channel(1);
        let source = MonitorSource::new(StreamType::Main, get_muxer_tx, subscribe_tx, codec_rx);

        // Stream down: no codec.
        assert_eq!(None, source.codec());

        // Muxer started.
        codec_tx.send_replace(Some("hvc1.1.6.L93.B0".to_owned()));
        assert_eq!(Some("hvc1.1.6.L93.B0".to_owned()), source.codec());

        // Muxer torn down.
        codec_tx.send_replace(None);
        assert_eq!(None, source.codec());
    }
}
