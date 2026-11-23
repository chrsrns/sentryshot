// SPDX-License-Identifier: GPL-2.0-or-later

//! Shared ONVIF SOAP-over-HTTP transport.
//!
//! Hand-rolled envelopes; the auth ladder is
//! bare → HTTP Digest on 401 → WSSE `PasswordDigest`
//! (device-time `Created`) → one retry after clock-offset recompute.
//! Consumed by `src/scan` (per-call `Duration` contract) and `src/ptz`
//! (chain-`Instant` deadline contract).

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use common::DynError;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, body::Bytes};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use roxmltree::Document;
use sha1::Digest as _;
use std::fmt::Write as _;
use std::{
    net::IpAddr,
    sync::atomic::{AtomicBool, AtomicI64, Ordering},
    time::{Duration, Instant},
};

const MAX_BODY: usize = 64 * 1024;

/// Transport failure taxonomy.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operation exceeded its bound.
    #[error("timeout")]
    Timeout,

    /// HTTP layer failure.
    #[error("http: {0}")]
    Http(#[from] DynError),

    /// Device answered, but the reply did not parse.
    #[error("malformed reply: {0}")]
    Malformed(String),
}

/// SOAP client over plain HTTP. `clock_offset` and `auth_used` carry
/// across calls on the same client.
pub struct SoapClient {
    client: Client<HttpConnector, Full<Bytes>>,
    /// Device-minus-local clock offset applied to WSSE `Created`.
    clock_offset: AtomicI64,
    /// Any call required a credential retry (401/NotAuthorized).
    auth_used: AtomicBool,
}

impl Default for SoapClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SoapClient {
    #[must_use]
    pub fn new() -> Self {
        let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        Self {
            client,
            clock_offset: AtomicI64::new(0),
            auth_used: AtomicBool::new(false),
        }
    }

    /// Record the measured device-minus-local offset in seconds.
    pub fn set_clock_offset(&self, secs: i64) {
        self.clock_offset.store(secs, Ordering::Relaxed);
    }

    #[must_use]
    pub fn clock_offset(&self) -> i64 {
        self.clock_offset.load(Ordering::Relaxed)
    }

    /// Whether any call required a credential retry.
    #[must_use]
    pub fn auth_used(&self) -> bool {
        self.auth_used.load(Ordering::Relaxed)
    }

    /// POST one SOAP action, applying the auth ladder.
    /// Each leg gets a fresh `timeout` — the scan contract.
    /// Returns `(body, http-status)`.
    pub async fn soap_call(
        &self,
        xaddr: &str,
        action_body: &str,
        creds: Option<(&str, &str)>,
        timeout: Duration,
    ) -> Result<(String, u16), Error> {
        self.call_inner(xaddr, action_body, creds, Budget::PerLeg(timeout))
            .await
    }

    /// POST one SOAP action where every leg — bare attempt and any
    /// auth retry — draws down the same `deadline`. Used for
    /// chain-budgeted command sessions.
    pub async fn soap_call_deadline(
        &self,
        xaddr: &str,
        action_body: &str,
        creds: Option<(&str, &str)>,
        deadline: Instant,
    ) -> Result<(String, u16), Error> {
        self.call_inner(xaddr, action_body, creds, Budget::Shared(deadline))
            .await
    }

    async fn call_inner(
        &self,
        xaddr: &str,
        action_body: &str,
        creds: Option<(&str, &str)>,
        budget: Budget,
    ) -> Result<(String, u16), Error> {
        let do_post = |body: String, auth: Option<String>| {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(xaddr)
                .header("content-type", "application/soap+xml; charset=utf-8");
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            req.body(Full::new(Bytes::from(body)))
        };

        let have_creds = creds.is_some_and(|(u, p)| !(u.is_empty() && p.is_empty()));
        // bare attempt
        let (t, dl) = budget.leg();
        let res = tokio::time::timeout(
            t,
            self.client.request(
                do_post(soap_envelope(action_body, None), None)
                    .map_err(|e| -> DynError { e.into() })?,
            ),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|e| Error::Http(e.into()))?;
        let status = res.status().as_u16();
        let www_auth = res
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = self.collect_body(res, dl).await?;

        if status != 401 {
            if text.contains("NotAuthorized") && have_creds {
                self.auth_used.store(true, Ordering::Relaxed);
                let (u, p) = creds.expect("have_creds");
                let body2 = soap_envelope(action_body, Some((u, p, self.clock_offset())));
                return self.round_trip(&do_post, body2, None, budget).await;
            }
            return Ok((text, status));
        }
        if !have_creds {
            self.auth_used.store(true, Ordering::Relaxed);
            return Ok((text, status));
        }
        let (u, p) = creds.expect("have_creds");
        // 401 → HTTP Digest when the server offers it, else WSSE once.
        let challenge = www_auth.as_deref().and_then(parse_digest_challenge);
        let Some(challenge) = challenge else {
            self.auth_used.store(true, Ordering::Relaxed);
            let body2 = soap_envelope(action_body, Some((u, p, self.clock_offset())));
            return self.round_trip(&do_post, body2, None, budget).await;
        };
        let path = url::Url::parse(xaddr).map_or_else(|_| "/".to_owned(), |u| u.path().to_owned());
        let auth = digest_authorization(&challenge, u, p, &path, "POST");
        self.round_trip(
            &do_post,
            soap_envelope(action_body, None),
            Some(auth),
            budget,
        )
        .await
    }

