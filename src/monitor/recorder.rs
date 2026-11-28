// SPDX-License-Identifier: GPL-2.0-or-later

use crate::ArcMonitorHooks;
use crate::decoder;
use common::{
    ArcLogger, ArcMsgLogger, ArcStreamerMuxer, Event, LogEntry, LogLevel, MonitorId, MsgLogger,
    Segment, TrackParameters,
    monitor::{ArcSource, MonitorConfig},
    recording::{RecordingData, RecordingId},
    time::{Duration, DurationH264, UnixH264, UnixNano},
};
use futures_lite::Future;
use recdb::{NewRecordingError, OpenFileError, RecDb, RecordingHandle};
use recording::{CreateVideoWriterError, MetaHeader, VideoWriter, WriteSampleError};
use sentryshot_convert::{
    ConvertError, Frame, NewConverterError, PixelFormat, PixelFormatConverter,
};
use sentryshot_ffmpeg_h264::{
    DrainError, H264BuilderError, Packet, PaddedBytes, ReceiveFrameError, SendPacketError,
};
use sentryshot_util::ImageCopyToBufferError;
use std::{pin::Pin, sync::Arc, task::Poll};
use thiserror::Error;
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    sync::mpsc,
    time::sleep,
};
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTrackerToken};

#[allow(clippy::too_many_arguments, clippy::module_name_repetitions)]
pub fn new_recorder(
    token: CancellationToken,
    task_token: TaskTrackerToken,
    hooks: ArcMonitorHooks,
    logger: ArcLogger,
    monitor_id: MonitorId,
    source_main: ArcSource,
    config: MonitorConfig,
    recdb: Arc<RecDb>,
) -> mpsc::Sender<(Duration, Event)> {
    let (send_event_tx, mut send_event_rx) = mpsc::channel::<(Duration, Event)>(1);
    let c = RecordingContext {
        hooks,
        logger: Arc::new(RecorderMsgLogger::new(logger, monitor_id)),
        source_main,
        config,
        recdb,
    };

    // Recorder actor.
    tokio::spawn(async move {
        let _task_token = task_token;

        let mut state = if c.config.always_record() {
            c.log(LogLevel::Debug, "alwaysRecord=true");
            State::Recording(RecordingSession::new(&token, None, c.clone(), None))
        } else {
            c.log(LogLevel::Debug, "alwaysRecord=false");
            State::NotRecording(None)
        };

        loop {
            match state {
                State::Recording(mut session) => {
                    tokio::select! {
                        () = token.cancelled() => {
                            // Wait for session to exit.
                            _ = session.on_exit_rx.recv().await;
                            return;
                        }

                        event = send_event_rx.recv() => { // Incomming events.
                            session.handle_event(event);
                            state = State::Recording(session);
                        }

                        () = session.sleep_until_timer_end() => {
                            assert!(!c.config.always_record());

                            c.log(LogLevel::Debug, "timer reached end, canceling session");
                            session.token.cancel();

                            // Avoid sleeping again.
                            let prev_seg = session.on_exit_rx.recv().await.expect("complete");
                            c.log(LogLevel::Debug, "session stopped");
                            state = State::NotRecording(prev_seg);
                        }

                        prev_seg = session.on_exit_rx.recv() => {
                            c.log(LogLevel::Debug, "session stopped2");
                            state = State::NotRecording(prev_seg.expect("complete"));
                        }
                    }
                }
                State::NotRecording(prev_seg) => {
                    tokio::select! {
                        () = token.cancelled() => return,
                        event = send_event_rx.recv() => { // Incomming events.
                            let Some((trigger_duration, event)) = event else {
                                return
                            };
                            let Some(end) = live_event_end(
                                event.time,
                                trigger_duration,
                                UnixNano::now(),
                            ) else {
                                state = State::NotRecording(prev_seg);
                                continue
                            };

                            state = State::Recording(RecordingSession::new(
                                &token,
                                Some(end),
                                c.clone(),
                                prev_seg,
                            ));
                        }
                    };
                }
            }
        }
    });

    send_event_tx
}

