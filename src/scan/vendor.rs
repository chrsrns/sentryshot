// SPDX-License-Identifier: GPL-2.0-or-later

//! Vendor HTTP fast-paths for per-IP mode: one-shot channel
//! enumeration before RTSP path probing (Hikvision ISAPI, Dahua CGI).

use crate::{
    ScanError,
    onvif::{digest_authorization, parse_digest_challenge},
    types::Creds,
};
use http_body_util::{BodyExt, Empty};
use hyper::{Method, Request, body::Bytes};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use std::{net::Ipv4Addr, time::Duration};

const VENDOR_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_BODY: usize = 64 * 1024;

/// Candidate RTSP paths discovered from a vendor endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorHit {
    /// `rtsp` path without host, e.g. `Streaming/Channels/101`.
    pub path: String,
    /// Whether the endpoint demanded auth (scheme tag).
    pub auth: Option<String>,
}

async fn http_get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    url: &str,
    auth: Option<String>,
) -> Result<(u16, Option<String>, String), ScanError> {
    let mut req = Request::builder().method(Method::GET).uri(url);
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let res = tokio::time::timeout(
        VENDOR_TIMEOUT,
        client.request(
            req.body(Empty::new())
                .map_err(|e| -> common::DynError { e.into() })?,
        ),
    )
    .await
    .map_err(|_| ScanError::Timeout)?
    .map_err(|e| ScanError::Http(e.into()))?;
    let status = res.status().as_u16();
    let www_auth = res
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = tokio::time::timeout(VENDOR_TIMEOUT, res.into_body().collect())
        .await
        .map_err(|_| ScanError::Timeout)?
        .map_err(|e| ScanError::Http(e.into()))?
        .to_bytes();
    Ok((
        status,
        www_auth,
        String::from_utf8_lossy(&body[..body.len().min(MAX_BODY)]).into_owned(),
    ))
}

/// GET with digest retry: bare → on 401, answer `WWW-Authenticate`
/// when creds are supplied.
async fn authed_get(url: &str, creds: &Creds) -> Result<(u16, Option<String>, String), ScanError> {
    let connector = HttpConnector::new();
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let (status, www_auth, body) = http_get(&client, url, None).await?;
    if status != 401 || creds.is_empty() {
        return Ok((status, www_auth, body));
    }
    let Some(challenge) = www_auth.as_deref().and_then(parse_digest_challenge) else {
        return Ok((status, www_auth, body));
    };
    let path = url::Url::parse(url).map_or_else(|_| "/".to_owned(), |u| u.path().to_owned());
    let auth = digest_authorization(&challenge, &creds.username, &creds.password, &path, "GET");
    http_get(&client, url, Some(auth)).await
}

/// Hikvision `GET /ISAPI/Streaming/channels` → `Streaming/Channels/{id}`
/// paths (101=main, 102=sub, ...). Returns hits or the auth tag on 401.
pub async fn isapi_channels(ip: Ipv4Addr, creds: &Creds) -> Vec<VendorHit> {
    isapi_channels_at(&format!("http://{ip}"), creds).await
}

pub(crate) async fn isapi_channels_at(base: &str, creds: &Creds) -> Vec<VendorHit> {
    let url = format!("{base}/ISAPI/Streaming/channels");
    let Ok((status, _wa, body)) = authed_get(&url, creds).await else {
        return Vec::new();
    };
    if status == 401 {
        return vec![VendorHit {
            path: String::new(),
            auth: Some("digest".to_owned()),
        }];
    }
    if status != 200 {
        return Vec::new();
    }
    let Ok(doc) = roxmltree::Document::parse(&body) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "StreamingChannel")
        .filter_map(|c| {
            c.descendants()
                .find(|d| d.tag_name().name() == "id")
                .and_then(|d| d.text())
                .map(str::to_owned)
        })
        .map(|id| VendorHit {
            path: format!("Streaming/Channels/{id}"),
            auth: None,
        })
        .collect()
}

/// Dahua `GET /cgi-bin/configManager.cgi?action=getConfig&name=ChannelTitle`
/// → `cam/realmonitor?channel=N&subtype={0,1}` per channel.
pub async fn dahua_channels(ip: Ipv4Addr, creds: &Creds) -> Vec<VendorHit> {
    dahua_channels_at(&format!("http://{ip}"), creds).await
}

pub(crate) async fn dahua_channels_at(base: &str, creds: &Creds) -> Vec<VendorHit> {
    let url = format!("{base}/cgi-bin/configManager.cgi?action=getConfig&name=ChannelTitle");
    let Ok((status, _wa, body)) = authed_get(&url, creds).await else {
        return Vec::new();
    };
    if status == 401 {
        return vec![VendorHit {
            path: String::new(),
            auth: Some("digest".to_owned()),
        }];
    }
    if status != 200 {
        return Vec::new();
    }
    // `table.ChannelTitle[N].Name=...` lines → count channels.
    let mut max_idx = 0_u32;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("table.ChannelTitle[")
            && let Some(idx) = rest.split(']').next().and_then(|n| n.parse::<u32>().ok())
        {
            max_idx = max_idx.max(idx + 1);
        }
    }
    let mut out = Vec::new();
    for ch in 1..=max_idx {
        for sub in 0..=1_u8 {
            out.push(VendorHit {
                path: format!("cam/realmonitor?channel={ch}&subtype={sub}"),
                auth: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, response::Response, routing::get};
    use pretty_assertions::assert_eq;

    const ISAPI_BODY: &str = r#"<?xml version="1.0"?><StreamingChannelList xmlns="http://www.hikvision.com/ver10/XMLSchema"><StreamingChannel><id>101</id></StreamingChannel><StreamingChannel><id>102</id></StreamingChannel></StreamingChannelList>"#;
    const DAHUA_BODY: &str =
        "table.ChannelTitle[0].Name=Front\r\ntable.ChannelTitle[1].Name=Back\r\n";

    async fn spawn(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn isapi_parses_channel_ids() {
        let base =
            spawn(Router::new().route("/ISAPI/Streaming/channels", get(|| async { ISAPI_BODY })))
                .await;
        let hits = isapi_channels_at(&base, &Creds::default()).await;
        assert_eq!(
            vec![
                VendorHit {
                    path: "Streaming/Channels/101".to_owned(),
                    auth: None
                },
                VendorHit {
                    path: "Streaming/Channels/102".to_owned(),
                    auth: None
                },
            ],
            hits
        );
    }

    #[tokio::test]
    async fn dahua_counts_channels() {
        let base =
            spawn(Router::new().route("/cgi-bin/configManager.cgi", get(|| async { DAHUA_BODY })))
                .await;
        let hits = dahua_channels_at(&base, &Creds::default()).await;
        assert_eq!(4, hits.len());
        assert!(
            hits.iter()
                .any(|h| h.path == "cam/realmonitor?channel=2&subtype=1")
        );
    }

    #[tokio::test]
    async fn vendor_401_flags_auth() {
        async fn guarded() -> Response {
            Response::builder()
                .status(401)
                .header(
                    "www-authenticate",
                    r#"Digest realm="cam", nonce="x", qop="auth""#,
                )
                .body(axum::body::Body::empty())
                .unwrap()
        }
        let base = spawn(Router::new().route("/ISAPI/Streaming/channels", get(guarded))).await;
        let hits = isapi_channels_at(&base, &Creds::default()).await;
        assert_eq!(
            vec![VendorHit {
                path: String::new(),
                auth: Some("digest".to_owned())
            }],
            hits
        );
    }
}
