// SPDX-License-Identifier: GPL-2.0-or-later

//! Persistent RTSP-over-TCP client — request/response with
//! `$`-interleaved frame demux.
//!
//! A single reader task owns the socket's inbound side and demuxes:
//! `$`-framed interleaved data → the media channel; RTSP response
//! heads → the pending-request table (matched by `CSeq`). Writes are
//! serialized through a mutex so talk-RTP frames and requests can
//! interleave on the wire.
//!
//! Auth: preemptive Basic once a `401` challenges it, Digest
//! challenge-response honoring `algorithm` via the shared `onvif`
//! helpers; one auth retry per request.

use crate::Error;
use base64::Engine as _;
use onvif::{digest_authorization, parse_digest_challenge};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Cap on a response head (status line + headers).
const MAX_HEAD: usize = 16 * 1024;
/// Cap on a response body — SDP bodies are a few KB.
const MAX_BODY: usize = 256 * 1024;
/// Media channel depth — listen frames buffer briefly when the
/// service-side pump is slow; bounded per the drop-oldest posture.
const MEDIA_DEPTH: usize = 64;

/// One RTSP response.
#[derive(Debug)]
pub struct Response {
    /// Numeric status (`200`, `401`, `551`, …).
    pub status: u16,
    /// Header pairs, original order/casing preserved.
    pub headers: Vec<(String, String)>,
    /// Response body (e.g. SDP); empty when `Content-Length` absent.
    pub body: Vec<u8>,
}

impl Response {
    /// Case-insensitive header lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An inbound `$`-interleaved frame.
#[derive(Debug)]
pub struct Interleaved {
    /// Interleaved channel id.
    pub channel: u8,
    /// Frame payload (an RTP packet for audio sessions).
    pub payload: Vec<u8>,
}

/// Auth state — upgraded from `None` on the first `401`.
enum Auth {
    /// No challenge seen yet.
    None,
    /// `Basic` — the precomputed header value.
    Basic(String),
    /// `Digest` — stored challenge, re-signed per request.
    Digest(onvif::DigestChallenge),
}

/// Shared reader/writer state.
struct Shared {
    /// Request `CSeq` → response completer.
    pending: Mutex<HashMap<u32, oneshot::Sender<Response>>>,
    /// Challenged auth.
    auth: Mutex<Auth>,
    /// Username from the RTSP URL userinfo.
    user: String,
    /// Password from the RTSP URL userinfo.
    pass: String,
}

/// Persistent RTSP connection. Inbound interleaved frames are
/// split out at connect — the session's recv pump owns the
/// receiver while `&self` request/send paths stay independent.
pub struct RtspClient {
    shared: Arc<Shared>,
    writer: Mutex<OwnedWriteHalf>,
    cseq: AtomicU32,
    /// Inbound interleaved frames — taken once at session build.
    media_rx: Option<mpsc::Receiver<Interleaved>>,
    /// Reader task handle — dropped on close.
    reader: tokio::task::JoinHandle<()>,
    /// Request-URI base with userinfo stripped.
    base: String,
}

impl RtspClient {
    /// TCP-connect to the URL's host:port (default 554) and spawn
    /// the demux reader.
    pub async fn connect(url: &url::Url) -> Result<Self, Error> {
        let host = url
            .host_str()
            .ok_or_else(|| Error::Protocol("rtsp url lacks host".into()))?;
        let port = url.port_or_known_default().unwrap_or(554);
        let stream = TcpStream::connect((host, port)).await?;
        stream.set_nodelay(true).ok();
        let (rd, wr) = stream.into_split();

        let (media_tx, media_rx) = mpsc::channel(MEDIA_DEPTH);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            auth: Mutex::new(Auth::None),
            user: url.username().to_owned(),
            pass: url.password().unwrap_or("").to_owned(),
        });
        let reader = tokio::spawn(read_loop(rd, shared.clone(), media_tx));