enum State {
    Recording(RecordingSession),
    NotRecording(PrevSeg),
}

type PrevSeg = Option<Segment>;

// Extend a running session's timer when an event's end lands past the
// current end. `None` timer_end (alwaysRecord) and unrepresentable ends
// leave the timer untouched.
fn extend_timer(
    timer_end: Option<UnixNano>,
    event_time: UnixNano,
    trigger_duration: Duration,
) -> Option<UnixNano> {
    let timer_end = timer_end?;
    let Some(end) = event_time.checked_add(trigger_duration.into()) else {
        return Some(timer_end);
    };
    Some(if end.after(timer_end) { end } else { timer_end })
}

// Event end if the event is still live at `now`, else None (drop expired).
fn live_event_end(
    event_time: UnixNano,
    trigger_duration: Duration,
    now: UnixNano,
) -> Option<UnixNano> {
    let end = event_time.checked_add(trigger_duration.into())?;
    end.after(now).then_some(end)
}

struct RecordingSession {
    token: CancellationToken,
    logger: ArcMsgLogger,
    timer_end: Option<UnixNano>, // None if always record.
    on_exit_rx: mpsc::Receiver<PrevSeg>,
}

impl RecordingSession {
    fn new(
        parent_token: &CancellationToken,
        timer_end: Option<UnixNano>,
        c: RecordingContext,
        mut prev_seg: PrevSeg,
    ) -> Self {
        c.log(LogLevel::Debug, "starting recording session");

        let token = parent_token.child_token();
        let (on_exit_tx, on_exit_rx) = mpsc::channel::<PrevSeg>(1);
        let recording_session = RecordingSession {
            token: token.clone(),
            logger: c.logger.clone(),
            timer_end,
            on_exit_rx,
        };

        tokio::spawn(async move {
            run_recording_session(token, c, &mut prev_seg).await;
            on_exit_tx
                .send(prev_seg)
                .await
                .expect("recorder should wait for the session to exit");
        });

        recording_session
    }

    fn handle_event(&mut self, event: Option<(Duration, Event)>) {
        let Some((trigger_duration, event)) = event else {
            return;
        };
        if self.timer_end.is_none() {
            return;
        }

        // Update timer if the monitor isn't set to always record.
        let new_timer_end = extend_timer(self.timer_end, event.time, trigger_duration);
        if new_timer_end != self.timer_end {
            self.logger.log(
                LogLevel::Debug,
                "new event, already recording, updating timer",
            );
            self.timer_end = new_timer_end;
        }
    }

    fn sleep_until_timer_end(&self) -> Sleep {
        let Some(timer_end) = self.timer_end else {
            return Sleep(None);
        };
        let Some(timer_end) = UnixNano::until(timer_end) else {
            self.logger.log(LogLevel::Error, "Duration::until failed");
            return Sleep(None);
        };
        let Some(timer_end) = timer_end.as_std() else {
            self.logger.log(LogLevel::Error, "timer_end.as_std failed");
            return Sleep(None);
        };
        Sleep(Some(Box::pin(tokio::time::sleep(timer_end))))
    }
}

// Future will always return pending if this is None.
struct Sleep(Option<Pin<Box<tokio::time::Sleep>>>);

impl Future for Sleep {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match &mut self.0 {
            Some(v) => Pin::new(v).poll(cx),
            None => Poll::Pending,
        }
    }
}

type RunFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), RunRecordingError>> + Send + 'a>>;
type RunRecordingFn =
    for<'a> fn(CancellationToken, RecordingContext, &'a mut PrevSeg) -> RunFuture<'a>;

async fn run_recording_session(
    recording_session_token: CancellationToken,
    c: RecordingContext,
    prev_seg: &mut PrevSeg,
) {
    fn run(t: CancellationToken, c: RecordingContext, p: &mut PrevSeg) -> RunFuture<'_> {
        Box::pin(run_recording(t, c, p))
    }
    run_recording_session_loop(
        recording_session_token,
        c,
        prev_seg,
        run,
        std::time::Duration::from_secs(3),
    )
    .await;
}

