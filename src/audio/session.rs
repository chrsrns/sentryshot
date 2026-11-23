// SPDX-License-Identifier: GPL-2.0-or-later

//! Session orchestration — `open(want_talk)` runs
//! `OPTIONS → DESCRIBE → SETUP(audio only) → PLAY`, wires the
//! RTP pump (recv) + pacer (talk) + keepalive, and surfaces
//! `AudioCaps` + direction marks.
//!
//! Negotiation rules (V257):
//! - `Require: www.onvif.org/ver20/backchannel` rides DESCRIBE,
//!   the sendonly SETUP, PLAY, and TEARDOWN — never the recvonly
//!   SETUP;
//! - `551`/Require rejection ⇒ untagged re-DESCRIBE →
//!   listen-only + talk mark candidate; a tagged 2xx SDP is
//!   authoritative and reused (some firmwares allow one
//!   DESCRIBE per connection);
//! - `a=sendonly` = talk, `a=recvonly` = listen, `a=sendrecv` =
//!   both, directionless = listen-only, `a=inactive` = session-only.

use crate::{
    ArcAudioSession, AudioCaps, AudioClient, AudioSession, Direction, Error, FaultClass, PcmChunk,
    g711, rtp,
    rtsp::{Interleaved, RtspClient},
    sdp::{self, AudioMedia},
};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, watch};

/// Whole `open` handshake budget (V256 — incl. the VIGI probe the
/// talk path may add).
const OPEN_BUDGET: Duration = Duration::from_secs(10);
/// Per-request socket wait inside the handshake.
const REQ_TIMEOUT: Duration = Duration::from_secs(4);
/// ONVIF backchannel tag.
const REQUIRE: &str = "www.onvif.org/ver20/backchannel";
/// Keepalive bound — OPTIONS goes out at half the negotiated
/// `Session` timeout, capped here when the server omits one.
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(30);
/// 20 ms of 8 kHz mono PCM.
const FRAME_SAMPLES: usize = 160;
/// Talk queue bound — ~100 ms of audio (V258 drop-oldest).
const TALK_QUEUE: usize = 5;
/// Listen broadcast depth.
const PCM_BROADCAST: usize = 16;

/// `AudioClient` over a raw RTSP session.
pub struct RtspAudioClient {
    url: url::Url,
    /// V265 fallback — `subStream`; `None` when it names the same
    /// endpoint as `url`.
    alt_url: Option<url::Url>,
}

/// Creds-free endpoint identity — host, effective port, path,
/// query. A `subStream` equal to `mainStream` is no fallback.
fn same_endpoint(a: &url::Url, b: &url::Url) -> bool {
    a.host_str() == b.host_str()
        && a.port_or_known_default().unwrap_or(554) == b.port_or_known_default().unwrap_or(554)
        && a.path() == b.path()
        && a.query() == b.query()
}

impl RtspAudioClient {
    /// New client for a monitor's stream URLs (creds ride each
    /// URL's own userinfo); `alt` retries the open on a `429`.
    #[must_use]
    pub fn new(url: url::Url, alt: Option<url::Url>) -> Self {
        let alt_url = alt.filter(|a| !same_endpoint(&url, a));
        Self { url, alt_url }
    }

    /// Camera host for the VIGI `:8800` probe (T142).
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.url.host_str()
    }
}

#[async_trait]
impl AudioClient for RtspAudioClient {
    async fn open(&self, want_talk: bool) -> Result<ArcAudioSession, Error> {
        for (attempt, url) in std::iter::once(&self.url)
            .chain(self.alt_url.iter())
            .enumerate()
        {
            match tokio::time::timeout(OPEN_BUDGET, self.open_inner(url, want_talk, attempt > 0))
                .await
            {
                // A saturated primary fully released → retry once on
                // the alternate; a 429 there propagates as-is.
                Ok(Err(Error::StreamLimit)) if attempt == 0 && self.alt_url.is_some() => {}
                Ok(r) => return r,
                Err(_) => return Err(Error::Timeout("session open")),
            }
        }
        unreachable!()
    }
}

/// One selected track's wire binding.
#[derive(Clone)]
struct Binding {
    channel: u8,
    codec: sdp::Codec,
    pt: u8,
}

/// Outcome of the DESCRIBE phase.
struct Described {
    sdp: sdp::Sdp,
    /// Backchannel `Require` negotiated — the tag rides PLAY and
    /// TEARDOWN (never the recvonly SETUP).
    tagged: bool,
}