    async fn round_trip(
        &self,
        do_post: &(
             dyn Fn(String, Option<String>) -> Result<Request<Full<Bytes>>, hyper::http::Error>
                 + Sync
         ),
        body: String,
        auth: Option<String>,
        budget: Budget,
    ) -> Result<(String, u16), Error> {
        let (t, dl) = budget.leg();
        let res = tokio::time::timeout(
            t,
            self.client.request(
                do_post(body, auth).map_err(|e: hyper::http::Error| -> DynError { e.into() })?,
            ),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|e| Error::Http(e.into()))?;
        let status = res.status().as_u16();
        let text = self.collect_body(res, dl).await?;
        Ok((text, status))
    }

    async fn collect_body(
        &self,
        res: hyper::Response<hyper::body::Incoming>,
        deadline: Instant,
    ) -> Result<String, Error> {
        let bounded = http_body_util::Limited::new(res.into_body(), MAX_BODY + 1);
        let body = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            bounded.collect(),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(Error::Http)?
        .to_bytes();
        Ok(String::from_utf8_lossy(&body[..body.len().min(MAX_BODY)]).into_owned())
    }
}

/// Per-leg budget: `PerLeg` gives every attempt a fresh `Duration`
/// (scan contract); `Shared` makes all legs draw down one `Instant`.
#[derive(Clone, Copy)]
enum Budget {
    PerLeg(Duration),
    Shared(Instant),
}

impl Budget {
    /// `(leg timeout, body-collect deadline)` for the next leg.
    fn leg(&self) -> (Duration, Instant) {
        let now = Instant::now();
        match self {
            Self::PerLeg(t) => (*t, now + *t),
            Self::Shared(d) => (d.saturating_duration_since(now), *d),
        }
    }
}

/// XML-escape a string interpolated into an envelope — device-supplied
/// tokens and user creds must not break the document.
#[must_use]
pub fn xml_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn soap_envelope(action_body: &str, creds: Option<(&str, &str, i64)>) -> String {
    let security = match creds {
        None => String::new(),
        Some((user, pass, offset)) => {
            let nonce: [u8; 16] = rand::random();
            // Created in device time: local now + measured offset.
            let created = jiff::Timestamp::now().as_second() + offset;
            let created_iso = jiff::Timestamp::from_second(created)
                .map_or_else(|_| String::new(), |t| t.to_string());
            let mut sha = sha1::Sha1::new();
            sha.update(nonce);
            sha.update(created_iso.as_bytes());
            sha.update(pass.as_bytes());
            let digest = B64.encode(sha.finalize());
            format!(
                r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</wsse:Nonce><wsu:Created>{created_iso}</wsu:Created><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password></wsse:UsernameToken></wsse:Security>"#,
                xml_escape(user),
                B64.encode(nonce),
            )
        }
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Header>{security}</s:Header><s:Body>{action_body}</s:Body></s:Envelope>"#
    )
}

/// Rewrite a device-reported URI's host to the responder IP, keeping the
/// URI's own port and path. Returns `None` on unparseable input.
#[must_use]
pub fn rewrite_host_ip(uri: &str, responder: IpAddr) -> Option<String> {
    let mut url = url::Url::parse(uri).ok()?;
    url.set_ip_host(responder).ok()?;
    Some(url.to_string())
}

/// Rewrite a device-reported URI's host to an arbitrary host — IP
/// literal or DNS name — keeping the URI's own port and path.
#[must_use]
pub fn rewrite_host(uri: &str, host: &str) -> Option<String> {
    let mut url = url::Url::parse(uri).ok()?;
    url.set_host(Some(host)).ok()?;
    Some(url.to_string())
}

/// Percent-decoded `(username, password)` from a URL's userinfo.
/// Both empty when absent or the input is not a URL.
#[must_use]
pub fn creds_from_url(raw: &str) -> (String, String) {
    let Ok(url) = url::Url::parse(raw) else {
        return (String::new(), String::new());
    };
    let dec = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    (
        dec(url.username()),
        url.password().map_or_else(String::new, dec),
    )
}

