// SPDX-License-Identifier: GPL-2.0-or-later

mod decoder;
mod recorder;
mod source;

pub use source::MonitorSource;

use crate::{recorder::new_recorder, source::SourceRtsp};
use async_trait::async_trait;
use common::{
    ArcLogger, DynError, Event, LogEntry, LogLevel, MonitorId, StreamType,
    monitor::{
        ArcMonitorHooks, ArcSource, ArcStreamer, CreateEventDbError, IMonitorManager,
        MonitorConfig, MonitorConfigs, MonitorDeleteError, MonitorImpl, MonitorInfo,
        MonitorRestartError, MonitorSetAndRestartError, MonitorSetError, SourceConfig,
    },
    time::Duration,
    write_file_atomic2,
};
use eventdb::EventDb;
use recdb::RecDb;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::{
    self,
    runtime::Handle,
    sync::{mpsc, oneshot},
};
use tokio_util::{
    sync::CancellationToken,
    task::{TaskTracker, task_tracker::TaskTrackerToken},
};

type Monitors = HashMap<MonitorId, Arc<Monitor>>;
pub struct Monitor {
    token: CancellationToken,
    tracker: TaskTracker,
    id: MonitorId,
    eventdb: EventDb,
    hooks: ArcMonitorHooks,
    config: MonitorConfig,
    source_main_tx: mpsc::Sender<oneshot::Sender<ArcSource>>,
    source_sub_tx: mpsc::Sender<oneshot::Sender<Option<ArcSource>>>,
    send_event_tx: mpsc::Sender<(Duration, Event)>,
}

#[async_trait]
impl MonitorImpl for Monitor {
    fn config(&self) -> &MonitorConfig {
        &self.config
    }

    async fn stop(&self) {
        self.token.cancel();
        self.tracker.wait().await;
    }

    // Return sub stream if it exists otherwise returns main stream.
    // Returns None if cancelled
    async fn get_smallest_source(&self) -> Option<ArcSource> {
        match self.source_sub().await {
            // Sub stream exists.
            Some(Some(sub_stream)) => Some(sub_stream),
            // Sub stream doesn't exist, use main.
            Some(None) => self.source_main().await,
            // Sub stream is cancelled.
            None => None,
        }
    }

    // Returns None if cancelled.
    async fn source_main(&self) -> Option<ArcSource> {
        let (res_tx, res_rx) = oneshot::channel();
        tokio::select! {
            () = self.token.cancelled() => return None,
            _ = self.source_main_tx.send(res_tx) => {}
        }
        tokio::select! {
            () = self.token.cancelled() => None,
            res = res_rx => {
                res.ok()
            }
        }
    }

    // Returns None if cancelled and Some(None) if sub stream doesn't exist.
    async fn source_sub(&self) -> Option<Option<ArcSource>> {
        let (res_tx, res_rx) = oneshot::channel();
        tokio::select! {
            () = self.token.cancelled() => return None,
            _ = self.source_sub_tx.send(res_tx) => {}
        }
        tokio::select! {
            () = self.token.cancelled() => None,
            res = res_rx => {
                res.ok()
            }
        }
    }

    async fn trigger(
        &self,
        trigger_duration: Duration,
        event: Event,
    ) -> Result<(), CreateEventDbError> {
        self.eventdb
            .write_event_deduplicate_time(&self.id, event.clone())
            .await?;

        self.hooks
            .on_event(event.clone(), self.config.clone())
            .await;

        tokio::select! {
            () = self.token.cancelled() => {},
            _ = self.send_event_tx.send((trigger_duration, event)) => {},
        }
        Ok(())
    }
}

pub(crate) fn log_monitor(logger: &ArcLogger, level: LogLevel, id: &MonitorId, msg: &str) {
    logger.log(LogEntry::new(level, "monitor", id, msg));
}

#[derive(Debug, Error)]
pub enum InitializeMonitorManagerError {
    #[error("create directory: {0}")]
    CreateDir(std::io::Error),

    #[error("read directory: {0}")]
    ReadDir(std::io::Error),

    #[error("stat file:")]
    StatFile(std::io::Error),

    #[error("get file metadata: {0}")]
    GetFileMetadata(std::io::Error),

    #[error("read file: {0}")]
    ReadFile(std::io::Error),

    #[error("deserialize config '{0}': {1}")]
    Deserialize(String, serde_json::Error),

    #[error("migrate config for monitor {0}: {1}")]
    Migrate(MonitorId, DynError),
}

#[rustfmt::skip]
enum MonitorManagerRequest {
    Initialize((oneshot::Sender<Result<(), InitializeMonitorManagerError>>, InitializeRequest)),
    MonitorRestart((oneshot::Sender<Result<(), MonitorRestartError>>, MonitorId)),
    MonitorSet((oneshot::Sender<Result<bool, MonitorSetError>>, MonitorConfig)),
    MonitorSetAndRestart((oneshot::Sender<Result<bool, MonitorSetAndRestartError>>, MonitorConfig)),
    MonitorDelete((oneshot::Sender<Result<(), MonitorDeleteError>>, MonitorId)),
    MonitorsInfo(oneshot::Sender<HashMap<MonitorId, MonitorInfo>>),
    MonitorConfig((oneshot::Sender<Option<MonitorConfig>>, MonitorId)),
    MonitorConfigs(oneshot::Sender<MonitorConfigs>),
    MonitorUnmarkAudio((oneshot::Sender<Result<(), MonitorSetError>>, MonitorId, Vec<String>)),
    Cancel(oneshot::Sender<()>),
}

struct InitializeRequest {
    token: CancellationToken,
    task_token: TaskTrackerToken,
    configs_path: PathBuf,
    recdb: Arc<RecDb>,
    eventdb: EventDb,
    logger: ArcLogger,
    streamer: ArcStreamer,
    hooks: ArcMonitorHooks,
}

#[derive(Clone)]
pub struct MonitorManager(mpsc::Sender<MonitorManagerRequest>);