        let mut base = url.clone();
        base.set_username("").ok();
        base.set_password(None).ok();
        Ok(Self {
            shared,
            writer: Mutex::new(wr),
            cseq: AtomicU32::new(1),
            media_rx: Some(media_rx),
            reader,
            base: base.to_string(),
        })
    }

    /// Take the inbound interleaved-frame stream — once; the
    /// session's recv pump owns it thereafter.
    pub fn take_media(&mut self) -> Option<mpsc::Receiver<Interleaved>> {
        self.media_rx.take()
    }

    /// Request-URI base with userinfo stripped — the URL every
    /// method targets.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Issue one request; `extra` headers ride verbatim. On `401`
    /// the request is retried once with the challenged scheme. The
    /// caller bounds the wait (session keeps its own deadline).
    pub async fn request(
        &self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
    ) -> Result<Response, Error> {
        for attempt in 0..2 {
            let cseq = self.cseq.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.shared.pending.lock().await.insert(cseq, tx);

            let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n");
            if let Some(a) = self.auth_header(method, uri).await {
                write!(req, "Authorization: {a}\r\n").ok();
            }
            for (k, v) in extra {
                write!(req, "{k}: {v}\r\n").ok();
            }
            req.push_str("\r\n");

            let write = self.writer.lock().await.write_all(req.as_bytes()).await;
            match write {
                Ok(()) => {}
                Err(e) => {
                    self.shared.pending.lock().await.remove(&cseq);
                    return Err(e.into());
                }
            }

            let Ok(resp) = rx.await else {
                // Reader task died — socket-level failure.
                return Err(Error::Transport(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "rtsp reader terminated",
                )));
            };
            if resp.status == 401 && attempt == 0 {
                if self.challenge(resp.header("WWW-Authenticate")).await {
                    continue;
                }
                return Err(Error::Auth);
            }
            if resp.status == 401 {
                return Err(Error::Auth);
            }
            return Ok(resp);
        }
        unreachable!()
    }

    /// Write a `$`-interleaved frame (talk RTP) on the wire.
    pub async fn send_interleaved(&self, channel: u8, payload: &[u8]) -> Result<(), Error> {
        let len = u16::try_from(payload.len())
            .map_err(|_| Error::Protocol("interleaved frame > 64KiB".into()))?;
        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.push(b'$');
        buf.push(channel);
        buf.extend_from_slice(&len.to_be_bytes());
        buf.extend_from_slice(payload);
        self.writer.lock().await.write_all(&buf).await?;
        Ok(())
    }

    /// `TEARDOWN` + socket close — aborts the reader.
    pub fn close(&self) {
        self.reader.abort();
    }

    /// Compute the `Authorization` value for this request under the
    /// stored scheme; `None` when unchallenged.
    async fn auth_header(&self, method: &str, uri: &str) -> Option<String> {
        match &*self.shared.auth.lock().await {
            Auth::None => None,
            Auth::Basic(v) => Some(v.clone()),
            Auth::Digest(c) => Some(digest_authorization(
                c,
                &self.shared.user,
                &self.shared.pass,
                uri,
                method,
            )),
        }
    }

    /// Consume a `WWW-Authenticate` header from a `401`; `true` when
    /// a usable scheme was stored and a retry is warranted.
    async fn challenge(&self, header: Option<&str>) -> bool {
        let Some(h) = header else { return false };
        if let Some(rest) = h.strip_prefix("Basic") {
            let _ = rest;
            let tok = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", self.shared.user, self.shared.pass));
            *self.shared.auth.lock().await = Auth::Basic(format!("Basic {tok}"));
            return true;
        }
        if let Some(c) = parse_digest_challenge(h) {
            *self.shared.auth.lock().await = Auth::Digest(c);
            return true;
        }
        false
    }
}

/// Inbound demux — parse `$` frames vs response heads until EOF.
async fn read_loop(
    mut rd: OwnedReadHalf,
    shared: Arc<Shared>,
    media_tx: mpsc::Sender<Interleaved>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let mut chunk = [0_u8; 4096];
        match rd.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // A single in-flight unit is bounded; a peer that
                // never terminates a head/frame kills the reader.
                if buf.len() > MAX_HEAD + MAX_BODY {
                    break;
                }
                while let Some(msg) = take_message(&mut buf) {
                    match msg {
                        Message::Interleaved(f) => {
                            if media_tx.send(f).await.is_err() {
                                return;
                            }
                        }
                        Message::Response(r) => {
                            if let Some(cseq) =
                                r.header("CSeq").and_then(|v| v.trim().parse::<u32>().ok())
                            {
                                if let Some(tx) = shared.pending.lock().await.remove(&cseq) {
                                    let _ = tx.send(r);
                                }
                            }
                            // Stray/unmatched responses are dropped.
                        }
                    }
                }
            }
        }
    }
    // EOF — fail all pending waiters.
    shared.pending.lock().await.clear();
}

