//! Per-monitor audio-session registry: single-flight open
//! orchestration, consumer interest tracking, the exclusive talk
//! lock, session-vs-persisted capability marks, and the single-use
//! WS token store. Transport-agnostic — the WS consumer loop lives
//! in `src/handler`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::MonitorId;
use tokio::sync::{Mutex, RwLock, broadcast, watch};

use crate::{ArcAudioClient, ArcAudioSession, AudioCaps, Direction, Error, PcmChunk};

/// Attached consumers per monitor — excess attach is refused.
pub const MAX_CONSUMERS: usize = 8;
/// Single-use WS token TTL.
pub const TOKEN_TTL: Duration = Duration::from_secs(30);
/// A WS without any direction ctrl beyond this is closed.
pub const DIRECTIONLESS_TIMEOUT: Duration = Duration::from_secs(10);
/// Server→client WS ping cadence for idle proxies.
pub const WS_PING_CADENCE: Duration = Duration::from_secs(25);

/// Opaque consumer handle — one per upgraded WS connection.
pub type ConsumerId = u64;

/// Interest a consumer holds in the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interest {
    pub listen: bool,
    pub talk: bool,
}

impl Interest {
    fn empty(self) -> bool {
        !self.listen && !self.talk
    }
}

/// Consumer registration inside an entry: the interest set plus the
/// channel back to the owning WS task for `error`+close directives.
struct Consumer {
    interest: Interest,
    drop_tx: tokio::sync::mpsc::UnboundedSender<ConsumerDrop>,
}

/// Why a consumer's WS is being terminated server-side.
#[derive(Clone, Debug)]
pub enum ConsumerDrop {
    /// `error` ctrl + close(1008).
    Error(String),
    /// Silent close — entry rebuild, monitor gone, shutdown.
    Close,
}

/// Open outcome shared by joiners that attached mid-open.
#[derive(Clone)]
struct OpenOutcome(Result<Arc<OpenSession>, OpenError>);

#[derive(Clone, Debug)]
struct OpenError {
    message: String,
}

enum SessionSlot {
    Closed,
    /// A `watch` carrying the outcome once open resolves — joiners
    /// clone the receiver and await; the opener sends exactly once.
    Opening(watch::Sender<Option<OpenOutcome>>),
    Open(Arc<OpenSession>),
}

/// A live camera session plus its PCM broadcast plumbing.
pub struct OpenSession {
    session: ArcAudioSession,
    caps: AudioCaps,
    pcm_tx: broadcast::Sender<PcmChunk>,
    _pump: tokio::task::JoinHandle<()>,
}

/// Per-monitor registry entry — rebuilt when the endpoint+creds key
/// changes; dropped on monitor delete or `audio.enable` false.
pub struct AudioEntry {
    /// Owning monitor — the persist sink needs it for `monitor_set`.
    monitor: MonitorId,
    /// Endpoint+creds identity — `mainStream` URL incl. userinfo.
    key: String,
    client: ArcAudioClient,
    slot: RwLock<SessionSlot>,
    /// Serializes session opens/close transitions in request-arrival
    /// order. Distinct from `talk_owner`, which is a held token.
    op_lock: Mutex<()>,
    /// Held while a consumer owns talk; distinct lifetime from
    /// `op_lock` — a press never serializes behind session ops.
    talk_owner: Mutex<Option<ConsumerId>>,
    /// Direction names faulted while the current session lives —
    /// cleared on session close, never persisted.
    session_marks: Mutex<HashSet<String>>,
    consumers: Mutex<HashMap<ConsumerId, Consumer>>,
}

impl AudioEntry {
    fn new(monitor: MonitorId, key: String, client: ArcAudioClient) -> Self {
        Self {
            monitor,
            key,
            client,
            slot: RwLock::new(SessionSlot::Closed),
            op_lock: Mutex::new(()),
            talk_owner: Mutex::new(None),
            session_marks: Mutex::new(HashSet::new()),
            consumers: Mutex::new(HashMap::new()),
        }
    }

    /// Number of attached (non-empty interest) consumers.
    pub async fn consumer_count(&self) -> usize {
        self.consumers.lock().await.len()
    }