impl Default for MonitorManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorManager {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            run_monitor_manager(rx).await;
        });
        Self(tx)
    }

    // Monitor manager can't be initialized during creation because
    // there's a circular dependency between it and the plugin manager.
    #[allow(clippy::too_many_arguments)]
    pub async fn initialize(
        &self,
        token: CancellationToken,
        task_token: TaskTrackerToken,
        configs_path: PathBuf,
        rec_db: Arc<RecDb>,
        eventdb: EventDb,
        logger: ArcLogger,
        streamer: ArcStreamer,
        hooks: ArcMonitorHooks,
    ) -> Result<(), InitializeMonitorManagerError> {
        let (tx, rx) = oneshot::channel();
        let req = InitializeRequest {
            token: token.clone(),
            task_token,
            configs_path,
            recdb: rec_db,
            eventdb,
            logger,
            streamer,
            hooks,
        };

        let manager_tx = self.0.clone();
        tokio::spawn(async move {
            // Wait for token to be cancelled.
            token.cancelled().await;

            let (tx, rx) = oneshot::channel();
            manager_tx
                .send(MonitorManagerRequest::Cancel(tx))
                .await
                .expect("actor should still be active");

            rx.await.expect("actor should respond");
        });

        self.0
            .send(MonitorManagerRequest::Initialize((tx, req)))
            .await
            .expect("actor should still be active");
        rx.await.expect("actor should respond")
    }
}

#[async_trait]
impl IMonitorManager for MonitorManager {
    async fn monitor_restart(
        &self,
        monitor_id: MonitorId,
    ) -> Option<Result<(), MonitorRestartError>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorRestart((tx, monitor_id));
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitor_set(&self, config: MonitorConfig) -> Option<Result<bool, MonitorSetError>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorSet((tx, config));
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitor_set_and_restart(
        &self,
        config: MonitorConfig,
    ) -> Option<Result<bool, MonitorSetAndRestartError>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorSetAndRestart((tx, config));
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitor_delete(&self, id: MonitorId) -> Option<Result<(), MonitorDeleteError>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorDelete((tx, id));
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitors_info(&self) -> Option<HashMap<MonitorId, MonitorInfo>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorsInfo(tx);
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitor_config(&self, monitor_id: MonitorId) -> Option<Option<MonitorConfig>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorConfig((tx, monitor_id));
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitor_configs(&self) -> Option<MonitorConfigs> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorConfigs(tx);
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }

    async fn monitor_unmark_audio(
        &self,
        id: MonitorId,
        names: Vec<String>,
    ) -> Option<Result<(), MonitorSetError>> {
        let (tx, rx) = oneshot::channel();
        let req = MonitorManagerRequest::MonitorUnmarkAudio((tx, id, names));
        if self.0.send(req).await.is_err() {
            // Cancelled.
            return None;
        }
        Some(rx.await.expect("actor should respond"))
    }
}

struct MonitorManagerState {
    token: CancellationToken,
    task_token: TaskTrackerToken,

    configs: MonitorConfigs,
    started_monitors: Monitors,

    recdb: Arc<RecDb>,
    eventdb: EventDb,
    logger: ArcLogger,
    streamer: ArcStreamer,
    configs_path: PathBuf,

    hooks: ArcMonitorHooks,
}