/// Parsed inbound unit.
enum Message {
    Response(Response),
    Interleaved(Interleaved),
}

/// Pull one complete message off the buffer; `None` when more
/// bytes are needed.
fn take_message(buf: &mut Vec<u8>) -> Option<Message> {
    if buf.first() == Some(&b'$') {
        if buf.len() < 4 {
            return None;
        }
        let channel = buf[1];
        let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        if buf.len() < 4 + len {
            return None;
        }
        let payload = buf[4..4 + len].to_vec();
        buf.drain(..4 + len);
        return Some(Message::Interleaved(Interleaved { channel, payload }));
    }
    // RTSP response head — scan for the header terminator.
    let head_end = find_seq(buf, b"\r\n\r\n")? + 4;
    let Ok(head) = std::str::from_utf8(&buf[..head_end]) else {
        // Protocol garbage — drop it rather than stall the buffer.
        buf.drain(..head_end);
        return Some(Message::Response(Response {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
        }));
    };
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    let mut content_len = 0_usize;
    for line in head.lines().skip(1) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            continue;
        }
        if k.eq_ignore_ascii_case("content-length") {
            content_len = v.parse().unwrap_or(0).min(MAX_BODY);
        }
        headers.push((k.to_owned(), v.to_owned()));
    }
    if buf.len() < head_end + content_len {
        return None;
    }
    let body = buf[head_end..head_end + content_len].to_vec();
    buf.drain(..head_end + content_len);
    Some(Message::Response(Response {
        status,
        headers,
        body,
    }))
}