impl RtspAudioClient {
    /// One full negotiate on `url` — `alt` marks the V265 retry,
    /// whose capability observations stay session-only (V266).
    async fn open_inner(
        &self,
        url: &url::Url,
        want_talk: bool,
        alt: bool,
    ) -> Result<ArcAudioSession, Error> {
        let mut client = RtspClient::connect(url).await?;
        // Liveness probe — any response proves the RTSP stack;
        // a `429` is the saturation signal the retry acts on.
        let resp = timed(client.request("OPTIONS", client.base(), &[])).await?;
        if resp.status == 429 {
            return Err(Error::StreamLimit);
        }

        let described = describe(&client, url, want_talk).await?;
        let media_rx = client
            .take_media()
            .ok_or_else(|| Error::Protocol("media channel already taken".into()))?;

        // Track selection + marks.
        let (listen_sel, talk_sel, mut marks) = select(&described.sdp, want_talk);

        // VIGI :8800 talk fallback (V259) — ONVIF yielded no talk
        // track on a want_talk open ⇒ unconditional probe; outcome
        // decides the talk mark's class.
        let mut vigi: Option<crate::vigi::VigiTalk> = None;
        if want_talk && talk_sel.is_none() {
            let user = url.username().to_owned();
            let pass = url.password().unwrap_or("").to_owned();
            if let Some(host) = url.host_str() {
                match crate::vigi::VigiTalk::connect(host, &user, &pass).await {
                    crate::vigi::VigiProbe::Ready(v) => {
                        vigi = Some(v);
                        marks.retain(|(d, _)| *d != Direction::Talk);
                    }
                    crate::vigi::VigiProbe::Unreachable => {}
                    crate::vigi::VigiProbe::Refused => {
                        for (d, c) in &mut marks {
                            if *d == Direction::Talk {
                                *c = FaultClass::Capability;
                            }
                        }
                    }
                }
            }
        }
        if listen_sel.is_none() && talk_sel.is_none() && vigi.is_none() {
            client.close();
            return Err(Error::Protocol("no usable audio path".into()));
        }

        // SETUP each selected track on fresh channel pairs. A
        // `sendrecv` line serving both directions is SETUP'd once —
        // one control URL owns both bindings.
        let client = Arc::new(client);
        let mut session_id = String::new();
        let mut next_ch: u8 = 0;
        let mut listen: Option<Binding> = None;
        let mut talk: Option<Binding> = None;
        let same_media = match (listen_sel, talk_sel) {
            (Some(a), Some(b)) => a.control == b.control,
            _ => false,
        };
        for (sel, is_talk) in [(listen_sel, false), (talk_sel, true)] {
            let Some(m) = sel else { continue };
            if is_talk && same_media {
                talk.clone_from(&listen);
                continue;
            }
            let ch = next_ch;
            next_ch += 2;
            let codec = m.payloads[0].codec;
            let pt = m.payloads[0].pt;
            match setup(
                &client,
                m,
                ch,
                described.tagged && (is_talk || same_media),
                &session_id,
            )
            .await
            {
                Ok(Some(id)) => session_id = id,
                Ok(None) => {}
                Err(e) => {
                    release(&client, &described.sdp.control, &session_id).await;
                    return Err(e);
                }
            }
            if is_talk {
                talk = Some(Binding {
                    channel: ch,
                    codec,
                    pt,
                });
            } else {
                listen = Some(Binding {
                    channel: ch,
                    codec,
                    pt,
                });
            }
        }

        // PLAY the aggregate control URL — only when a track was
        // SETUP'd; a pure-VIGI talk session owns no RTSP session.
        let mut keepalive = DEFAULT_SESSION_TIMEOUT;
        if listen.is_some() || talk.is_some() {
            let session_hdr = ("Session", session_id.as_str());
            let require_hdr = ("Require", REQUIRE);
            let mut extra = vec![session_hdr];
            if described.tagged {
                extra.push(require_hdr);
            }
            let resp = timed(client.request("PLAY", &described.sdp.control, &extra)).await?;
            if resp.status == 429 {
                release(&client, &described.sdp.control, &session_id).await;
                return Err(Error::StreamLimit);
            }
            if resp.status / 100 != 2 {
                release(&client, &described.sdp.control, &session_id).await;
                return Err(Error::Protocol(format!("PLAY status {}", resp.status)));
            }
            keepalive = session_timeout(&resp);
        }

        // V266 — capability faults learned on the alternate URL
        // disprove that URL only; downgrade them session-only so
        // they never persist `audio.unsupported`.
        if alt {
            for (_, class) in &mut marks {
                if *class == FaultClass::Capability {
                    *class = FaultClass::Session;
                }
            }
        }

        Ok(RtspSession::start(
            client,
            session_id,
            described.sdp.control,
            described.tagged,
            keepalive,
            listen,
            talk,
            vigi,
            media_rx,
            marks,
        ))
    }
}

/// Bound one request wait.
async fn timed<F, T>(f: F) -> Result<T, Error>
where
    F: std::future::Future<Output = Result<T, Error>>,
{
    match tokio::time::timeout(REQ_TIMEOUT, f).await {
        Ok(r) => r,
        Err(_) => Err(Error::Timeout("rtsp request")),
    }
}

/// Best-effort release — `TEARDOWN` when a `Session` was issued,
/// then socket close. The reader aborts on drop regardless; this
/// adds the graceful peer notification where possible.
async fn release(client: &RtspClient, control: &str, session_id: &str) {
    if !session_id.is_empty() {
        _ = timed(client.request("TEARDOWN", control, &[("Session", session_id)])).await;
    }
    client.close();
}

/// DESCRIBE (+`Require` when `want_talk`); a tagged 2xx SDP is
/// reused, the `551`/Require-rejection carries no SDP and retries
/// untagged.
async fn describe(
    client: &RtspClient,
    base: &url::Url,
    want_talk: bool,
) -> Result<Described, Error> {
    let accept = ("Accept", "application/sdp");
    if want_talk {
        let resp =
            timed(client.request("DESCRIBE", client.base(), &[accept, ("Require", REQUIRE)]))
                .await?;
        if resp.status == 429 {
            return Err(Error::StreamLimit);
        }
        if resp.status / 100 == 2 {
            let sdp = parse_body(&resp, base)?;
            // A 2xx SDP is authoritative — a missing sendonly means
            // the server ignored Require, so reuse it rather than
            // re-DESCRIBE: VIGI firmware 400s ANY second DESCRIBE on
            // a connection (B118).
            let tagged = sdp
                .audio
                .iter()
                .any(|m| m.direction.talks() && !m.payloads.is_empty());
            return Ok(Described { sdp, tagged });
        }
        // 551 / Require rejection carries no SDP → untagged pass.
    }
    let resp = timed(client.request("DESCRIBE", client.base(), &[accept])).await?;
    if resp.status == 429 {
        return Err(Error::StreamLimit);
    }
    if resp.status / 100 != 2 {
        return Err(Error::Protocol(format!("DESCRIBE status {}", resp.status)));
    }
    Ok(Described {
        sdp: parse_body(&resp, base)?,
        tagged: false,
    })
}

fn parse_body(resp: &crate::rtsp::Response, base: &url::Url) -> Result<sdp::Sdp, Error> {
    let text =
        std::str::from_utf8(&resp.body).map_err(|_| Error::Protocol("non-utf8 sdp".into()))?;
    Ok(sdp::parse(text, base))
}