/// Parse a SOAP/XML reply.
pub fn parse_doc(text: &str) -> Result<Document<'_>, Error> {
    Document::parse(text).map_err(|e| Error::Malformed(e.to_string()))
}

/// Texts of all elements with a given local name, document order.
#[must_use]
pub fn find_text<'i>(doc: &'i Document<'i>, name: &str) -> Vec<&'i str> {
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == name)
        .filter_map(|n| n.text())
        .collect()
}

/// Parsed `WWW-Authenticate: Digest …` challenge parameters.
pub struct DigestChallenge {
    realm: String,
    nonce: String,
    qop: Option<String>,
    algorithm: Option<String>,
    opaque: Option<String>,
}

/// Parse a `WWW-Authenticate` header; `None` when not Digest or
/// missing the mandatory `realm`/`nonce`.
#[must_use]
pub fn parse_digest_challenge(header: &str) -> Option<DigestChallenge> {
    let rest = header
        .get(..6)
        .filter(|s| s.eq_ignore_ascii_case("digest"))
        .map(|_| &header[6..])?;
    let mut c = DigestChallenge {
        realm: String::new(),
        nonce: String::new(),
        qop: None,
        algorithm: None,
        opaque: None,
    };
    for part in split_quoted(rest, ',') {
        let mut kv = part.trim().splitn(2, '=');
        let (k, v) = (kv.next()?, kv.next()?);
        let v = v.trim().trim_matches('"').to_owned();
        match k.trim() {
            "realm" => c.realm = v,
            "nonce" => c.nonce = v,
            "qop" => c.qop = Some(v),
            "algorithm" => c.algorithm = Some(v),
            "opaque" => c.opaque = Some(v),
            _ => {}
        }
    }
    if c.realm.is_empty() || c.nonce.is_empty() {
        return None;
    }
    Some(c)
}