async fn run_recording_session_loop(
    recording_session_token: CancellationToken,
    c: RecordingContext,
    prev_seg: &mut PrevSeg,
    run: RunRecordingFn,
    restart_sleep: std::time::Duration,
) {
    loop {
        if let Err(e) = run(recording_session_token.clone(), c.clone(), prev_seg).await {
            c.log(LogLevel::Error, &format!("recording crashed: {e}"));

            tokio::select! {
                () = recording_session_token.cancelled() => return,
                () = sleep(restart_sleep) => {}
            }
            c.log(LogLevel::Debug, "recovering after crash");
            continue;
        }
        c.log(LogLevel::Debug, "recording finished");
        // Recoding reached videoLength and exited normally. The timer
        // is still active, so continue the loop and start another recording.
        tokio::select! {
            () = recording_session_token.cancelled() => return,
            () = sleep(restart_sleep) => {}
        }
    }
}

#[derive(Debug, Error)]
enum RunRecordingError {
    #[error("new recording: {0}")]
    NewRecording(#[from] NewRecordingError),

    #[error("generate video: {0}")]
    GenerateVideo(#[from] GenerateVideoError),

    #[error("save recording: {0}")]
    SaveRecording(#[from] SaveRecordingError),
}

#[derive(Clone)]
struct RecordingContext {
    hooks: ArcMonitorHooks,
    logger: ArcMsgLogger,
    source_main: ArcSource,
    config: MonitorConfig,
    recdb: Arc<RecDb>,
}

impl RecordingContext {
    fn log(&self, level: LogLevel, msg: &str) {
        self.logger.log(level, msg);
    }
}

async fn run_recording(
    recording_session_token: CancellationToken,
    c: RecordingContext,
    prev_seg: &mut PrevSeg,
) -> Result<(), RunRecordingError> {
    let Some(muxer) = c.source_main.muxer().await else {
        c.log(LogLevel::Debug, "source cancelled");
        return Ok(());
    };

    let Some(first_segment) = muxer.next_segment(prev_seg.clone()).await else {
        c.log(LogLevel::Debug, "muxer cancelled");
        return Ok(());
    };

    let start_time = first_segment.start_time();

    let monitor_id = c.config.id().to_owned();
    let recording = c
        .recdb
        .new_recording(monitor_id.clone(), start_time)
        .await?;

    let video_length = DurationH264::from(c.config.video_length());

    c.log(
        LogLevel::Info,
        &format!("starting recording: {:?}", recording.id()),
    );

    let params = muxer.params();

    let result = generate_thumbnail(
        c.hooks.clone(),
        &c.logger,
        c.config.clone(),
        &recording,
        &first_segment,
        params.codec.clone(),
        muxer.params().extra_data.clone(),
    )
    .await;
    if let Err(e) = result {
        c.log(
            LogLevel::Error,
            &format!("failed to generate thumbnail: {}", &e),
        );
    }

    let end_time = generate_video(
        recording_session_token,
        &recording,
        &muxer,
        first_segment,
        params,
        video_length,
        prev_seg,
    )
    .await?;

    c.log(
        LogLevel::Debug,
        &format!("video generated: {:?}", recording.id()),
    );

    save_recording(
        c.logger.clone(),
        recording.id(),
        &recording,
        UnixNano::from(start_time),
        UnixNano::from(end_time),
    )
    .await?;

    Ok(())
}

#[derive(Debug, Error)]
enum GenerateVideoError {
    #[error("open file: {0}")]
    OpenFile(#[from] OpenFileError),

    #[error("new video writer: {0}")]
    NewVideoWriter(#[from] CreateVideoWriterError),

    #[error("add")]
    Add,

    #[error("write sample: {0}")]
    WriteSample(#[from] WriteSampleError),

    #[error("skipped segment: expected: {0}, got: {1}. this may be a disk issue")]
    SkippedSegment(u64, u64),
}

// The next segment must be exactly prev_seg_id + 1, else a segment was
// skipped (disk or muxer issue). checked_add guards the u64::MAX edge the
// old `prev_seg_id + 1` would panic on.
fn check_next_segment(prev_seg_id: u64, seg_id: u64) -> Result<(), GenerateVideoError> {
    let expected = prev_seg_id
        .checked_add(1)
        .ok_or(GenerateVideoError::Add)?;
    if seg_id == expected {
        Ok(())
    } else {
        Err(GenerateVideoError::SkippedSegment(expected, seg_id))
    }
}

async fn generate_video(
    token: CancellationToken,
    recording: &RecordingHandle,
    muxer: &ArcStreamerMuxer,
    first_segment: Segment,
    params: &TrackParameters,
    max_duration: DurationH264,
    prev_seg: &mut PrevSeg,
) -> Result<UnixH264, GenerateVideoError> {
    use GenerateVideoError::*;

    let mut meta = recording.new_file("meta").await?;
    let mut meta = BufWriter::with_capacity(64 * 1024, &mut *meta);

    let mut mdat = recording.new_file("mdat").await?;
    let mut mdat = BufWriter::with_capacity(64 * 1024, &mut *mdat);

    let start_time = first_segment.start_time();
    let stop_time = first_segment
        .start_time()
        .checked_add(max_duration.into())
        .ok_or(GenerateVideoError::Add)?;

    let header = MetaHeader {
        version: 2,
        start_time,
        width: params.width,
        height: params.height,
        codec: params.codec.clone(),
        extra_data: params.extra_data.clone(),
    };

    let mut w = VideoWriter::new(&mut meta, &mut mdat, header).await?;

    w.write_frames(first_segment.frames()).await?;
    let end_time = first_segment
        .start_time()
        .checked_add(first_segment.duration().into())
        .ok_or(Add)?;
    recording.set_end_time(end_time);
    let mut prev_seg_id = first_segment.id();
    *prev_seg = Some(first_segment); // prev_seg should be updated after frames have been written.

    loop {
        if token.is_cancelled() {
            return Ok(end_time);
        }

        let Some(seg) = muxer.next_segment(prev_seg.clone()).await else {
            return Ok(end_time);
        };

        check_next_segment(prev_seg_id, seg.id())?;

        w.write_frames(seg.frames()).await?;
        let end_time = seg
            .start_time()
            .checked_add(seg.duration().into())
            .ok_or(Add)?;
        recording.set_end_time(end_time);
        prev_seg_id = seg.id();
        let seg_start_time = seg.start_time();
        *prev_seg = Some(seg);

        if seg_start_time.after(stop_time) {
            return Ok(end_time);
        }
    }
}

#[derive(Debug, Error)]
enum GenerateThumbnailError {
    #[error("no frame")]
    NoFrame,

    #[error("sample is not an IDR")]
    SampleNotIdr,

    #[error("avcc to jpeg: {0}")]
    AvccToJpeg(#[from] AvccToJpegError),

    #[error("open file: {0}")]
    OpenFile(#[from] OpenFileError),

    #[error("write file: {0}")]
    WriteFile(std::io::Error),

    #[error("flush jpeg file: {0}")]
    FlushFile(std::io::Error),
}

// The first video frame in firstSegment is wrapped in a mp4
// container and piped into FFmpeg and then converted to jpeg.
async fn generate_thumbnail(
    hooks: ArcMonitorHooks,
    logger: &ArcMsgLogger,
    config: MonitorConfig,
    recording: &RecordingHandle,
    first_segment: &Segment,
    codec: String,
    extradata: Vec<u8>,
) -> Result<(), GenerateThumbnailError> {
    use GenerateThumbnailError::*;

    logger.log(LogLevel::Debug, "generating thumbnail");

    let Some(first_sample) = first_segment.frames().next() else {
        return Err(NoFrame);
    };
    if !first_sample.random_access_present {
        return Err(SampleNotIdr);
    }

    let avcc = first_sample.avcc.clone();
    let jpeg_buf = tokio::task::spawn_blocking(move || {
        avcc_to_jpeg(&hooks, &config, &avcc, &codec, PaddedBytes::new(extradata))
    })
    .await
    .expect("join")?;

    let mut file = recording.new_file("jpeg").await?;
    file.write_all(&jpeg_buf).await.map_err(WriteFile)?;
    file.flush().await.map_err(FlushFile)?;

    logger.log(
        LogLevel::Debug,
        &format!("thumbnail generated: {:?}", file.path()),
    );

    Ok(())
}

#[derive(Debug, Error)]
enum AvccToJpegError {
    #[error("new video decoder: {0}")]
    NewVideoDecoder(#[from] H264BuilderError),

    #[error("unsupported codec: {0}")]
    UnsupportedCodec(String),

    #[error("send avcc packet: {0}")]
    SendPacket(#[from] SendPacketError),

    #[error("drain video decoder: {0}")]
    Drain(#[from] DrainError),

    #[error("receive frame: {0}")]
    ReceiveFrame(#[from] ReceiveFrameError),

    #[error("new converter: {0}")]
    NewConverter(#[from] NewConverterError),

    #[error("convert: {0}")]
    Convert(#[from] ConvertError),

    #[error("copy to buffer: {0}")]
    CopyToBuffer(#[from] ImageCopyToBufferError),

    #[error("encode jpeg: {0}")]
    EncodeJpeg(#[from] jpeg_encoder::EncodingError),
}

fn avcc_to_jpeg(
    hooks: &ArcMonitorHooks,
    config: &MonitorConfig,
    avcc: &PaddedBytes,
    codec: &str,
    extradata: PaddedBytes,
) -> Result<Vec<u8>, AvccToJpegError> {
    let mut decoder = decoder::build_video_decoder(codec, extradata).map_err(|e| match e {
        decoder::BuildVideoDecoderError::Builder(e) => AvccToJpegError::NewVideoDecoder(e),
        decoder::BuildVideoDecoderError::UnsupportedCodec(c) => {
            AvccToJpegError::UnsupportedCodec(c)
        }
    })?;

    decoder.send_packet(&Packet::new(avcc))?;

    let mut video_decoder = decoder.drain()?;

    let mut frame = Frame::new();
    video_decoder.receive_frame(&mut frame)?;

    let frame = hooks.on_thumb_save(config, frame);

    let mut converter = PixelFormatConverter::new(
        frame.width(),
        frame.height(),
        frame.color_range(),
        frame.pix_fmt(),
        PixelFormat::RGB24,
    )?;

    let mut rgb_frame = Frame::new();

    converter.convert(&frame, &mut rgb_frame)?;

    let mut raw_rgb_frame = Vec::new();
    rgb_frame.copy_to_buffer(&mut raw_rgb_frame, 1)?;

    let mut jpeg_buf = Vec::new();
    let jpeg_encoder = jpeg_encoder::Encoder::new(&mut jpeg_buf, 75);

    jpeg_encoder.encode(
        &raw_rgb_frame,
        rgb_frame.width().get(),
        rgb_frame.height().get(),
        jpeg_encoder::ColorType::Rgb,
    )?;

    Ok(jpeg_buf)
}

#[derive(Debug, Error)]
enum SaveRecordingError {
    #[error("serialize data: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("open file: {0}")]
    OpenFile(#[from] OpenFileError),

    #[error("write data file: {0}")]
    Write(std::io::Error),

    #[error("flush data file: {0}")]
    Flush(std::io::Error),
}

async fn save_recording(
    logger: ArcMsgLogger,
    rec_id: &RecordingId,
    recording: &RecordingHandle,
    start_time: UnixNano,
    end_time: UnixNano,
) -> Result<(), SaveRecordingError> {
    use SaveRecordingError::*;
    logger.log(LogLevel::Debug, &format!("saving recording: {rec_id:?}"));

    let data = RecordingData {
        start: start_time,
        end: end_time,
        events: Vec::new(),
    };

    let json = serde_json::to_vec_pretty(&data)?;

    let mut data_file = recording.new_file("json").await?;
    data_file.write_all(&json).await.map_err(Write)?;
    data_file.flush().await.map_err(Flush)?;

    //go r.hooks.RecSaved(r, filePath, data)

    logger.log(LogLevel::Info, &format!("recording saved: {rec_id:?}"));

    Ok(())
}

struct RecorderMsgLogger {
    logger: ArcLogger,
    monitor_id: MonitorId,
}

impl RecorderMsgLogger {
    fn new(logger: ArcLogger, monitor_id: MonitorId) -> Self {
        Self { logger, monitor_id }
    }
}

impl MsgLogger for RecorderMsgLogger {
    fn log(&self, level: LogLevel, msg: &str) {
        self.logger.log(LogEntry::new(
            level,
            "monitor",
            &self.monitor_id,
            &format!("recorder: {msg}"),
        ));
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use common::{DummyLogger, DummyStorage, time::MINUTE};
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::io::AsyncReadExt;

    async fn new_test_recdb(recordings_dir: &Path) -> RecDb {
        RecDb::new(
            DummyLogger::new(),
            recordings_dir.to_path_buf(),
            DummyStorage::new(),
        )
        .await
    }

    #[tokio::test]
    async fn test_save_recording() {
        let start = UnixNano::new(MINUTE);
        let end = UnixNano::new(11 * MINUTE);
        let tempdir = tempdir().unwrap();

        let rec_db = new_test_recdb(&tempdir.path().join("recordings")).await;
        let recording = rec_db.test_recording().await;

        save_recording(
            DummyLogger::new(),
            &"2000-01-01_01-01-01_x".to_owned().try_into().unwrap(),
            &recording,
            start,
            end,
        )
        .await
        .unwrap();

        let mut data_file = recording.open_file("json").await.unwrap();
        let mut got = String::new();
        data_file.read_to_string(&mut got).await.unwrap();

        let want = "{
  \"start\": 60000000000,
  \"end\": 660000000000,
  \"events\": []
}";
        assert_eq!(want, got);
    }

    #[test]
    fn extend_timer_extends_on_later_end() {
        let timer_end = UnixNano::new(1_000);
        let got = extend_timer(Some(timer_end), UnixNano::new(2_000), Duration::new(500));
        assert_eq!(Some(UnixNano::new(2_500)), got);
    }

    #[test]
    fn extend_timer_keeps_earlier_end() {
        let timer_end = UnixNano::new(5_000);
        let got = extend_timer(Some(timer_end), UnixNano::new(1_000), Duration::new(500));
        assert_eq!(Some(timer_end), got);
    }

    #[test]
    fn extend_timer_always_record_untouched() {
        // None timer_end means alwaysRecord: events never modify it.
        let got = extend_timer(None, UnixNano::new(1_000), Duration::new(500));
        assert_eq!(None, got);
    }

    #[test]
    fn extend_timer_overflow_keeps_end() {
        let timer_end = UnixNano::new(1_000);
        let got = extend_timer(Some(timer_end), UnixNano::new(i64::MAX), Duration::new(1));
        assert_eq!(Some(timer_end), got);
    }

    #[test]
    fn live_event_end_drops_expired() {
        // Event ended before now -> dropped.
        let got = live_event_end(UnixNano::new(1_000), Duration::new(100), UnixNano::new(2_000));
        assert_eq!(None, got);
    }

    #[test]
    fn live_event_end_accepts_live() {
        let got = live_event_end(UnixNano::new(1_000), Duration::new(5_000), UnixNano::new(2_000));
        assert_eq!(Some(UnixNano::new(6_000)), got);
    }

    #[test]
    fn live_event_end_drops_overflow() {
        let got = live_event_end(UnixNano::new(i64::MAX), Duration::new(1), UnixNano::new(0));
        assert_eq!(None, got);
    }

    #[test]
    fn check_next_segment_ok() {
        check_next_segment(0, 1).unwrap();
        check_next_segment(41, 42).unwrap();
    }

    #[test]
    fn check_next_segment_skipped() {
        // Fields are (expected, got) to match the error message.
        match check_next_segment(10, 12) {
            Err(GenerateVideoError::SkippedSegment(expected, got)) => {
                assert_eq!((11, 12), (expected, got));
            }
            other => panic!("expected SkippedSegment, got {other:?}"),
        }
        // Behind is also an error.
        assert!(matches!(
            check_next_segment(10, 10),
            Err(GenerateVideoError::SkippedSegment(..))
        ));
    }

    #[test]
    fn check_next_segment_prev_max() {
        // u64::MAX prev id: no overflow panic, clean Add error.
        assert!(matches!(
            check_next_segment(u64::MAX, 0),
            Err(GenerateVideoError::Add)
        ));
    }

    struct TestSource;

    #[async_trait::async_trait]
    impl common::monitor::Source for TestSource {
        fn stream_type(&self) -> &common::StreamType {
            &common::StreamType::Main
        }
        async fn muxer(&self) -> Option<ArcStreamerMuxer> {
            None
        }
        async fn subscribe(&self) -> Option<common::monitor::Feed> {
            None
        }
        async fn subscribe_decoded(
            &self,
            _rt_handle: tokio::runtime::Handle,
            _logger: ArcMsgLogger,
            _limiter: Option<common::recording::FrameRateLimiter>,
        ) -> Option<Result<common::monitor::FeedDecoded, common::monitor::SubscribeDecodedError>>
        {
            None
        }
    }

    async fn test_recording_context() -> RecordingContext {
        let tempdir = tempdir().unwrap();
        RecordingContext {
            hooks: common::monitor::DummyMonitorHooks::new(),
            logger: DummyLogger::new(),
            source_main: Arc::new(TestSource),
            config: MonitorConfig::new(
                common::monitor::Config {
                    id: "x".to_owned().try_into().unwrap(),
                    name: "x".to_owned().try_into().unwrap(),
                    enable: true,
                    source: common::monitor::SelectedSource::Rtsp,
                    always_record: false,
                    video_length: 0.0,
                    ptz: common::monitor::PtzConfig::default(),
                    audio: common::monitor::AudioConfig::default(),
                },
                common::monitor::SourceConfig::Rtsp(common::monitor::SourceRtspConfig {
                    protocol: common::monitor::Protocol::Tcp,
                    main_stream: "rtsp://x".parse().unwrap(),
                    sub_stream: None,
                }),
                serde_json::json!({}),
            ),
            recdb: Arc::new(new_test_recdb(&tempdir.path().join("recordings")).await),
        }
    }

    static CRASH_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn crashy_run(
        _t: CancellationToken,
        _c: RecordingContext,
        _p: &mut PrevSeg,
    ) -> RunFuture<'_> {
        let n = CRASH_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if n == 0 {
                Err(RunRecordingError::GenerateVideo(
                    GenerateVideoError::Add,
                ))
            } else {
                Ok(())
            }
        })
    }

    #[tokio::test]
    async fn session_restarts_after_crash_and_normal_exit() {
        use std::sync::atomic::Ordering;
        CRASH_RUNS.store(0, Ordering::SeqCst);

        let token = CancellationToken::new();
        let c = test_recording_context().await;
        let mut prev_seg = None;

        let handle = tokio::spawn({
            let token = token.clone();
            async move {
                run_recording_session_loop(
                    token,
                    c,
                    &mut prev_seg,
                    crashy_run,
                    std::time::Duration::from_millis(1),
                )
                .await;
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
        handle.await.unwrap();

        // crash -> sleep -> retry Ok -> sleep -> restart(s) before cancel.
        assert!(CRASH_RUNS.load(Ordering::SeqCst) >= 3);
    }
}
