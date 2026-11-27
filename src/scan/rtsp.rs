// SPDX-License-Identifier: GPL-2.0-or-later

//! RTSP path probing: OPTIONS fingerprint → vendor shortlist →
//! DESCRIBE classify → `retina` describe verify (externally timed).

use crate::{
    ScanError,
    types::{Creds, ScanRole},
};
use onvif::{digest_authorization, parse_digest_challenge};
use std::{net::Ipv4Addr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const OP_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_RESP: usize = 8 * 1024;

/// `(path, role-hint)` — vendor-tagged shortlist ordered common→niche.
const SHORTLIST: &[(&str, ScanRole)] = &[
    ("live", ScanRole::Extra),
    ("stream1", ScanRole::Main),
    ("stream2", ScanRole::Sub),
    ("Streaming/Channels/101", ScanRole::Main),
    ("Streaming/Channels/102", ScanRole::Sub),
    ("cam/realmonitor?channel=1&subtype=0", ScanRole::Main),
    ("cam/realmonitor?channel=1&subtype=1", ScanRole::Sub),
    ("axis-media/media.amp", ScanRole::Main),
    ("h264Preview_01_main", ScanRole::Main),
    ("h264Preview_01_sub", ScanRole::Sub),
    ("11", ScanRole::Main),
    ("12", ScanRole::Sub),
    ("onvif1", ScanRole::Main),
    ("onvif2", ScanRole::Sub),
    ("media/video1", ScanRole::Main),
    ("media/video2", ScanRole::Sub),
    ("ch1/main", ScanRole::Main),
    ("ch1/sub", ScanRole::Sub),
    ("live/ch00_0", ScanRole::Main),
    ("live/ch00_1", ScanRole::Sub),
    ("h264/ch1/main/av_stream", ScanRole::Main),
    ("h264/ch1/sub/av_stream", ScanRole::Sub),
];

/// One DESCRIBE result worth keeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHit {
    /// Path under `rtsp://ip:554/`.
    pub path: String,
    /// Role hint from the shortlist.
    pub role: ScanRole,
    /// Server demanded auth; scheme tag (`basic`/`digest`).
    pub auth: Option<String>,
    /// Video codec recorded when the candidate verified via
    /// `retina` describe; unset until then.
    pub codec: Option<String>,
}

/// Raw RTSP request → `(status, headers-text)`. ≤1.5 s each hop.
async fn request(
    stream: &mut tokio::net::TcpStream,
    req: &str,
) -> Result<(u16, String), ScanError> {
    let deadline = std::time::Instant::now() + OP_TIMEOUT;
    tokio::time::timeout(OP_TIMEOUT, stream.write_all(req.as_bytes()))
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(ScanError::Io)?;
    let mut buf = vec![0_u8; MAX_RESP];
    let n = tokio::time::timeout(
        deadline.saturating_duration_since(std::time::Instant::now()),
        stream.read(&mut buf),
    )
    .await
    .map_err(|_| ScanError::Timeout)?
    .map_err(ScanError::Io)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, text.into_owned()))
}

fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    resp.lines().find_map(|l| {
        l.split_once(':')
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    })
}

async fn connect(ip: Ipv4Addr) -> Result<tokio::net::TcpStream, ScanError> {
    tokio::time::timeout(OP_TIMEOUT, tokio::net::TcpStream::connect((ip, 554)))
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(ScanError::Io)
}