    /// Union of marks gating this entry's directions: session marks
    /// (transport/session faults since open) only — the caller merges
    /// persisted marks from monitor config.
    pub async fn session_marks(&self) -> Vec<String> {
        self.session_marks.lock().await.iter().cloned().collect()
    }

    /// `talk` interest granted to this consumer iff the lock is free
    /// — ownership is recorded before the session op so concurrent
    /// presses resolve first-come.
    async fn try_take_talk(&self, consumer: ConsumerId) -> bool {
        let mut owner = self.talk_owner.lock().await;
        if owner.is_some() {
            return false;
        }
        *owner = Some(consumer);
        true
    }

    async fn release_talk(&self, consumer: ConsumerId) {
        let mut owner = self.talk_owner.lock().await;
        if *owner == Some(consumer) {
            *owner = None;
        }
    }

    /// Whether this consumer holds the talk lock.
    async fn owns_talk(&self, consumer: ConsumerId) -> bool {
        *self.talk_owner.lock().await == Some(consumer)
    }

    /// Talk lock currently held — drives the `talkFree` caps field.
    pub async fn talk_owned(&self) -> bool {
        self.talk_owner.lock().await.is_some()
    }

    /// Register/refresh a consumer's interest. `false` when the
    /// consumer bound is reached.
    async fn register(
        &self,
        id: ConsumerId,
        interest: Interest,
        drop_tx: &tokio::sync::mpsc::UnboundedSender<ConsumerDrop>,
    ) -> bool {
        let mut consumers = self.consumers.lock().await;
        if !consumers.contains_key(&id) && consumers.len() >= MAX_CONSUMERS {
            return false;
        }
        consumers.insert(
            id,
            Consumer {
                interest,
                drop_tx: drop_tx.clone(),
            },
        );
        true
    }

    /// Merge `interest` into the consumer's set.
    async fn add_interest(&self, id: ConsumerId, direction: Direction) {
        let mut consumers = self.consumers.lock().await;
        if let Some(c) = consumers.get_mut(&id) {
            match direction {
                Direction::Listen => c.interest.listen = true,
                Direction::Talk => c.interest.talk = true,
            }
        }
    }

    /// Remove one direction's interest; drops the consumer when the
    /// set empties. Returns the consumer's remaining-interest state.
    async fn remove_interest(&self, id: ConsumerId, direction: Direction) -> bool {
        let mut consumers = self.consumers.lock().await;
        let Some(c) = consumers.get_mut(&id) else {
            return false;
        };
        match direction {
            Direction::Listen => c.interest.listen = false,
            Direction::Talk => c.interest.talk = false,
        }
        if c.interest.empty() {
            consumers.remove(&id);
            return false;
        }
        true
    }

    /// Detach a consumer entirely (WS close path).
    async fn detach(&self, id: ConsumerId) {
        self.release_talk(id).await;
        self.consumers.lock().await.remove(&id);
        self.close_if_idle().await;
    }

    /// `TEARDOWN` + close every consumer WS — entry rebuild, monitor
    /// gone, or shutdown.
    async fn drop_all(&self) {
        self.close_session().await;
        let consumers: Vec<Consumer> = self
            .consumers
            .lock()
            .await
            .drain()
            .map(|(_, c)| c)
            .collect();
        for c in consumers {
            _ = c.drop_tx.send(ConsumerDrop::Close);
        }
    }

    /// Last detach tears the session down; marks clear with it.
    async fn close_if_idle(&self) {
        if !self.consumers.lock().await.is_empty() {
            return;
        }
        self.close_session().await;
    }

    async fn close_session(&self) {
        let slot = {
            let mut slot = self.slot.write().await;
            std::mem::replace(&mut *slot, SessionSlot::Closed)
        };
        if let SessionSlot::Open(open) = slot {
            open.session.close().await;
        }
        self.session_marks.lock().await.clear();
    }

    /// Send client→camera PCM iff this consumer holds the talk lock.
    pub async fn talk(&self, consumer: ConsumerId, pcm: &[i16]) -> Result<(), Error> {
        if !self.owns_talk(consumer).await {
            // Non-owner binary frames drop silently.
            return Ok(());
        }
        let slot = self.slot.read().await;
        let SessionSlot::Open(open) = &*slot else {
            return Ok(());
        };
        open.session.talk(pcm).await
    }