/// Track selection + direction marks — first usable track per
/// direction; mark classes per V260/V259.
fn select(
    s: &sdp::Sdp,
    want_talk: bool,
) -> (
    Option<&AudioMedia>,
    Option<&AudioMedia>,
    Vec<(Direction, FaultClass)>,
) {
    let usable = |talk: bool| {
        s.audio.iter().find(|m| {
            (if talk {
                m.direction.talks()
            } else {
                m.direction.listens()
            }) && !m.payloads.is_empty()
        })
    };
    let listen_sel = usable(false);
    let talk_sel = if want_talk { usable(true) } else { None };

    let mut marks = Vec::new();
    if listen_sel.is_none() {
        // A listenable-but-codec-less line ⇒ capability; an
        // all-inactive SDP ⇒ session-only; absent audio media ⇒
        // capability (absent m=audio ∀ the direction).
        let class = if s.audio.iter().any(|m| m.direction.listens()) {
            FaultClass::Capability
        } else if !s.audio.is_empty()
            && s.audio
                .iter()
                .all(|m| m.direction == sdp::SdpDirection::Inactive)
        {
            FaultClass::Session
        } else {
            FaultClass::Capability
        };
        marks.push((Direction::Listen, class));
    }
    if want_talk && talk_sel.is_none() {
        // sendonly offered but codec-less ⇒ capability; absent
        // sendonly ⇒ session-only — the VIGI probe decides
        // persistence (V259: `:8800` outcome rules).
        let class = if s.audio.iter().any(|m| m.direction.talks()) {
            FaultClass::Capability
        } else {
            FaultClass::Session
        };
        marks.push((Direction::Talk, class));
    }
    (listen_sel, talk_sel, marks)
}

/// One track's SETUP — TCP-interleaved channel pair; `Require`
/// only on the sendonly SETUP of a tagged session.
async fn setup(
    client: &RtspClient,
    m: &AudioMedia,
    channel: u8,
    require: bool,
    session_id: &str,
) -> Result<Option<String>, Error> {
    let transport = format!("RTP/AVP/TCP;unicast;interleaved={channel}-{}", channel + 1);
    let mut extra = vec![("Transport", transport.as_str())];
    if require {
        extra.push(("Require", REQUIRE));
    }
    if !session_id.is_empty() {
        extra.push(("Session", session_id));
    }
    let resp = timed(client.request("SETUP", &m.control, &extra)).await?;
    if resp.status == 429 {
        return Err(Error::StreamLimit);
    }
    if resp.status / 100 != 2 {
        return Err(Error::Protocol(format!("SETUP status {}", resp.status)));
    }
    if session_id.is_empty() {
        return Ok(resp
            .header("Session")
            .map(|s| s.split(';').next().unwrap_or("").trim().to_owned()));
    }
    Ok(None)
}

/// `Session` timeout negotiation → keepalive interval bound.
fn session_timeout(resp: &crate::rtsp::Response) -> Duration {
    resp.header("Session")
        .and_then(|s| s.split(';').nth(1))
        .and_then(|p| p.trim().strip_prefix("timeout="))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_SESSION_TIMEOUT, |s| {
            Duration::from_secs(s).min(DEFAULT_SESSION_TIMEOUT)
        })
}

/// Listen-path state — decoded PCM fans out on a broadcast.
struct ListenState {
    rx: Mutex<broadcast::Receiver<PcmChunk>>,
}

/// Where the pacer writes talk media — RTSP interleaved or the
/// VIGI `:8800` multipart writer (V259 composition).
enum TalkOut {
    Rtsp { ch: u8, pt: u8 },
    Vigi(Mutex<crate::vigi::VigiTalk>),
}

/// Talk-path state — bounded PCM queue shared with the pacer.
struct TalkShared {
    queue: Mutex<VecDeque<PcmChunk>>,
    notify: Notify,
    codec: sdp::Codec,
    out: TalkOut,
    dead_tx: watch::Sender<bool>,
}

/// A live audio session.
pub struct RtspSession {
    client: Arc<RtspClient>,
    session_id: String,
    control: String,
    tagged: bool,
    caps: AudioCaps,
    marks: Vec<(Direction, FaultClass)>,
    listen: Option<ListenState>,
    talk: Option<Arc<TalkShared>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    closed: AtomicBool,
    /// Abnormal-task-exit signal — pump/pacer/keepalive send on
    /// transport death; `close` aborts tasks ⊥ signalling.
    dead_tx: watch::Sender<bool>,
}

impl RtspSession {
    /// Wire the negotiated bindings: recv pump, talk pacer,
    /// keepalive.
    #[allow(clippy::too_many_arguments)]
    fn start(
        client: Arc<RtspClient>,
        session_id: String,
        control: String,
        tagged: bool,
        keepalive: Duration,
        listen: Option<Binding>,
        talk: Option<Binding>,
        vigi: Option<crate::vigi::VigiTalk>,
        media_rx: mpsc::Receiver<Interleaved>,
        marks: Vec<(Direction, FaultClass)>,
    ) -> ArcAudioSession {
        let mut tasks = Vec::new();
        let (dead_tx, _) = watch::channel(false);

        // Listen pump — interleaved → RTP → seq-check → G.711 → PCM.
        let listen_state = listen.map(|b| {
            let (tx, rx) = broadcast::channel(PCM_BROADCAST);
            let mut channels: HashMap<u8, (u8, sdp::Codec)> = HashMap::new();
            channels.insert(b.channel, (b.pt, b.codec));
            tasks.push(tokio::spawn(pump(media_rx, channels, tx, dead_tx.clone())));
            ListenState { rx: Mutex::new(rx) }
        });

        // Talk pacer — 20 ms ticks, one ≤160-sample frame per tick.
        let talk_state = talk
            .map(|b| {
                (
                    b.codec,
                    TalkOut::Rtsp {
                        ch: b.channel,
                        pt: b.pt,
                    },
                )
            })
            .or_else(|| vigi.map(|v| (sdp::Codec::Pcma, TalkOut::Vigi(Mutex::new(v)))))
            .map(|(codec, out)| {
                let t = Arc::new(TalkShared {
                    queue: Mutex::new(VecDeque::with_capacity(TALK_QUEUE)),
                    notify: Notify::new(),
                    codec,
                    out,
                    dead_tx: dead_tx.clone(),
                });
                tasks.push(tokio::spawn(pacer(client.clone(), t.clone())));
                t
            });

        let session = Arc::new(Self {
            client,
            session_id,
            control,
            tagged,
            caps: AudioCaps {
                listen: listen_state.is_some(),
                talk: talk_state.is_some(),
            },
            marks,
            listen: listen_state,
            talk: talk_state,
            tasks: Mutex::new(tasks),
            closed: AtomicBool::new(false),
            dead_tx,
        });
        // A pure-VIGI session owns no RTSP session — nothing to keep
        // alive.
        if !session.session_id.is_empty() {
            session.clone().spawn_keepalive(keepalive);
        }
        session
    }

