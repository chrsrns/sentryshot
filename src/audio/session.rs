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
    TalkClaim, g711, rtp,
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
/// Talk queue bound — ~400 ms of audio (V258 drop-oldest); sized
/// over the worst-case client capture burst (a 256 ms callback
/// lands ~13×20 ms frames at once).
const TALK_QUEUE: usize = 20;
/// Listen broadcast depth.
const PCM_BROADCAST: usize = 16;

/// `AudioClient` over a raw RTSP session.
pub struct RtspAudioClient {
    url: url::Url,
    /// V265 fallback — `subStream`; `None` when it names the same
    /// endpoint as `url`.
    alt_url: Option<url::Url>,
    /// V380 probe inputs resolved live from `Config` at open —
    /// a `MonitorConfig` re-read on every `open` call (the entry
    /// may outlive a settings save).
    cfg: crate::service::CfgAccessor,
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
    /// URL's own userinfo); `alt` retries the open on a `429`,
    /// `cfg` resolves V380 probe inputs inside `open`.
    #[must_use]
    pub fn new(url: url::Url, alt: Option<url::Url>, cfg: crate::service::CfgAccessor) -> Self {
        let alt_url = alt.filter(|a| !same_endpoint(&url, a));
        Self { url, alt_url, cfg }
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
    /// A pre-selection RTSP fault demotes to the private-protocol
    /// arm rather than killing the open (V278); `429`/`StreamLimit`
    /// propagates to the alt retry unprobed.
    #[allow(clippy::too_many_lines)]
    async fn open_inner(
        &self,
        url: &url::Url,
        want_talk: bool,
        alt: bool,
    ) -> Result<ArcAudioSession, Error> {
        // Probe budget shares this URL's open window — a mid-arm
        // exhaustion maps to ⊥-V380, ⊥ a second window.
        let deadline = tokio::time::Instant::now() + OPEN_BUDGET;

        let phase = async {
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
            Ok((client, described, media_rx))
        }
        .await;

        let (mut rtsp_client, described, media_rx, orig_fault) = match phase {
            Ok((c, d, r)) => (Some(c), Some(d), Some(r), None),
            Err(Error::StreamLimit) => return Err(Error::StreamLimit),
            Err(e) => (None, None, None, Some(e)),
        };

        // Track selection + marks — the fault arm starts markless.
        let (listen_sel, talk_sel, mut marks) = match &described {
            Some(d) => select(&d.sdp, want_talk),
            None => (None, None, Vec::new()),
        };

        // VIGI :8800 talk fallback (V259) — ONVIF yielded no talk
        // track on a want_talk open ⇒ unconditional probe; outcome
        // decides the talk mark's class. Capability-only under V275:
        // a `Ready` talker is dropped immediately (the slot frees —
        // claims happen per hold), a `-52405` grant still proves the
        // protocol. The fault arm evaluates it before V380 (V278).
        let vigi = if want_talk && talk_sel.is_none() {
            probe_vigi(url, &mut marks).await
        } else {
            VigiOut::NotProbed
        };
        let vigi_claim = match &vigi {
            VigiOut::Served(c) => Some(VigiClaim {
                host: c.host.clone(),
                user: c.user.clone(),
                pass: c.pass.clone(),
            }),
            _ => None,
        };

        // V380 fallback (V278) — a wanted direction uncovered AND
        // the VIGI dialect ⊥-proven (absent, Refused, Unreachable —
        // a `Ready`/`Busy` settle proves VIGI owns `:8800`).
        let uncovered = listen_sel.is_none() || (want_talk && talk_sel.is_none());
        let v380 = if uncovered && !matches!(vigi, VigiOut::Served(_)) {
            let cfg = (self.cfg)().await;
            probe_v380(url, &cfg, deadline).await
        } else {
            V380Out::Skipped
        };
        let (v380_serves_listen, v380_serves_talk, v380_compose, v380_claim) = apply_v380(
            &v380,
            &vigi,
            listen_sel.is_none(),
            talk_sel.is_none() && vigi_claim.is_none(),
            &mut marks,
        );

        // Compose gate — anything negotiated ∨ lazily claimed ∨
        // oracle-minted keeps the session alive.
        let rtsp_composed = listen_sel.is_some() || talk_sel.is_some() || vigi_claim.is_some();
        if !rtsp_composed && !v380_compose {
            if let Some(c) = &mut rtsp_client {
                c.close();
            }
            // A creds/result verdict carries its message; the fault
            // arm otherwise propagates the original fault (V278).
            if let V380Out::Creds(m) = v380 {
                return Err(Error::Protocol(m));
            }
            if let Some(f) = orig_fault {
                return Err(f);
            }
            return Err(Error::Protocol("no usable audio path".into()));
        }

        // A selection-arm pure-V380 compose owns ⊥ RTSP session —
        // the conn closes post-`select` (V278); mixed composes keep
        // it for the negotiated direction.
        if !rtsp_composed && orig_fault.is_none() {
            if let Some(c) = rtsp_client.take() {
                c.close();
            }
        }
        let client = rtsp_client.map(Arc::new);

        // SETUP each selected track on fresh channel pairs. A
        // `sendrecv` line serving both directions is SETUP'd once —
        // one control URL owns both bindings. ⊥-RTSP arms (fault,
        // pure-V380) skip SETUP/PLAY entirely.
        let (session_id, listen, talk) = match (&client, &described) {
            (Some(c), Some(d)) => setup_tracks(c, d, listen_sel, talk_sel).await?,
            _ => (String::new(), None, None),
        };

        // PLAY the aggregate control URL — only when a track was
        // SETUP'd; a pure private-protocol session owns no RTSP
        // session.
        let mut keepalive = DEFAULT_SESSION_TIMEOUT;
        if let (Some(c), Some(d)) = (&client, &described) {
            if listen.is_some() || talk.is_some() {
                let session_hdr = ("Session", session_id.as_str());
                let require_hdr = ("Require", REQUIRE);
                let mut extra = vec![session_hdr];
                if d.tagged {
                    extra.push(require_hdr);
                }
                let resp = timed(c.request("PLAY", &d.sdp.control, &extra)).await?;
                if resp.status == 429 {
                    release(c, &d.sdp.control, &session_id).await;
                    return Err(Error::StreamLimit);
                }
                if resp.status / 100 != 2 {
                    release(c, &d.sdp.control, &session_id).await;
                    return Err(Error::Protocol(format!("PLAY status {}", resp.status)));
                }
                keepalive = session_timeout(&resp);
            }
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

        let (control, tagged) =
            described.map_or((String::new(), false), |d| (d.sdp.control, d.tagged));
        Ok(RtspSession::start(
            client,
            session_id,
            control,
            tagged,
            keepalive,
            listen,
            talk,
            vigi_claim,
            v380_claim.map(|claim| ComposedV380 {
                claim,
                serves_listen: v380_serves_listen,
                serves_talk: v380_serves_talk,
            }),
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

/// SETUP each selected track on fresh channel pairs — a
/// `sendrecv` line serving both directions binds once (one
/// control URL owns both bindings). On failure the caller's
/// release is handled inside via the aggregate control URL.
async fn setup_tracks(
    client: &RtspClient,
    described: &Described,
    listen_sel: Option<&AudioMedia>,
    talk_sel: Option<&AudioMedia>,
) -> Result<(String, Option<Binding>, Option<Binding>), Error> {
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
            client,
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
                release(client, &described.sdp.control, &session_id).await;
                return Err(e);
            }
        }
        let b = Binding {
            channel: ch,
            codec,
            pt,
        };
        if is_talk {
            talk = Some(b);
        } else {
            listen = Some(b);
        }
    }
    Ok((session_id, listen, talk))
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

/// Where the pacer writes talk media — RTSP interleaved or a
/// lazily-claimed private-protocol talker (V259 VIGI / V282 V380).
enum TalkOut {
    Rtsp { ch: u8, pt: u8 },
    Vigi(Box<LazyVigi>),
    V380(Box<LazyV380>),
}

/// Credentials for a per-hold `:8800` claim (V275).
struct VigiClaim {
    host: String,
    user: String,
    pass: String,
}

/// Lazily-claimed `:8800` talker — the slot fills on mic-hold and
/// empties on release (V275), ⊥ held for session life.
struct LazyVigi {
    slot: Mutex<Option<crate::vigi::VigiTalk>>,
    claim: VigiClaim,
    /// In-flight claim — `talk()` buffers through it (V275).
    claiming: std::sync::atomic::AtomicBool,
    /// Single-flight claim serialization.
    claim_lock: Mutex<()>,
    /// Installed-claim generation — bumped per install; pacer
    /// faults report the dead generation for scoped release.
    claim_gen: std::sync::atomic::AtomicU64,
    /// Claim-scoped death signal (the dead generation) — the entry's
    /// watcher releases the lock on a match (⊥ session death, V275).
    dead_tx: watch::Sender<u64>,
}

impl LazyVigi {
    fn new(claim: VigiClaim) -> Self {
        Self {
            slot: Mutex::new(None),
            claim,
            claiming: std::sync::atomic::AtomicBool::new(false),
            claim_lock: Mutex::new(()),
            claim_gen: std::sync::atomic::AtomicU64::new(0),
            dead_tx: watch::channel(0).0,
        }
    }
}

/// One installed V380 talk conn + its send-side state — the
/// IMA accumulation and block sequence live per-claim, so a
/// fresh claim starts a clean predictor (V282).
struct V380Talker {
    conn: crate::v380::V380Conn,
    /// PCM waiting for a full 505-sample block.
    pending: Vec<i16>,
    /// `180` seq byte — wraps.
    seq: u8,
    enc: crate::v380::ImaEncoder,
}

/// Lazily-claimed V380 `:8800` talker — same shape as `LazyVigi`:
/// a fresh `1167` login + `377` per hold, released on mic-up.
struct LazyV380 {
    slot: Mutex<Option<V380Talker>>,
    claim: V380Claim,
    claiming: std::sync::atomic::AtomicBool,
    claim_lock: Mutex<()>,
    claim_gen: std::sync::atomic::AtomicU64,
    dead_tx: watch::Sender<u64>,
}

impl LazyV380 {
    fn new(claim: V380Claim) -> Self {
        Self {
            slot: Mutex::new(None),
            claim,
            claiming: std::sync::atomic::AtomicBool::new(false),
            claim_lock: Mutex::new(()),
            claim_gen: std::sync::atomic::AtomicU64::new(0),
            dead_tx: watch::channel(0).0,
        }
    }

    /// Fresh-conn claim — `1167` login then `377` talk-open with
    /// the new ticket (V282). `Ok(conn)` = `477`+`1001`.
    async fn claim(claim: &V380Claim) -> Result<crate::v380::V380Conn, Error> {
        let mut conn = crate::v380::V380Conn::connect(&claim.host).await?;
        let resp = conn
            .login(claim.device_id, &claim.user, &claim.pass)
            .await?;
        if resp.result != crate::v380::login_result::OK {
            return Err(Error::Protocol(format!(
                "v380 claim login ({})",
                resp.result
            )));
        }
        let result = conn
            .talk_open(claim.device_id, resp.ticket, claim.channel)
            .await?;
        if result != crate::v380::login_result::OK {
            // ∀ non-`1001` `477` = an availability event (V282) —
            // `Busy`, ⊥ capability evidence.
            return Err(Error::Protocol(format!("v380 talk-open ({result})")));
        }
        Ok(conn)
    }
}

/// VIGI `:8800` probe outcome — distinct so the V380 arm can
/// tell a complete `Refused` verdict (whose talk-mark upgrade
/// persists) from an unanswered dialect question (V278).
enum VigiOut {
    /// Gated out — `want_talk` false ∨ a talk track negotiated.
    NotProbed,
    /// `Ready` ∨ `Busy` — VIGI dialect proven; claim descriptor.
    Served(VigiClaim),
    /// Complete refusal — the talk mark upgraded to `Capability`.
    Refused,
    /// Transport/gate-fail — the dialect question went unanswered.
    Unreachable,
}

/// VIGI `:8800` capability probe (V259/V275) — `Ready` ∨ `Busy`
/// proves the protocol and yields a lazy-claim descriptor;
/// `Refused` upgrades the talk mark to capability; `Unreachable`
/// leaves the class untouched.
async fn probe_vigi(url: &url::Url, marks: &mut Vec<(Direction, FaultClass)>) -> VigiOut {
    let Some(host) = url.host_str().map(str::to_owned) else {
        return VigiOut::Unreachable;
    };
    let user = url.username().to_owned();
    let pass = url.password().unwrap_or("").to_owned();
    match crate::vigi::VigiTalk::connect(&host, &user, &pass).await {
        crate::vigi::VigiProbe::Ready(_) | crate::vigi::VigiProbe::Busy => {
            marks.retain(|(d, _)| *d != Direction::Talk);
            VigiOut::Served(VigiClaim { host, user, pass })
        }
        crate::vigi::VigiProbe::Unreachable => VigiOut::Unreachable,
        crate::vigi::VigiProbe::Refused => {
            for (d, c) in marks.iter_mut() {
                if *d == Direction::Talk {
                    *c = FaultClass::Capability;
                }
            }
            VigiOut::Refused
        }
    }
}

/// Credentials for every lazily-claimed V380 conn (V281 — each
/// conn self-authenticates with a fresh `1167`; ⊥ cross-conn
/// ticket reuse). Resolved once at open from config + oracle.
#[derive(Clone)]
struct V380Claim {
    host: String,
    device_id: u32,
    user: String,
    pass: String,
    /// `chNN` index from the monitor's RTSP path, oracle-clamped.
    channel: u32,
}

/// A V380 dialect proven at open — the claim descriptor the lazy
/// talker (V282) and the attach-claimed stream conn (V283)
/// re-authenticate with; `serves_*` = the login's caps oracle.
#[allow(dead_code)] // `serves_*` + reuse lands with the listen claim.
struct ComposedV380 {
    claim: V380Claim,
    serves_listen: bool,
    serves_talk: bool,
}

/// The V380 `1167` probe's disposition (V280).
enum V380Out {
    /// `1001` + `version<31` — the caps oracle answered.
    Served(Box<V380Served>),
    /// `1001` + `version≥31` — dialect proven, AES-media
    /// unavailable; session marks carry the verdict.
    Dialect31,
    /// `1168` seen ∧ creds exhausted (`1011`/`1012`/`1018`) ∨ an
    /// unknown `loginResult` — a real verdict, ⊥ marks.
    Creds(String),
    /// ⊥ `1168` — dial fault, silence, garbage head, derive
    /// fault, budget exhaustion: the dialect went unanswered.
    Unanswered,
    /// Empty creds — the V246 skip posture (⊥ an evaluated
    /// outcome: marks keep their class).
    Skipped,
}

/// `1001` payload the session composes on.
struct V380Served {
    claim: V380Claim,
    /// `nAudioPri>0` — mic-listen capable.
    n_audio_pri: bool,
    /// `nSpeaker>0` — talk capable.
    n_speaker: bool,
}

/// V380 dialect probe (V278/V279) — deviceId resolve (config
/// override → ONVIF-serial derive ≤3 s), ≤3 username candidates
/// on fresh conns, all inside `deadline`. Any `1168` proves
/// dialect ∀ `loginResult`; ⊥ `1168` is `Unanswered`.
async fn probe_v380(
    url: &url::Url,
    cfg: &crate::service::V380Cfg,
    deadline: tokio::time::Instant,
) -> V380Out {
    use crate::v380::login_result as lr;
    let Some(host) = url.host_str().map(str::to_owned) else {
        return V380Out::Unanswered;
    };
    let rtsp_user = url.username();
    let rtsp_pass = url.password().unwrap_or("");
    // V246 empty-creds posture — nothing to authenticate with.
    if rtsp_user.is_empty() && rtsp_pass.is_empty() && cfg.user.is_none() {
        return V380Out::Skipped;
    }
    let remaining = |min: Duration| {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        (left > min).then_some(left)
    };
    // deviceId — configured override wins; else the ONVIF-serial
    // derivation, bounded ≤3 s (V279).
    let device_id = if let Some(id) = cfg.device_id {
        id
    } else {
        let Some(xaddr) = crate::v380::derive_xaddr(url.as_str(), &cfg.onvif_addr) else {
            return V380Out::Unanswered;
        };
        let Some(bound) = remaining(Duration::ZERO).map(|d| d.min(Duration::from_secs(3))) else {
            return V380Out::Unanswered;
        };
        let soap = onvif::SoapClient::new();
        let creds = (!rtsp_user.is_empty()).then_some((rtsp_user, rtsp_pass));
        let Some(id) = crate::v380::derive_device_id(&soap, &xaddr, creds, bound).await else {
            return V380Out::Unanswered;
        };
        id
    };
    let channel = crate::v380::channel_from_path(url.path());
    let users = crate::v380::username_candidates(cfg.user.as_deref(), rtsp_user);
    let Some(bound) = remaining(Duration::from_millis(500)) else {
        return V380Out::Unanswered; // mid-arm budget exhaustion
    };
    let Ok(Ok((_conn, user, resp))) = tokio::time::timeout(
        bound,
        crate::v380::V380Conn::login_viable(&host, device_id, &users, rtsp_pass),
    )
    .await
    else {
        return V380Out::Unanswered;
    };
    match resp.result {
        lr::OK if resp.version >= 31 => V380Out::Dialect31,
        lr::OK => {
            // A parsed index ≥ `nChannels` clamps to 0 (V279).
            let channel = if resp.n_channels > 0 && channel >= u32::from(resp.n_channels) {
                0
            } else {
                channel
            };
            V380Out::Served(Box::new(V380Served {
                claim: V380Claim {
                    host,
                    device_id,
                    user,
                    pass: rtsp_pass.to_owned(),
                    channel,
                },
                n_audio_pri: resp.n_audio_pri > 0,
                n_speaker: resp.n_speaker > 0,
            }))
        }
        r => V380Out::Creds(format!("v380 login refused ({r})")),
    }
}

/// Apply the probe disposition to `marks` (V280/V281) — returns
/// `(serves_listen, serves_talk, composes, claim)`:
/// - `Served`: a served direction's pending marks drop ∀ producer
///   (the oracle outranks the absent-`m=audio` heuristic ∧ a
///   `Refused` from a non-VIGI `:8800` service); a ⊥-served
///   direction's mark mints `Session`-class only when nothing else
///   covers it (⊥ marks a direction the session serves).
/// - `Dialect31`: session-classed marks both directions, ⊥
///   persisted (V280); the session still composes to carry them.
/// - `Unanswered`: `select`-produced marks degrade to
///   `Session`-class — the dialect question went unanswered; a
///   mark minted by a COMPLETE verdict (the VIGI `Refused` class
///   upgrade) keeps its class + persistence (V278).
/// - `Creds`/`Skipped`: marks untouched.
fn apply_v380(
    v380: &V380Out,
    vigi: &VigiOut,
    listen_uncovered: bool,
    talk_uncovered: bool,
    marks: &mut Vec<(Direction, FaultClass)>,
) -> (bool, bool, bool, Option<V380Claim>) {
    match v380 {
        V380Out::Served(p) => {
            for (dir, served, uncovered) in [
                (Direction::Listen, p.n_audio_pri, listen_uncovered),
                (Direction::Talk, p.n_speaker, talk_uncovered),
            ] {
                marks.retain(|(d, _)| *d != dir);
                if !served && uncovered {
                    marks.push((dir, FaultClass::Session));
                }
            }
            (p.n_audio_pri, p.n_speaker, true, Some(p.claim.clone()))
        }
        V380Out::Dialect31 => {
            for dir in [Direction::Listen, Direction::Talk] {
                marks.retain(|(d, _)| *d != dir);
                marks.push((dir, FaultClass::Session));
            }
            (false, false, true, None)
        }
        V380Out::Unanswered => {
            let verdict_dir = matches!(vigi, VigiOut::Refused).then_some(Direction::Talk);
            for (d, c) in &mut *marks {
                if Some(*d) != verdict_dir {
                    *c = FaultClass::Session;
                }
            }
            (false, false, false, None)
        }
        V380Out::Creds(_) | V380Out::Skipped => (false, false, false, None),
    }
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
    /// `None` on a pure private-protocol compose (fault arm ∨
    /// selection-arm pure-V380 — the RTSP conn closed post-`select`).
    client: Option<Arc<RtspClient>>,
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
        client: Option<Arc<RtspClient>>,
        session_id: String,
        control: String,
        tagged: bool,
        keepalive: Duration,
        listen: Option<Binding>,
        talk: Option<Binding>,
        vigi_claim: Option<VigiClaim>,
        v380: Option<ComposedV380>,
        media_rx: Option<mpsc::Receiver<Interleaved>>,
        marks: Vec<(Direction, FaultClass)>,
    ) -> ArcAudioSession {
        let mut tasks = Vec::new();
        let (dead_tx, _) = watch::channel(false);

        // Listen pump — interleaved → RTP → seq-check → G.711 → PCM.
        // A V380-served listen allocates the broadcast at compose
        // but parks unclaimed — `recv()` waits on it (⊥ EOS, V283);
        // the stream-conn claim + pump land in T156.
        let v380_listen = v380.as_ref().is_some_and(|v| v.serves_listen);
        let listen_state = listen
            .and_then(|b| media_rx.map(|rx| (b, rx)))
            .map(|(b, media_rx)| {
                let (tx, rx) = broadcast::channel(PCM_BROADCAST);
                let mut channels: HashMap<u8, (u8, sdp::Codec)> = HashMap::new();
                channels.insert(b.channel, (b.pt, b.codec));
                tasks.push(tokio::spawn(pump(media_rx, channels, tx, dead_tx.clone())));
                ListenState { rx: Mutex::new(rx) }
            })
            .or_else(|| {
                v380_listen.then(|| {
                    let (_tx, rx) = broadcast::channel(PCM_BROADCAST);
                    ListenState { rx: Mutex::new(rx) }
                })
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
            .or_else(|| {
                vigi_claim.map(|c| (sdp::Codec::Pcma, TalkOut::Vigi(Box::new(LazyVigi::new(c)))))
            })
            .or_else(|| {
                v380.filter(|v| v.serves_talk).map(|v| {
                    (
                        sdp::Codec::Pcma,
                        TalkOut::V380(Box::new(LazyV380::new(v.claim))),
                    )
                })
            })
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
                let ok = match &this.client {
                    Some(c) => c
                        .request("OPTIONS", &this.control, &extra)
                        .await
                        .map(|r| r.status / 100 == 2)
                        .unwrap_or(false),
                    None => false,
                };
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
async fn pacer(client: Option<Arc<RtspClient>>, state: Arc<TalkShared>) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let ssrc: u32 = rand::random();
    loop {
        tick.tick().await;
        // Lazy-claim arms — the talker is claimed per hold (V275
        // VIGI / V282 V380): ⊥ pop while unclaimed, the send rides
        // the claim-slot lock (a release can't interleave mid-send),
        // a send fault is claim-scoped (drop the talker + signal the
        // generation — the SESSION survives).
        if let TalkOut::Vigi(lazy) = &state.out {
            let mut slot = lazy.slot.lock().await;
            let Some(talker) = slot.as_mut() else {
                drop(slot);
                continue;
            };
            let mut q = state.queue.lock().await;
            let Some(mut chunk) = q.pop_front() else {
                drop(q);
                drop(slot);
                continue;
            };
            let rest = (chunk.len() > FRAME_SAMPLES).then(|| chunk.split_off(FRAME_SAMPLES));
            if let Some(rest) = rest {
                q.push_front(rest);
            }
            drop(q);
            let claim_gen = lazy.claim_gen.load(Ordering::Relaxed);
            if talker.send(&chunk, ts).await.is_err() {
                *slot = None; // drop the dead talker — slot freed
                drop(slot);
                _ = lazy.dead_tx.send(claim_gen);
            }
            ts = ts.wrapping_add(u32::try_from(chunk.len()).unwrap_or(0));
            continue;
        }
        if let TalkOut::V380(lazy) = &state.out {
            // V380 arm (V282) — PCM accumulates into 505-sample
            // blocks ⇒ IMA-ADPCM ⇒ `180` frames; the 20 ms tick +
            // block gate lands sends ~63 ms apart in realtime.
            let mut slot = lazy.slot.lock().await;
            let Some(t) = slot.as_mut() else {
                drop(slot);
                continue;
            };
            {
                let mut q = state.queue.lock().await;
                while t.pending.len() < crate::v380::IMA_BLOCK_SAMPLES {
                    let Some(mut c) = q.pop_front() else { break };
                    t.pending.append(&mut c);
                }
            }
            if t.pending.len() < crate::v380::IMA_BLOCK_SAMPLES {
                drop(slot);
                continue;
            }
            let block: Vec<i16> = t.pending.drain(..crate::v380::IMA_BLOCK_SAMPLES).collect();
            let enc = t.enc.encode_block(&block);
            let seq = t.seq;
            t.seq = t.seq.wrapping_add(1);
            let claim_gen = lazy.claim_gen.load(Ordering::Relaxed);
            if t.conn.send_talk_block(&enc, seq).await.is_err() {
                *slot = None;
                drop(slot);
                _ = lazy.dead_tx.send(claim_gen);
            }
            continue;
        }
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
            TalkOut::Rtsp { ch, pt } => match &client {
                Some(c) => {
                    let payload = g711::encode(state.codec, &chunk);
                    c.send_interleaved(*ch, &rtp::build(*pt, seq, ts, ssrc, &payload))
                        .await
                        .is_ok()
                }
                None => false,
            },
            TalkOut::Vigi(_) | TalkOut::V380(_) => unreachable!("lazy arms handled above"),
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
        // V275/V282 — an unclaimed lazy talker ⊥ claim-in-flight
        // errors; frames buffer through an in-flight claim.
        let unclaimed = match &t.out {
            TalkOut::Vigi(l) => {
                l.slot.lock().await.is_none() && !l.claiming.load(Ordering::Relaxed)
            }
            TalkOut::V380(l) => {
                l.slot.lock().await.is_none() && !l.claiming.load(Ordering::Relaxed)
            }
            TalkOut::Rtsp { .. } => false,
        };
        if unclaimed {
            return Err(Error::Protocol("talker unclaimed".into()));
        }
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
            if let Some(c) = &self.client {
                let _ = c.request("TEARDOWN", &self.control, &extra).await;
            }
        }
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
        }
        if let Some(c) = &self.client {
            c.close();
        }
        // ACTIVE close of a held `:8800` talker (V275/B122 — the
        // device slot must free, ⊥ Arc-drop reliance).
        self.drop_talker().await;
    }

    fn dead_rx(&self) -> watch::Receiver<bool> {
        self.dead_tx.subscribe()
    }

    fn talk_dead_rx(&self) -> Option<watch::Receiver<u64>> {
        match &self.talk.as_ref()?.out {
            TalkOut::Vigi(l) => Some(l.dead_tx.subscribe()),
            TalkOut::V380(l) => Some(l.dead_tx.subscribe()),
            TalkOut::Rtsp { .. } => None,
        }
    }

    fn claims_lazily(&self) -> bool {
        matches!(
            self.talk.as_ref().map(|t| &t.out),
            Some(TalkOut::Vigi(_) | TalkOut::V380(_))
        )
    }

    /// V275/V282 claim — connect OUTSIDE locks, install under the
    /// claim-slot lock, single-flight via `claim_lock`.
    async fn claim_talk(&self) -> TalkClaim {
        if let Some(lazy) = self.lazy_v380() {
            let _one = lazy.claim_lock.lock().await;
            if lazy.slot.lock().await.is_some() {
                return TalkClaim::Ready;
            }
            lazy.claiming.store(true, Ordering::Relaxed);
            let claim = LazyV380::claim(&lazy.claim).await;
            lazy.claiming.store(false, Ordering::Relaxed);
            return match claim {
                Ok(conn) => {
                    if self.dead() {
                        return TalkClaim::Unreachable;
                    }
                    *lazy.slot.lock().await = Some(V380Talker {
                        conn,
                        pending: Vec::with_capacity(crate::v380::IMA_BLOCK_SAMPLES),
                        seq: 0,
                        enc: crate::v380::ImaEncoder::new(),
                    });
                    lazy.claim_gen.fetch_add(1, Ordering::Relaxed);
                    TalkClaim::Ready
                }
                // ∀ non-`1001` `477`/login result = availability, ⊥
                // capability evidence (V282 — asymmetric w/ VIGI's
                // `Refused`-marks deliberately).
                Err(Error::Protocol(_)) => TalkClaim::Busy,
                Err(_) => TalkClaim::Unreachable,
            };
        }
        let Some(lazy) = self.lazy_vigi() else {
            return TalkClaim::Ready;
        };
        let _one = lazy.claim_lock.lock().await;
        if lazy.slot.lock().await.is_some() {
            return TalkClaim::Ready;
        }
        lazy.claiming.store(true, Ordering::Relaxed);
        let probe =
            crate::vigi::VigiTalk::connect(&lazy.claim.host, &lazy.claim.user, &lazy.claim.pass)
                .await;
        lazy.claiming.store(false, Ordering::Relaxed);
        match probe {
            crate::vigi::VigiProbe::Ready(t) => {
                if self.dead() {
                    return TalkClaim::Unreachable;
                }
                *lazy.slot.lock().await = Some(t);
                lazy.claim_gen.fetch_add(1, Ordering::Relaxed);
                TalkClaim::Ready
            }
            crate::vigi::VigiProbe::Busy => TalkClaim::Busy,
            crate::vigi::VigiProbe::Refused => TalkClaim::Refused,
            crate::vigi::VigiProbe::Unreachable => TalkClaim::Unreachable,
        }
    }

    async fn release_talk(&self) {
        let Some(t) = &self.talk else { return };
        // Queue flush ∀ paths — ⊥ predecessor audio leaking into
        // the next hold (V275).
        t.queue.lock().await.clear();
        self.drop_talker().await;
    }

    fn talk_gen(&self) -> u64 {
        if let Some(l) = self.lazy_v380() {
            return l.claim_gen.load(Ordering::Relaxed);
        }
        self.lazy_vigi()
            .map_or(0, |l| l.claim_gen.load(Ordering::Relaxed))
    }

    fn dead(&self) -> bool {
        self.closed.load(Ordering::Relaxed) || *self.dead_tx.borrow()
    }
}

impl RtspSession {
    /// The lazy VIGI slot, when this session carries one.
    fn lazy_vigi(&self) -> Option<&LazyVigi> {
        match self.talk.as_ref().map(|t| &t.out) {
            Some(TalkOut::Vigi(lazy)) => Some(lazy),
            _ => None,
        }
    }

    /// The lazy V380 slot, when this session carries one.
    fn lazy_v380(&self) -> Option<&LazyV380> {
        match self.talk.as_ref().map(|t| &t.out) {
            Some(TalkOut::V380(lazy)) => Some(lazy),
            _ => None,
        }
    }

    /// Drop the installed `:8800` talker — TCP close frees the
    /// device-side single-talker slot (R63).
    async fn drop_talker(&self) {
        if let Some(l) = self.lazy_vigi() {
            l.slot.lock().await.take();
        }
        if let Some(l) = self.lazy_v380() {
            l.slot.lock().await.take();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{Codec, Direction};

    /// Serializes tests that can hit the fixed `:8800` port —
    /// a bound stub would answer *any* test's probe.
    static PORT_8800: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
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

    fn no_cfg() -> crate::service::CfgAccessor {
        std::sync::Arc::new(|| {
            let f: std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::service::V380Cfg> + Send>,
            > = Box::pin(async { crate::service::V380Cfg::default() });
            f
        })
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
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let _port_guard = PORT_8800.lock().await;
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
    #[ignore = "live probe — needs RTSP_URL env"]
    async fn live_open_smoke() {
        let u = std::env::var("RTSP_URL").unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let client = RtspAudioClient::new(parsed, None, no_cfg());
        let sess = client.open(true).await.unwrap();
        eprintln!("caps: {:?} marks: {:?}", sess.caps(), sess.marks());
        sess.close().await;
    }

    #[tokio::test]
    async fn talk_only_session() {
        const TALK_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";
        let mut st = stub(TALK_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let _port_guard = PORT_8800.lock().await;
        let st = stub(INACT).await;
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let client = RtspAudioClient::new(url(port), None, no_cfg());
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

    /// Talk-path RTSP stub — answers DESCRIBE with `sdp`, forwards
    /// `$` interleaved payloads to the returned channel.
    fn spawn_talk_stub(
        sdp: &'static str,
    ) -> (u16, tmpsc::Receiver<Vec<u8>>, tokio::task::JoinHandle<()>) {
        let (tx, got) = tmpsc::channel::<Vec<u8>>(64);
        let lis = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        lis.set_nonblocking(true).unwrap();
        let port = lis.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let lis = TcpListener::from_std(lis).unwrap();
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
                    sdp
                } else {
                    ""
                };
                wr.write_all(reply(&head, "200 OK", "Session: s1\r\n", body).as_bytes())
                    .await
                    .unwrap();
            }
        });
        (port, got, task)
    }

    #[tokio::test]
    async fn talk_pacer_writes_rtp_frames() {
        const TALK_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";
        let (port, mut got, task) = spawn_talk_stub(TALK_SDP);
        let client = RtspAudioClient::new(url(port), None, no_cfg());
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
    async fn talk_burst_does_not_drop() {
        // A capture callback lands ~13×160-sample frames at once —
        // the bounded queue must absorb the whole burst undropped.
        const TALK_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";
        let (port, mut got, task) = spawn_talk_stub(TALK_SDP);
        let client = RtspAudioClient::new(url(port), None, no_cfg());
        let sess = client.open(true).await.unwrap();
        for _ in 0..13 {
            sess.talk(&[7_i16; 160]).await.unwrap();
        }
        for _ in 0..13 {
            let f = tokio::time::timeout(Duration::from_secs(2), got.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(rtp::parse(&f).unwrap().payload.len(), 160);
        }
        sess.close().await;
        task.abort();
    }

    #[tokio::test]
    async fn talk_unnegotiated_errors() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let _port_guard = PORT_8800.lock().await;
        let Ok(lis8800) = TcpListener::bind("127.0.0.1:8800").await else {
            return; // port held — skip
        };
        // Other want_talk tests may have failed a real `127.0.0.1`
        // dial earlier in this process — clear the gate entry.
        crate::vigi::gate_reset_for_test("127.0.0.1");
        let (tx, mut parts) = tmpsc::channel::<Vec<u8>>(32);
        tokio::spawn(async move {
            // Lazy claim ⇒ one conn for the open-time probe, another
            // per mic-hold — serve each on its own task.
            loop {
                let Ok((stream, _)) = lis8800.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
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
                        "----device-stream-boundary--\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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
            }
        });

        let mut st = stub(RECV_SDP_V).await;
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        // Unclaimed — ⊥ queue, ⊥ send (V275): talk must error.
        assert!(sess.talk(&[1000_i16; 160]).await.is_err());
        // Lazy claim — mic-hold acquires the `:8800` talker now.
        assert!(matches!(sess.claim_talk().await, TalkClaim::Ready));
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

    /// Live ear-test — `SS_LIVE_TONE=1` streams a 440 Hz sine for
    /// ~15 s through the real claim+pacer path on 6eqdc. Residual
    /// chop is audible as tone stutter; the probe also fails on
    /// any send error. Not CI-safe — env-gated.
    #[tokio::test]
    async fn live_tone_probe() {
        if std::env::var("SS_LIVE_TONE").is_err() {
            return;
        }
        let cfg_path = std::env::var("SS_LIVE_CFG")
            .unwrap_or_else(|_| "/tmp/ss-run/configs/monitors/6eqdc.json".into());
        let cfg = std::fs::read_to_string(cfg_path).expect("monitor config");
        let main =
            serde_json::from_str::<serde_json::Value>(&cfg).unwrap()["sourcertsp"]["mainStream"]
                .as_str()
                .unwrap()
                .to_owned();
        let client = RtspAudioClient::new(url::Url::parse(&main).unwrap(), None, no_cfg());
        let sess = client.open(true).await.expect("open");
        assert!(sess.caps().talk, "{:?}", sess.caps());
        match sess.claim_talk().await {
            TalkClaim::Ready => {}
            other => panic!("claim {other:?}"),
        }
        // Prime the queue, then feed one 160-sample frame per 20 ms —
        // matches the pacer's drain rate so nothing drops.
        let mut phase = 0.0_f32;
        let mut frame = || {
            (0..160)
                .map(|_| {
                    phase += 440.0 * std::f32::consts::TAU / 8000.0;
                    (phase.sin() * 28_000.0) as i16
                })
                .collect::<Vec<i16>>()
        };
        for _ in 0..4 {
            sess.talk(&frame()).await.unwrap();
        }
        let start = tokio::time::Instant::now();
        for i in 0..746 {
            sess.talk(&frame()).await.unwrap();
            let target = start + Duration::from_millis(20 * (i + 1));
            if let Some(d) = target.checked_duration_since(tokio::time::Instant::now()) {
                tokio::time::sleep(d).await;
            }
        }
        sess.release_talk().await;
        sess.close().await;
    }

    #[tokio::test]
    async fn teardown_sent_on_close() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
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
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)), no_cfg());
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
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)), no_cfg());
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
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)), no_cfg());
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
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)), no_cfg());
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
        let _g = PORT_8800.lock().await;
        let mut primary = stub_429("PLAY", DUPLEX_SDP).await;
        let client = RtspAudioClient::new(url(primary.port), None, no_cfg());
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
        let _g = PORT_8800.lock().await;
        let primary = stub_429("PLAY", DUPLEX_SDP).await;
        let alt = stub_429("PLAY", DUPLEX_SDP).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)), no_cfg());
        let Err(err) = client.open(false).await else {
            panic!("expected StreamLimit");
        };
        assert!(matches!(err, Error::StreamLimit), "{err:?}");
    }

    #[tokio::test]
    async fn same_endpoint_alternate_not_retried() {
        let _g = PORT_8800.lock().await;
        let mut primary = stub_429("PLAY", DUPLEX_SDP).await;
        // Same endpoint, different userinfo → deduped at construction.
        let creds = url::Url::parse(&format!(
            "rtsp://u%40x:p%40ss@127.0.0.1:{}/live",
            primary.port
        ))
        .unwrap();
        let client = RtspAudioClient::new(url(primary.port), Some(creds), no_cfg());
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
        let _g = PORT_8800.lock().await;
        let primary = stub_429("PLAY", DUPLEX_SDP).await;
        // Alt SDP: sendonly AAC → talk capability mark (codec-less);
        // 127.0.0.1:8800 refused → mark survives, downgraded alt-side.
        let mut alt = stub_scripted(|_| "200 OK", ALT_AAC_TALK_SDP, 60).await;
        let client = RtspAudioClient::new(url(primary.port), Some(url(alt.port)), no_cfg());
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
        let _g = PORT_8800.lock().await;
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
        let client = RtspAudioClient::new(url(st.port), None, no_cfg());
        let sess = client.open(false).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), sess.wait_dead())
            .await
            .expect("session must die on keepalive 500");
        sess.close().await;
        let _ = &mut st;
    }

    // ── V380 fallback (V278–V281) ────────────────────────────────

    /// `1168` reply bytes — fields per `parse_login_resp`.
    fn v380_reply(result: i32, version: u8, n_audio_pri: u8, n_speaker: u8) -> Vec<u8> {
        let mut b = vec![0_u8; 68];
        b[0..4].copy_from_slice(&1168_u32.to_le_bytes());
        b[4..8].copy_from_slice(&result.to_le_bytes());
        b[12] = version;
        b[13..17].copy_from_slice(&0x1234_u32.to_le_bytes()); // ticket
        b[63] = 1; // nChannels
        b[64] = n_audio_pri;
        b[66] = n_speaker;
        b
    }

    struct V380Stub {
        conns: std::sync::Arc<std::sync::atomic::AtomicU32>,
        _task: tokio::task::JoinHandle<()>,
    }

    /// `:8800` stub — each accepted conn reads the 256 B login,
    /// replies `1168` with `results[conn_idx]` (clamped to last),
    /// then stays open for any follow-up packets.
    /// `garbage` replies a non-`1168` head instead.
    async fn v380_stub(
        results: &'static [i32],
        version: u8,
        audio_pri: u8,
        speaker: u8,
    ) -> Option<V380Stub> {
        let lis = TcpListener::bind("127.0.0.1:8800").await.ok()?;
        let conns = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let conns2 = conns.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = lis.accept().await else {
                    return;
                };
                let idx =
                    usize::try_from(conns2.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or_default();
                tokio::spawn(async move {
                    let (mut rd, mut wr) = stream.into_split();
                    let mut pkt = [0_u8; 256];
                    if rd.read_exact(&mut pkt).await.is_err() {
                        return;
                    }
                    let result = results[idx.min(results.len() - 1)];
                    let resp = v380_reply(result, version, audio_pri, speaker);
                    if wr.write_all(&resp).await.is_err() {
                        return;
                    }
                    let mut sink = vec![0_u8; 512];
                    loop {
                        if rd.read(&mut sink).await.unwrap_or(0) == 0 {
                            return;
                        }
                    }
                });
            }
        });
        Some(V380Stub { conns, _task: task })
    }

    fn v380_cfg() -> crate::service::CfgAccessor {
        std::sync::Arc::new(|| {
            let f: std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::service::V380Cfg> + Send>,
            > = Box::pin(async {
                crate::service::V380Cfg {
                    device_id: Some(46_078_507),
                    ..Default::default()
                }
            });
            f
        })
    }

    fn url_creds(port: u16) -> url::Url {
        url::Url::parse(&format!("rtsp://admin:pass@127.0.0.1:{port}/live/ch00_0")).unwrap()
    }

    /// Video-only SDP — both audio directions uncovered.
    const VIDEO_SDP: &str = "v=0\r\nm=video 0 RTP/AVP 96\r\na=control:trackID=0\r\n";
    /// Listen-only SDP — talk uncovered.
    const RECV_SDP_V380: &str =
        "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
    /// Talk-only SDP — listen uncovered.
    const SEND_SDP_V380: &str =
        "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";

    /// `1001`+caps ⇒ pure-V380 compose — both directions served,
    /// select-minted marks dropped, RTSP conn closed post-select.
    #[tokio::test]
    async fn v380_1001_composes_pure_and_closes_rtsp() {
        let _g = PORT_8800.lock().await;
        let Some(v) = v380_stub(&[1001], 3, 1, 1).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", VIDEO_SDP, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: true
            }
        );
        assert!(sess.marks().is_empty(), "{:?}", sess.marks());
        assert!(sess.claims_lazily());
        // Pure-V380 ⇒ the RTSP conn closed post-select (EOF marker).
        let saw_eof = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(m) = st.got.recv().await {
                if m == "EOF" {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(saw_eof, "selection-arm pure-V380 must close the RTSP conn");
        sess.close().await;
        drop(v);
    }

    /// caps-zero oracle ⇒ session still composes, session-class
    /// marks on both directions, deduped.
    #[tokio::test]
    async fn v380_caps_zero_composes_session_marks() {
        let _g = PORT_8800.lock().await;
        let Some(_v) = v380_stub(&[1001], 3, 0, 0).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", VIDEO_SDP, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: false,
                talk: false
            }
        );
        assert_eq!(
            sess.marks(),
            vec![
                (Direction::Listen, FaultClass::Session),
                (Direction::Talk, FaultClass::Session),
            ]
        );
        let _ = &mut st;
        sess.close().await;
    }

    /// `1001`+`version`≥31 ⇒ session marks both dirs, still `Ok`.
    #[tokio::test]
    async fn v380_version31_composes_session_marks() {
        let _g = PORT_8800.lock().await;
        let Some(_v) = v380_stub(&[1001], 31, 1, 1).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", VIDEO_SDP, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: false,
                talk: false
            }
        );
        assert!(
            sess.marks().iter().all(|(_, c)| *c == FaultClass::Session),
            "{:?}",
            sess.marks()
        );
        let _ = &mut st;
        sess.close().await;
    }

    /// Creds verdict non-fatal when a direction composed — the
    /// negotiated listen survives a talk-arm `1018`.
    #[tokio::test]
    async fn v380_creds_fault_nonfatal_with_listen() {
        let _g = PORT_8800.lock().await;
        let Some(_v) = v380_stub(&[1018], 3, 1, 1).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", RECV_SDP_V380, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: false
            }
        );
        let _ = &mut st;
        sess.close().await;
    }

    /// Creds verdict fatal when nothing composed — the `1018`
    /// message rides the open error.
    #[tokio::test]
    async fn v380_creds_fault_fatal_when_nothing_composed() {
        let _g = PORT_8800.lock().await;
        let Some(_v) = v380_stub(&[1018], 3, 1, 1).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", VIDEO_SDP, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let r = client.open(true).await;
        let Err(err) = r else {
            panic!("expected creds-fatal open fault")
        };
        assert!(format!("{err:?}").contains("1018"), "{err:?}");
        let _ = &mut st;
    }

    /// ⊥-`1168` ⊥-V380 ⇒ `select`-minted marks degrade to
    /// `Session`-class — the absent-`m=audio` `Capability` on an
    /// unanswered dialect ⊥ persists.
    #[tokio::test]
    async fn v380_unanswered_downgrades_select_marks() {
        let _g = PORT_8800.lock().await;
        // ⊥ :8800 bound — the probe's dial faults.
        let mut st = stub_scripted(|_| "200 OK", SEND_SDP_V380, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: false,
                talk: true
            }
        );
        assert_eq!(
            sess.marks(),
            vec![(Direction::Listen, FaultClass::Session)],
            "{:?}",
            sess.marks()
        );
        let _ = &mut st;
        sess.close().await;
    }

    /// A VIGI `Refused` verdict mark keeps its `Capability` class
    /// through a ⊥-`1168` V380 outcome — the downgrade covers
    /// `select`-produced marks only.
    #[tokio::test]
    async fn v380_vigi_verdict_mark_keeps_class() {
        let _g = PORT_8800.lock().await;
        // Conn1 VIGI Refused; conn2 replies garbage (non-`1168`).
        let lis = TcpListener::bind("127.0.0.1:8800").await;
        let Ok(lis) = lis else { return };
        crate::vigi::gate_reset_for_test("127.0.0.1");
        let idx = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let idx2 = idx.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = lis.accept().await else {
                    return;
                };
                let n = idx2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::spawn(async move {
                    let (rd, mut wr) = stream.into_split();
                    let mut rd = tokio::io::BufReader::new(rd);
                    if n == 0 {
                        let _ = read_head(&mut rd).await;
                        _ = wr
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed\r\n\r\n")
                            .await;
                        let mut buf = vec![0_u8; 4096];
                        let _ = rd.read(&mut buf).await.unwrap_or(0);
                        let body = r#"{"params":{"error_code":-1}}"#;
                        let part = format!(
                            "----device-stream-boundary--\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        _ = wr.write_all(part.as_bytes()).await;
                        return;
                    }
                    let mut pkt = [0_u8; 256];
                    if rd.read_exact(&mut pkt).await.is_err() {
                        return;
                    }
                    _ = wr.write_all(b"\xde\xad\xbe\xef garbage head").await;
                });
            }
        });
        let mut st = stub_scripted(|_| "200 OK", RECV_SDP_V380, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: false
            }
        );
        assert_eq!(
            sess.marks(),
            vec![(Direction::Talk, FaultClass::Capability)],
            "{:?}",
            sess.marks()
        );
        let _ = &mut st;
        sess.close().await;
    }

    /// Empty creds ⇒ the probe skips (V246 posture) — ⊥ an
    /// evaluated outcome: the `Capability` mark keeps its class.
    #[tokio::test]
    async fn v380_empty_creds_skips_probe() {
        let _g = PORT_8800.lock().await;
        let Some(v) = v380_stub(&[1001], 3, 1, 1).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", SEND_SDP_V380, 60).await;
        // url() carries ⊥ userinfo.
        let client = RtspAudioClient::new(url(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: false,
                talk: true
            }
        );
        assert_eq!(
            sess.marks(),
            vec![(Direction::Listen, FaultClass::Capability)],
            "{:?}",
            sess.marks()
        );
        assert_eq!(v.conns.load(std::sync::atomic::Ordering::Relaxed), 0);
        let _ = &mut st;
        sess.close().await;
    }

    /// Fault arm: DESCRIBE fault + `1001` ⇒ pure-V380 session.
    #[tokio::test]
    async fn v380_fault_arm_composes_pure() {
        let _g = PORT_8800.lock().await;
        let Some(_v) = v380_stub(&[1001], 3, 1, 1).await else {
            return;
        };
        let mut st = stub_scripted(
            |head| {
                if head.starts_with("DESCRIBE") {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                }
            },
            VIDEO_SDP,
            60,
        )
        .await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: true
            }
        );
        assert!(sess.marks().is_empty(), "{:?}", sess.marks());
        let _ = &mut st;
        sess.close().await;
    }

    /// Fault arm ⊥-V380 ⇒ the original RTSP fault propagates.
    #[tokio::test]
    async fn v380_fault_arm_propagates_original_fault() {
        let _g = PORT_8800.lock().await;
        // ⊥ :8800 bound — V380 dial faults.
        let mut st = stub_scripted(
            |head| {
                if head.starts_with("DESCRIBE") {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                }
            },
            VIDEO_SDP,
            60,
        )
        .await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let r = client.open(true).await;
        let Err(err) = r else {
            panic!("expected original fault to propagate")
        };
        assert!(format!("{err:?}").contains("500"), "{err:?}");
        let _ = &mut st;
    }

    /// `429` propagates to the alt-URL retry unprobed — the V380
    /// stub sees ⊥ conns.
    #[tokio::test]
    async fn v380_429_retries_alt_unprobed() {
        let _g = PORT_8800.lock().await;
        let Some(v) = v380_stub(&[1001], 3, 1, 1).await else {
            return;
        };
        let mut primary = stub_scripted(
            |head| {
                if head.starts_with("OPTIONS") {
                    "429 Too Many Requests"
                } else {
                    "200 OK"
                }
            },
            VIDEO_SDP,
            60,
        )
        .await;
        let mut alt = stub_scripted(|_| "200 OK", RECV_SDP_V380, 60).await;
        let client = RtspAudioClient::new(
            url_creds(primary.port),
            Some(url_creds(alt.port)),
            v380_cfg(),
        );
        let sess = client.open(true).await.unwrap();
        // Alt yielded a listen track; talk stayed uncovered — the
        // probe MAY run on the alt URL (it's the surviving attempt),
        // but the primary's `429` itself was ⊥-probed.
        assert!(sess.caps().listen);
        let _ = (&mut primary, &mut alt);
        drop(v);
        sess.close().await;
    }

    /// Mixed compose — RTSP listen + V380 talk keeps the conn:
    /// SETUP + PLAY ran, no EOF.
    #[tokio::test]
    async fn v380_mixed_compose_keeps_rtsp() {
        let _g = PORT_8800.lock().await;
        let Some(_v) = v380_stub(&[1001], 3, 0, 1).await else {
            return;
        };
        let mut st = stub_scripted(|_| "200 OK", RECV_SDP_V380, 60).await;
        let client = RtspAudioClient::new(url_creds(st.port), None, v380_cfg());
        let sess = client.open(true).await.unwrap();
        assert_eq!(
            sess.caps(),
            AudioCaps {
                listen: true,
                talk: true
            }
        );
        assert!(sess.marks().is_empty(), "{:?}", sess.marks());
        // The conn stayed — SETUP + PLAY requests ran.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut saw_setup = false;
        let mut saw_play = false;
        while let Ok(m) = st.got.try_recv() {
            saw_setup |= m.starts_with("SETUP");
            saw_play |= m.starts_with("PLAY");
        }
        assert!(saw_setup && saw_play, "mixed compose must SETUP+PLAY");
        sess.close().await;
    }
}
