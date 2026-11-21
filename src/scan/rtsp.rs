// SPDX-License-Identifier: GPL-2.0-or-later

//! RTSP path probing: OPTIONS fingerprint → vendor shortlist →
//! DESCRIBE classify → `retina` describe verify (externally timed).

use crate::{
    ScanError,
    onvif::{digest_authorization, parse_digest_challenge},
    types::{Creds, ScanRole},
};
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
}

/// Raw RTSP request → `(status, headers-text)`. ≤1.5 s each hop.
async fn request(
    stream: &mut tokio::net::TcpStream,
    req: &str,
) -> Result<(u16, String), ScanError> {
    tokio::time::timeout(OP_TIMEOUT, stream.write_all(req.as_bytes()))
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(ScanError::Io)?;
    let mut buf = vec![0_u8; MAX_RESP];
    let n = tokio::time::timeout(OP_TIMEOUT, stream.read(&mut buf))
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
pub async fn probe_paths(ip: Ipv4Addr, creds: &Creds) -> Vec<PathHit> {
    let mut hits = Vec::new();
    for (path, role) in SHORTLIST {
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
                role: role.clone(),
                auth: None,
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
                        role: role.clone(),
                        auth: scheme,
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
                match request(&mut s, &req2).await {
                    Ok((200, _)) => hits.push(PathHit {
                        path: (*path).to_owned(),
                        role: role.clone(),
                        auth: None,
                    }),
                    Ok((401, _)) => hits.push(PathHit {
                        path: (*path).to_owned(),
                        role: role.clone(),
                        auth: scheme,
                    }),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    hits
}

/// Verify a candidate via `retina` DESCRIBE, externally timed — the
/// pinned client has no request timeout (spec calls this out).
pub async fn verify(url: url::Url, creds: &Creds, budget: Duration) -> Result<(), ScanError> {
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
    tokio::time::timeout(budget, fut)
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(|e| ScanError::Malformed(e.to_string()))?;
    Ok(())
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
