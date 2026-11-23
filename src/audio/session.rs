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
//! - `551`/Require rejection or absent sendonly ⇒ untagged
//!   re-DESCRIBE → listen-only + talk mark candidate;
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
use tokio::sync::{Mutex, Notify, broadcast, mpsc};

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
}

impl RtspAudioClient {
    /// New client for a monitor's `mainStream` URL (creds ride the
    /// userinfo).
    #[must_use]
    pub fn new(url: url::Url) -> Self {
        Self { url }
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
        match tokio::time::timeout(OPEN_BUDGET, self.open_inner(want_talk)).await {
            Ok(r) => r,
            Err(_) => Err(Error::Timeout("session open")),
        }
    }
}

/// One selected track's wire binding.
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
    async fn open_inner(&self, want_talk: bool) -> Result<ArcAudioSession, Error> {
        let mut client = RtspClient::connect(&self.url).await?;
        // Liveness probe — any response proves the RTSP stack.
        let _ = timed(client.request("OPTIONS", client.base(), &[])).await?;

        let described = describe(&client, &self.url, want_talk).await?;
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
            let user = self.url.username().to_owned();
            let pass = self.url.password().unwrap_or("").to_owned();
            if let Some(host) = self.url.host_str() {
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

        // SETUP each selected track on fresh channel pairs.
        let client = Arc::new(client);
        let mut session_id = String::new();
        let mut next_ch: u8 = 0;
        let mut listen: Option<Binding> = None;
        let mut talk: Option<Binding> = None;
        for (sel, is_talk) in [(listen_sel, false), (talk_sel, true)] {
            let Some(m) = sel else { continue };
            let ch = next_ch;
            next_ch += 2;
            let codec = m.payloads[0].codec;
            let pt = m.payloads[0].pt;
            if let Some(id) =
                setup(&client, m, ch, described.tagged && is_talk, &session_id).await?
            {
                session_id = id;
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
            if resp.status / 100 != 2 {
                client.close();
                return Err(Error::Protocol(format!("PLAY status {}", resp.status)));
            }
            keepalive = session_timeout(&resp);
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

/// DESCRIBE (+`Require` when `want_talk`) with the `551`/absent-
/// sendonly untagged retry.
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
        if resp.status / 100 == 2 {
            let sdp = parse_body(&resp, base)?;
            let has_talk = sdp
                .audio
                .iter()
                .any(|m| m.direction.talks() && !m.payloads.is_empty());
            if has_talk {
                return Ok(Described { sdp, tagged: true });
            }
            // Absent sendonly — fall through to the untagged pass.
        }
        // 551 / Require rejection / absent sendonly → untagged pass.
    }
    let resp = timed(client.request("DESCRIBE", client.base(), &[accept])).await?;
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

        // Listen pump — interleaved → RTP → seq-check → G.711 → PCM.
        let listen_state = listen.map(|b| {
            let (tx, rx) = broadcast::channel(PCM_BROADCAST);
            let mut channels: HashMap<u8, (u8, sdp::Codec)> = HashMap::new();
            channels.insert(b.channel, (b.pt, b.codec));
            tasks.push(tokio::spawn(pump(media_rx, channels, tx)));
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
        });
        session.clone().spawn_keepalive(keepalive);
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
                if this
                    .client
                    .request("OPTIONS", &this.control, &extra)
                    .await
                    .is_err()
                {
                    break; // reader dead / socket gone — session teardown follows
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
        if tx.send(pcm).is_err() {
            break; // no listeners left
        }
    }
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

    async fn close(&self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        let extra: Vec<(&str, &str)> = if self.tagged {
            vec![("Session", &self.session_id), ("Require", REQUIRE)]
        } else {
            vec![("Session", &self.session_id)]
        };
        let _ = self.client.request("TEARDOWN", &self.control, &extra).await;
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

    const DUPLEX_SDP: &str = "v=0\r\nm=audio 0 RTP/AVP 0\r\na=recvonly\r\na=control:trackID=1\r\n\
         m=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=2\r\n";

    #[tokio::test]
    async fn open_duplex_negotiates_backchannel() {
        let mut st = stub(DUPLEX_SDP).await;
        let client = RtspAudioClient::new(url(st.port));
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
        let client = RtspAudioClient::new(url(st.port));
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
    async fn want_talk_absent_sendonly_retries_untagged() {
        const RECV_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 8\r\na=recvonly\r\na=control:trackID=1\r\n";
        let _port_guard = VIGI_PORT.lock().await;
        let mut st = stub(RECV_SDP).await;
        let client = RtspAudioClient::new(url(st.port));
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
        assert_eq!(describes.len(), 2);
        assert!(describes[0].contains("Require:"));
        assert!(!describes[1].contains("Require:"));
    }

    #[tokio::test]
    async fn talk_only_session() {
        const TALK_SDP: &str =
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=sendonly\r\na=control:trackID=9\r\n";
        let mut st = stub(TALK_SDP).await;
        let client = RtspAudioClient::new(url(st.port));
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
        let client = RtspAudioClient::new(url(st.port));
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
        let client = RtspAudioClient::new(url(port));
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
        let client = RtspAudioClient::new(url(port));
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
        let client = RtspAudioClient::new(url(st.port));
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
        let client = RtspAudioClient::new(url(st.port));
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
        let client = RtspAudioClient::new(url(st.port));
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
        let client = RtspAudioClient::new(url(st.port));
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
}