/// Split on `sep` ignoring separators inside double-quoted spans —
/// Digest params like `qop="auth,auth-int"` carry literal commas.
fn split_quoted(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            c if c == sep && !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Build a Digest `Authorization` header value for one request.
/// Honors the challenge `algorithm` (MD5 default, SHA-256, and
/// `-sess` variants) and emits `qop`/`nc`/`cnonce`/`opaque`/`algorithm`
/// fields when the challenge carries them.
#[must_use]
pub fn digest_authorization(
    challenge: &DigestChallenge,
    user: &str,
    pass: &str,
    uri: &str,
    method: &str,
) -> String {
    let cnonce = format!("{:x}", md5::compute(rand::random::<u64>().to_string()));
    digest_authorization_with(challenge, user, pass, uri, method, &cnonce)
}

/// Deterministic core of `digest_authorization` — the caller
/// supplies the cnonce (RFC 7616 wants it unpredictable; tests
/// pin it).
fn digest_authorization_with(
    challenge: &DigestChallenge,
    user: &str,
    pass: &str,
    uri: &str,
    method: &str,
    cnonce: &str,
) -> String {
    let sha256 = challenge
        .algorithm
        .as_deref()
        .is_some_and(|a| a.to_ascii_lowercase().starts_with("sha-256"));
    let sess = challenge
        .algorithm
        .as_deref()
        .is_some_and(|a| a.to_ascii_lowercase().ends_with("-sess"));
    let hash = |v: &str| -> String {
        if sha256 {
            format!("{:x}", sha2::Sha256::digest(v.as_bytes()))
        } else {
            format!("{:x}", md5::compute(v))
        }
    };
    let mut ha1 = hash(&format!("{user}:{}:{pass}", challenge.realm));
    if sess {
        ha1 = hash(&format!("{ha1}:{}:{cnonce}", challenge.nonce));
    }
    let ha2 = hash(&format!("{method}:{uri}"));
    // `qop` may arrive as a comma list — `auth` is the only form
    // supported; an auth-int-only challenge gets `auth` anyway
    // (universal camera posture).
    let qop = challenge.qop.as_deref().map(|_| "auth");
    let nc = "00000001";
    let response = match qop {
        Some(q) => hash(&format!(
            "{ha1}:{}:{nc}:{cnonce}:{q}:{ha2}",
            challenge.nonce
        )),
        None => hash(&format!("{ha1}:{}:{ha2}", challenge.nonce)),
    };
    let mut out = format!(
        "Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
        challenge.realm, challenge.nonce
    );
    if let Some(a) = &challenge.algorithm {
        write!(out, ", algorithm={a}").ok();
    }
    if let Some(o) = &challenge.opaque {
        write!(out, ", opaque=\"{o}\"").ok();
    }
    if let Some(q) = qop {
        write!(out, ", qop={q}, nc={nc}, cnonce=\"{cnonce}\"").ok();
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn digest_roundtrip() {
        let c =
            parse_digest_challenge(r#"Digest realm="cam", nonce="abc123", qop="auth""#).unwrap();
        let auth = digest_authorization(&c, "admin", "pass", "/x", "POST");
        assert!(auth.contains("username=\"admin\""));
        assert!(auth.contains("realm=\"cam\""));
        assert!(auth.contains("response=\""));
        assert!(auth.contains("uri=\"/x\""));
    }

    // RFC 7616 §3.9.1 — MD5 response with qop=auth.
    #[test]
    fn digest_rfc7616_md5() {
        let c = parse_digest_challenge(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        let auth = digest_authorization_with(
            &c,
            "Mufasa",
            "Circle Of Life",
            "/dir/index.html",
            "GET",
            "0a4f113b",
        );
        assert!(
            auth.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "{auth}"
        );
        assert!(auth.contains("qop=auth"), "{auth}");
        assert!(auth.contains("nc=00000001"), "{auth}");
        assert!(auth.contains("cnonce=\"0a4f113b\""), "{auth}");
        assert!(
            auth.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""),
            "{auth}"
        );
    }

    // RFC 7616 §3.9.1 — SHA-256 response with qop=auth (the
    // example's params differ from the MD5 one).
    #[test]
    fn digest_rfc7616_sha256() {
        let c = parse_digest_challenge(
            r#"Digest realm="http-auth@example.org", qop="auth,auth-int", nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS", algorithm=SHA-256"#,
        )
        .unwrap();
        let auth = digest_authorization_with(
            &c,
            "Mufasa",
            "Circle of Life",
            "/dir/index.html",
            "GET",
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
        );
        assert!(
            auth.contains(
                "response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""
            ),
            "{auth}"
        );
        assert!(auth.contains("algorithm=SHA-256"), "{auth}");
    }

    // qop-less challenge: no qop/nc/cnonce fields emitted, legacy
    // HA1:nonce:HA2 response form.
    #[test]
    fn digest_no_qop() {
        let c = parse_digest_challenge(r#"Digest realm="cam", nonce="abc123""#).unwrap();
        let auth = digest_authorization_with(&c, "admin", "pass", "/x", "GET", "zz");
        assert!(!auth.contains("qop"), "{auth}");
        assert!(!auth.contains("cnonce"), "{auth}");
        assert!(auth.contains("response=\""), "{auth}");
    }

    #[test]
    fn rewrite_host_ip_keeps_port_and_path() {
        assert_eq!(
            "rtsp://127.0.0.9:8554/live/x",
            rewrite_host_ip("rtsp://0.0.0.0:8554/live/x", "127.0.0.9".parse().unwrap()).unwrap()
        );
    }

    #[test]
    fn rewrite_host_accepts_hostname() {
        assert_eq!(
            "http://cam.local:8080/onvif/x",
            rewrite_host("http://0.0.0.0:8080/onvif/x", "cam.local").unwrap()
        );
        assert_eq!(
            "rtsp://10.0.0.5:554/s",
            rewrite_host("rtsp://0.0.0.0:554/s", "10.0.0.5").unwrap()
        );
    }

    #[test]
    fn creds_from_url_decodes_userinfo() {
        assert_eq!(
            ("admin".to_owned(), "p@ss".to_owned()),
            creds_from_url("rtsp://admin:p%40ss@cam/x")
        );
        assert_eq!(
            (String::new(), String::new()),
            creds_from_url("rtsp://cam/x")
        );
        assert_eq!((String::new(), String::new()), creds_from_url("not a url"));
    }

    #[test]
    fn envelope_escapes_creds_and_token() {
        let env = soap_envelope("<x/>", Some(("ad<min", "p&w", 0)));
        assert!(env.contains("ad&lt;min"));
        assert!(!env.contains("<min"));
        // Password is never sent in clear — digest only — but the raw
        // value must not appear in the envelope.
        assert!(!env.contains("p&w"));
        assert_eq!("a&lt;b&gt;&amp;&quot;c&quot;", xml_escape("a<b>&\"c\""));
    }

    #[test]
    fn malformed_soap_response_is_error_not_panic() {
        assert!(parse_doc("<broken").is_err());
        assert!(parse_doc("not xml at all").is_err());
    }

    #[tokio::test]
    async fn deadline_variant_times_out_when_budget_spent() {
        use axum::{Router, routing::post};
        // Stub sleeps past the shared deadline — the in-flight leg
        // must fail `Timeout` rather than wait out a fresh budget.
        let app = Router::new().route(
            "/dev",
            post(|| async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                "ok"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = SoapClient::new();
        let err = client
            .soap_call_deadline(
                &format!("http://{addr}/dev"),
                "<x/>",
                None,
                Instant::now() + Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert_eq!("timeout", err.to_string());
    }
}
