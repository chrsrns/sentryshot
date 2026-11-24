// SPDX-License-Identifier: GPL-2.0-or-later

//! Two-way camera audio seam — a dedicated on-demand session per
//! monitor carrying camera↔client audio (ONVIF `Require:backchannel`
//! negotiated RTSP, VIGI `:8800` talk fallback). Server↔browser is
//! always raw PCM Int16 LE 8 kHz mono; camera codec is negotiated
//! per track (PCMU/PCMA only).
//!
//! The seam isolates consumers from the transport: `AudioService`
//! (handler crate) sees only `AudioClient`/`AudioSession` — a future
//! retina ≥0.4.22 `SessionOptions::backchannel` impl slots behind
//! the same traits (deferral D17).

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;

mod g711;
mod rtp;
mod rtsp;
mod sdp;
mod service;
mod session;
mod v380;
mod vigi;

pub use rtsp::{Interleaved, Response, RtspClient};
pub use sdp::{AudioMedia, Codec, PayloadMap, Sdp, SdpDirection, parse as parse_sdp};
pub use service::{
    Attach, AudioEntry, AudioService, ConsumerDrop, DIRECTIONLESS_TIMEOUT, MAX_CONSUMERS,
    TOKEN_TTL, WS_PING_CADENCE,
};
pub use session::RtspAudioClient;
pub use v380::{
    IMA_BLOCK_BYTES, IMA_BLOCK_SAMPLES, ImaEncoder, LoginResponse, Reassembler, V380Conn,
    derive_device_id, derive_xaddr, ima_decode_block, is_login_resp, login_result,
    serial_low8_to_device_id, username_candidates,
};
pub use vigi::{VigiProbe, VigiTalk};

/// Decoded PCM frame — Int16 little-endian, 8 kHz, mono.
pub type PcmChunk = Vec<i16>;

/// Audio direction — values are the wire/config mark names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Camera → browser.
    Listen,
    /// Browser → camera.
    Talk,
}

impl Direction {
    /// Wire/`unsupported`-mark name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Listen => "listen",
            Self::Talk => "talk",
        }
    }

    /// Parse a wire/mark name back; `None` on anything else.
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "listen" => Some(Self::Listen),
            "talk" => Some(Self::Talk),
            _ => None,
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What the negotiated session can do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioCaps {
    /// Session delivers camera→client audio.
    pub listen: bool,
    /// Session accepts client→camera audio.
    pub talk: bool,
}

/// Capability vs transport split — persisted `audio.unsupported`
/// marks are written for capability faults only; transport and
/// session-bound faults mark session-only (V260).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultClass {
    /// Peer answered and disproved support — persists.
    Capability,
    /// Connect timeout/RST/EOF/overrun — session-only, retried.
    Transport,
    /// Session-scoped non-persistence — e.g. an `a=inactive`
    /// track: unusable for this open but not a permanent defect.
    Session,
}

/// Failures across RTSP negotiation, media IO, and the VIGI
/// fallback. `class()` carries the persistence decision.
#[derive(Debug, Error)]
pub enum Error {
    /// A bounded operation overran its deadline.
    #[error("timeout during {0}")]
    Timeout(&'static str),

    /// Socket-level failure (connect refused/RST/EOF/write).
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),

    /// Peer response was unparseable or violated the protocol.
    #[error("protocol: {0}")]
    Protocol(String),

    /// Peer answered auth and rejected it (401 loop, VIGI auth
    /// or proto fault).
    #[error("authentication failed")]
    Auth,

    /// Camera declined for stream/session budget (`429`) —
    /// retryable on an alternate URL or later. Payload-free:
    /// the `Display` rides `error` ctrls (V258 creds rule).
    #[error("stream limit reached")]
    StreamLimit,

    /// The direction is unsupported by the device/session —
    /// absent m=audio, codec outside PCMU/PCMA, `a=inactive`,
    /// or peer-declared non-support.
    #[error("unsupported {0} direction")]
    Unsupported(Direction),
}