/// First index of `needle` in `hay`.
fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc as tmpsc;

    /// Read one RTSP request head off a server socket.
    async fn read_req(rd: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
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

    /// Reply string for a request's `CSeq`.
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

    /// Spawn a one-connection stub running `script` against each
    /// request; returns the client URL.
    async fn stub<F, Fut>(script: F) -> (url::Url, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = lis.local_addr().unwrap().port();
        let h = tokio::spawn(async move {
            let (s, _) = lis.accept().await.unwrap();
            script(s).await;
        });
        (
            url::Url::parse(&format!("rtsp://127.0.0.1:{port}/live")).unwrap(),
            h,
        )
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let (url, srv) = stub(|s| async move {
            let (rd, mut wr) = s.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let req = read_req(&mut rd).await;
            assert!(req.starts_with("OPTIONS rtsp://"), "{req}");
            wr.write_all(reply(&req, "200 OK", "Public: OPTIONS\r\n", "").as_bytes())
                .await
                .unwrap();
        })
        .await;
        let c = RtspClient::connect(&url).await.unwrap();
        let r = c.request("OPTIONS", c.base(), &[]).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("public"), Some("OPTIONS"));
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn describe_body_parsed() {
        let sdp = "v=0\r\nm=audio 0 RTP/AVP 0\r\na=recvonly\r\n";
        let (url, srv) = stub(move |s| async move {
            let (rd, mut wr) = s.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let req = read_req(&mut rd).await;
            assert!(req.starts_with("DESCRIBE "), "{req}");
            wr.write_all(reply(&req, "200 OK", "", sdp).as_bytes())
                .await
                .unwrap();
        })
        .await;
        let c = RtspClient::connect(&url).await.unwrap();
        let r = c
            .request("DESCRIBE", c.base(), &[("Accept", "application/sdp")])
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let parsed = crate::sdp::parse(std::str::from_utf8(&r.body).unwrap(), &url);
        assert_eq!(parsed.audio.len(), 1);
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn digest_401_retries_with_authorization() {
        let (url, srv) = stub(|s| async move {
            let (rd, mut wr) = s.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let r1 = read_req(&mut rd).await;
            assert!(!r1.contains("Authorization:"), "{r1}");
            wr.write_all(
                reply(
                    &r1,
                    "401 Unauthorized",
                    "WWW-Authenticate: Digest realm=\"cam\", nonce=\"n1\", qop=\"auth\"\r\n",
                    "",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let r2 = read_req(&mut rd).await;
            assert!(r2.contains("Authorization: Digest "), "{r2}");
            assert!(r2.contains("response=\""), "{r2}");
            assert!(r2.contains("qop=auth"), "{r2}");
            wr.write_all(reply(&r2, "200 OK", "", "").as_bytes())
                .await
                .unwrap();
        })
        .await;
        let url = {
            let mut u = url;
            u.set_username("admin").unwrap();
            u.set_password(Some("pw")).unwrap();
            u
        };
        let c = RtspClient::connect(&url).await.unwrap();
        let r = c.request("OPTIONS", c.base(), &[]).await.unwrap();
        assert_eq!(r.status, 200);
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn basic_401_retries_and_stays_preemptive() {
        let (tx, mut got) = tmpsc::channel::<bool>(4);
        let (url, srv) = stub(|s| async move {
            let (rd, mut wr) = s.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let r1 = read_req(&mut rd).await;
            wr.write_all(
                reply(
                    &r1,
                    "401 Unauthorized",
                    "WWW-Authenticate: Basic realm=\"cam\"\r\n",
                    "",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let r2 = read_req(&mut rd).await;
            tx.send(r2.contains("Authorization: Basic ")).await.unwrap();
            wr.write_all(reply(&r2, "200 OK", "", "").as_bytes())
                .await
                .unwrap();
            // Second request carries auth preemptively.
            let r3 = read_req(&mut rd).await;
            tx.send(r3.contains("Authorization: Basic ")).await.unwrap();
            wr.write_all(reply(&r3, "200 OK", "", "").as_bytes())
                .await
                .unwrap();
        })
        .await;
        let mut url = url;
        url.set_username("admin").unwrap();
        url.set_password(Some("pw")).unwrap();
        let c = RtspClient::connect(&url).await.unwrap();
        assert_eq!(
            c.request("OPTIONS", c.base(), &[]).await.unwrap().status,
            200
        );
        assert!(got.recv().await.unwrap());
        assert_eq!(
            c.request("OPTIONS", c.base(), &[]).await.unwrap().status,
            200
        );
        assert!(got.recv().await.unwrap());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn interleaved_frames_demuxed() {
        let (url, srv) = stub(|s| async move {
            let (_, mut wr) = s.into_split();
            // Two $-frames on channels 2 + 4.
            for (ch, payload) in [(2_u8, &[1_u8, 2, 3][..]), (4, &[9_u8][..])] {
                let mut f = vec![b'$', ch, 0, u8::try_from(payload.len()).unwrap()];
                f.extend_from_slice(payload);
                wr.write_all(&f).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        })
        .await;
        let mut c = RtspClient::connect(&url).await.unwrap();
        let mut rx = c.take_media().unwrap();
        let f1 = rx.recv().await.unwrap();
        assert_eq!((f1.channel, f1.payload), (2, vec![1, 2, 3]));
        let f2 = rx.recv().await.unwrap();
        assert_eq!((f2.channel, f2.payload), (4, vec![9]));
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn send_interleaved_writes_dollar_frame() {
        let (tx, mut got) = tmpsc::channel::<Vec<u8>>(2);
        let (url, srv) = stub(|s| async move {
            let (mut rd, _) = s.into_split();
            let mut buf = vec![0_u8; 4 + 3];
            rd.read_exact(&mut buf).await.unwrap();
            tx.send(buf).await.unwrap();
        })
        .await;
        let c = RtspClient::connect(&url).await.unwrap();
        c.send_interleaved(6, &[0xAA, 0xBB, 0xCC]).await.unwrap();
        assert_eq!(
            got.recv().await.unwrap(),
            vec![b'$', 6, 0, 3, 0xAA, 0xBB, 0xCC]
        );
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn reader_death_fails_pending_request() {
        let (url, srv) = stub(|s| async move {
            let (rd, _wr) = s.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let _req = read_req(&mut rd).await;
            // Drop socket without replying.
        })
        .await;
        let c = RtspClient::connect(&url).await.unwrap();
        let err = c.request("OPTIONS", c.base(), &[]).await.unwrap_err();
        assert!(matches!(err, Error::Transport(_)), "{err:?}");
        srv.await.unwrap();
    }
}
