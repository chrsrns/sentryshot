// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(clippy::unused_async)]

#[cfg(test)]
mod test;

use axum::{
    Extension, Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use common::{
    AccountId, AccountSetRequest, AccountsMap, ArcAuth, ArcLogger, AuthAccountDeleteError,
    AuthenticatedUser, ILogger, LogEntry, LogLevel, MonitorId,
    monitor::{
        ArcMonitorManager, MonitorConfig, MonitorDeleteError, MonitorRestartError, MonitorSetError,
    },
    recording::RecordingId,
};
use eventdb::{EventDb, EventQuery};
use hls::{HlsQuery, HlsServer};
use http::HeaderValue;
use log::{
    Logger,
    log_db::{LogDb, LogQuery, QueryLogsError},
    slow_poller::{self, PollQuery, SlowPoller},
};
use monitor_groups::ArcMonitorGroups;
use recdb::{DeleteRecordingError, RecDb, RecDbQuery, RecordingResponse};
use recording::{VideoCache, new_video_reader};
use rust_embed::EmbeddedFiles;
use serde::Deserialize;
use std::{io::Cursor, num::NonZeroUsize, path::PathBuf, sync::Arc};
use streamer::{PlayReponse, StartSessionReponse, Streamer};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;
use vod::{CreateVodReaderError, VodCache, VodQuery, VodReader};
use web::{Templater, serve_mp4_content};

pub async fn template_handler(
    Extension(user): Extension<AuthenticatedUser>,
    State(templater): State<Arc<Templater<'_>>>,
    uri: Uri,
) -> Response {
    let path = uri.to_string();
    let path = path.strip_prefix('/').unwrap_or(&path);

    let log = |msg: &str| {
        templater
            .logger()
            .log(LogEntry::new2(LogLevel::Info, "app", msg));
    };

    let Some(template) = templater.get_template(path) else {
        log(&format!("handle_templates: get template for path: {path}"));
        return (StatusCode::INTERNAL_SERVER_ERROR, "template does not exist").into_response();
    };

    let Some(data) = templater
        .get_data(path.to_owned(), user.is_admin, user.stored_token)
        .await
    else {
        // Cancelled.
        return StatusCode::NOT_FOUND.into_response();
    };

    match template.render(data).to_string() {
        Ok(content) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=UTF-8"),
                (header::CACHE_CONTROL, "private, no-cache"),
            ],
            content,
        )
            .into_response(),
        Err(e) => {
            log(&format!("handle_templates: render template '{path}': {e}"));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to render template",
            )
                .into_response()
        }
    }
}

pub async fn asset_handler(
    Path(path): Path<String>,
    headers: HeaderMap,
    State(assets_and_etag): State<(EmbeddedFiles, String)>,
) -> Response {
    let (assets, etag) = assets_and_etag;

    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(if_none_match) = if_none_match.to_str() {
            if if_none_match == etag {
                return StatusCode::NOT_MODIFIED.into_response();
            }
        }
    }

    match assets.get(path.as_str()) {
        Some(content) => {
            let body = Body::from(content.clone());
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref()), (header::ETAG, &etag)],
                body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "404").into_response(),
    }
}

#[allow(clippy::unwrap_used)]
pub async fn hls_handler(
    Path(path): Path<String>,
    State(hls_server): State<HlsServer>,
    method: Method,
    req_headers: HeaderMap,
    query: Query<HlsQuery>,
) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("Server", HeaderValue::from_str("sentryshot").unwrap());
    headers.insert(
        "Access-Control-Allow-Credentials",
        HeaderValue::from_str("true").unwrap(),
    );

    match method {
        Method::GET => {}
        Method::OPTIONS => {
            headers.insert(
                "Access-Control-Allow-Methods",
                HeaderValue::from_static("GET, OPTIONS"),
            );
            headers.insert(
                "Access-Control-Allow-Headers",
                req_headers
                    .get("Access-Control-Request-Headers")
                    .unwrap_or(&HeaderValue::from_str("").unwrap())
                    .to_owned(),
            );
            return (StatusCode::OK, headers).into_response();
        }
        _ => return (StatusCode::METHOD_NOT_ALLOWED, headers).into_response(),
    }

    let (muxer_name, file_name) = match parse_path(path) {
        Ok(v) => v,
        Err(e) => {
            return (
                headers,
                Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(format!("parse path: {e}"))
                    .unwrap(),
            )
                .into_response();
        }
    };

    let Some(Some(muxer)) = hls_server.muxer_by_name(muxer_name).await else {
        return (StatusCode::NOT_FOUND, headers, "muxer not found").into_response();
    };
    let res = muxer.file(&file_name, &query.0).await;

    if let Some(h) = res.headers {
        for (k, v) in h {
            headers.insert(k, v);
        }
    }

    if let Some(body) = res.body {
        let stream = ReaderStream::new(body);
        let body = Body::from_stream(stream);
        (res.status, headers, body).into_response()
    } else {
        (headers, res.status).into_response()
    }
}

#[derive(Debug, Error)]
pub enum ParsePathError {
    #[error("no directory")]
    NoDir,

    #[error("invalid directory")]
    InvalidDir,

    #[error("no file name")]
    NoFileName,