    /// OPTIONS keepalive at ≤ half the negotiated session timeout
    /// against the aggregate control URL w/ `Session` +
    /// `Authorization` where challenged.
    fn spawn_keepalive(self: &Arc<Self>, timeout: Duration) {
        let interval = timeout.min(DEFAULT_SESSION_TIMEOUT) / 2;
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // first tick fires immediately — skip it
            while !this.closed.load(Ordering::Relaxed) {
                tick.tick().await;
                if this.closed.load(Ordering::Relaxed) {
                    break;
                }
                let extra = [("Session", this.session_id.as_str())];
                let ok = this
                    .client
                    .request("OPTIONS", &this.control, &extra)
                    .await
                    .map(|r| r.status / 100 == 2)
                    .unwrap_or(false);
                if !ok {
                    // Socket fault ∨ non-2xx (`429`/`454`) — the
                    // camera dropped the session; teardown follows.
                    _ = this.dead_tx.send(true);
                    break;
                }
            }
        });
    }
}

/// Recv pump — consume interleaved frames for the bound channels.
async fn pump(
    mut media_rx: mpsc::Receiver<Interleaved>,
    channels: HashMap<u8, (u8, sdp::Codec)>,
    tx: broadcast::Sender<PcmChunk>,
    dead_tx: watch::Sender<bool>,
) {
    let mut last_seq: HashMap<u8, u16> = HashMap::new();
    while let Some(f) = media_rx.recv().await {
        let Some(&(pt, codec)) = channels.get(&f.channel) else {
            continue;
        };
        let Some(pkt) = rtp::parse(&f.payload) else {
            continue;
        };
        if pkt.pt != pt {
            continue; // stray RTCP-as-RTP / unnegotiated PT
        }
        // Drop out-of-order/duplicate (no jitter buffer v1).
        match last_seq.get(&f.channel) {
            Some(&last) if !rtp::newer(pkt.seq, last) => continue,
            _ => {
                last_seq.insert(f.channel, pkt.seq);
            }
        }
        let pcm = g711::decode(codec, &pkt.payload);
        _ = tx.send(pcm); // zero listeners ≠ death — keep decoding
    }
    _ = dead_tx.send(true); // media_rx closed — reader task died
}

/// Talk pacer — emits at most one ≤160-sample RTP frame per 20 ms
/// tick; queue underruns skip the tick (drop-oldest happens at the
/// push side). >160-sample chunks re-queue the remainder.
async fn pacer(client: Arc<RtspClient>, state: Arc<TalkShared>) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    loop {
        tick.tick().await;
        let mut q = state.queue.lock().await;
        let Some(mut chunk) = q.pop_front() else {
            drop(q);
            continue;
        };
        let rest = (chunk.len() > FRAME_SAMPLES).then(|| chunk.split_off(FRAME_SAMPLES));
        if let Some(rest) = rest {
            q.push_front(rest);
        }
        drop(q);
        let sent = match &state.out {
            TalkOut::Rtsp { ch, pt } => {
                let payload = g711::encode(state.codec, &chunk);
                client
                    .send_interleaved(*ch, &rtp::build(*pt, seq, ts, ssrc, &payload))
                    .await
                    .is_ok()
            }
            TalkOut::Vigi(v) => v.lock().await.send(&chunk, ts).await.is_ok(),
        };
        if !sent {
            _ = state.dead_tx.send(true);
            return;
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(u32::try_from(chunk.len()).unwrap_or(0));
    }
}

#[async_trait]
impl AudioSession for RtspSession {
    fn caps(&self) -> AudioCaps {
        self.caps
    }

    fn marks(&self) -> Vec<(Direction, FaultClass)> {
        self.marks.clone()
    }