impl Error {
    /// Persistence class — capability faults persist
    /// `audio.unsupported` marks; transport faults are
    /// session-only.
    #[must_use]
    pub fn class(&self) -> FaultClass {
        match self {
            Self::Timeout(_) | Self::Transport(_) | Self::StreamLimit => FaultClass::Transport,
            Self::Protocol(_) | Self::Auth | Self::Unsupported(_) => FaultClass::Capability,
        }
    }
}

/// One open audio session for a monitor — owns the RTSP
/// connection (and any composed VIGI talk transport).
#[async_trait]
pub trait AudioSession: Send + Sync {
    /// Negotiated capabilities of this open session.
    fn caps(&self) -> AudioCaps;

    /// Direction marks discovered while negotiating —
    /// `(direction, class)`; `Capability` persists to
    /// `audio.unsupported`, the rest stay session-only.
    fn marks(&self) -> Vec<(Direction, FaultClass)>;

    /// Next decoded camera→client PCM chunk; `None` at EOS or
    /// session teardown. Driven by the service's broadcast task.
    async fn recv(&self) -> Option<PcmChunk>;

    /// Queue client→camera PCM (Int16 LE 8 kHz mono). Encoding +
    /// pacing are internal; an unnegotiated talk direction errors.
    async fn talk(&self, pcm: &[i16]) -> Result<(), Error>;

    /// `TEARDOWN` + close. Idempotent.
    async fn close(&self);

    /// Resolves when the session dies underneath — transport fault,
    /// reader/pump/keepalive task exit. Never resolves for a session
    /// that lives until `close`.
    async fn wait_dead(&self);

    /// Session-death receiver for the entry's watcher — cloned
    /// receivers hold ⊥ the session `Arc` (B122).
    fn dead_rx(&self) -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    /// Claim-scoped talker-death receiver (V275) — `Some` only on
    /// lazily-claimed (VIGI) sessions; carries the dead claim's
    /// generation.
    fn talk_dead_rx(&self) -> Option<watch::Receiver<u64>> {
        None
    }

    /// Whether talk rides a lazily-claimed vendor talker (V275) —
    /// `false` for negotiated sendonly sessions.
    fn claims_lazily(&self) -> bool {
        false
    }

    /// Connect + install the `:8800` talker for the current hold
    /// (V275). No-op `Ready` for non-VIGI sessions.
    async fn claim_talk(&self) -> TalkClaim {
        TalkClaim::Ready
    }

    /// Drop any claimed talker (TCP close — the device slot frees)
    /// + flush the talk queue. Every talk-lock release path (V275).
    async fn release_talk(&self) {}

    /// Current installed-claim generation — `0` when unclaimed.
    fn talk_gen(&self) -> u64 {
        0
    }

    /// Session dead flag — install-validation input (V275).
    fn dead(&self) -> bool {
        false
    }
}

/// `claim_talk` outcome — the entry maps to attach outcomes (V275).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TalkClaim {
    /// Talker installed (or no claim needed).
    Ready,
    /// Device reported another active talker (`-52405`, R63).
    Busy,
    /// Complete protocol refusal (nonzero `error_code`, auth/proto).
    Refused,
    /// Transport unreachable ∨ gate-suppressed.
    Unreachable,
}

/// Factory for monitor audio sessions — one instance per monitor
/// built from endpoint + creds by `AudioService`.
#[async_trait]
pub trait AudioClient: Send + Sync {
    /// Open a session. `want_talk` asks for sendonly negotiation
    /// (`Require:backchannel` + VIGI fallback evaluation); a `false`
    /// open never evaluates talk.
    async fn open(&self, want_talk: bool) -> Result<Arc<dyn AudioSession>, Error>;
}

/// `Arc` aliases for the seam — services hold these, never
/// concrete transport types.
pub type ArcAudioClient = Arc<dyn AudioClient>;
/// Shared session handle held by consumers + service entry.
pub type ArcAudioSession = Arc<dyn AudioSession>;