    #[error("invalid file name")]
    InvalidFileName,
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn parse_path(path: String) -> Result<(String, String), ParsePathError> {
    use ParsePathError::*;
    if path.ends_with(".m3u8")
        || path.ends_with(".ts")
        || path.ends_with(".mp4")
        || path.ends_with(".mp")
    {
        let p = PathBuf::from(path);
        Ok((
            p.parent()
                .ok_or(NoDir)?
                .to_str()
                .ok_or(InvalidDir)?
                .to_owned(),
            p.file_name()
                .ok_or(NoFileName)?
                .to_str()
                .ok_or(InvalidFileName)?
                .to_owned(),
        ))
    } else {
        Ok((path, String::new()))
    }
}

#[derive(Clone)]
pub struct VodHandlerState {
    pub logger: Arc<Logger>,
    pub recdb: Arc<RecDb>,
    pub cache: VodCache,
}

pub async fn vod_handler(
    State(state): State<VodHandlerState>,
    query: Query<VodQuery>,
    headers: HeaderMap,
) -> Response {
    use CreateVodReaderError::*;
    let monitor_id = query.0.monitor_id.clone();
    let reader = match VodReader::new(&state.recdb, &state.cache, query.0).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::NOT_FOUND, "no video found").into_response(),
        Err(e @ (NegativeDuration | MaxDuration)) => {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
        Err(e) => {
            state.logger.log(LogEntry::new(
                LogLevel::Error,
                "app",
                &monitor_id,
                &format!("vod handler: {e}"),
            ));
            return (StatusCode::INTERNAL_SERVER_ERROR, "error printed to logs").into_response();
        }
    };
    serve_mp4_content(&Method::GET, &headers, None, reader.size(), reader).await
}

const API_HTML: &str = include_str!("./api.html");

pub async fn api_page_handler() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=UTF-8")],
        API_HTML,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct AccountDeleteQuery {
    pub id: AccountId,
}