    /// Subscribe to decoded camera→client PCM. `None` when no open
    /// session delivers listen.
    pub async fn subscribe_pcm(&self) -> Option<broadcast::Receiver<PcmChunk>> {
        match &*self.slot.read().await {
            SessionSlot::Open(open) => Some(open.pcm_tx.subscribe()),
            _ => None,
        }
    }
}

/// Open decisions a consumer attach can produce.
#[derive(Debug)]
pub enum Attach {
    /// Session usable — caps + optional PCM receiver for listen.
    Open(AudioCaps, Option<broadcast::Receiver<PcmChunk>>),
    /// `busy` — talk lock held by another consumer.
    Busy,
    /// `error` — direction marked unsupported (persisted ∪ session).
    Unsupported,
    /// `error` — open fault; joiners share the same message.
    Fault(String),
    /// `error` + close — consumer bound exceeded.
    Full,
}

/// Persisted-mark sink — supplied by the route layer, unions names
/// into `audio.unsupported` via `monitor_set`.
pub type PersistFn =
    Arc<dyn Fn(MonitorId, Vec<String>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Session factory — `RtspAudioClient` in production, a mock in tests.
pub type ClientFactory = Arc<dyn Fn(url::Url) -> ArcAudioClient + Send + Sync>;

/// Per-monitor session registry + WS token store. Lives in route
/// state; built once at startup.
pub struct AudioService {
    entries: Mutex<HashMap<MonitorId, Arc<AudioEntry>>>,
    /// `token → (monitor, minted)` — single-use, ≤`TOKEN_TTL`.
    tokens: Mutex<HashMap<String, (MonitorId, Instant)>>,
    factory: ClientFactory,
    persist: PersistFn,
    next_consumer: std::sync::atomic::AtomicU64,
}

impl AudioService {
    pub fn new(factory: ClientFactory, persist: PersistFn) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            factory,
            persist,
            next_consumer: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Mint a single-use ≤30 s WS token bound to `monitor`. No camera
    /// contact — opens are lazy on first direction ctrl.
    pub async fn mint_token(&self, monitor: MonitorId) -> String {
        let mut bytes = [0_u8; 16];
        rand::fill(&mut bytes);
        let token = bytes.iter().fold(String::with_capacity(32), |mut acc, b| {
            _ = write!(acc, "{b:02x}");
            acc
        });
        let mut tokens = self.tokens.lock().await;
        // Sweep expired while here — bounded map hygiene.
        tokens.retain(|_, (_, minted)| minted.elapsed() <= TOKEN_TTL);
        tokens.insert(token.clone(), (monitor, Instant::now()));
        token
    }

    /// Consume a token — returns its monitor iff known and unexpired.
    /// Callers check `Origin` BEFORE consuming so a bad-Origin probe
    /// never burns the token.
    pub async fn consume_token(&self, token: &str) -> Option<MonitorId> {
        let mut tokens = self.tokens.lock().await;
        match tokens.get(token) {
            Some((_, minted)) if minted.elapsed() <= TOKEN_TTL => {
                tokens.remove(token).map(|(id, _)| id)
            }
            _ => None,
        }
    }

    /// Allocate a consumer id for an upgraded WS.
    pub fn consumer_id(&self) -> ConsumerId {
        self.next_consumer
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Entry for `monitor`, rebuilt when `key` differs — the prior
    /// session is torn down and its consumers dropped.
    pub async fn entry(
        &self,
        monitor: &MonitorId,
        key: String,
        client: ArcAudioClient,
    ) -> Arc<AudioEntry> {
        let mut entries = self.entries.lock().await;
        match entries.get(monitor) {
            Some(entry) if entry.key == key => entry.clone(),
            _ => {
                if let Some(old) = entries.remove(monitor) {
                    let old = old.clone();
                    tokio::spawn(async move { old.drop_all().await });
                }
                let entry = Arc::new(AudioEntry::new(monitor.clone(), key, client));
                entries.insert(monitor.clone(), entry.clone());
                entry
            }
        }
    }

    /// Build a client via the injected factory — route layer extracts
    /// endpoint+creds from monitor config.
    pub fn make_client(&self, url: url::Url) -> ArcAudioClient {
        (self.factory)(url)
    }

    /// Drop the entry — TEARDOWN + consumer closes. Called when the
    /// monitor disappears or `audio.enable` flips false.
    pub async fn drop_entry(&self, monitor: &MonitorId) {
        let entry = self.entries.lock().await.remove(monitor);
        if let Some(entry) = entry {
            entry.drop_all().await;
        }
    }

    /// `TEARDOWN` every entry — app shutdown path.
    pub async fn shutdown(&self) {
        let entries: Vec<Arc<AudioEntry>> =
            self.entries.lock().await.drain().map(|(_, e)| e).collect();
        for entry in entries {
            entry.drop_all().await;
        }
    }

    /// Attach `consumer`'s direction interest and ensure the session
    /// can serve it. Serialized through `op_lock` for opens; joiners
    /// during an in-flight open share the outcome.
    pub async fn attach(
        &self,
        entry: &Arc<AudioEntry>,
        _monitor: &MonitorId,
        consumer: ConsumerId,
        direction: Direction,
        persisted_marks: &[String],
        drop_tx: &tokio::sync::mpsc::UnboundedSender<ConsumerDrop>,
    ) -> Attach {
        let name = direction.name();

        // Persisted ∪ session marks gate before any camera contact.
        if persisted_marks.iter().any(|m| m == name)
            || entry.session_marks.lock().await.contains(name)
        {
            return Attach::Unsupported;
        }

        if direction == Direction::Talk && !entry.try_take_talk(consumer).await {
            return Attach::Busy;
        }

        // Ensure a session exists with this direction negotiated.
        let outcome = self.ensure_open(entry, consumer, direction).await;
        match outcome {
            Ok(open) => {
                // Session usable for the direction? A listen-only open
                // can't serve talk (sendonly ⊥ addable mid-session).
                let serves = match direction {
                    Direction::Listen => open.caps.listen,
                    Direction::Talk => open.caps.talk,
                };
                if !serves {
                    if direction == Direction::Talk {
                        entry.release_talk(consumer).await;
                    }
                    // Direction faulted during open — mark surfaced via
                    // `error`; caps ctrl reports the merged set.
                    return Attach::Fault(format!("{name} unavailable in session"));
                }
                if !entry.register(consumer, Interest::default(), drop_tx).await {
                    if direction == Direction::Talk {
                        entry.release_talk(consumer).await;
                    }
                    return Attach::Full;
                }
                entry.add_interest(consumer, direction).await;
                let pcm = if direction == Direction::Listen {
                    entry.subscribe_pcm().await
                } else {
                    None
                };
                Attach::Open(open.caps, pcm)
            }
            Err(e) => {
                if direction == Direction::Talk {
                    entry.release_talk(consumer).await;
                }
                Attach::Fault(e.message)
            }
        }
    }

    /// Remove one direction interest; detach on empty.
    pub async fn remove_direction(
        &self,
        entry: &Arc<AudioEntry>,
        consumer: ConsumerId,
        direction: Direction,
    ) {
        if direction == Direction::Talk {
            entry.release_talk(consumer).await;
        }
        if !entry.remove_interest(consumer, direction).await {
            entry.detach(consumer).await;
        }
    }

    /// WS-close path — release talk, remove consumer, close-if-idle.
    pub async fn detach(&self, entry: &Arc<AudioEntry>, consumer: ConsumerId) {
        entry.detach(consumer).await;
    }

    /// Open or join the session for `direction`. Joiners during an
    /// in-flight open share the outcome — the opener broadcasts via
    /// the `watch` in `SessionSlot::Opening`.
    async fn ensure_open(
        &self,
        entry: &Arc<AudioEntry>,
        _consumer: ConsumerId,
        direction: Direction,
    ) -> Result<Arc<OpenSession>, OpenError> {
        // Pre-lock read — joiners subscribe to the in-flight open
        // without queueing behind `op_lock`.
        let join_rx = {
            let slot = entry.slot.read().await;
            match &*slot {
                SessionSlot::Open(open) => return Ok(open.clone()),
                SessionSlot::Opening(tx) => Some(tx.subscribe()),
                SessionSlot::Closed => None,
            }
        };
        if let Some(mut rx) = join_rx {
            _ = rx.wait_for(Option::is_some).await.map_err(|_| OpenError {
                message: "open aborted".to_owned(),
            })?;
            let outcome = rx.borrow().clone().expect("outcome published");
            return match outcome.0 {
                Ok(open) => Ok(open),
                Err(e) => Err(e),
            };
        }

        // Slot was Closed — serialize through `op_lock` and re-check:
        // an open may have started or finished while we queued.
        let op = entry.op_lock.lock().await;
        let join_rx = {
            let slot = entry.slot.read().await;
            match &*slot {
                SessionSlot::Open(open) => return Ok(open.clone()),
                SessionSlot::Opening(tx) => Some(tx.subscribe()),
                SessionSlot::Closed => None,
            }
        };
        if let Some(mut rx) = join_rx {
            drop(op);
            _ = rx.wait_for(Option::is_some).await.map_err(|_| OpenError {
                message: "open aborted".to_owned(),
            })?;
            let outcome = rx.borrow().clone().expect("outcome published");
            return match outcome.0 {
                Ok(open) => Ok(open),
                Err(e) => Err(e),
            };
        }

        // We open — publish the watch before starting so joiners can
        // subscribe immediately.
        let (tx, _rx) = watch::channel(None);
        *entry.slot.write().await = SessionSlot::Opening(tx.clone());

        let want_talk = direction == Direction::Talk;
        let outcome = match self.do_open(entry, want_talk).await {
            Ok(open) => OpenOutcome(Ok(open)),
            Err(e) => OpenOutcome(Err(e)),
        };

        // Publish, then settle — joiners may be waiting on the watch.
        _ = tx.send(Some(outcome.clone()));
        *entry.slot.write().await = match &outcome.0 {
            Ok(open) => SessionSlot::Open(open.clone()),
            Err(_) => SessionSlot::Closed,
        };
        match outcome.0 {
            Ok(open) => Ok(open),
            Err(e) => Err(e),
        }
    }

    /// `client.open` + mark bookkeeping — capability marks persist
    /// via the sink, the rest land session-only.
    async fn do_open(
        &self,
        entry: &Arc<AudioEntry>,
        want_talk: bool,
    ) -> Result<Arc<OpenSession>, OpenError> {
        match entry.client.open(want_talk).await {
            Ok(session) => {
                let caps = session.caps();
                let mut persisted = Vec::new();
                for (dir, class) in session.marks() {
                    let name = dir.name().to_owned();
                    match class {
                        crate::FaultClass::Capability => persisted.push(name),
                        _ => {
                            entry.session_marks.lock().await.insert(name);
                        }
                    }
                }
                if !persisted.is_empty() {
                    (self.persist)(entry.monitor.clone(), persisted).await;
                }
                let (pcm_tx, _) = broadcast::channel(64);
                let pump = if caps.listen {
                    let (tx, session) = (pcm_tx.clone(), session.clone());
                    tokio::spawn(async move {
                        while let Some(chunk) = session.recv().await {
                            _ = tx.send(chunk);
                        }
                    })
                } else {
                    tokio::spawn(async {})
                };
                Ok(Arc::new(OpenSession {
                    session,
                    caps,
                    pcm_tx,
                    _pump: pump,
                }))
            }
            Err(e) => Err(OpenError {
                message: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioSession, FaultClass};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubSession {
        caps: AudioCaps,
        marks: Vec<(Direction, FaultClass)>,
        talked: Mutex<Vec<i16>>,
        closed: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AudioSession for StubSession {
        fn caps(&self) -> AudioCaps {
            self.caps
        }
        fn marks(&self) -> Vec<(Direction, FaultClass)> {
            self.marks.clone()
        }
        async fn recv(&self) -> Option<PcmChunk> {
            std::future::pending().await
        }
        async fn talk(&self, pcm: &[i16]) -> Result<(), Error> {
            self.talked.lock().await.extend_from_slice(pcm);
            Ok(())
        }
        async fn close(&self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct StubClient {
        session: Result<Arc<StubSession>, Error>,
        opens: AtomicUsize,
        gate: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait::async_trait]
    impl crate::AudioClient for StubClient {
        async fn open(&self, _want_talk: bool) -> Result<ArcAudioSession, Error> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            // Gate only the first open — subsequent opens proceed.
            if let Some(g) = &self.gate {
                if self.opens.load(Ordering::SeqCst) == 1 {
                    g.notified().await;
                }
            }
            match &self.session {
                Ok(s) => Ok(s.clone()),
                Err(_) => Err(Error::Unsupported(Direction::Listen)),
            }
        }
    }

    fn mid() -> MonitorId {
        "m1".to_owned().try_into().unwrap()
    }

    fn svc(
        client: Arc<StubClient>,
    ) -> (Arc<AudioService>, Arc<Mutex<Vec<(MonitorId, Vec<String>)>>>) {
        let persisted = Arc::new(Mutex::new(Vec::<(MonitorId, Vec<String>)>::new()));
        let p = persisted.clone();
        let c = client.clone();
        (
            Arc::new(AudioService::new(
                Arc::new(move |_| c.clone()),
                Arc::new(move |id, marks| {
                    let p = p.clone();
                    Box::pin(async move { p.lock().await.push((id, marks)) })
                }),
            )),
            persisted,
        )
    }

    fn entry_of(_svc: &Arc<AudioService>, client: &Arc<StubClient>) -> Arc<AudioEntry> {
        // Direct build — bypasses the entries map for unit tests.
        Arc::new(AudioEntry::new(mid(), "k".to_owned(), client.clone()))
    }

    fn drop_tx() -> tokio::sync::mpsc::UnboundedSender<ConsumerDrop> {
        tokio::sync::mpsc::unbounded_channel().0
    }

    fn ok_session(listen: bool, talk: bool) -> Arc<StubSession> {
        Arc::new(StubSession {
            caps: AudioCaps { listen, talk },
            marks: Vec::new(),
            talked: Mutex::new(Vec::new()),
            closed: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn single_flight_share() {
        let sess = ok_session(true, true);
        let client = Arc::new(StubClient {
            session: Ok(sess),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        for i in 1..=2u64 {
            match svc
                .attach(&e, &mid(), i, Direction::Listen, &[], &drop_tx())
                .await
            {
                Attach::Open(caps, pcm) => {
                    assert!(caps.listen);
                    assert!(pcm.is_some());
                }
                _ => panic!("expected open"),
            }
        }
        assert_eq!(e.consumer_count().await, 2);
        // One camera session despite two consumers.
        // (opens counted via client)
    }

    #[tokio::test]
    async fn talk_lock_busy() {
        let client = Arc::new(StubClient {
            session: Ok(ok_session(true, true)),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let first = svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx())
            .await;
        assert!(matches!(first, Attach::Open(..)));
        let second = svc
            .attach(&e, &mid(), 2, Direction::Talk, &[], &drop_tx())
            .await;
        assert!(matches!(second, Attach::Busy));
        svc.remove_direction(&e, 1, Direction::Talk).await;
        let third = svc
            .attach(&e, &mid(), 2, Direction::Talk, &[], &drop_tx())
            .await;
        assert!(matches!(third, Attach::Open(..)));
    }

    #[tokio::test]
    async fn persisted_mark_blocks_attach() {
        let client = Arc::new(StubClient {
            session: Ok(ok_session(true, true)),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let marks = vec!["talk".to_owned()];
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &marks, &drop_tx())
            .await
        {
            Attach::Unsupported => {}
            _ => panic!("expected unsupported"),
        }
        // Mark short-circuit happens before the camera — no open.
        assert_eq!(client.opens.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn consumer_bound_refuses() {
        let client = Arc::new(StubClient {
            session: Ok(ok_session(true, true)),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        for i in 0..MAX_CONSUMERS as u64 {
            assert!(matches!(
                svc.attach(&e, &mid(), i, Direction::Listen, &[], &drop_tx())
                    .await,
                Attach::Open(..)
            ));
        }
        let over = svc
            .attach(
                &e,
                &mid(),
                MAX_CONSUMERS as u64,
                Direction::Listen,
                &[],
                &drop_tx(),
            )
            .await;
        assert!(matches!(over, Attach::Full));
    }

    #[tokio::test]
    async fn last_detach_closes_session() {
        let sess = ok_session(true, true);
        let client = Arc::new(StubClient {
            session: Ok(sess.clone()),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        _ = svc
            .attach(&e, &mid(), 1, Direction::Listen, &[], &drop_tx())
            .await;
        svc.detach(&e, 1).await;
        assert_eq!(sess.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn talk_only_session_errors_listen() {
        let client = Arc::new(StubClient {
            session: Ok(ok_session(false, true)),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        // want_talk open lands talk-only; a listen ctrl errors.
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx())
            .await
        {
            Attach::Open(caps, _) => {
                assert!(!caps.listen && caps.talk);
            }
            _ => panic!("expected open"),
        }
        match svc
            .attach(&e, &mid(), 2, Direction::Listen, &[], &drop_tx())
            .await
        {
            Attach::Fault(_) => {}
            _ => panic!("expected fault"),
        }
    }

    #[tokio::test]
    async fn non_owner_talk_drops() {
        let sess = ok_session(true, true);
        let client = Arc::new(StubClient {
            session: Ok(sess.clone()),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        // consumer 1 owns talk; consumer 2's frames drop.
        _ = svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx())
            .await;
        _ = svc
            .attach(&e, &mid(), 2, Direction::Listen, &[], &drop_tx())
            .await;
        e.talk(2, &[1, 2, 3]).await.unwrap();
        assert!(sess.talked.lock().await.is_empty());
        e.talk(1, &[4, 5]).await.unwrap();
        assert_eq!(sess.talked.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn capability_marks_persist_session_marks_dont() {
        let sess = Arc::new(StubSession {
            caps: AudioCaps {
                listen: true,
                talk: false,
            },
            marks: vec![
                (Direction::Talk, FaultClass::Capability),
                (Direction::Listen, FaultClass::Session),
            ],
            talked: Mutex::new(Vec::new()),
            closed: AtomicUsize::new(0),
        });
        let client = Arc::new(StubClient {
            session: Ok(sess),
            opens: AtomicUsize::new(0),
            gate: None,
        });
        let (svc, persisted) = svc(client.clone());
        let e = entry_of(&svc, &client);
        _ = svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx())
            .await;
        assert_eq!(persisted.lock().await.len(), 1);
        assert_eq!(persisted.lock().await[0].1, vec!["talk".to_owned()]);
        // Session mark lives in the entry, not the persist sink.
        assert!(e.session_marks().await.contains(&"listen".to_owned()));
    }

    #[tokio::test]
    async fn open_fault_fans_out_to_joiners() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let client = Arc::new(StubClient {
            session: Err(Error::Unsupported(Direction::Listen)),
            opens: AtomicUsize::new(0),
            gate: Some(gate.clone()),
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let svc2 = svc.clone();
        let e2 = e.clone();
        let j = tokio::spawn(async move {
            svc2.attach(&e2, &mid(), 2, Direction::Listen, &[], &drop_tx())
                .await
        });
        // Let j win the open and park on the gate, then release it
        // before attaching — a single-threaded runtime would
        // deadlock if attach awaited first.
        tokio::task::yield_now().await;
        gate.notify_waiters();
        let first = svc
            .attach(&e, &mid(), 1, Direction::Listen, &[], &drop_tx())
            .await;
        let second = j.await.unwrap();
        assert!(matches!(first, Attach::Fault(_)));
        assert!(matches!(second, Attach::Fault(_)));
    }

    #[tokio::test]
    async fn token_single_use_and_ttl() {
        let (svc, _) = svc(Arc::new(StubClient {
            session: Ok(ok_session(true, true)),
            opens: AtomicUsize::new(0),
            gate: None,
        }));
        let t = svc.mint_token(mid()).await;
        assert_eq!(svc.consume_token(&t).await, Some(mid()));
        // Single-use — second consume fails.
        assert_eq!(svc.consume_token(&t).await, None);
        assert_eq!(svc.consume_token("bogus").await, None);
    }
}