/// Reorder the shortlist when the `Server:` fingerprint matches a
/// known vendor — matching paths probe first.
#[must_use]
pub fn shortlist_for(server: Option<&str>) -> Vec<(&'static str, ScanRole)> {
    let mut v: Vec<(&'static str, ScanRole)> = SHORTLIST.to_vec();
    if let Some(s) = server.map(str::to_ascii_lowercase) {
        let tag = if s.contains("hikvision") || s.contains("hipcam") {
            "Streaming/Channels"
        } else if s.contains("dahua") || s.contains("amcrest") || s.contains("lorex") {
            "cam/realmonitor"
        } else if s.contains("reolink") {
            "h264Preview"
        } else if s.contains("axis") {
            "axis-media"
        } else {
            ""
        };
        if !tag.is_empty() {
            v.sort_by_key(|(p, _)| !p.starts_with(tag));
        }
    }
    v
}

/// `OPTIONS` once → `Server:` header fingerprint.
pub async fn fingerprint(ip: Ipv4Addr) -> Option<String> {
    let mut s = connect(ip).await.ok()?;
    let req = format!("OPTIONS rtsp://{ip}:554/ RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    let (_status, resp) = request(&mut s, &req).await.ok()?;
    header(&resp, "server").map(str::to_owned)
}

/// Probe the shortlist: `DESCRIBE` each path on its own connection,
/// classify 200/401/other; 401 retries once with user creds when a
/// challenge is offered (`digest` or `basic`).
pub async fn probe_paths(
    deadline: std::time::Instant,
    ip: Ipv4Addr,
    creds: &Creds,
    server: Option<&str>,
) -> Vec<PathHit> {
    let mut hits = Vec::new();
    for (path, role) in shortlist_for(server) {
        if deadline <= std::time::Instant::now() {
            break;
        }
        let url = format!("rtsp://{ip}:554/{path}");
        let Ok(mut s) = connect(ip).await else {
            continue;
        };
        let req = format!("DESCRIBE {url} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n");
        let Ok((status, resp)) = request(&mut s, &req).await else {
            continue;
        };
        match status {
            200 => hits.push(PathHit {
                path: (*path).to_owned(),
                role,
                auth: None,
                codec: None,
            }),
            401 => {
                let scheme = header(&resp, "www-authenticate").map(|v| {
                    if v.to_ascii_lowercase().starts_with("basic") {
                        "basic".to_owned()
                    } else {
                        "digest".to_owned()
                    }
                });
                if creds.is_empty() {
                    hits.push(PathHit {
                        path: (*path).to_owned(),
                        role,
                        auth: scheme,
                        codec: None,
                    });
                    continue;
                }
                // credentialed retry on the same connection
                let auth_hdr = match scheme.as_deref() {
                    Some("basic") => Some(format!(
                        "Basic {}",
                        base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            format!("{}:{}", creds.username, creds.password),
                        )
                    )),
                    _ => header(&resp, "www-authenticate")
                        .and_then(parse_digest_challenge)
                        .map(|c| {
                            digest_authorization(
                                &c,
                                &creds.username,
                                &creds.password,
                                &url,
                                "DESCRIBE",
                            )
                        }),
                };
                let Some(auth_hdr) = auth_hdr else {
                    continue;
                };
                let req2 = format!(
                    "DESCRIBE {url} RTSP/1.0\r\nCSeq: 3\r\nAccept: application/sdp\r\nAuthorization: {auth_hdr}\r\n\r\n"
                );
                // Verified-but-auth-required: keep the scheme tag so
                // creds embed in the emitted URL (assemble).
                if let Ok((200 | 401, _)) = request(&mut s, &req2).await {
                    hits.push(PathHit {
                        path: (*path).to_owned(),
                        role,
                        auth: scheme,
                        codec: None,
                    });
                }
            }
            _ => {}
        }
    }
    hits
}

/// Verify a candidate via `retina` DESCRIBE, externally timed — the
/// pinned client has no request timeout (spec calls this out).
///
/// On success returns the SDP video stream's `encoding_name`
/// (`"h264"`, `"h265"`, …), or `"none"` when the SDP carries no video
/// stream.
pub async fn describe_codec(
    url: url::Url,
    creds: &Creds,
    budget: Duration,
) -> Result<String, ScanError> {
    let rcreds = if creds.is_empty() {
        None
    } else {
        Some(retina::client::Credentials {
            username: creds.username.clone(),
            password: creds.password.clone(),
        })
    };
    let group = std::sync::Arc::new(retina::client::SessionGroup::default());
    let fut = retina::client::Session::describe(
        url,
        retina::client::SessionOptions::default()
            .creds(rcreds)
            .session_group(group)
            .teardown(retina::client::TeardownPolicy::Always),
    );
    let session = tokio::time::timeout(budget, fut)
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(|e| ScanError::Malformed(e.to_string()))?;
    Ok(session
        .streams()
        .iter()
        .find(|s| s.media() == "video")
        .map_or_else(|| "none".to_owned(), |s| s.encoding_name().to_owned()))
}

/// Outcome of a DESCRIBE + PLAY probe.
pub struct ProbeOutcome {
    /// SDP video `encoding_name` (`"none"` when the SDP has no video).
    pub codec: String,
    /// `Some(true)` when ≥1 RTP media packet arrived inside the PLAY
    /// window, `Some(false)` when the handshake completed yet nothing
    /// arrived — a proven-dead stream. `None` when SETUP/PLAY was
    /// protocol-rejected: the verdict is undetermined (a busy,
    /// session-limited stream is ⊥ dead) and the caller keeps the
    /// enumeration value.
    pub live: Option<bool>,
}

/// `DESCRIBE` then `PLAY`: codec from the SDP, then at most
/// `play_window` (also bounded by `budget`) awaiting the first RTP
/// media packet — early-exit on first packet, RTCP ⊥ counts. A stream
/// answering the full handshake yet sending nothing is `live =
/// Some(false)`; a SETUP/PLAY rejection keeps the DESCRIBE codec but
/// leaves `live` undetermined (`None`).
pub async fn describe_play_probe(
    url: url::Url,
    creds: &Creds,
    budget: Duration,
    play_window: Duration,
) -> Result<ProbeOutcome, ScanError> {
    let deadline = std::time::Instant::now() + budget;
    let rcreds = if creds.is_empty() {
        None
    } else {
        Some(retina::client::Credentials {
            username: creds.username.clone(),
            password: creds.password.clone(),
        })
    };
    let group = std::sync::Arc::new(retina::client::SessionGroup::default());
    let fut = retina::client::Session::describe(
        url,
        retina::client::SessionOptions::default()
            .creds(rcreds)
            .session_group(group)
            .teardown(retina::client::TeardownPolicy::Always),
    );
    let session = tokio::time::timeout(budget, fut)
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(|e| ScanError::Malformed(e.to_string()))?;
    let mut session = session;
    let codec = session
        .streams()
        .iter()
        .find(|s| s.media() == "video")
        .map_or_else(|| "none".to_owned(), |s| s.encoding_name().to_owned());

    let window = deadline
        .saturating_duration_since(std::time::Instant::now())
        .min(play_window);
    if window.is_zero() {
        return Err(ScanError::Timeout);
    }
    let mut playing = match tokio::time::timeout(window, async {
        for i in 0..session.streams().len() {
            session
                .setup(
                    i,
                    retina::client::SetupOptions::default().transport(
                        retina::client::Transport::Tcp(
                            retina::client::TcpTransportOptions::default(),
                        ),
                    ),
                )
                .await?;
        }
        session.play(retina::client::PlayOptions::default()).await
    })
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => return Ok(ProbeOutcome { codec, live: None }),
        Err(_) => {
            return Ok(ProbeOutcome {
                codec,
                live: Some(false),
            });
        }
    };
    let live = tokio::time::timeout(window, async {
        while let Some(item) = futures_lite::StreamExt::next(&mut playing).await {
            match item {
                Ok(retina::client::PacketItem::Rtp(_)) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    })
    .await
    .unwrap_or_default();
    Ok(ProbeOutcome {
        codec,
        live: Some(live),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn fingerprint_reads_server_header() {
        // Fake binds on :0, not :554 — exercise parsing via request() on
        // a connected stream instead.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nServer: FakeCam/1.0\r\n\r\n")
                .await
                .unwrap();
        });
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (status, resp) = request(&mut s, "OPTIONS rtsp://x/ RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(200, status);
        assert_eq!(Some("FakeCam/1.0"), header(&resp, "server"));
    }

    /// Scripted RTSP responder: `200` to everything, canned SDP on
    /// `DESCRIBE`. Returns once the peer closes (TEARDOWN/drop).
    async fn fake_describe_server(sdp: &'static str) -> url::Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                // Read one request (headers end at \r\n\r\n).
                loop {
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let cseq = header(&req, "cseq").unwrap_or("0").to_owned();
                buf.clear();
                let resp = if req.starts_with("DESCRIBE") {
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                        sdp.len()
                    )
                } else {
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: OPTIONS, DESCRIBE, TEARDOWN\r\n\r\n"
                    )
                };
                if s.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        url::Url::parse(&format!("rtsp://{}:{}/x", addr.ip(), addr.port())).unwrap()
    }

    const SDP_H264: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id=42001f\r\na=control:trackID=0\r\n";

    const SDP_AUDIO_ONLY: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=control:trackID=0\r\n";

    /// Full-handshake fake: DESCRIBE serves `sdp`, SETUP echoes
    /// Transport + issues a Session, PLAY replies 200 then sends one
    /// interleaved RTP packet when `send_rtp`. Returns on peer close.
    async fn fake_play_server(sdp: &'static str, send_rtp: bool) -> url::Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                loop {
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let cseq = header(&req, "cseq").unwrap_or("0").to_owned();
                buf.clear();
                let resp = if req.starts_with("DESCRIBE") {
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                        sdp.len()
                    )
                } else if req.starts_with("SETUP") {
                    let transport = header(&req, "transport").unwrap_or("");
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: abc123;timeout=60\r\nTransport: {transport}\r\n\r\n"
                    )
                } else {
                    format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: abc123\r\n\r\n")
                };
                if s.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
                if req.starts_with("PLAY") && send_rtp {
                    // Interleaved RTP on channel 0: 12B header + payload.
                    let mut rtp = vec![
                        0x24, 0x00, 0x00, 0x00, // $, chan, len (filled below)
                        0x80, 0xE0, 0x00, 0x01, // V2+marker+PT96, seq
                        0x00, 0x00, 0x03, 0x84, // ts
                        0xDE, 0xAD, 0xBE, 0xEF, // ssrc
                        0x67, 0x42, 0x00, 0x1f, // payload
                    ];
                    let len = u16::try_from(rtp.len() - 4).unwrap();
                    rtp[2..4].copy_from_slice(&len.to_be_bytes());
                    let _ = s.write_all(&rtp).await;
                }
            }
        });
        url::Url::parse(&format!("rtsp://{}:{}/x", addr.ip(), addr.port())).unwrap()
    }

    /// Rejecting fake: DESCRIBE + SETUP succeed, PLAY answers an error
    /// status (an in-use stream on a session-limited camera).
    async fn fake_play_reject_server(sdp: &'static str) -> url::Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                loop {
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let cseq = header(&req, "cseq").unwrap_or("0").to_owned();
                buf.clear();
                let resp = if req.starts_with("DESCRIBE") {
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                        sdp.len()
                    )
                } else if req.starts_with("SETUP") {
                    let transport = header(&req, "transport").unwrap_or("");
                    format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: abc123;timeout=60\r\nTransport: {transport}\r\n\r\n"
                    )
                } else if req.starts_with("PLAY") {
                    format!(
                        "RTSP/1.0 429 Too Many Requests\r\nCSeq: {cseq}\r\nSession: abc123\r\n\r\n"
                    )
                } else {
                    format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: abc123\r\n\r\n")
                };
                if s.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        url::Url::parse(&format!("rtsp://{}:{}/x", addr.ip(), addr.port())).unwrap()
    }

    #[tokio::test]
    async fn play_probe_alive_when_rtp_flows() {
        let url = fake_play_server(SDP_H264, true).await;
        let out = describe_play_probe(
            url,
            &Creds::default(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!("h264", out.codec);
        assert_eq!(Some(true), out.live);
    }

    #[tokio::test]
    async fn play_probe_dead_when_no_media_in_window() {
        let url = fake_play_server(SDP_H264, false).await;
        let out = describe_play_probe(
            url,
            &Creds::default(),
            Duration::from_secs(5),
            Duration::from_millis(200),
        )
        .await
        .unwrap();
        assert_eq!("h264", out.codec);
        assert_eq!(Some(false), out.live);
    }

    #[tokio::test]
    async fn play_probe_keeps_codec_when_play_rejected() {
        let url = fake_play_reject_server(SDP_H264).await;
        let out = describe_play_probe(
            url,
            &Creds::default(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!("h264", out.codec);
        assert_eq!(None, out.live);
    }

    #[tokio::test]
    async fn play_probe_errs_on_describe_failure() {
        let url = url::Url::parse("rtsp://127.0.0.1:1/x").unwrap();
        let r = describe_play_probe(
            url,
            &Creds::default(),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn describe_codec_reads_video_encoding() {
        let url = fake_describe_server(SDP_H264).await;
        let codec = describe_codec(url, &Creds::default(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!("h264", codec);
    }

    #[tokio::test]
    async fn describe_codec_none_when_no_video() {
        let url = fake_describe_server(SDP_AUDIO_ONLY).await;
        let codec = describe_codec(url, &Creds::default(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!("none", codec);
    }

    #[tokio::test]
    async fn describe_codec_errs_when_unreachable() {
        // Closed port on loopback fails fast.
        let url = url::Url::parse("rtsp://127.0.0.1:1/x").unwrap();
        assert!(
            describe_codec(url, &Creds::default(), Duration::from_secs(5))
                .await
                .is_err()
        );
    }

    #[test]
    fn describe_status_classification() {
        let ok = "RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n";
        let unauth = "RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n";
        assert_eq!(Some("200"), ok.split_whitespace().nth(1));
        assert_eq!(
            Some("Basic realm=\"cam\""),
            header(unauth, "www-authenticate")
        );
    }
}