pub async fn account_delete_handler(
    State(auth): State<ArcAuth>,
    query: Query<AccountDeleteQuery>,
) -> (StatusCode, String) {
    use AuthAccountDeleteError::*;
    match auth.account_delete(&query.id).await {
        Ok(()) => (StatusCode::OK, String::new()),
        Err(e @ AccountNotExist(_)) => (StatusCode::NOT_FOUND, e.to_string()),
        Err(SaveAccounts(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn account_put_handler(
    State(auth): State<ArcAuth>,
    Json(payload): Json<AccountSetRequest>,
) -> Response {
    match auth.account_set(payload).await {
        Ok(created) => {
            if created {
                StatusCode::CREATED.into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn accounts_handler(State(auth): State<ArcAuth>) -> Json<AccountsMap> {
    Json(auth.accounts().await)
}

pub async fn account_my_token_handler(Extension(user): Extension<AuthenticatedUser>) -> Response {
    user.stored_token.into_response()
}

#[derive(Clone)]
pub struct RecordingQueryHandlerState {
    pub logger: Arc<Logger>,
    pub recdb: Arc<RecDb>,
    pub eventdb: EventDb,
}

pub async fn recording_query_handler(
    State(s): State<RecordingQueryHandlerState>,
    query: Query<RecDbQuery>,
) -> Result<Json<Vec<RecordingResponse>>, StatusCode> {
    let mut recordings = match s.recdb.recordings_by_query(&query.0).await {
        Ok(v) => v,
        Err(e) => {
            s.logger.log(LogEntry::new2(
                LogLevel::Error,
                "recdb",
                &format!("query recordings: {e}"),
            ));
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    for rec in &mut recordings {
        let monitor_id = rec.id().monitor_id().clone();
        let data = match rec {
            RecordingResponse::Active(v) => &mut v.data,
            RecordingResponse::Finalized(v) => &mut v.data,
            RecordingResponse::Incomplete(_) => continue,
        };
        let Some(data) = data else { continue };
        if !data.events.is_empty() {
            // Old events populated from recdb.
            continue;
        }
        if data.start == data.end {
            // Newly started recording.
            continue;
        }
        let query = EventQuery {
            start: data.start,
            end: data.end,
            limit: NonZeroUsize::new(100_000).expect("not zero"),
        };
        match s.eventdb.query(monitor_id.clone(), query).await {
            Ok(Some(v)) => data.events = v,
            Ok(None) => {}
            Err(e) => {
                s.logger.log(LogEntry::new(
                    LogLevel::Error,
                    "eventdb",
                    &monitor_id,
                    &format!("query events: {e}"),
                ));
            }
        }
    }
    Ok(Json(recordings))
}

pub async fn log_slow_poll_handler(
    State(poller): State<SlowPoller>,
    query: Query<PollQuery>,
) -> Response {
    match poller.slow_poll(query.0).await {
        slow_poller::Response::Ok(v) => Json::from(v).into_response(),
        slow_poller::Response::TooManyConncetions => StatusCode::TOO_MANY_REQUESTS.into_response(),
        slow_poller::Response::Cancelled => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn log_query_handler(State(log_db): State<LogDb>, query: Query<LogQuery>) -> Response {
    match log_db.query(query.0).await {
        Ok(v) => Json::from(v).into_response(),
        Err(QueryLogsError::ListChunks(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(QueryLogsError::TimeToId(e)) => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MonitorIdQuery {
    id: MonitorId,
}

/// Route state for monitor lifecycle handlers — the audio service
/// is needed so delete/disable/key-change tear live audio sessions
/// down instead of waiting for the next consumer op.
#[derive(Clone)]
pub struct MonitorLifecycleState {
    pub monitors: ArcMonitorManager,
    pub audio: Arc<audio::AudioService>,
}

/// Endpoint+creds key — covers `mainStream` ∪ `subStream`; mirrors
/// the attach key in `attach_direction`.
fn audio_key(config: &MonitorConfig) -> String {
    let common::monitor::SourceConfig::Rtsp(rtsp) = config.source();
    let main = (*rtsp.main_stream).to_string();
    match &rtsp.sub_stream {
        Some(sub) => format!("{main}|{}", (**sub)),
        None => main,
    }
}

pub async fn monitor_delete_handler(
    State(state): State<MonitorLifecycleState>,
    query: Query<MonitorIdQuery>,
) -> Response {
    match state.monitors.monitor_delete(query.id.clone()).await {
        Some(Ok(())) => {
            state.audio.drop_entry(&query.id).await;
            (StatusCode::OK, String::new()).into_response()
        }
        Some(Err(e @ MonitorDeleteError::NotExist(_))) => {
            (StatusCode::NOT_FOUND, e.to_string()).into_response()
        }
        Some(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn monitor_put_handler(
    State(state): State<MonitorLifecycleState>,
    Json(payload): Json<MonitorConfig>,
) -> Response {
    let id = payload.id().clone();
    let old_key = match state.monitors.monitor_config(id.clone()).await {
        Some(Some(old)) => Some(audio_key(&old)),
        _ => None,
    };
    let audio_disabled = !payload.config().audio.enable();
    let new_key = audio_key(&payload);
    tokio::spawn(async move {
        match state.monitors.monitor_set(payload).await {
            Some(Ok(created)) => {
                // Disable ∨ endpoint/creds change ⇒ TEARDOWN + drop
                // every consumer now, not on the next op.
                if audio_disabled || old_key.is_some_and(|k| k != new_key) {
                    state.audio.drop_entry(&id).await;
                }
                if created {
                    StatusCode::CREATED.into_response()
                } else {
                    StatusCode::OK.into_response()
                }
            }
            Some(Err(e @ MonitorSetError::InvalidPtzAddr)) => {
                (StatusCode::BAD_REQUEST, e.to_string()).into_response()
            }
            Some(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    })
    .await
    .expect("join")
}

pub async fn monitor_restart_handler(
    State(monitor_manager): State<ArcMonitorManager>,
    query: Query<MonitorIdQuery>,
) -> Response {
    match monitor_manager.monitor_restart(query.id.clone()).await {
        Some(Ok(())) => StatusCode::OK.into_response(),
        Some(Err(MonitorRestartError::NotExist(_))) | None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn monitors_handler(State(monitor_manager): State<ArcMonitorManager>) -> Response {
    match monitor_manager.monitor_configs().await.clone() {
        Some(v) => Json(v).into_response(),
        // Cancelled.
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn monitor_groups_get_handler(
    State(monitor_groups): State<ArcMonitorGroups>,
) -> Json<monitor_groups::Groups> {
    Json(monitor_groups.get().await)
}

pub async fn monitor_groups_put_handler(
    State(monitor_groups): State<ArcMonitorGroups>,
    Json(payload): Json<monitor_groups::Groups>,
) -> Response {
    tokio::spawn(async move {
        match monitor_groups.set(payload).await {
            Ok(()) => StatusCode::OK.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    })
    .await
    .expect("join")
}

#[derive(Debug, Deserialize)]
pub struct Mp4StreamerQuery {
    #[serde(rename = "session-id")]
    session_id: u32,

    #[serde(rename = "monitor-id")]
    monitor_id: MonitorId,

    #[serde(rename = "sub-stream")]
    sub_stream: bool,
}

pub async fn streamer_start_session_handler(
    State(mp4_streamer): State<Streamer>,
    query: Query<Mp4StreamerQuery>,
) -> Response {
    use StartSessionReponse::*;
    let result = mp4_streamer
        .start_session(query.monitor_id.clone(), query.sub_stream, query.session_id)
        .await;
    match result {
        Ready(v) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            (headers, Json(v)).into_response()
        }
        SessionAlreadyExist => (StatusCode::BAD_REQUEST, "session already exists").into_response(),
        NotReady | MuxerNotExist | StreamerCancelled | MuxerCancelled => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

pub async fn streamer_play_handler(
    State(mp4_streamer): State<Streamer>,
    query: Query<Mp4StreamerQuery>,
) -> Response {
    use PlayReponse::*;
    let result = mp4_streamer
        .play(query.monitor_id.clone(), query.sub_stream, query.session_id)
        .await;
    match result {
        Ready(ready) => match ready {
            Ok(ready) => {
                let mut headers = HeaderMap::new();
                headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

                let stream = ReaderStream::with_capacity(
                    Cursor::new(ready),
                    streamer::Muxer::HTTP_BUFFER_SIZE,
                );
                let body = Body::from_stream(stream);

                (headers, body).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
        FramesExpired => StatusCode::GONE.into_response(),
        SessionNotExist => (
            StatusCode::PRECONDITION_REQUIRED,
            "start a session with /api/streamer/start-session first",
        )
            .into_response(),
        SessionAlreadyLocked => (
            StatusCode::BAD_REQUEST,
            "session cannot be polled concurrently",
        )
            .into_response(),
        MuxerNotExist | StreamerCancelled | MuxerCancelled => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

pub async fn recording_delete_handler(
    State(rec_db): State<Arc<RecDb>>,
    Path(rec_id): Path<RecordingId>,
) -> Response {
    let (_, err) = rec_db.delete_recording(rec_id).await;
    match err {
        None => StatusCode::OK.into_response(),
        Some(e @ DeleteRecordingError::Active) => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Some(DeleteRecordingError::NotExist) => StatusCode::NOT_FOUND.into_response(),
        Some(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn recording_thumbnail_handler(
    State(rec_db): State<Arc<RecDb>>,
    Path(rec_id): Path<RecordingId>,
) -> Response {
    let Some(path) = rec_db.thumbnail_path(&rec_id).await else {
        return (StatusCode::NOT_FOUND).into_response();
    };

    let file = match tokio::fs::OpenOptions::new().read(true).open(path).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("open file: {e}")).into_response();
        }
    };

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    (
        ([
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "max-age=31536000, immutable"),
        ]),
        body,
    )
        .into_response()
}

#[derive(Clone)]
pub struct RecordingVideoState {
    pub rec_db: Arc<RecDb>,
    pub video_cache: Arc<Mutex<VideoCache>>,
    pub logger: ArcLogger,
}

#[derive(Debug, Deserialize)]
pub struct RecordingVideoQuery {
    #[serde(default, rename = "cache-id")]
    cache_id: u32,
}

pub async fn recording_video_handler(
    State(state): State<RecordingVideoState>,
    Path(rec_id): Path<RecordingId>,
    query: Query<RecordingVideoQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(meta_path) = state.rec_db.recording_file_by_ext(&rec_id, "meta").await else {
        return (StatusCode::NOT_FOUND).into_response();
    };
    let Some(mdat_path) = state.rec_db.recording_file_by_ext(&rec_id, "mdat").await else {
        return (StatusCode::NOT_FOUND).into_response();
    };

    let video = match new_video_reader(
        &meta_path,
        &mdat_path,
        query.cache_id,
        &Some(state.video_cache),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            state.logger.log(LogEntry::new2(
                LogLevel::Error,
                "app",
                &format!("video request: {e}"),
            ));
            return (StatusCode::INTERNAL_SERVER_ERROR, "see logs for details").into_response();
        }
    };

    serve_mp4_content(
        &Method::GET,
        &headers,
        Some(video.last_modified()),
        video.size(),
        video,
    )
    .await
}

/// `POST /api/scan` — admin-gated camera discovery.
///
/// Error bodies are static strings only — nothing user- or
/// device-supplied echoes back (in particular not
/// credential-bearing stream URLs).
pub async fn scan_handler(
    State(scanner): State<Arc<scan::scanner::Scanner>>,
    Json(req): Json<scan::types::ScanRequest>,
) -> Response {
    match scanner.scan(req).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => scan_error_response(&e),
    }
}

fn scan_error_response(e: &scan::ScanError) -> Response {
    match e {
        scan::ScanError::InvalidHost(_) => {
            (StatusCode::BAD_REQUEST, "invalid host").into_response()
        }
        scan::ScanError::Busy => (StatusCode::CONFLICT, "scan already in progress").into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "scan failed").into_response(),
    }
}

/// Per-monitor PTZ dispatch entry: a cached client plus a lock that
/// serializes commands in request order — `stop` cannot leapfrog `move`
/// on slow links. Rebuilt when the endpoint+creds key changes (config
/// edit).
struct PtzEntry {
    key: String,
    client: ptz::ArcPtzClient,
    lock: tokio::sync::Mutex<()>,
    /// Command names the camera faulted `not implemented`/`not
    /// recognized` — session-scoped marks; persisted copies live in
    /// `ptz.unsupported` on the monitor config.
    unsupported: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Builds and caches `PtzEntry`s per monitor.
pub struct PtzService {
    factory: Arc<dyn Fn(String, (String, String)) -> ptz::ArcPtzClient + Send + Sync>,
    entries: std::sync::Mutex<std::collections::HashMap<String, Arc<PtzEntry>>>,
}

impl PtzService {
    pub fn new(
        factory: Arc<dyn Fn(String, (String, String)) -> ptz::ArcPtzClient + Send + Sync>,
    ) -> Self {
        Self {
            factory,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Entry for `id`, rebuilt when `key` (endpoint+creds) differs from
    /// the cached one.
    fn entry(
        &self,
        id: &str,
        key: String,
        endpoint: String,
        creds: (String, String),
    ) -> Arc<PtzEntry> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match entries.get(id) {
            Some(entry) if entry.key == key => entry.clone(),
            _ => {
                let entry = Arc::new(PtzEntry {
                    key,
                    client: (self.factory)(endpoint, creds),
                    lock: tokio::sync::Mutex::new(()),
                    unsupported: std::sync::Mutex::new(std::collections::HashSet::new()),
                });
                entries.insert(id.to_owned(), entry.clone());
                entry
            }
        }
    }
}

/// Route state for `POST /api/monitor/ptz`.
pub struct PtzRouteState {
    pub monitors: ArcMonitorManager,
    pub service: PtzService,
}

/// `POST /api/monitor/ptz?id=` — relay one PTZ command to the
/// monitor's camera. Admin + CSRF via `route_admin`.
///
/// Credentials come only from the stored monitor `mainStream`
/// userinfo — never the request body.
pub async fn ptz_handler(
    State(state): State<Arc<PtzRouteState>>,
    Query(query): Query<MonitorIdQuery>,
    Json(cmd): Json<ptz::PtzCommand>,
) -> Response {
    if let Some(resp) = ptz_validate(&cmd) {
        return resp;
    }

    let Some(Some(config)) = state.monitors.monitor_config(query.id.clone()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !config.config().ptz.enable() {
        return (StatusCode::CONFLICT, "ptz disabled").into_response();
    }

    let common::monitor::SourceConfig::Rtsp(rtsp) = config.source();
    let creds = onvif::creds_from_url(rtsp.main_stream.as_str());
    let endpoint =
        match ptz::resolve_endpoint(rtsp.main_stream.as_str(), config.config().ptz.onvif_addr()) {
            Ok(e) => e,
            Err(e) => return ptz_error_response(&e),
        };
    let key = format!("{endpoint}\u{0}{}\u{0}{}", creds.0, creds.1);
    let entry = state.service.entry(&query.id, key, endpoint, creds);

    // Persisted marks ∪ session marks — already-proven-unsupported
    // commands short-circuit without a camera round trip.
    let unsupported = |extra: &std::collections::HashSet<String>| {
        config
            .config()
            .ptz
            .unsupported()
            .iter()
            .cloned()
            .chain(extra.iter().cloned())
            .collect::<std::collections::HashSet<String>>()
    };
    {
        let session = entry
            .unsupported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let all = unsupported(&session);
        if cmd.command != "capabilities" && all.contains(cmd.command.as_str()) {
            return ptz_error_response(&ptz::Error::NotImplemented);
        }
    }

    // Serialized per monitor — move lands before stop, in request order.
    let _guard = entry.lock.lock().await;
    let client = &entry.client;

    if cmd.command == "capabilities" {
        return ptz_capabilities(&state, &query.id, &entry, &unsupported).await;
    }

    let result = match cmd.command.as_str() {
        "move" => {
            client
                .move_cam(cmd.pan, cmd.tilt, cmd.zoom, cmd.timeout.as_deref())
                .await
        }
        "stop" => client.stop().await,
        "home" => client.home().await,
        "getPresets" => match client.get_presets().await {
            Ok(presets) => {
                return Json(ptz::PtzResponse {
                    presets: Some(presets),
                    unsupported: None,
                })
                .into_response();
            }
            Err(e) => Err(e),
        },
        "setPreset" => {
            // Validated above.
            client
                .set_preset(cmd.preset_name.as_deref().unwrap_or_default())
                .await
                .map(|_| ())
        }
        "gotoPreset" => {
            client
                .goto_preset(cmd.preset_token.as_deref().unwrap_or_default())
                .await
        }
        "removePreset" => {
            client
                .remove_preset(cmd.preset_token.as_deref().unwrap_or_default())
                .await
        }
        _ => unreachable!("validated above"),
    };
    match result {
        Ok(()) => Json(ptz::PtzResponse {
            presets: None,
            unsupported: None,
        })
        .into_response(),
        Err(e) => {
            if matches!(e, ptz::Error::NotImplemented) {
                mark_unsupported(&state, &query.id, &entry, &cmd.command).await;
            }
            ptz_error_response(&e)
        }
    }
}

/// Command validation — `Some(response)` on failure, before any
/// camera traffic.
fn ptz_validate(cmd: &ptz::PtzCommand) -> Option<Response> {
    let bad = |msg: &'static str| Some((StatusCode::BAD_REQUEST, msg).into_response());
    match cmd.command.as_str() {
        "move" => {
            if cmd.pan.is_none() && cmd.tilt.is_none() && cmd.zoom.is_none() {
                return bad("move requires at least one velocity");
            }
            for v in [cmd.pan, cmd.tilt, cmd.zoom].into_iter().flatten() {
                if !(-1.0..=1.0).contains(&v) {
                    return bad("velocity out of range −1.0..=1.0");
                }
            }
            if let Some(t) = &cmd.timeout {
                if !["PT0.5S", "PT1S", "PT2S"].contains(&t.as_str()) {
                    return bad("timeout must be PT0.5S, PT1S, or PT2S");
                }
            }
        }
        "gotoPreset" | "removePreset" => {
            if cmd.timeout.is_some() {
                return bad("timeout applies to move only");
            }
            if cmd.preset_token.is_none() {
                return bad("presetToken required");
            }
        }
        "setPreset" => {
            if cmd.timeout.is_some() {
                return bad("timeout applies to move only");
            }
            match &cmd.preset_name {
                Some(name) if !name.is_empty() && name.chars().count() <= 64 => {}
                _ => return bad("presetName required, 1..=64 chars"),
            }
        }
        "stop" | "home" | "getPresets" | "capabilities" => {
            if cmd.timeout.is_some() {
                return bad("timeout applies to move only");
            }
        }
        _ => return bad("unknown command"),
    }
    None
}

/// Record a capability mark: session set + persisted
/// `ptz.unsupported` on the stored monitor config.
async fn mark_unsupported(
    state: &Arc<PtzRouteState>,
    monitor_id: &MonitorId,
    entry: &Arc<PtzEntry>,
    command: &str,
) {
    entry
        .unsupported
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(command.to_owned());
    persist_mark(state, monitor_id, command).await;
}

/// `capabilities` — eager `GetPresets` probe (safe read drives the
/// session resolve; a `not implemented` fault marks `getPresets`
/// before the FE ever asks for the list), then the persisted ∪
/// session mark set. Caller holds the entry lock.
async fn ptz_capabilities(
    state: &Arc<PtzRouteState>,
    monitor_id: &MonitorId,
    entry: &Arc<PtzEntry>,
    unsupported: &impl Fn(&std::collections::HashSet<String>) -> std::collections::HashSet<String>,
) -> Response {
    let probe_needed = {
        let session = entry
            .unsupported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !unsupported(&session).contains("getPresets")
    };
    if probe_needed
        && matches!(
            entry.client.get_presets().await,
            Err(ptz::Error::NotImplemented)
        )
    {
        mark_unsupported(state, monitor_id, entry, "getPresets").await;
    }
    let session = entry
        .unsupported
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut marks: Vec<String> = unsupported(&session).into_iter().collect();
    marks.sort();
    Json(ptz::PtzResponse {
        presets: None,
        unsupported: Some(marks),
    })
    .into_response()
}

fn ptz_error_response(e: &ptz::Error) -> Response {
    // Seam-level validation failures are client errors, not camera
    // faults — route validation catches them first, but a caller
    // bypassing it shouldn't mislabel a 400-class problem as 502.
    let status = match e {
        ptz::Error::BadVelocity | ptz::Error::BadTimeout | ptz::Error::BadPresetName => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, e.to_string()).into_response()
}

/// Write a capability mark into the stored monitor config —
/// `ptz.unsupported` gains `command`. Best-effort: a read-modify-write
/// failure is ignored, the session mark still protects this session.
async fn persist_mark(state: &Arc<PtzRouteState>, monitor_id: &MonitorId, command: &str) {
    let Some(Some(mut stored)) = state.monitors.monitor_config(monitor_id.clone()).await else {
        return;
    };
    let mut marks: Vec<String> = stored.config().ptz.unsupported().to_vec();
    if marks.iter().any(|c| c == command) {
        return;
    }
    marks.push(command.to_owned());
    marks.sort();
    if let Some(ptz_obj) = stored
        .raw_mut()
        .as_object_mut()
        .and_then(|raw| raw.get_mut("ptz"))
        .and_then(serde_json::Value::as_object_mut)
    {
        ptz_obj.insert("unsupported".to_owned(), serde_json::json!(marks));
    } else {
        return;
    }
    // Typed views deserialize once — re-parse before storing.
    let Ok(stored) = serde_json::from_value::<common::monitor::MonitorConfig>(stored.raw().clone())
    else {
        return;
    };
    drop(state.monitors.monitor_set(stored).await);
}

// SPDX-SnippetBegin
// SPDX-Snippet-CopyrightText: two-way audio routes

/// Route state for the audio endpoints — monitor lookups drive the
/// lazy delete/disable checks; the service owns sessions + tokens.
pub struct AudioRouteState {
    pub monitors: ArcMonitorManager,
    pub service: Arc<audio::AudioService>,
}

/// `POST /api/monitor/audio-session?id=` — mints the single-use
/// ≤30 s WS token only; camera contact is lazy per the session
/// model (opens on the first direction ctrl post-upgrade).
pub async fn audio_session_handler(
    State(state): State<Arc<AudioRouteState>>,
    Query(query): Query<MonitorIdQuery>,
) -> Response {
    let Some(Some(config)) = state.monitors.monitor_config(query.id.clone()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let audio_cfg = &config.config().audio;
    if !audio_cfg.enable() {
        return (StatusCode::CONFLICT, "audio disabled").into_response();
    }
    // All-directions-marked is the only pre-attach short-circuit —
    // per-direction marks surface via `error` ctrls post-upgrade.
    let marks = audio_cfg.unsupported();
    if marks.iter().any(|m| m == "listen") && marks.iter().any(|m| m == "talk") {
        return (StatusCode::CONFLICT, "audio unsupported").into_response();
    }

    let token = state.service.mint_token(query.id.clone()).await;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "token": token })),
    )
        .into_response()
}

/// Query shape for the WS upgrade — token-only, monitor binding
/// rides inside the stored token record.
#[derive(Debug, Deserialize)]
pub struct AudioWsQuery {
    token: String,
}

/// `GET /api/monitor/audio?token=` — `route_no_auth` WS upgrade.
/// `Origin` host:port must match the request `Host` (a browser WS
/// can't set `X-CSRF-TOKEN`) and is checked BEFORE the token is
/// consumed so a bad-Origin probe never burns it.
pub async fn audio_ws_handler(
    State(state): State<Arc<AudioRouteState>>,
    Query(query): Query<AudioWsQuery>,
    headers: HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    if !origin_matches_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(monitor_id) = state.service.consume_token(&query.token).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| audio_consumer(state, monitor_id, socket))
        .into_response()
}

/// `Origin` host:port == request `Host`, port-normalized (absent =
/// scheme default). Missing `Origin` fails closed.
fn origin_matches_host(headers: &HeaderMap) -> bool {
    let (Some(origin), Some(host)) = (
        headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
    ) else {
        return false;
    };
    let Ok(origin_url) = url::Url::parse(origin) else {
        return false;
    };
    let Some(origin_host) = origin_url.host_str() else {
        return false;
    };
    let origin_port = origin_url.port().unwrap_or_else(|| {
        if origin_url.scheme() == "https" {
            443
        } else {
            80
        }
    });
    // `Host` may omit the port — default to the request scheme's.
    let (host_name, host_port) = match host.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(p) => (h, p),
            Err(_) => (host, 80),
        },
        None => (host, 80),
    };
    host_name.eq_ignore_ascii_case(origin_host) && host_port == origin_port
}

/// Client→server ctrl — only the direction verbs are meaningful;
/// `caps`/`busy`/`error` from a client are ignored (server emits).
#[derive(Debug, Deserialize)]
struct AudioCtrl {
    #[serde(rename = "type")]
    kind: String,
}

/// One consumer WS — ctrl dispatch, PCM bridge, keepalive ping, and
/// the directionless clock. Attach happens on the first direction
/// ctrl, never at upgrade.
#[allow(clippy::too_many_lines)]
async fn audio_consumer(
    state: Arc<AudioRouteState>,
    monitor_id: MonitorId,
    socket: axum::extract::ws::WebSocket,
) {
    use axum::extract::ws::Message;

    let consumer = state.service.consumer_id();
    let (drop_tx, mut drop_rx) = tokio::sync::mpsc::unbounded_channel::<audio::ConsumerDrop>();
    let mut socket = socket;
    let mut ping = tokio::time::interval(audio::WS_PING_CADENCE);
    ping.tick().await; // first tick is immediate — skip it

    let mut entry: Option<Arc<audio::AudioEntry>> = None;
    // Interest-tracking: the directionless clock runs while the WS
    // holds no direction — on upgrade and re-armed on detach.
    let mut has_interest = false;
    let mut directionless = Box::pin(tokio::time::sleep(audio::DIRECTIONLESS_TIMEOUT));
    let mut pcm_rx: Option<tokio::sync::broadcast::Receiver<audio::PcmChunk>> = None;
    let mut ctrl_rx: Option<tokio::sync::broadcast::Receiver<String>> = None;

    loop {
        tokio::select! {
            frame = socket.recv() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let Ok(ctrl) = serde_json::from_str::<AudioCtrl>(&text) else {
                        continue;
                    };
                    // The lazy monitor check — every op re-reads the
                    // stored config so delete/disable drops the entry.
                    let Some(Some(config)) =
                        state.monitors.monitor_config(monitor_id.clone()).await
                    else {
                        _ = socket.send(close_frame(1000)).await;
                        break;
                    };
                    if !config.config().audio.enable() {
                        state.service.drop_entry(&monitor_id).await;
                        _ = socket.send(close_frame(1000)).await;
                        break;
                    }
                    match ctrl.kind.as_str() {
                        "listen" => {
                            match attach_direction(
                                &state, &monitor_id, &config, &mut entry,
                                consumer, audio::Direction::Listen, &drop_tx, &mut socket, &mut ctrl_rx,
                            ).await {
                                Some(e) => {
                                    // Only a granted attach counts —
                                    // busy/unsupported leaves the
                                    // directionless clock armed.
                                    has_interest = e.has_interest(consumer).await;
                                    if has_interest {
                                        pcm_rx = e.subscribe_pcm().await;
                                    }
                                }
                                None => break,
                            }
                        }
                        "talk-start" => {
                            match attach_direction(
                                &state, &monitor_id, &config, &mut entry,
                                consumer, audio::Direction::Talk, &drop_tx, &mut socket, &mut ctrl_rx,
                            ).await {
                                Some(e) => {
                                    has_interest = e.has_interest(consumer).await;
                                }
                                None => break,
                            }
                        }
                        "listen-stop" => {
                            if let Some(e) = &entry {
                                state.service.remove_direction(
                                    e, consumer, audio::Direction::Listen,
                                ).await;
                                pcm_rx = None;
                                if !e.has_interest(consumer).await {
                                    has_interest = false;
                                    directionless.as_mut().reset(
                                        tokio::time::Instant::now()
                                            + audio::DIRECTIONLESS_TIMEOUT,
                                    );
                                }
                            }
                        }
                        "talk-end" => {
                            if let Some(e) = &entry {
                                state.service.remove_direction(
                                    e, consumer, audio::Direction::Talk,
                                ).await;
                                if !e.has_interest(consumer).await {
                                    has_interest = false;
                                    directionless.as_mut().reset(
                                        tokio::time::Instant::now()
                                            + audio::DIRECTIONLESS_TIMEOUT,
                                    );
                                }
                            }
                        }
                        _ => {} // unknown/client-sent server types: ignored
                    }
                    // Any direction-ctrl receipt re-arms the clock —
                    // a denied attach keeps its retry window.
                    if matches!(
                        ctrl.kind.as_str(),
                        "listen" | "listen-stop" | "talk-start" | "talk-end"
                    ) {
                        directionless.as_mut().reset(
                            tokio::time::Instant::now() + audio::DIRECTIONLESS_TIMEOUT,
                        );
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    // Non-owner/negotiation-less frames drop inside
                    // `entry.talk` — parse is byte-pair decode only.
                    if let Some(e) = &entry {
                        let pcm: Vec<i16> = bytes
                            .chunks_exact(2)
                            .map(|c| i16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        if !pcm.is_empty() {
                            let _ = e.talk(consumer, &pcm).await;
                        }
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                _ => ()
            },
            pcm = async {
                match &mut pcm_rx {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(chunk) = pcm {
                    let bytes: Vec<u8> = chunk
                        .iter()
                        .flat_map(|s| s.to_le_bytes())
                        .collect();
                    _ = socket.send(Message::Binary(bytes.into())).await;
                }
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            },
            drop = drop_rx.recv() => match drop {
                Some(audio::ConsumerDrop::Error(msg, code)) => {
                    _ = socket.send(error_ctrl(&msg)).await;
                    _ = socket.send(close_frame(code)).await;
                    break;
                }
                Some(audio::ConsumerDrop::Close(code)) => {
                    _ = socket.send(close_frame(code)).await;
                    break;
                }
                None => break,
            },
            // Server→consumer ctrl broadcast — `caps` on lock
            // release / mark change / session events.
            ctrl = async {
                match &mut ctrl_rx {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(text) = ctrl {
                    _ = socket.send(Message::Text(text.into())).await;
                }
            },
            () = &mut directionless, if !has_interest => {
                _ = socket.send(close_frame(1008)).await;
                break;
            },
        }
    }

    if let Some(e) = &entry {
        state.service.detach(e, consumer).await;
    }
}

/// Resolve entry + run `service.attach` for one direction ctrl;
/// emits `caps`/`busy`/`error` back. `None` = consumer dropped.
#[allow(clippy::too_many_arguments)]
async fn attach_direction(
    state: &Arc<AudioRouteState>,
    monitor_id: &MonitorId,
    config: &MonitorConfig,
    entry: &mut Option<Arc<audio::AudioEntry>>,
    consumer: u64,
    direction: audio::Direction,
    drop_tx: &tokio::sync::mpsc::UnboundedSender<audio::ConsumerDrop>,
    socket: &mut axum::extract::ws::WebSocket,
    ctrl_rx: &mut Option<tokio::sync::broadcast::Receiver<String>>,
) -> Option<Arc<audio::AudioEntry>> {
    // Endpoint+creds key — both stream URLs carry their userinfo;
    // `subStream` is the session's 429 fallback.
    let common::monitor::SourceConfig::Rtsp(rtsp) = config.source();
    let url: url::Url = (*rtsp.main_stream).clone();
    let alt: Option<url::Url> = rtsp.sub_stream.as_ref().map(|u| (**u).clone());
    let key = match &alt {
        Some(a) => format!("{url}|{a}"),
        None => url.to_string(),
    };

    // Every attach re-keys — a mainStream/subStream/creds change
    // rebuilds the entry and drops the stale session's consumers.
    // The V380 probe resolves its inputs live from `Config` inside
    // `open` — a saved `v380*`/`ptz.onvifAddr` edit takes effect
    // on the next open without an entry rebuild.
    let monitors = state.monitors.clone();
    let cfg_id = monitor_id.clone();
    let client = state.service.make_client(
        url,
        alt,
        Arc::new(move || {
            let monitors = monitors.clone();
            let id = cfg_id.clone();
            let f: std::pin::Pin<Box<dyn std::future::Future<Output = audio::V380Cfg> + Send>> =
                Box::pin(async move {
                    let Some(Some(config)) = monitors.monitor_config(id).await else {
                        return audio::V380Cfg::default();
                    };
                    let a = &config.config().audio;
                    audio::V380Cfg {
                        device_id: a.v380_device_id(),
                        user: a.v380_user().map(str::to_owned),
                        onvif_addr: config.config().ptz.onvif_addr().to_owned(),
                    }
                });
            f
        }),
    );
    *entry = Some(state.service.entry(monitor_id, key, client).await);
    let e = entry.as_ref().expect("entry resolved above");

    match state
        .service
        .attach(
            e,
            monitor_id,
            consumer,
            direction,
            config.config().audio.unsupported(),
            drop_tx,
        )
        .await
    {
        audio::Attach::Open(caps, _pcm, rx) => {
            *ctrl_rx = Some(rx);
            // `caps` — grant/report: merged mark set + lock state.
            let mut marks: Vec<String> = config.config().audio.unsupported().to_vec();
            marks.extend(e.session_marks().await);
            marks.sort();
            marks.dedup();
            _ = socket.send(axum::extract::ws::Message::Text(
                serde_json::json!({
                    "type": "caps",
                    "listen": caps.listen,
                    "talk": caps.talk,
                    "talkFree": caps.talk && !e.talk_owned().await,
                    "unsupported": marks,
                })
                .to_string()
                .into(),
            ));
            Some(e.clone())
        }
        audio::Attach::Busy => {
            _ = socket.send(busy_ctrl()).await;
            Some(e.clone())
        }
        audio::Attach::Retryable(msg) => {
            _ = socket.send(error_ctrl(&msg)).await;
            Some(e.clone())
        }
        audio::Attach::Unsupported => {
            _ = socket
                .send(error_ctrl(&format!("{} unsupported", direction.name())))
                .await;
            Some(e.clone())
        }
        audio::Attach::Fault(msg) => {
            _ = socket.send(error_ctrl(&msg)).await;
            _ = socket.send(close_frame(1000)).await;
            None
        }
        audio::Attach::Full => {
            _ = drop_tx.send(audio::ConsumerDrop::Error(
                "consumer bound".to_owned(),
                1008,
            ));
            None
        }
    }
}

/// Server-initiated close frame carrying a WS status code.
fn close_frame(code: u16) -> axum::extract::ws::Message {
    axum::extract::ws::Message::Close(Some(axum::extract::ws::CloseFrame {
        code,
        reason: "".into(),
    }))
}

/// `{"type":"busy","command":"talk-start"}` — the denied verb.
fn busy_ctrl() -> axum::extract::ws::Message {
    axum::extract::ws::Message::Text(
        serde_json::json!({"type": "busy", "command": "talk-start"})
            .to_string()
            .into(),
    )
}

/// `{"type":"error","message":..}` outbound ctrl.
fn error_ctrl(message: &str) -> axum::extract::ws::Message {
    axum::extract::ws::Message::Text(
        serde_json::json!({"type": "error", "message": message})
            .to_string()
            .into(),
    )
}
// SPDX-SnippetEnd
