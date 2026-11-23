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

use crate::{ArcAudioClient, ArcAudioSession, AudioCaps, Direction, Error, PcmChunk, TalkClaim};

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

/// Why a consumer's WS is being terminated server-side. Codes:
/// `1008` policy (excess attach, directionless); `1000` operational
/// (fault, monitor gone/disable, key change, shutdown).
#[derive(Clone, Debug)]
pub enum ConsumerDrop {
    /// `error` ctrl + close with the given WS code.
    Error(String, u16),
    /// Silent close with the given WS code.
    Close(u16),
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
    _death_watch: tokio::task::JoinHandle<()>,
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
    /// Last-seen persisted mark set — merged into `caps` broadcasts
    /// (the live source of truth stays monitor config).
    persisted: Mutex<HashSet<String>>,
    /// Server→consumer ctrl broadcast (`caps` on lock release /
    /// mark change / session death).
    ctrl_tx: broadcast::Sender<String>,
    /// Installed-claim generation owned by `talk_owner` — scopes
    /// claim-death releases (V275).
    claim_gen: Mutex<Option<u64>>,
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
            persisted: Mutex::new(HashSet::new()),
            ctrl_tx: broadcast::channel(16).0,
            claim_gen: Mutex::new(None),
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
            *self.claim_gen.lock().await = None;
            drop(owner);
            // V275 — active `:8800` release + queue flush ∀ paths.
            if let SessionSlot::Open(open) = &*self.slot.read().await {
                open.session.release_talk().await;
            }
        }
    }

    /// Whether this consumer still holds any direction interest.
    pub async fn has_interest(&self, consumer: ConsumerId) -> bool {
        self.consumers.lock().await.contains_key(&consumer)
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

    /// Mid-session transport fault — `error` ctrl + close(1000) to
    /// every consumer + session teardown.
    async fn session_died(&self) {
        self.close_session().await;
        let consumers: Vec<Consumer> = self
            .consumers
            .lock()
            .await
            .drain()
            .map(|(_, c)| c)
            .collect();
        for c in consumers {
            _ = c
                .drop_tx
                .send(ConsumerDrop::Error("session lost".to_owned(), 1000));
        }
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
            _ = c.drop_tx.send(ConsumerDrop::Close(1000));
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
    /// Claim-scoped talker death (V275) — drop the dead claim's
    /// lock + interest (⊥ mark, ⊥ session-fatal); the RTSP session
    /// survives for listen. Generation-scoped: a superseded claim's
    /// late fault ⊥ revokes a new owner.
    async fn talker_died(&self, claim_gen: u64) {
        if *self.claim_gen.lock().await != Some(claim_gen) {
            return;
        }
        let Some(owner) = *self.talk_owner.lock().await else {
            return;
        };
        self.release_talk(owner).await;
        if self.has_interest(owner).await && !self.remove_interest(owner, Direction::Talk).await {
            self.detach(owner).await;
        }
        _ = self.ctrl_tx.send(
            serde_json::json!({
                "type": "error",
                "message": "talk claim lost",
            })
            .to_string(),
        );
        self.broadcast_caps().await;
    }

    /// `caps` broadcast to every attached consumer — current session
    /// caps + persisted ∪ session marks + live lock state (V258).
    async fn broadcast_caps(&self) {
        let caps = match &*self.slot.read().await {
            SessionSlot::Open(open) => open.caps,
            _ => AudioCaps {
                listen: false,
                talk: false,
            },
        };
        let mut marks: Vec<String> = self.persisted.lock().await.iter().cloned().collect();
        marks.extend(self.session_marks.lock().await.iter().cloned());
        marks.sort();
        marks.dedup();
        _ = self.ctrl_tx.send(
            serde_json::json!({
                "type": "caps",
                "listen": caps.listen,
                "talk": caps.talk,
                "talkFree": caps.talk && !self.talk_owned().await,
                "unsupported": marks,
            })
            .to_string(),
        );
    }
}

/// Open decisions a consumer attach can produce.
#[derive(Debug)]
pub enum Attach {
    /// Session usable — caps, optional PCM receiver for listen, and
    /// the entry's server→consumer ctrl broadcast (`caps`/`error`).
    Open(
        AudioCaps,
        Option<broadcast::Receiver<PcmChunk>>,
        broadcast::Receiver<String>,
    ),
    /// `busy` — talk lock held by another consumer.
    Busy,
    /// `error` — a claim-time fault (V275): transient, retryable —
    /// ⊥ mark (⊥ `Unsupported`), ⊥ WS close (⊥ `Fault`).
    Retryable(String),
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
pub type ClientFactory = Arc<dyn Fn(url::Url, Option<url::Url>) -> ArcAudioClient + Send + Sync>;

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
    /// endpoint+creds from monitor config. `alt` = the `subStream`
    /// URL the session retries on a `429`.
    pub fn make_client(&self, url: url::Url, alt: Option<url::Url>) -> ArcAudioClient {
        (self.factory)(url, alt)
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
        *entry.persisted.lock().await = persisted_marks.iter().cloned().collect();

        if direction == Direction::Talk && !entry.try_take_talk(consumer).await {
            return Attach::Busy;
        }

        // Ensure a session exists. `want_talk` = talk unmarked —
        // derived from the persisted set, ⊥ the attaching direction
        // (sendonly ⊥ addable mid-session).
        let want_talk = !persisted_marks.iter().any(|m| m == "talk");
        let outcome = self.ensure_open(entry, consumer, want_talk).await;
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
                // V275 — lazily-claimed talker: the exclusive lock
                // is held; run the `:8800` handshake pre-register.
                if direction == Direction::Talk && open.session.claims_lazily() {
                    if let Err(attach) = self.claim_for(entry, consumer, &open).await {
                        return attach;
                    }
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
                Attach::Open(open.caps, pcm, entry.ctrl_tx.subscribe())
            }
            Err(e) => {
                if direction == Direction::Talk {
                    entry.release_talk(consumer).await;
                }
                Attach::Fault(e.message)
            }
        }
    }

    /// Remove one direction interest; detach on empty. A talk
    /// release broadcasts `caps` so `busy`-marked consumers unmark.
    pub async fn remove_direction(
        &self,
        entry: &Arc<AudioEntry>,
        consumer: ConsumerId,
        direction: Direction,
    ) {
        if direction == Direction::Talk {
            entry.release_talk(consumer).await;
            self.broadcast_caps(entry).await;
        }
        if !entry.remove_interest(consumer, direction).await {
            entry.detach(consumer).await;
        }
    }

    /// WS-close path — release talk, remove consumer, close-if-idle.
    pub async fn detach(&self, entry: &Arc<AudioEntry>, consumer: ConsumerId) {
        let was_owner = entry.owns_talk(consumer).await;
        entry.detach(consumer).await;
        if was_owner {
            self.broadcast_caps(entry).await;
        }
    }

    /// Revoke a held talk lock + the owner's talk interest —
    /// implied `talk-end` on a mid-session talk mark.
    async fn revoke_talk(entry: &Arc<AudioEntry>) {
        let owner = entry.talk_owner.lock().await.take();
        *entry.claim_gen.lock().await = None;
        if owner.is_some() {
            if let SessionSlot::Open(open) = &*entry.slot.read().await {
                open.session.release_talk().await;
            }
        }
        if let Some(owner) = owner {
            // A tentative owner (attach still pre-register) has no
            // interest to strip — detaching it would close the
            // freshly opened session via `close_if_idle`.
            if entry.has_interest(owner).await
                && !entry.remove_interest(owner, Direction::Talk).await
            {
                entry.detach(owner).await;
            }
        }
    }

    /// `caps` broadcast to every attached consumer — current session
    /// caps + persisted ∪ session marks + live lock state.
    async fn broadcast_caps(&self, entry: &Arc<AudioEntry>) {
        entry.broadcast_caps().await;
    }

    /// Lazy `:8800` claim under the held talk lock (V275) — runs
    /// the private-protocol handshake pre-register. A claim
    /// resolving against a closed/replaced session drops its
    /// talker (⊥ installs); every failure path releases the lock.
    async fn claim_for(
        &self,
        entry: &Arc<AudioEntry>,
        consumer: ConsumerId,
        open: &Arc<OpenSession>,
    ) -> Result<(), Attach> {
        let outcome = open.session.claim_talk().await;
        let valid = {
            let slot = entry.slot.read().await;
            entry.owns_talk(consumer).await
                && matches!(&*slot, SessionSlot::Open(o) if Arc::ptr_eq(o, open))
                && !open.session.dead()
        };
        if !valid {
            open.session.release_talk().await;
            entry.release_talk(consumer).await;
            return Err(Attach::Retryable("talk unavailable in session".to_owned()));
        }
        match outcome {
            TalkClaim::Ready => {
                *entry.claim_gen.lock().await = Some(open.session.talk_gen());
                Ok(())
            }
            TalkClaim::Busy => {
                entry.release_talk(consumer).await;
                Err(Attach::Retryable(
                    "talk busy — another talker active".to_owned(),
                ))
            }
            TalkClaim::Unreachable => {
                entry.release_talk(consumer).await;
                Err(Attach::Retryable("talk unavailable in session".to_owned()))
            }
            TalkClaim::Refused => {
                // Claim-time refusal is session-scoped (V275) —
                // capability was already proven at open.
                entry.session_marks.lock().await.insert("talk".to_owned());
                entry.release_talk(consumer).await;
                self.broadcast_caps(entry).await;
                Err(Attach::Retryable("talk unavailable in session".to_owned()))
            }
        }
    }

    /// Open or join the session for `direction`. Joiners during an
    /// in-flight open share the outcome — the opener broadcasts via
    /// the `watch` in `SessionSlot::Opening`.
    async fn ensure_open(
        &self,
        entry: &Arc<AudioEntry>,
        _consumer: ConsumerId,
        want_talk: bool,
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
                    entry
                        .persisted
                        .lock()
                        .await
                        .extend(persisted.iter().cloned());
                    (self.persist)(entry.monitor.clone(), persisted).await;
                }
                // Mid-session talk mark ⇒ revoke the owner (implied
                // `talk-end`) + `caps` broadcast.
                if session.marks().iter().any(|(d, _)| *d == Direction::Talk) {
                    Self::revoke_talk(entry).await;
                }
                self.broadcast_caps(entry).await;
                // Session-death + claim-death watcher — receiver
                // clones hold ⊥ the session Arc (B122): sender
                // drop ends the wait on close.
                let weak = Arc::downgrade(entry);
                let mut dead_rx = session.dead_rx();
                let mut talk_dead_rx = session.talk_dead_rx();
                let dead_watch = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            r = async {
                                dead_rx.wait_for(|d| *d).await.map(|_| ())
                            } => {
                                if r.is_ok() {
                                    if let Some(e) = weak.upgrade() {
                                        e.session_died().await;
                                    }
                                }
                                return;
                            }
                            r = async {
                                match &mut talk_dead_rx {
                                    Some(rx) => rx
                                        .wait_for(|g| *g != 0)
                                        .await
                                        .map(|g| *g),
                                    None => std::future::pending().await,
                                }
                            } => {
                                match r {
                                    Ok(claim_gen) => {
                                        if let Some(e) = weak.upgrade() {
                                            e.talker_died(claim_gen).await;
                                        }
                                    }
                                    Err(_) => talk_dead_rx = None,
                                }
                            }
                        }
                    }
                });
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
                let open = Arc::new(OpenSession {
                    session,
                    caps,
                    pcm_tx,
                    _pump: pump,
                    _death_watch: dead_watch,
                });
                // V275 — a talk owner surviving a session replace
                // re-claims on the fresh session (defensive; live
                // drains clear the lock first).
                if open.session.claims_lazily() {
                    let owner = *entry.talk_owner.lock().await;
                    if let Some(owner) = owner {
                        if entry.has_interest(owner).await && open.session.talk_gen() == 0 {
                            _ = self.claim_for(entry, owner, &open).await;
                        }
                    }
                }
                Ok(open)
            }
            Err(e) => Err(OpenError {
                message: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::as_conversions,
    clippy::needless_pass_by_value,
    clippy::type_complexity,
    clippy::unseparated_literal_suffix,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::{AudioSession, FaultClass};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Lazy-claim control surface for `StubSession` (V275 tests).
    struct StubLazy {
        outcome: Mutex<TalkClaim>,
        claims: AtomicUsize,
        releases: AtomicUsize,
        dead_tx: watch::Sender<u64>,
        gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    }

    struct StubSession {
        caps: AudioCaps,
        marks: Vec<(Direction, FaultClass)>,
        talked: Mutex<Vec<i16>>,
        closed: AtomicUsize,
        dead: std::sync::Mutex<Option<watch::Receiver<bool>>>,
        /// Sender kept alive so the default `dead_rx` never closes —
        /// mimics a living session (a dropped sender ends the watch).
        dead_keep: watch::Sender<bool>,
        lazy: Option<StubLazy>,
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
        async fn wait_dead(&self) {
            let mut rx = self.dead.lock().unwrap().clone();
            match rx.as_mut() {
                Some(rx) => {
                    _ = rx.wait_for(|d| *d).await;
                }
                None => std::future::pending::<()>().await,
            }
        }
        fn dead_rx(&self) -> watch::Receiver<bool> {
            self.dead
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| self.dead_keep.subscribe())
        }
        fn claims_lazily(&self) -> bool {
            self.lazy.is_some()
        }
        async fn claim_talk(&self) -> TalkClaim {
            let Some(l) = &self.lazy else {
                return TalkClaim::Ready;
            };
            l.claims.fetch_add(1, Ordering::SeqCst);
            let gate = l.gate.lock().await.clone();
            if let Some(g) = gate {
                g.notified().await;
            }
            *l.outcome.lock().await
        }
        async fn release_talk(&self) {
            if let Some(l) = &self.lazy {
                l.releases.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn talk_gen(&self) -> u64 {
            u64::try_from(
                self.lazy
                    .as_ref()
                    .map(|l| l.claims.load(Ordering::SeqCst))
                    .unwrap_or(0),
            )
            .unwrap_or(0)
        }
        fn talk_dead_rx(&self) -> Option<watch::Receiver<u64>> {
            self.lazy.as_ref().map(|l| l.dead_tx.subscribe())
        }
    }

    struct StubClient {
        session: Result<Arc<StubSession>, Error>,
        opens: AtomicUsize,
        gate: Option<Arc<tokio::sync::Notify>>,
        want_talk: Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl crate::AudioClient for StubClient {
        async fn open(&self, want_talk: bool) -> Result<ArcAudioSession, Error> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.want_talk.lock().await.push(want_talk);
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
                Arc::new(move |_, _| c.clone()),
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
            dead: std::sync::Mutex::new(None),
            dead_keep: watch::channel(false).0,
            lazy: None,
        })
    }

    fn lazy_session(outcome: TalkClaim) -> Arc<StubSession> {
        Arc::new(StubSession {
            caps: AudioCaps {
                listen: true,
                talk: true,
            },
            marks: Vec::new(),
            talked: Mutex::new(Vec::new()),
            closed: AtomicUsize::new(0),
            dead: std::sync::Mutex::new(None),
            dead_keep: watch::channel(false).0,
            lazy: Some(StubLazy {
                outcome: Mutex::new(outcome),
                claims: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                dead_tx: watch::channel(0).0,
                gate: Mutex::new(None),
            }),
        })
    }

    fn stub_client(sess: Arc<StubSession>) -> Arc<StubClient> {
        Arc::new(StubClient {
            session: Ok(sess),
            opens: AtomicUsize::new(0),
            gate: None,
            want_talk: Mutex::new(Vec::new()),
        })
    }

    #[tokio::test]
    async fn lazy_claim_busy_is_retryable_listen_survives() {
        let sess = lazy_session(TalkClaim::Busy);
        let client = stub_client(sess.clone());
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        match svc
            .attach(&e, &mid(), 1, Direction::Listen, &[], &drop_tx)
            .await
        {
            Attach::Open(..) => {}
            other => panic!("expected open, got {other:?}"),
        }
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Retryable(msg) => assert!(msg.contains("busy"), "{msg}"),
            other => panic!("expected Retryable, got {other:?}"),
        }
        // Lock released + session alive — retry ⊥ wedge, listen ⊥ harmed.
        assert!(!e.talk_owned().await);
        assert_eq!(e.consumer_count().await, 1);
        assert_eq!(sess.closed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lazy_claim_refused_marks_session_only() {
        let sess = lazy_session(TalkClaim::Refused);
        let client = stub_client(sess.clone());
        let (svc, persisted) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        match svc
            .attach(&e, &mid(), 1, Direction::Listen, &[], &drop_tx)
            .await
        {
            Attach::Open(..) => {}
            other => panic!("expected open, got {other:?}"),
        }
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Retryable(_) => {}
            other => panic!("expected Retryable, got {other:?}"),
        }
        // Claim-time refusal marks the SESSION — capability was
        // proven at open; nothing persists (V275).
        assert!(persisted.lock().await.is_empty());
        assert!(e.session_marks.lock().await.contains("talk"));
        // The mark gates later attaches on this session.
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Unsupported => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lazy_claim_unreachable_retryable_no_mark() {
        let sess = lazy_session(TalkClaim::Unreachable);
        let client = stub_client(sess);
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Retryable(_) => {}
            other => panic!("expected Retryable, got {other:?}"),
        }
        assert!(!e.talk_owned().await);
        assert!(!e.session_marks.lock().await.contains("talk"));
    }

    #[tokio::test]
    async fn lazy_claim_install_release_reclaim() {
        let sess = lazy_session(TalkClaim::Ready);
        let client = stub_client(sess.clone());
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Open(..) => {}
            other => panic!("expected open, got {other:?}"),
        }
        assert_eq!(sess.lazy.as_ref().unwrap().claims.load(Ordering::SeqCst), 1);
        // Mic-up drops the claimed talker + frees the lock.
        svc.remove_direction(&e, 1, Direction::Talk).await;
        assert_eq!(
            sess.lazy.as_ref().unwrap().releases.load(Ordering::SeqCst),
            1
        );
        assert!(!e.talk_owned().await);
        // Second hold claims afresh.
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Open(..) => {}
            other => panic!("expected open, got {other:?}"),
        }
        assert_eq!(sess.lazy.as_ref().unwrap().claims.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lazy_claim_death_releases_owner() {
        let sess = lazy_session(TalkClaim::Ready);
        let dead_tx = sess.lazy.as_ref().unwrap().dead_tx.clone();
        let client = stub_client(sess);
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctrl = match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx)
            .await
        {
            Attach::Open(_, _, rx) => rx,
            other => panic!("expected open, got {other:?}"),
        };
        // A stale-generation death ⊥ releases (V275 scoping).
        dead_tx.send(9).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(e.talk_owned().await);
        // The held claim's death ⇒ lock release + implied talk-end.
        dead_tx.send(1).unwrap();
        let mut saw_error = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Ok(msg)) = tokio::time::timeout_at(deadline, ctrl.recv()).await {
            if msg.contains("talk claim lost") {
                saw_error = true;
                break;
            }
        }
        assert!(saw_error);
        assert!(!e.talk_owned().await);
    }

    #[tokio::test]
    async fn stale_claim_drops_talker_no_zombie() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let sess = lazy_session(TalkClaim::Ready);
        *sess.lazy.as_ref().unwrap().gate.lock().await = Some(gate.clone());
        let (dead_tx, dead_rx) = watch::channel(false);
        *sess.dead.lock().unwrap() = Some(dead_rx);
        let client = stub_client(sess.clone());
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Consumer 2 holds the session via listen.
        match svc
            .attach(&e, &mid(), 2, Direction::Listen, &[], &drop_tx)
            .await
        {
            Attach::Open(..) => {}
            other => panic!("expected open, got {other:?}"),
        }
        // Consumer 1's talk attach claims — the stub blocks on the gate.
        let (svc2, e2, dt2) = (svc.clone(), e.clone(), drop_tx.clone());
        let attach_task = tokio::spawn(async move {
            svc2.attach(&e2, &mid(), 1, Direction::Talk, &[], &dt2)
                .await
        });
        // Wait for the in-flight claim, then kill the session.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while sess.lazy.as_ref().unwrap().claims.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "claim never started"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        dead_tx.send(true).unwrap();
        gate.notify_one();
        match attach_task.await.unwrap() {
            Attach::Retryable(_) => {}
            other => panic!("expected Retryable, got {other:?}"),
        }
        // ⊥ zombie — lock released, claim's talker dropped.
        assert!(!e.talk_owned().await);
        assert_eq!(
            sess.lazy.as_ref().unwrap().releases.load(Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn single_flight_share() {
        let sess = ok_session(true, true);
        let client = Arc::new(StubClient {
            session: Ok(sess),
            opens: AtomicUsize::new(0),
            gate: None,
            want_talk: Mutex::new(Vec::new()),
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        for i in 1..=2u64 {
            match svc
                .attach(&e, &mid(), i, Direction::Listen, &[], &drop_tx())
                .await
            {
                Attach::Open(caps, pcm, _ctrl) => {
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
            want_talk: Mutex::new(Vec::new()),
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
            want_talk: Mutex::new(Vec::new()),
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
            want_talk: Mutex::new(Vec::new()),
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
            want_talk: Mutex::new(Vec::new()),
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
            want_talk: Mutex::new(Vec::new()),
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        // want_talk open lands talk-only; a listen ctrl errors.
        match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx())
            .await
        {
            Attach::Open(caps, _, _ctrl) => {
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
            want_talk: Mutex::new(Vec::new()),
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
            dead: std::sync::Mutex::new(None),
            dead_keep: watch::channel(false).0,
            lazy: None,
        });
        let client = Arc::new(StubClient {
            session: Ok(sess),
            opens: AtomicUsize::new(0),
            gate: None,
            want_talk: Mutex::new(Vec::new()),
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
            want_talk: Mutex::new(Vec::new()),
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
            want_talk: Mutex::new(Vec::new()),
        }));
        let t = svc.mint_token(mid()).await;
        assert_eq!(svc.consume_token(&t).await, Some(mid()));
        // Single-use — second consume fails.
        assert_eq!(svc.consume_token(&t).await, None);
        assert_eq!(svc.consume_token("bogus").await, None);
    }

    #[tokio::test]
    async fn want_talk_derives_from_persisted_marks_not_direction() {
        let client = Arc::new(StubClient {
            session: Ok(ok_session(true, true)),
            opens: AtomicUsize::new(0),
            gate: None,
            want_talk: Mutex::new(Vec::new()),
        });
        let (svc, _) = svc(client.clone());

        // A listen attach on an unmarked monitor still opens
        // want_talk — sendonly ⊥ addable mid-session.
        let e = entry_of(&svc, &client);
        _ = svc
            .attach(&e, &mid(), 1, Direction::Listen, &[], &drop_tx())
            .await;
        // Talk marked persisted → the open skips sendonly.
        let e2 = entry_of(&svc, &client);
        _ = svc
            .attach(
                &e2,
                &mid(),
                2,
                Direction::Listen,
                &["talk".to_owned()],
                &drop_tx(),
            )
            .await;
        assert_eq!(*client.want_talk.lock().await, vec![true, false]);
    }

    #[tokio::test]
    async fn talk_release_broadcasts_caps() {
        let client = Arc::new(StubClient {
            session: Ok(ok_session(true, true)),
            opens: AtomicUsize::new(0),
            gate: None,
            want_talk: Mutex::new(Vec::new()),
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let mut ctrl1 = match svc
            .attach(&e, &mid(), 1, Direction::Talk, &[], &drop_tx())
            .await
        {
            Attach::Open(_, _, rx) => rx,
            _ => panic!("expected open"),
        };
        let mut ctrl2 = match svc
            .attach(&e, &mid(), 2, Direction::Listen, &[], &drop_tx())
            .await
        {
            Attach::Open(_, _, rx) => rx,
            _ => panic!("expected open"),
        };
        svc.remove_direction(&e, 1, Direction::Talk).await;
        for rx in [&mut ctrl1, &mut ctrl2] {
            let msg = rx.recv().await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
            assert_eq!(v["type"], "caps");
            assert_eq!(v["talkFree"], true);
        }
    }

    #[tokio::test]
    async fn session_death_drops_consumers_with_error_1000() {
        let (dead_tx, dead_rx) = watch::channel(false);
        let sess = ok_session(true, false);
        *sess.dead.lock().unwrap() = Some(dead_rx);
        let client = Arc::new(StubClient {
            session: Ok(sess.clone()),
            opens: AtomicUsize::new(0),
            gate: None,
            want_talk: Mutex::new(Vec::new()),
        });
        let (svc, _) = svc(client.clone());
        let e = entry_of(&svc, &client);
        let (drop_tx, mut drop_rx) = tokio::sync::mpsc::unbounded_channel();
        match svc
            .attach(&e, &mid(), 1, Direction::Listen, &[], &drop_tx)
            .await
        {
            Attach::Open(..) => {}
            _ => panic!("expected open"),
        }
        dead_tx.send(true).unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(2), drop_rx.recv())
            .await
            .unwrap()
        {
            Some(ConsumerDrop::Error(_, 1000)) => {}
            other => panic!("expected error+close(1000), got {other:?}"),
        }
        // Session was torn down — the entry's slot is closed.
        assert!(sess.closed.load(Ordering::SeqCst) > 0);
    }
}