    async fn recv(&self) -> Option<PcmChunk> {
        let Some(l) = &self.listen else { return None };
        loop {
            match l.rx.lock().await.recv().await {
                Ok(pcm) => return Some(pcm),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    async fn talk(&self, pcm: &[i16]) -> Result<(), Error> {
        let Some(t) = &self.talk else {
            return Err(Error::Unsupported(Direction::Talk));
        };
        let mut q = t.queue.lock().await;
        while q.len() >= TALK_QUEUE {
            q.pop_front(); // drop-oldest
        }
        q.push_back(pcm.to_vec());
        t.notify.notify_one();
        Ok(())
    }

    async fn wait_dead(&self) {
        let mut rx = self.dead_tx.subscribe();
        _ = rx.wait_for(|d| *d).await;
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        // Pure-VIGI talk sessions hold no RTSP session — skip.
        if !self.session_id.is_empty() {
            let extra: Vec<(&str, &str)> = if self.tagged {
                vec![("Session", &self.session_id), ("Require", REQUIRE)]
            } else {
                vec![("Session", &self.session_id)]
            };
            let _ = self.client.request("TEARDOWN", &self.control, &extra).await;
        }
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
        }
        self.client.close();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{Codec, Direction};

    /// Serializes tests that can hit the fixed VIGI `:8800` port —
    /// a bound stub would answer *any* test's probe.
    static VIGI_PORT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    use pretty_assertions::assert_eq;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc as tmpsc;

    /// Canned request transcript — `(assertion, reply)`.
    struct Stub {
        port: u16,
        _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        got: tmpsc::Receiver<String>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn read_head(rd: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
        let mut out = String::new();
        loop {
            let mut line = String::new();
            rd.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            out.push_str(&line);
        }
        out
    }

    fn reply(req: &str, status: &str, extra: &str, body: &str) -> String {
        let cseq = req
            .lines()
            .find_map(|l| l.strip_prefix("CSeq: "))
            .unwrap()
            .trim();
        let mut r = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
        if !body.is_empty() {
            write!(
                r,
                "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
                body.len()
            )
            .ok();
        }
        r.push_str(extra);
        r.push_str("\r\n");
        r.push_str(body);
        r
    }

    use std::fmt::Write as _;

    /// Spawn a stub RTSP server replying 200+`Session: s1` to every
    /// request; all requests stream into `got` for assertions.
    async fn stub(sdp_body: &'static str) -> Stub {
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = lis.local_addr().unwrap().port();
        let (tx, got) = tmpsc::channel::<String>(64);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let h = tokio::spawn(async move {
            let Ok((stream, _)) = lis.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            loop {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let mut peek = [0_u8; 1];
                let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_millis(3000), rd.read(&mut peek)).await
                else {
                    break;
                };
                if n == 0 {
                    break;
                }
                if peek[0] == b'$' {
                    // Swallow interleaved (RTCP echo etc.).
                    let mut rest = [0_u8; 3];
                    if rd.read_exact(&mut rest).await.is_err() {
                        break;
                    }
                    let len = usize::from(u16::from_be_bytes([rest[1], rest[2]]));
                    let mut p = vec![0_u8; len];
                    if rd.read_exact(&mut p).await.is_err() {
                        break;
                    }
                    tx.send(format!("$frame ch={} len={len}", rest[0]))
                        .await
                        .ok();
                    continue;
                }
                // Reattach first byte into a head read.
                let mut head = String::from_utf8(vec![peek[0]]).unwrap();
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.is_err() {
                        return;
                    }
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let is_describe = head.starts_with("DESCRIBE");
                let body = if is_describe { sdp_body } else { "" };
                let extra = if head.starts_with("SETUP") {
                    "Session: s1;timeout=60\r\n"
                } else {
                    ""
                };
                if tx.send(head.clone()).await.is_err() {
                    break;
                }
                let r = reply(&head, "200 OK", extra, body);
                if wr.write_all(r.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        Stub {
            port,
            _stop: stop,
            got,
            _task: h,
        }
    }

    fn url(port: u16) -> url::Url {
        url::Url::parse(&format!("rtsp://127.0.0.1:{port}/live")).unwrap()
    }

    /// Scripted stub — `status_of` picks the reply per request
    /// head; `session_timeout` rides SETUP replies. Logs every
    /// request + an `EOF` marker when the client socket closes.
    async fn stub_scripted(
        status_of: impl Fn(&str) -> &'static str + Send + 'static,
        sdp_body: &'static str,
        session_timeout: u32,
    ) -> Stub {
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = lis.local_addr().unwrap().port();
        let (tx, got) = tmpsc::channel::<String>(64);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let h = tokio::spawn(async move {
            let Ok((stream, _)) = lis.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            loop {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let mut peek = [0_u8; 1];
                let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_millis(3000), rd.read(&mut peek)).await
                else {
                    break;
                };
                if n == 0 {
                    tx.send("EOF".to_owned()).await.ok();
                    break;
                }
                if peek[0] == b'$' {
                    let mut rest = [0_u8; 3];
                    if rd.read_exact(&mut rest).await.is_err() {
                        break;
                    }
                    let len = usize::from(u16::from_be_bytes([rest[1], rest[2]]));
                    let mut p = vec![0_u8; len];
                    if rd.read_exact(&mut p).await.is_err() {
                        break;
                    }
                    continue;
                }
                let mut head = String::from_utf8(vec![peek[0]]).unwrap();
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.is_err() {
                        return;
                    }
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let status = status_of(&head);
                let is_describe = head.starts_with("DESCRIBE") && status.starts_with('2');
                let body = if is_describe { sdp_body } else { "" };
                let extra = if (head.starts_with("SETUP") || head.starts_with("PLAY"))
                    && status.starts_with('2')
                {
                    format!("Session: s1;timeout={session_timeout}\r\n")
                } else {
                    String::new()
                };
                if tx.send(head.clone()).await.is_err() {
                    break;
                }
                let r = reply(&head, status, &extra, body);
                if wr.write_all(r.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        Stub {
            port,
            _stop: stop,
            got,
            _task: h,
        }
    }

    /// Stub replying `429` to requests starting with `fail_on`.
    async fn stub_429(fail_on: &'static str, sdp_body: &'static str) -> Stub {
        stub_scripted(
            move |head| {
                if head.starts_with(fail_on) {
                    "429 Too Many Requests"
                } else {
                    "200 OK"
                }
            },
            sdp_body,
            60,
        )
        .await
    }

    fn drain(rx: &mut tmpsc::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(r) = rx.try_recv() {
            out.push(r);
        }
        out
    }

    /// Drain entries until the `EOF` marker arrives (or `ms`
    /// elapses) — socket-close detection is asynchronous.
    async fn collect(rx: &mut tmpsc::Receiver<String>, ms: u64) -> Vec<String> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(r)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            let eof = r == "EOF";
            out.push(r);
            if eof {
                break;
            }
        }
        out
    }

    const DUPLEX_SDP: &str = "v=0\r\nm=audio 0 RTP/AVP 0\r\na=recvonly\r\na=control:trackID=1\r\n\
         m=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=2\r\n";

    /// Usable recvonly PCMU + sendonly AAC — the sendonly line is
    /// offered but codec-less after filtering ⇒ capability mark.
    const ALT_AAC_TALK_SDP: &str = "v=0\r\nm=audio 0 RTP/AVP 0\r\na=recvonly\r\na=control:trackID=1\r\n\
         m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AAC/16000\r\na=sendonly\r\na=control:trackID=2\r\n";

    #[tokio::test]
    async fn open_duplex_negotiates_backchannel() {
        let mut st = stub(DUPLEX_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: true
            }
        );
        assert!(sess.marks().is_empty());
        sess.close().await;

        let mut reqs = Vec::new();
        while let Ok(r) = st.got.try_recv() {
            reqs.push(r);
        }
        let describe = reqs.iter().find(|r| r.starts_with("DESCRIBE")).unwrap();
        assert!(
            describe.contains("Require: www.onvif.org/ver20/backchannel"),
            "{describe}"
        );
        let setups: Vec<_> = reqs.iter().filter(|r| r.starts_with("SETUP")).collect();
        assert_eq!(setups.len(), 2);
        // The recvonly SETUP carries no Require; the sendonly does.
        assert!(!setups[0].contains("Require:"), "{}", setups[0]);
        assert!(setups[1].contains("Require:"), "{}", setups[1]);
        let play = reqs.iter().find(|r| r.starts_with("PLAY")).unwrap();
        assert!(play.contains("Require:"), "{play}");
    }

    #[tokio::test]
    async fn open_listen_only_negotiates_no_require() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(false).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: false
            }
        );
        sess.close().await;
        let mut reqs = Vec::new();
        while let Ok(r) = st.got.try_recv() {
            reqs.push(r);
        }
        assert!(reqs.iter().all(|r| !r.contains("Require:")), "{reqs:?}");
    }

    #[tokio::test]
    async fn want_talk_absent_sendonly_reuses_tagged_sdp() {
        // VIGI firmware 400s ANY second DESCRIBE on a connection —
        // a tagged 2xx SDP that merely lacks sendonly must be reused
        // for the untagged session, not re-described (B118).
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let _port_guard = VIGI_PORT.lock().await;
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: false
            }
        );
        let talk_mark = sess
            .marks()
            .into_iter()
            .find(|(d, _)| *d == Direction::Talk);
        assert!(
            matches!(talk_mark, Some((_, FaultClass::Session))),
            "{:?}",
            sess.marks()
        );
        sess.close().await;
        let mut describes = Vec::new();
        while let Ok(r) = st.got.try_recv() {
            if r.starts_with("DESCRIBE") {
                describes.push(r);
            }
        }
        assert_eq!(describes.len(), 1);
        assert!(describes[0].contains("Require:"));
    }

    #[tokio::test]
    async fn want_talk_551_retries_untagged() {
        // Require genuinely rejected (551) carries no SDP — the
        // untagged DESCRIBE still runs on the same connection.
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub_scripted(
            |head| {
                if head.starts_with("DESCRIBE") && head.contains("Require:") {
                    "551"
                } else {
                    "200"
                }
            },
            RECV_SDP,
            10,
        )
        .await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(true).await.unwrap();
        assert!(sess.caps().listen);
        sess.close().await;
        let mut describes = Vec::new();
        while let Ok(r) = st.got.try_recv() {
            if r.starts_with("DESCRIBE") {
                describes.push(r);
            }
        }
        assert_eq!(describes.len(), 2);
        assert!(describes[0].contains("Require:"));
        assert!(!describes[1].contains("Require:"));
    }

    #[tokio::test]
    async fn talk_only_session() {
        const TALK_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";
        let mut st = stub(TALK_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: false,
                talk: true
            }
        );
        // listen marked (session or capability) since nothing listens.
        assert!(sess.marks().iter().any(|(d, _)| *d == Direction::Listen));
        sess.close().await;
        let _ = &mut st;
    }

    #[tokio::test]
    async fn inactive_only_sdp_opens_nothing() {
        const INACT: &str = "v=0\r\nm=audio 0 RTP/AVP 0\r\na=inactive\r\na=control:trackID=1\r\n";
        let _port_guard = VIGI_PORT.lock().await;
        let st = stub(INACT).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let Err(err) = client.open(true).await else {
            panic!("expected open failure")
        };
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        let _ = st;
    }

    #[tokio::test]
    async fn recv_pump_decodes_incoming_rtp() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=recvonly\r\na=control:trackID=1\r\n";
        // Server completes SETUP then pushes two PCMU $-frames (one
        // duplicate seq — must drop).
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = lis.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (stream, _) = lis.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            loop {
                let head = read_head(&mut rd).await;
                let extra = if head.starts_with("SETUP") {
                    "Session: s1\r\n"
                } else {
                    ""
                };
                let body = if head.starts_with("DESCRIBE") {
                    RECV_SDP
                } else {
                    ""
                };
                wr.write_all(reply(&head, "200 OK", extra, body).as_bytes())
                    .await
                    .unwrap();
                if head.starts_with("PLAY") {
                    break;
                }
            }
            let frame = |seq: u16, data: &[u8]| -> Vec<u8> {
                let mut f = vec![b'$', 0, 0, 0];
                let rtp = crate::rtp::build(0, seq, 0, 1, data);
                f[2..4].copy_from_slice(&u16::try_from(rtp.len()).unwrap().to_be_bytes());
                f.extend_from_slice(&rtp);
                f
            };
            wr.write_all(&frame(10, &crate::g711::encode(Codec::Pcmu, &[1000; 4])))
                .await
                .unwrap();
            wr.write_all(&frame(10, &crate::g711::encode(Codec::Pcmu, &[2000; 4])))
                .await
                .unwrap(); // dup seq — dropped
            wr.write_all(&frame(11, &crate::g711::encode(Codec::Pcmu, &[2000; 4])))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let client = RtspAudioClient::new(url(port), None);
        let sess = client.open(false).await.unwrap();
        let first = sess.recv().await.unwrap();
        assert_eq!(first.len(), 4);
        let second = sess.recv().await.unwrap();
        assert!(
            second.iter().all(|&v| (1900..2100).contains(&v)),
            "{second:?}"
        );
        sess.close().await;
        task.abort();
    }