async fn run_monitor_manager(mut rx: mpsc::Receiver<MonitorManagerRequest>) {
    struct StateOption(Option<MonitorManagerState>);
    impl StateOption {
        fn get(&mut self) -> &mut MonitorManagerState {
            self.0.as_mut().expect("initialized")
        }
    }

    let mut state = StateOption(None);
    while let Some(request) = rx.recv().await {
        match request {
            MonitorManagerRequest::Initialize((res, req)) => {
                assert!(state.0.is_none(), "already initialized");
                let response = match MonitorManagerState::new(req).await {
                    Ok(v) => {
                        state.0 = Some(v);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                res.send(response).expect("caller should receive response");
            }
            MonitorManagerRequest::MonitorRestart((res, monitor_id)) => {
                _ = res.send(state.get().monitor_restart(&monitor_id).await);
            }
            MonitorManagerRequest::MonitorSet((res, config)) => {
                _ = res.send(state.get().monitor_set(config).await);
            }
            MonitorManagerRequest::MonitorSetAndRestart((res, config)) => {
                _ = res.send(state.get().monitor_set_and_restart(config).await);
            }
            MonitorManagerRequest::MonitorDelete((res, monitor_id)) => {
                _ = res.send(state.get().monitor_delete(&monitor_id).await);
            }
            MonitorManagerRequest::MonitorsInfo(res) => {
                _ = res.send(state.get().monitors_info().await);
            }
            MonitorManagerRequest::MonitorConfig((res, monitor_id)) => {
                _ = res.send(state.get().configs.get(&monitor_id).cloned());
            }
            MonitorManagerRequest::MonitorConfigs(res) => _ = res.send(state.get().configs.clone()),
            MonitorManagerRequest::MonitorUnmarkAudio((res, monitor_id, names)) => {
                _ = res.send(state.get().monitor_unmark_audio(&monitor_id, &names).await);
            }
            MonitorManagerRequest::Cancel(res) => {
                state.0.expect("initialized").cancel().await;
                res.send(()).expect("caller should receive response");
                return;
            }
        }
    }
}

impl MonitorManagerState {
    async fn new(req: InitializeRequest) -> Result<Self, InitializeMonitorManagerError> {
        use InitializeMonitorManagerError::*;
        common::create_dir_all(&req.configs_path).map_err(CreateDir)?;

        let mut configs = HashMap::new();
        for entry in std::fs::read_dir(&req.configs_path).map_err(ReadDir)? {
            let entry = entry.map_err(StatFile)?;

            if entry.metadata().map_err(GetFileMetadata)?.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let is_json_file = Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            if !is_json_file {
                continue;
            }

            let json = std::fs::read(entry.path()).map_err(ReadFile)?;
            let mut config: MonitorConfig =
                serde_json::from_slice(&json).map_err(|e| Deserialize(name, e))?;

            req.hooks
                .migrate_monitor(config.raw_mut())
                .map_err(|e| Migrate(config.id().to_owned(), e))?;

            configs.insert(config.id().to_owned(), config);
        }

        let mut state = Self {
            token: req.token,
            task_token: req.task_token,
            configs,
            started_monitors: HashMap::new(),
            recdb: req.recdb,
            eventdb: req.eventdb,
            logger: req.logger,
            streamer: req.streamer,
            configs_path: req.configs_path,
            hooks: req.hooks,
        };
        state.start_monitors().await;
        Ok(state)
    }

    pub async fn start_monitors(&mut self) {
        for (id, config) in &self.configs {
            if let Some(monitor) = self.start_monitor(config.to_owned()).await {
                self.started_monitors.insert(id.to_owned(), monitor);
            }
        }
    }

    // Stops monitor (if running) and starts it again.
    pub async fn monitor_restart(&mut self, id: &MonitorId) -> Result<(), MonitorRestartError> {
        use MonitorRestartError::*;
        let Some(raw_config) = self.configs.get(id) else {
            return Err(NotExist(id.clone()));
        };

        // Stop monitor if running.
        if let Some(monitor) = self.started_monitors.remove(id) {
            self.log(LogLevel::Info, id, "stopping");
            monitor.stop().await;
            self.log(LogLevel::Debug, id, "stopped");
        }

        // Restart monitor.
        if let Some(monitor) = self.start_monitor(raw_config.to_owned()).await {
            self.started_monitors.insert(id.to_owned(), monitor);
        }

        Ok(())
    }

    // Sets config for specified monitor.
    // Changes are not applied until the montior restarts.
    // Returns `true` if monitor was created.
    pub async fn monitor_set(
        &mut self,
        mut config: MonitorConfig,
    ) -> Result<bool, MonitorSetError> {
        if !config.config().ptz.onvif_addr_ok() {
            return Err(MonitorSetError::InvalidPtzAddr);
        }
        let id = config.id().clone();

        // The server owns capability marks — the FE form payload can
        // carry a stale copy from before a mid-session fault, so the
        // stored marks union into the incoming set before the
        // endpoint-change clear evaluates it.
        if let Some(old) = self.configs.get(&id) {
            let mut union: Vec<String> = old.config().ptz.unsupported().to_vec();
            for mark in config.config().ptz.unsupported() {
                if !union.contains(mark) {
                    union.push(mark.clone());
                }
            }
            if union != config.config().ptz.unsupported() {
                if let Some(ptz) = config
                    .raw_mut()
                    .get_mut("ptz")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    ptz.insert("unsupported".to_owned(), serde_json::json!(union));
                }
                if let Ok(fresh) = serde_json::from_value::<MonitorConfig>(config.raw().clone()) {
                    config = fresh;
                }
            }

            union_audio_marks(old, &mut config);
        }

        // Endpoint surface changed → stale capability marks are dropped.
        let endpoint_changed =
            self.configs
                .get(&id)
                .is_some_and(|old| match (old.source(), config.source()) {
                    (SourceConfig::Rtsp(a), SourceConfig::Rtsp(b)) => {
                        a.main_stream != b.main_stream
                    }
                });
        let stale_marks = self.configs.get(&id).is_some_and(|old| {
            if config.config().ptz.unsupported().is_empty() {
                return false;
            }
            if old.config().ptz.onvif_addr() != config.config().ptz.onvif_addr() {
                return true;
            }
            endpoint_changed
        });
        if stale_marks {
            if let Some(ptz) = config
                .raw_mut()
                .get_mut("ptz")
                .and_then(serde_json::Value::as_object_mut)
            {
                ptz.insert("unsupported".to_owned(), serde_json::json!([]));
            }
            // Typed views deserialize once — re-parse so the stored
            // config reflects the patched raw.
            if let Ok(fresh) = serde_json::from_value::<MonitorConfig>(config.raw().clone()) {
                config = fresh;
            }
        }

        // Audio marks clear on the same mainStream/creds change —
        // the session endpoint key is the main stream URL itself.
        if endpoint_changed && !config.config().audio.unsupported().is_empty() {
            if let Some(audio) = config
                .raw_mut()
                .get_mut("audio")
                .and_then(serde_json::Value::as_object_mut)
            {
                audio.insert("unsupported".to_owned(), serde_json::json!([]));
            }
            if let Ok(fresh) = serde_json::from_value::<MonitorConfig>(config.raw().clone()) {
                config = fresh;
            }
        }

        // Write config to file.
        let path = self.config_path(&id);
        let mut temp_path = path.clone();
        temp_path.set_file_name(id.to_string() + ".json.tmp");
        let json = serde_json::to_vec_pretty(&config)?;
        write_file_atomic2(Handle::current(), path, temp_path, json)
            .await
            .map_err(MonitorSetError::WriteFile)?;

        let created = !self.configs.contains_key(&id);
        if created {
            self.log(LogLevel::Info, &id, "created");
        } else {
            self.log(LogLevel::Info, &id, "saved");
        }

        self.configs.insert(id, config);
        Ok(created)
    }

    /// Remove `names` from `audio.unsupported` — the V281 raw-patch
    /// removal op. NOT a `monitor_set` RMW: `union_audio_marks`
    /// unions stored ∪ incoming and would resurrect a reduced set.
    /// Patches `raw` directly, atomic-writes, updates `configs` —
    /// the endpoint-clear posture. A missing monitor ∨ absent
    /// mark set is a no-op.
    pub async fn monitor_unmark_audio(
        &mut self,
        id: &MonitorId,
        names: &[String],
    ) -> Result<(), MonitorSetError> {
        let Some(config) = self.configs.get(id) else {
            return Ok(());
        };
        let remaining: Vec<String> = config
            .config()
            .audio
            .unsupported()
            .iter()
            .filter(|m| !names.contains(m))
            .cloned()
            .collect();
        if remaining.len() == config.config().audio.unsupported().len() {
            return Ok(()); // nothing to remove
        }
        let mut config = config.clone();
        let raw = config.raw_mut();
        match raw
            .get_mut("audio")
            .and_then(serde_json::Value::as_object_mut)
        {
            Some(audio) => {
                audio.insert("unsupported".to_owned(), serde_json::json!(remaining));
            }
            None => return Ok(()), // no audio object ⇒ no marks stored
        }
        // Typed views deserialize once — re-parse so the stored
        // config reflects the patched raw.
        if let Ok(fresh) = serde_json::from_value::<MonitorConfig>(config.raw().clone()) {
            config = fresh;
        }
        let path = self.config_path(id);
        let mut temp_path = path.clone();
        temp_path.set_file_name(id.to_string() + ".json.tmp");
        let json = serde_json::to_vec_pretty(&config)?;
        write_file_atomic2(Handle::current(), path, temp_path, json)
            .await
            .map_err(MonitorSetError::WriteFile)?;
        self.configs.insert(id.clone(), config);
        Ok(())
    }

    pub async fn monitor_set_and_restart(
        &mut self,
        config: MonitorConfig,
    ) -> Result<bool, MonitorSetAndRestartError> {
        use MonitorSetAndRestartError::*;
        let id = config.id().clone();
        let created = self.monitor_set(config).await.map_err(Set)?;
        self.monitor_restart(&id).await.map_err(Restart)?;
        Ok(created)
    }

    // MonitorDelete deletes monitor by id.
    pub async fn monitor_delete(&mut self, id: &MonitorId) -> Result<(), MonitorDeleteError> {
        use MonitorDeleteError::*;

        if let Some(monitor) = self.started_monitors.remove(id) {
            self.log(LogLevel::Info, id, "stopping");
            monitor.stop().await;
            self.log(LogLevel::Debug, id, "stopped");
        }

        if self.configs.remove(id).is_none() {
            return Err(NotExist(id.to_string()));
        }

        tokio::fs::remove_file(self.config_path(id)).await?;
        self.log(LogLevel::Info, id, "deleted");
        Ok(())
    }

    // Returns common information about the monitors.
    // This will be accessesable by normal users.
    pub async fn monitors_info(&self) -> HashMap<MonitorId, MonitorInfo> {
        let mut configs = HashMap::new();
        for raw_conf in self.configs.values() {
            let c = raw_conf;

            let (codec, sub_codec) = match self.started_monitors.get(c.id()) {
                Some(monitor) => {
                    let codec = monitor.source_main().await.and_then(|s| s.codec());
                    let sub_codec = match monitor.source_sub().await {
                        Some(Some(s)) => s.codec(),
                        _ => None,
                    };
                    (codec, sub_codec)
                }
                None => (None, None),
            };

            configs.insert(
                c.id().to_owned(),
                MonitorInfo::new(
                    c.id().to_owned(),
                    c.name().to_owned(),
                    c.enabled(),
                    c.has_sub_stream(),
                    codec,
                    sub_codec,
                ),
            );
        }
        configs
    }

    fn config_path(&self, id: &MonitorId) -> PathBuf {
        fn monitor_config_path(path: &Path, id: String) -> PathBuf {
            path.join(id + ".json")
        }
        monitor_config_path(&self.configs_path, id.to_string())
    }

    async fn start_monitor(&self, config: MonitorConfig) -> Option<Arc<Monitor>> {
        let id = config.id().to_owned();
        if !config.enabled() {
            self.log(LogLevel::Info, &id, "disabled");
            return None;
        }
        self.log(LogLevel::Info, &id, "starting");

        let monitor_token = self.token.child_token();
        let tracker = TaskTracker::new();

        let (source_main, source_sub): (ArcSource, Option<ArcSource>) = match config.source() {
            SourceConfig::Rtsp(conf) => {
                let source_main = SourceRtsp::new(
                    monitor_token.child_token(),
                    tracker.token(),
                    self.logger.clone(),
                    self.streamer.clone(),
                    id.clone(),
                    conf.to_owned(),
                    StreamType::Main,
                )
                .expect("source main should never be None");

                let source_sub = SourceRtsp::new(
                    monitor_token.child_token(),
                    tracker.token(),
                    self.logger.clone(),
                    self.streamer.clone(),
                    id.clone(),
                    conf.to_owned(),
                    StreamType::Sub,
                );

                (
                    Arc::new(source_main),
                    source_sub.map(|v| {
                        let v: ArcSource = Arc::new(v);
                        v
                    }),
                )
            }
        };

        let send_event_tx = new_recorder(
            monitor_token.clone(),
            tracker.token(),
            self.hooks.clone(),
            self.logger.clone(),
            id.clone(),
            source_main.clone(),
            config.clone(),
            self.recdb.clone(),
        );

        let (source_main_tx, mut source_main_rx) = mpsc::channel(1);
        let (source_sub_tx, mut source_sub_rx) = mpsc::channel(1);

        let monitor = Arc::new(Monitor {
            token: monitor_token.clone(),
            tracker: tracker.clone(),
            id: id.clone(),
            eventdb: self.eventdb.clone(),
            hooks: self.hooks.clone(),
            config: config.clone(),
            source_main_tx,
            source_sub_tx,
            send_event_tx,
        });
        tracker.close();

        // Monitor actor.
        let monitor_token2 = monitor_token.clone();
        tokio::spawn(async move {
            let _task_token = tracker.token();
            loop {
                tokio::select! {
                    () = monitor_token2.cancelled() => return,
                    res = source_main_rx.recv() => {
                        let Some(res) = res else {
                            return
                        };
                        _ = res.send(source_main.clone());
                    },
                    res = source_sub_rx.recv() => {
                        let Some(res) = res else {
                            return
                        };
                        _ = res.send(source_sub.clone());
                    },
                };
            }
        });

        self.hooks
            .on_monitor_start(monitor_token, monitor.clone())
            .await;

        Some(monitor)
    }

    pub async fn cancel(mut self) {
        // Cancel token.
        self.token.cancel();

        // Stop monitors.
        for (_, monitor) in self.started_monitors.drain() {
            monitor.stop().await;
        }

        // Break circular reference.
        drop(self.hooks);

        drop(self.task_token);
    }

    fn log(&self, level: LogLevel, id: &MonitorId, msg: &str) {
        log_monitor(&self.logger, level, id, msg);
    }
}

#[allow(clippy::needless_pass_by_value, clippy::unwrap_used)]

/// Server-owned union for `audio.unsupported` — stored ∪ incoming,
/// written back through `raw` so a payload lacking the `audio`
/// object still preserves stored marks (V260).
fn union_audio_marks(old: &MonitorConfig, config: &mut MonitorConfig) {
    let mut union: Vec<String> = old.config().audio.unsupported().to_vec();
    for mark in config.config().audio.unsupported() {
        if !union.contains(mark) {
            union.push(mark.clone());
        }
    }
    if union == config.config().audio.unsupported() {
        return;
    }
    let raw = config.raw_mut();
    if let Some(audio) = raw
        .get_mut("audio")
        .and_then(serde_json::Value::as_object_mut)
    {
        audio.insert("unsupported".to_owned(), serde_json::json!(union));
    } else if let Some(obj) = raw.as_object_mut() {
        // Third-party PUT with no `audio` object at all — stored
        // marks still survive.
        obj.insert(
            "audio".to_owned(),
            serde_json::json!({"unsupported": union}),
        );
    }
    if let Ok(fresh) = serde_json::from_value::<MonitorConfig>(config.raw().clone()) {
        *config = fresh;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use common::{
        ArcStreamerMuxer, DummyLogger, DummyStorage, DynError, H264Data, MonitorName,
        ParseMonitorIdError, TrackParameters,
        monitor::{
            Config, DummyMonitorHooks, DynH264Writer, Protocol, PtzConfig, SelectedSource,
            SourceConfig, SourceRtspConfig, StreamerImpl,
        },
        time::{UnixH264, UnixNano},
    };
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::{fs, path::PathBuf};
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio_util::task::TaskTracker;

    #[test_case("", ParseMonitorIdError::Empty; "empty")]
    #[test_case("@", ParseMonitorIdError::InvalidChars("@".to_owned()); "invalid_chars")]
    fn source_parse(input: &str, want: ParseMonitorIdError) {
        assert_eq!(
            want,
            MonitorId::try_from(input.to_owned()).expect_err("expected error")
        );
    }

    fn prepare_dir() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();

        let test_config_dir = temp_dir.path().join("monitors");
        fs::create_dir_all(&test_config_dir).unwrap();

        std::fs::write(
            test_config_dir.join("1.json"),
            "{
                \"id\": \"1\",
                \"name\": \"one\",
                \"enable\": false,
                \"source\": \"rtsp\",
                \"sourcertsp\": {
                    \"protocol\": \"tcp\",
                    \"mainStream\": \"rtsp://x1\"
                },
                \"alwaysRecord\": false,
                \"videoLength\": 0.0
            }",
        )
        .unwrap();

        std::fs::write(
            test_config_dir.join("2.json"),
            "{
                \"id\": \"2\",
                \"name\": \"two\",
                \"enable\": false,
                \"source\": \"rtsp\",
                \"sourcertsp\": {
                    \"protocol\": \"udp\",
                    \"mainStream\": \"rtsp://x1\",
                    \"subStream\": \"rtsp://x2\"
                },
                \"alwaysRecord\": false,
                \"videoLength\": 0.0
            }",
        )
        .unwrap();

        (temp_dir, test_config_dir)
    }

    async fn new_test_recdb(recordings_dir: &Path) -> RecDb {
        RecDb::new(
            DummyLogger::new(),
            recordings_dir.to_path_buf(),
            DummyStorage::new(),
        )
        .await
    }

    fn new_test_eventdb(events_dir: &Path) -> EventDb {
        EventDb::new(
            CancellationToken::new(),
            TaskTracker::new().token(),
            DummyLogger::new(),
            events_dir.to_path_buf(),
        )
    }

    async fn new_test_manager() -> (
        CancellationToken,
        TaskTracker,
        TempDir,
        PathBuf,
        MonitorManager,
    ) {
        let token = CancellationToken::new();
        let tracker = TaskTracker::new();
        let (temp_dir, config_dir) = prepare_dir();

        let manager = MonitorManager::new();
        manager
            .initialize(
                token.clone(),
                tracker.token(),
                config_dir.clone(),
                Arc::new(new_test_recdb(temp_dir.path()).await),
                new_test_eventdb(temp_dir.path()),
                DummyLogger::new(),
                DummyStreamer::new(),
                DummyMonitorHooks::new(),
            )
            .await
            .unwrap();

        (token, tracker, temp_dir, config_dir, manager)
    }

    async fn stop_manager(token: CancellationToken, tracker: TaskTracker) {
        token.cancel();
        tracker.close();
        tracker.wait().await;
    }

    fn read_config(path: PathBuf) -> MonitorConfig {
        let json = fs::read(path).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    fn m_id(s: &str) -> MonitorId {
        s.to_owned().try_into().unwrap()
    }
    fn name(s: &str) -> MonitorName {
        s.to_owned().try_into().unwrap()
    }

    #[tokio::test]
    async fn test_new_manager_ok() {
        let (token, tracker, _temp_dir, config_dir, manager) = new_test_manager().await;

        let want = manager.monitor_configs().await.unwrap()[&m_id("1")].clone();
        let got = read_config(config_dir.join("1.json"));
        assert_eq!(want, got);

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn test_new_manager_unmarshal_error() {
        let (temp_dir, config_dir) = prepare_dir();

        std::fs::write(config_dir.join("1.json"), "{").unwrap();

        assert!(matches!(
            MonitorManager::new()
                .initialize(
                    CancellationToken::new(),
                    TaskTracker::new().token(),
                    config_dir,
                    Arc::new(new_test_recdb(temp_dir.path()).await),
                    new_test_eventdb(temp_dir.path()),
                    DummyLogger::new(),
                    DummyStreamer::new(),
                    DummyMonitorHooks::new(),
                )
                .await,
            Err(InitializeMonitorManagerError::Deserialize(..))
        ));
    }

    #[tokio::test]
    async fn test_monitor_set_create_new() {
        let (token, tracker, _temp_dir, config_dir, manager) = new_test_manager().await;

        let config = MonitorConfig::new(
            Config {
                id: m_id("new"),
                name: name("new"),
                enable: false,
                source: SelectedSource::Rtsp,
                always_record: false,
                video_length: 0.0,
                ptz: PtzConfig::default(),
                audio: common::monitor::AudioConfig::default(),
            },
            SourceConfig::Rtsp(SourceRtspConfig {
                protocol: Protocol::Tcp,
                main_stream: "rtsp://x1".parse().unwrap(),
                sub_stream: None,
            }),
            json!({
                "id": "new",
                "name": "new",
                "enable": false,
                "unused": "x",
                "source": "rtsp",
                "sourcertsp": {
                    "protocol": "tcp",
                    "mainStream": "rtsp://x1",
                },
                "alwaysRecord": false,
                "videoLength": 0.0,
            }),
        );

        let created = manager.monitor_set(config).await.unwrap().unwrap();
        assert!(created);

        let new = &m_id("new");
        let config = manager.monitor_config(new.clone()).await.unwrap().unwrap();
        let new_name = config.name();
        assert_eq!(&name("new"), new_name);

        // Check if changes were saved to file.
        let saved_config = read_config(config_dir.join("new.json"));
        assert_eq!(manager.monitor_configs().await.unwrap()[new], saved_config);

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn test_monitor_set_update() {
        let (token, tracker, _temp_dir, config_dir, manager) = new_test_manager().await;

        let one = m_id("1");
        let old_monitor = &manager.monitor_configs().await.unwrap()[&one];

        let old_name = old_monitor.name();
        assert_eq!(&name("one"), old_name);

        let config = MonitorConfig::new(
            Config {
                id: m_id("1"),
                name: name("two"),
                enable: false,
                source: SelectedSource::Rtsp,
                always_record: false,
                video_length: 0.0,
                ptz: PtzConfig::default(),
                audio: common::monitor::AudioConfig::default(),
            },
            SourceConfig::Rtsp(SourceRtspConfig {
                protocol: Protocol::Tcp,
                main_stream: "rtsp://x1".parse().unwrap(),
                sub_stream: None,
            }),
            json!({
                "id": "1",
                "name": "two",
                "enable": false,
                "unused": "x",
                "source": "rtsp",
                "sourcertsp": {
                    "protocol": "tcp",
                    "mainStream": "rtsp://x1",
                },
                "alwaysRecord": false,
                "videoLength": 0,
            }),
        );

        let created = manager.monitor_set(config).await.unwrap().unwrap();
        assert!(!created);

        let config = manager.monitor_config(one.clone()).await.unwrap().unwrap();
        let new_name = config.name();
        assert_eq!(&name("two"), new_name);

        // Check if changes were saved to file.
        let saved_config = read_config(config_dir.join("1.json"));
        assert_eq!(manager.monitor_configs().await.unwrap()[&one], saved_config);

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn test_monitor_delete_exist_error() {
        let (_, _, _temp_dir, _, manager) = new_test_manager().await;
        assert!(matches!(
            manager.monitor_delete(m_id("nil")).await.unwrap(),
            Err(MonitorDeleteError::NotExist(_))
        ));
    }

    #[tokio::test]
    async fn test_monitors_info() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;
        let want: HashMap<MonitorId, MonitorInfo> = HashMap::from([
            (
                m_id("1"),
                MonitorInfo {
                    id: m_id("1"),
                    name: name("one"),
                    enable: false,
                    has_sub_stream: false,
                    codec: None,
                    sub_codec: None,
                },
            ),
            (
                m_id("2"),
                MonitorInfo {
                    id: m_id("2"),
                    name: name("two"),
                    enable: false,
                    has_sub_stream: true,
                    codec: None,
                    sub_codec: None,
                },
            ),
        ]);
        let got = manager.monitors_info().await.unwrap();
        assert_eq!(want, got);

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn test_monitor_set_rejects_bad_onvif_addr() {
        let (token, tracker, _temp_dir, _config_dir, manager) = new_test_manager().await;

        let config = MonitorConfig::new(
            Config {
                id: m_id("bad"),
                name: name("bad"),
                enable: false,
                source: SelectedSource::Rtsp,
                always_record: false,
                video_length: 0.0,
                ptz: PtzConfig::new(true, "https://cam".to_owned()),
                audio: common::monitor::AudioConfig::default(),
            },
            SourceConfig::Rtsp(SourceRtspConfig {
                protocol: Protocol::Tcp,
                main_stream: "rtsp://x1".parse().unwrap(),
                sub_stream: None,
            }),
            json!({
                "id": "bad",
                "name": "bad",
                "enable": false,
                "source": "rtsp",
                "sourcertsp": {
                    "protocol": "tcp",
                    "mainStream": "rtsp://x1",
                },
                "alwaysRecord": false,
                "videoLength": 0.0,
                "ptz": {"enable": true, "onvifAddr": "https://cam"},
            }),
        );

        assert!(matches!(
            manager.monitor_set(config).await.unwrap(),
            Err(MonitorSetError::InvalidPtzAddr)
        ));

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn monitor_set_clears_ptz_marks_on_endpoint_change() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;
        let base = || {
            json!({
                "id": "cam", "name": "cam", "enable": false,
                "source": "rtsp",
                "sourcertsp": {"protocol": "tcp", "mainStream": "rtsp://x1"},
                "alwaysRecord": false, "videoLength": 0.0,
                "ptz": {
                    "enable": true, "onvifAddr": "",
                    "unsupported": ["getPresets", "home"],
                },
            })
        };

        // Ordinary save — same endpoint surface → marks preserved.
        manager
            .monitor_set(serde_json::from_value(base()).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert_eq!(&["getPresets", "home"], got.config().ptz.unsupported());

        // mainStream change → different camera surface → marks dropped.
        let mut changed = base();
        changed["sourcertsp"]["mainStream"] = json!("rtsp://x2");
        manager
            .monitor_set(serde_json::from_value(changed).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert!(got.config().ptz.unsupported().is_empty());

        // onvifAddr change also clears.
        let mut marks = base();
        marks["sourcertsp"]["mainStream"] = json!("rtsp://x2");
        manager
            .monitor_set(serde_json::from_value(marks).unwrap())
            .await
            .unwrap().unwrap();
        let mut addr = base();
        addr["sourcertsp"]["mainStream"] = json!("rtsp://x2");
        addr["ptz"]["onvifAddr"] = json!("http://other:8899/onvif/device_service");
        manager
            .monitor_set(serde_json::from_value(addr).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert!(got.config().ptz.unsupported().is_empty());

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn monitor_set_unions_ptz_marks_on_same_endpoint() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;
        let base = || {
            json!({
                "id": "cam", "name": "cam", "enable": false,
                "source": "rtsp",
                "sourcertsp": {"protocol": "tcp", "mainStream": "rtsp://x1"},
                "alwaysRecord": false, "videoLength": 0.0,
                "ptz": {
                    "enable": true, "onvifAddr": "",
                    "unsupported": ["getPresets", "home"],
                },
            })
        };

        manager
            .monitor_set(serde_json::from_value(base()).unwrap())
            .await
            .unwrap().unwrap();

        // Stale form payload: loaded before `home` faulted mid-session.
        // Same endpoint → stored marks merge with the stale incoming set.
        let mut stale = base();
        stale["ptz"]["unsupported"] = json!(["getPresets"]);
        manager
            .monitor_set(serde_json::from_value(stale).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert_eq!(&["getPresets", "home"], got.config().ptz.unsupported());

        // An incoming payload lacking the field entirely unions to stored.
        let mut bare = base();
        bare["ptz"].as_object_mut().unwrap().remove("unsupported");
        manager
            .monitor_set(serde_json::from_value(bare).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert_eq!(&["getPresets", "home"], got.config().ptz.unsupported());

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn monitor_set_union_still_clears_on_endpoint_change() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;
        let base = || {
            json!({
                "id": "cam", "name": "cam", "enable": false,
                "source": "rtsp",
                "sourcertsp": {"protocol": "tcp", "mainStream": "rtsp://x1"},
                "alwaysRecord": false, "videoLength": 0.0,
                "ptz": {
                    "enable": true, "onvifAddr": "",
                    "unsupported": ["home"],
                },
            })
        };

        manager
            .monitor_set(serde_json::from_value(base()).unwrap())
            .await
            .unwrap().unwrap();

        // Empty incoming set + endpoint change → union restores the mark
        // into the config first, then the endpoint-diff clears it.
        let mut changed = base();
        changed["ptz"]["unsupported"] = json!([]);
        changed["sourcertsp"]["mainStream"] = json!("rtsp://x2");
        manager
            .monitor_set(serde_json::from_value(changed).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert!(got.config().ptz.unsupported().is_empty());

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn monitor_set_unions_audio_marks_even_without_audio_key() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;
        let base = || {
            json!({
                "id": "cam", "name": "cam", "enable": false,
                "source": "rtsp",
                "sourcertsp": {"protocol": "tcp", "mainStream": "rtsp://x1"},
                "alwaysRecord": false, "videoLength": 0.0,
                "audio": {"enable": true, "unsupported": ["talk", "listen"]},
            })
        };

        manager
            .monitor_set(serde_json::from_value(base()).unwrap())
            .await
            .unwrap().unwrap();

        // Third-party PUT with no `audio` object at all — stored
        // marks still survive the union.
        let mut bare = base();
        bare.as_object_mut().unwrap().remove("audio");
        manager
            .monitor_set(serde_json::from_value(bare).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert_eq!(&["talk", "listen"], got.config().audio.unsupported());

        // Endpoint change still clears the marks.
        let mut changed = base();
        changed["sourcertsp"]["mainStream"] = json!("rtsp://x2");
        manager
            .monitor_set(serde_json::from_value(changed).unwrap())
            .await
            .unwrap().unwrap();
        let got = manager.monitor_config(m_id("cam")).await.unwrap().unwrap();
        assert!(got.config().audio.unsupported().is_empty());

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn test_monitor_configs() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;

        let got = manager.monitor_configs().await.unwrap();
        let want: HashMap<MonitorId, MonitorConfig> = HashMap::from([
            (
                m_id("1"),
                MonitorConfig::new(
                    Config {
                        id: m_id("1"),
                        name: name("one"),
                        enable: false,
                        source: SelectedSource::Rtsp,
                        always_record: false,
                        video_length: 0.0,
                        ptz: PtzConfig::default(),
                        audio: common::monitor::AudioConfig::default(),
                    },
                    SourceConfig::Rtsp(SourceRtspConfig {
                        protocol: Protocol::Tcp,
                        main_stream: "rtsp://x1".parse().unwrap(),
                        sub_stream: None,
                    }),
                    json!({
                        "id": "1",
                        "name": "one",
                        "enable": false,
                        "source": "rtsp",
                        "sourcertsp": {
                            "protocol": "tcp",
                            "mainStream": "rtsp://x1",
                        },
                        "alwaysRecord": false,
                        "videoLength": 0.0,
                    }),
                ),
            ),
            (
                m_id("2"),
                MonitorConfig::new(
                    Config {
                        id: m_id("2"),
                        name: name("two"),
                        enable: false,
                        source: SelectedSource::Rtsp,
                        always_record: false,
                        video_length: 0.0,
                        ptz: PtzConfig::default(),
                        audio: common::monitor::AudioConfig::default(),
                    },
                    SourceConfig::Rtsp(SourceRtspConfig {
                        protocol: Protocol::Udp,
                        main_stream: "rtsp://x1".parse().unwrap(),
                        sub_stream: Some("rtsp://x2".parse().unwrap()),
                    }),
                    json!({
                        "id": "2",
                        "name": "two",
                        "enable": false,
                        "source": "rtsp",
                        "sourcertsp": {
                            "protocol": "udp",
                            "mainStream": "rtsp://x1",
                            "subStream": "rtsp://x2",
                        },
                        "alwaysRecord": false,
                        "videoLength": 0.0,
                    }),
                ),
            ),
        ]);

        assert_eq!(want, got);

        stop_manager(token, tracker).await;
    }

    #[tokio::test]
    async fn test_restart_monitor_not_exist_error() {
        let (_, _, _temp_dir, _, manager) = new_test_manager().await;
        assert!(matches!(
            manager.monitor_restart(m_id("x")).await.unwrap(),
            Err(MonitorRestartError::NotExist(_))
        ));
    }

    #[tokio::test]
    async fn test_restart_monitor() {
        let (token, tracker, _temp_dir, _, manager) = new_test_manager().await;

        let config = MonitorConfig::new(
            Config {
                id: m_id("r1"),
                name: name("r1"),
                enable: false,
                source: SelectedSource::Rtsp,
                always_record: false,
                video_length: 0.0,
                ptz: PtzConfig::default(),
                audio: common::monitor::AudioConfig::default(),
            },
            SourceConfig::Rtsp(SourceRtspConfig {
                protocol: Protocol::Tcp,
                main_stream: "rtsp://x".parse().unwrap(),
                sub_stream: None,
            }),
            json!({
                "id": "r1",
                "name": "r1",
                "enable": false,
                "source": "rtsp",
                "sourcertsp": { "protocol": "tcp", "mainStream": "rtsp://x" },
                "alwaysRecord": false,
                "videoLength": 0.0,
            }),
        );
        manager.monitor_set(config).await.unwrap().unwrap();

        // Existing monitor restarts cleanly.
        manager.monitor_restart(m_id("r1")).await.unwrap().unwrap();

        stop_manager(token, tracker).await;
    }

    struct OrderCheckHooks {
        eventdb: EventDb,
        monitor_id: MonitorId,
        hook_called: std::sync::atomic::AtomicBool,
        persisted_before_hook: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl common::monitor::MonitorHooks for OrderCheckHooks {
        async fn on_monitor_start(&self, _: CancellationToken, _: common::monitor::ArcMonitor) {}
        fn on_thumb_save(&self, _: &MonitorConfig, frame: sentryshot_util::Frame) -> sentryshot_util::Frame {
            frame
        }
        async fn on_event(&self, event: Event, _: MonitorConfig) {
            use std::sync::atomic::Ordering::SeqCst;
            self.hook_called.store(true, SeqCst);
            // Ordering contract: the event must already be durable in
            // eventdb when hooks fire.
            let persisted = self
                .eventdb
                .query(
                    self.monitor_id.clone(),
                    eventdb::EventQuery {
                        start: UnixNano::new(0),
                        end: UnixNano::new(i64::MAX),
                        limit: std::num::NonZeroUsize::new(10).unwrap(),
                    },
                )
                .await
                .unwrap_or_default()
                .unwrap_or_default()
                .contains(&event);
            self.persisted_before_hook.store(persisted, SeqCst);
        }
        fn migrate_monitor(&self, _: &mut serde_json::Value) -> Result<(), DynError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_trigger_ordering() {
        use std::sync::atomic::Ordering::SeqCst;

        let temp_dir = tempfile::tempdir().unwrap();
        let eventdb = new_test_eventdb(temp_dir.path());
        let hooks: Arc<OrderCheckHooks> = Arc::new(OrderCheckHooks {
            eventdb: eventdb.clone(),
            monitor_id: m_id("t1"),
            hook_called: std::sync::atomic::AtomicBool::new(false),
            persisted_before_hook: std::sync::atomic::AtomicBool::new(false),
        });

        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (main_tx, _main_rx) = mpsc::channel(1);
        let (sub_tx, _sub_rx) = mpsc::channel(1);

        let monitor = Monitor {
            token: CancellationToken::new(),
            tracker: TaskTracker::new(),
            id: m_id("t1"),
            eventdb,
            hooks: hooks.clone(),
            config: MonitorConfig::new(
                Config {
                    id: m_id("t1"),
                    name: name("t1"),
                    enable: false,
                    source: SelectedSource::Rtsp,
                    always_record: false,
                    video_length: 0.0,
                    ptz: PtzConfig::default(),
                    audio: common::monitor::AudioConfig::default(),
                },
                SourceConfig::Rtsp(SourceRtspConfig {
                    protocol: Protocol::Tcp,
                    main_stream: "rtsp://x".parse().unwrap(),
                    sub_stream: None,
                }),
                json!({}),
            ),
            source_main_tx: main_tx,
            source_sub_tx: sub_tx,
            send_event_tx: event_tx,
        };

        let event = Event {
            time: UnixNano::now(),
            duration: Duration::from_secs(1),
            detections: Vec::new(),
            source: None,
        };
        monitor
            .trigger(Duration::from_secs(1), event.clone())
            .await
            .unwrap();

        // Ordering: eventdb write -> hooks -> send_event_tx.
        assert!(hooks.hook_called.load(SeqCst), "on_event must run");
        assert!(
            hooks.persisted_before_hook.load(SeqCst),
            "event must be durable in eventdb before hooks fire"
        );
        assert!(
            event_rx.try_recv().is_ok(),
            "event must reach send_event_tx after hooks"
        );
    }

    struct DummyStreamer;

    impl DummyStreamer {
        #[allow(clippy::new_ret_no_self)]
        fn new() -> ArcStreamer {
            Arc::new(Self {})
        }
    }

    #[async_trait]
    impl StreamerImpl for DummyStreamer {
        async fn new_muxer(
            &self,
            _: CancellationToken,
            _: MonitorId,
            _: bool,
            _: TrackParameters,
            _: UnixH264,
            _: H264Data,
        ) -> Result<Option<(ArcStreamerMuxer, DynH264Writer)>, DynError> {
            Ok(None)
        }
    }
    /// Raw-patch `audio.unsupported` removal — named marks drop
    /// from the live config AND the atomic file write, others
    /// survive; ⊥-matching removals are no-ops.
    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn monitor_unmark_audio_removes_named_marks() {
        let (token, tracker, _temp_dir, config_dir, manager) = new_test_manager().await;
        let base = || {
            json!({
                "id": "cam", "name": "cam", "enable": false,
                "source": "rtsp",
                "sourcertsp": {"protocol": "tcp", "mainStream": "rtsp://x1"},
                "alwaysRecord": false, "videoLength": 0.0,
                "audio": {"enable": true, "unsupported": ["talk", "listen", "ptz"]},
            })
        };
        manager
            .monitor_set(serde_json::from_value(base()).unwrap())
            .await
            .unwrap().unwrap();
        let id = m_id("cam");
        manager
            .monitor_unmark_audio(id.clone(), vec!["talk".to_owned()])
            .await
            .unwrap()
            .unwrap();
        let got = manager.monitor_config(id.clone()).await.unwrap().unwrap();
        assert_eq!(got.config().audio.unsupported(), &["listen", "ptz"]);
        // Atomic write reached disk too.
        let on_disk = read_config(config_dir.join("cam.json"));
        assert_eq!(on_disk.config().audio.unsupported(), &["listen", "ptz"]);
        // ⊥-matching removal = no-op, no fault.
        manager
            .monitor_unmark_audio(id.clone(), vec!["talk".to_owned()])
            .await
            .unwrap()
            .unwrap();
        // Unknown monitor = no-op.
        manager
            .monitor_unmark_audio(m_id("ghost"), vec!["talk".to_owned()])
            .await
            .unwrap()
            .unwrap();
        stop_manager(token, tracker).await;
    }
}