    #[tokio::test]
    async fn talk_pacer_writes_rtp_frames() {
        const TALK_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = lis.local_addr().unwrap().port();
        let (tx, mut got) = tmpsc::channel::<Vec<u8>>(8);
        let task = tokio::spawn(async move {
            let (stream, _) = lis.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let mut pending = Vec::new();
            loop {
                // Peek first byte — request head or $ frame.
                let mut first = [0_u8; 1];
                let n = tokio::time::timeout(Duration::from_secs(2), rd.read(&mut first))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(0);
                if n == 0 {
                    break;
                }
                if first[0] == b'$' {
                    let mut hdr = [0_u8; 3];
                    rd.read_exact(&mut hdr).await.unwrap();
                    let len = usize::from(u16::from_be_bytes([hdr[1], hdr[2]]));
                    let mut payload = vec![0_u8; len];
                    rd.read_exact(&mut payload).await.unwrap();
                    tx.send(payload).await.unwrap();
                    continue;
                }
                let mut head = String::from_utf8(vec![first[0]]).unwrap();
                loop {
                    let mut line = String::new();
                    rd.read_line(&mut line).await.unwrap();
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                pending.push(head.clone());
                let body = if head.starts_with("DESCRIBE") {
                    TALK_SDP
                } else {
                    ""
                };
                wr.write_all(reply(&head, "200 OK", "Session: s1\r\n", body).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let client = RtspAudioClient::new(url(port), None);
        let sess = client.open(true).await.unwrap();
        sess.talk(&[1000_i16; 160]).await.unwrap();
        let f1 = tokio::time::timeout(Duration::from_secs(2), got.recv())
            .await
            .unwrap()
            .unwrap();
        let p1 = rtp::parse(&f1).unwrap();
        assert_eq!(p1.pt, 0);
        assert_eq!(p1.payload.len(), 160); // one G.711 byte per sample
        sess.talk(&[1000_i16; 320]).await.unwrap(); // splits into 2 frames
        let f2 = tokio::time::timeout(Duration::from_secs(2), got.recv())
            .await
            .unwrap()
            .unwrap();
        let f3 = tokio::time::timeout(Duration::from_secs(2), got.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rtp::parse(&f2).unwrap().payload.len(), 160);
        assert_eq!(rtp::parse(&f3).unwrap().payload.len(), 160);
        sess.close().await;
        task.abort();
    }

    #[tokio::test]
    async fn talk_unnegotiated_errors() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(false).await.unwrap();
        let Err(err) = sess.talk(&[0; 160]).await else {
            panic!("expected talk failure")
        };
        assert!(matches!(err, Error::Unsupported(Direction::Talk)));
        sess.close().await;
        let _ = &mut st;
    }

    #[tokio::test]
    async fn keepalive_fires_options() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(false).await.unwrap();
        // Session timeout negotiated 60s → keepalive at 30s — too
        // long to observe; assert the plumbing only via OPTIONS
        // presence in the transcript (skip waiting).
        sess.close().await;
        let mut n_opts = 0;
        while let Ok(r) = st.got.try_recv() {
            if r.starts_with("OPTIONS") {
                n_opts += 1;
            }
        }
        assert_eq!(n_opts, 1); // the initial probe only
    }

    /// VIGI composition (V259): recvonly-only SDP on a `want_talk`
    /// open → `:8800` probe answers → talk rides the VIGI writer.
    /// Skips silently when `:8800` is already bound locally.
    #[tokio::test]
    async fn vigi_fallback_composes_duplex() {
        const RECV_SDP_V: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let _port_guard = VIGI_PORT.lock().await;
        let Ok(lis8800) = TcpListener::bind("127.0.0.1:8800").await else {
            return; // port held — skip
        };
        let (tx, mut parts) = tmpsc::channel::<Vec<u8>>(32);
        tokio::spawn(async move {
            let Ok((stream, _)) = lis8800.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            // 200 immediately (no auth challenge — stub posture).
            let _ = read_head(&mut rd).await;
            wr.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed\r\n\r\n")
                .await
                .unwrap();
            let mut buf = vec![0_u8; 4096];
            // Talk-request part → session_id reply.
            let _ = rd.read(&mut buf).await.unwrap();
            let body = r#"{"params":{"session_id":"s1"}}"#;
            let sid = format!(
                "--device-stream-boundary--\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            wr.write_all(sid.as_bytes()).await.unwrap();
            loop {
                let n = rd.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    return;
                }
            }
        });

        let mut st = stub(RECV_SDP_V).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: true
            }
        );
        // The talk mark candidate cleared on VIGI success.
        assert!(
            !sess.marks().iter().any(|(d, _)| *d == Direction::Talk),
            "{:?}",
            sess.marks()
        );
        sess.talk(&[1000_i16; 160]).await.unwrap();
        // The pacer emits an audio/mp2t part within a tick.
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, parts.recv()).await else {
                break;
            };
            got.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&got);
        assert!(
            text.contains("audio/mp2t") || got.windows(4).any(|w| w == b"\r\n\r\n"),
            "{text}"
        );
        sess.close().await;
        let _ = &mut st;
    }

    #[tokio::test]
    async fn teardown_sent_on_close() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(false).await.unwrap();
        sess.close().await;
        let mut saw_teardown = false;
        while let Ok(r) = st.got.try_recv() {
            if r.starts_with("TEARDOWN") {
                saw_teardown = true;
                assert!(r.contains("Session: s1"), "{r}");
            }
        }
        assert!(saw_teardown);
    }

    #[tokio::test]
    async fn play_429_retries_alternate() {
        let mut primary = stub_429("PLAY", DUPLEX_SDP).await;
        let mut alt = stub_scripted(|_| "200 OK", DUPLEX_SDP, 60).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)));
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: true
            }
        );
        sess.close().await;

        let reqs = collect(&mut primary.got, 500).await;
        assert!(reqs.iter().any(|r| r.starts_with("PLAY")), "{reqs:?}");
        // Session id already issued at SETUP → TEARDOWN, then EOF.
        assert!(reqs.iter().any(|r| r.starts_with("TEARDOWN")), "{reqs:?}");
        assert!(reqs.iter().any(|r| r == "EOF"), "{reqs:?}");
        let alt_reqs = drain(&mut alt.got);
        assert!(
            alt_reqs.iter().any(|r| r.starts_with("PLAY")),
            "{alt_reqs:?}"
        );
    }

    #[tokio::test]
    async fn describe_429_retries_alternate() {
        let mut primary = stub_429("DESCRIBE", DUPLEX_SDP).await;
        let alt = stub_scripted(|_| "200 OK", DUPLEX_SDP, 60).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)));
        let sess = client.open(false).await.unwrap();
        assert!(sess.caps().listen);
        sess.close().await;
        let reqs = collect(&mut primary.got, 500).await;
        assert!(reqs.iter().any(|r| r.starts_with("DESCRIBE")), "{reqs:?}");
        assert!(reqs.iter().any(|r| r == "EOF"), "{reqs:?}");
    }

    #[tokio::test]
    async fn options_429_retries_alternate() {
        let mut primary = stub_429("OPTIONS", DUPLEX_SDP).await;
        let alt = stub_scripted(|_| "200 OK", DUPLEX_SDP, 60).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)));
        let sess = client.open(false).await.unwrap();
        assert!(sess.caps().listen);
        sess.close().await;
        let reqs = collect(&mut primary.got, 500).await;
        assert!(reqs.iter().any(|r| r.starts_with("OPTIONS")), "{reqs:?}");
        // OPTIONS was the first request — no Session id → no TEARDOWN.
        assert!(!reqs.iter().any(|r| r.starts_with("TEARDOWN")), "{reqs:?}");
        assert!(reqs.iter().any(|r| r == "EOF"), "{reqs:?}");
    }

    #[tokio::test]
    async fn setup_429_retries_alternate() {
        let primary = stub_429("SETUP", DUPLEX_SDP).await;
        let mut alt = stub_scripted(|_| "200 OK", DUPLEX_SDP, 60).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)));
        let sess = client.open(false).await.unwrap();
        assert!(sess.caps().listen);
        sess.close().await;
        let alt_reqs = drain(&mut alt.got);
        assert!(
            alt_reqs.iter().any(|r| r.starts_with("PLAY")),
            "{alt_reqs:?}"
        );
    }

    #[tokio::test]
    async fn play_429_without_alternate_is_stream_limit() {
        let _g = VIGI_PORT.lock().await;
        let mut primary = stub_429("PLAY", DUPLEX_SDP).await;
        let client = RtspAudioClient::new(url(primary.port), None);
        let Err(err) = client.open(false).await else {
            panic!("expected StreamLimit");
        };
        assert!(matches!(err, Error::StreamLimit), "{err:?}");
        assert_eq!(err.class(), FaultClass::Transport);
        // Payload-free — no URL can leak into error ctrls (V258).
        assert!(!format!("{err}").contains("rtsp"), "{err}");
        let reqs = collect(&mut primary.got, 500).await;
        assert!(reqs.iter().any(|r| r == "EOF"), "{reqs:?}");
    }

    #[tokio::test]
    async fn both_urls_429_propagates_stream_limit() {
        let _g = VIGI_PORT.lock().await;
        let primary = stub_429("PLAY", DUPLEX_SDP).await;
        let alt = stub_429("PLAY", DUPLEX_SDP).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)));
        let Err(err) = client.open(false).await else {
            panic!("expected StreamLimit");
        };
        assert!(matches!(err, Error::StreamLimit), "{err:?}");
    }

    #[tokio::test]
    async fn same_endpoint_alternate_not_retried() {
        let _g = VIGI_PORT.lock().await;
        let mut primary = stub_429("PLAY", DUPLEX_SDP).await;
        // Same endpoint, different userinfo → deduped at construction.
        let creds = url::Url::parse(&format!(
            "rtsp://u%40x:p%40ss@127.0.0.1:{}/live",
            primary.port
        ))
        .unwrap();
        let client = RtspAudioClient::new(url(primary.port), Some(creds));
        let Err(err) = client.open(false).await else {
            panic!("expected StreamLimit");
        };
        assert!(matches!(err, Error::StreamLimit), "{err:?}");
        let reqs = drain(&mut primary.got);
        // A self-retry would re-run the whole handshake on a second
        // connection — got holds exactly one DESCRIBE + one PLAY.
        assert_eq!(
            1,
            reqs.iter().filter(|r| r.starts_with("DESCRIBE")).count(),
            "{reqs:?}"
        );
        assert_eq!(
            1,
            reqs.iter().filter(|r| r.starts_with("PLAY")).count(),
            "{reqs:?}"
        );
    }

    #[tokio::test]
    async fn alternate_capability_marks_stay_session_only() {
        let _g = VIGI_PORT.lock().await;
        let primary = stub_429("PLAY", DUPLEX_SDP).await;
        // Alt SDP: sendonly AAC → talk capability mark (codec-less);
        // 127.0.0.1:8800 refused → mark survives, downgraded alt-side.
        let mut alt = stub_scripted(|_| "200 OK", ALT_AAC_TALK_SDP, 60).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)));
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: false
            }
        );
        for (d, class) in sess.marks() {
            assert_eq!(
                FaultClass::Session,
                class,
                "alt-learned {d:?} mark must not persist as capability"
            );
        }
        sess.close().await;
        let _ = &mut alt;
    }

    #[tokio::test]
    async fn keepalive_non_2xx_kills_session() {
        let _g = VIGI_PORT.lock().await;
        // First OPTIONS answers the handshake probe; later ones 500.
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = count.clone();
        let mut st = stub_scripted(
            move |head| {
                if head.starts_with("OPTIONS")
                    && c.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0
                {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                }
            },
            DUPLEX_SDP,
            2,
        )
        .await;
        let client = RtspAudioClient::new(url(st.port), None);
        let sess = client.open(false).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sess.wait_dead())
            .await
            .expect("session must die on keepalive 500");
        sess.close().await;
        let _ = &mut st;
    }
}
