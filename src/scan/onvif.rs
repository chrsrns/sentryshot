// SPDX-License-Identifier: GPL-2.0-or-later

//! ONVIF Profile-S enumeration behind the `OnvifClient` seam.
//!
//! SOAP transport lives in the shared `onvif` crate; this file owns the
//! Profile-S chain (capabilities → profiles → stream URIs).

use crate::{
    ScanError,
    seam::OnvifClient as _,
    types::{Creds, DiscoveredDevice, Profile, Resolution, Services},
};
use std::{net::IpAddr, time::Duration};

/// Concrete ONVIF client over plain HTTP.
pub struct OnvifHttp {
    soap: onvif::SoapClient,
    /// WSD responder — device-advertised hosts are unreliable, so every
    /// returned `XAddr` is re-anchored to it.
    responder: IpAddr,
}

impl OnvifHttp {
    #[must_use]
    pub fn new(responder: IpAddr) -> Self {
        Self {
            soap: onvif::SoapClient::new(),
            responder,
        }
    }

    async fn call(
        &self,
        xaddr: &str,
        action_body: &str,
        creds: &Creds,
    ) -> Result<(String, u16), ScanError> {
        self.soap
            .soap_call(
                xaddr,
                action_body,
                Some((&creds.username, &creds.password)),
                Duration::from_secs(2),
            )
            .await
            .map_err(ScanError::from)
    }

    /// Whether any call required a credential retry.
    #[must_use]
    pub fn auth_used(&self) -> bool {
        self.soap.auth_used()
    }
}

#[async_trait::async_trait]
impl crate::seam::OnvifClient for OnvifHttp {
    async fn discover(&self, window: Duration) -> Result<Vec<DiscoveredDevice>, ScanError> {
        crate::wsd::probe_and_collect(window).await
    }

    async fn device_time(&self, xaddr: &str, creds: &Creds) -> Result<jiff::Timestamp, ScanError> {
        let (text, status) = self
            .call(
                xaddr,
                r#"<tds:GetSystemDateAndTime xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>"#,
                creds,
            )
            .await?;
        if status != 200 {
            return Err(ScanError::Malformed(format!("status {status}")));
        }
        let doc = onvif::parse_doc(&text)?;
        let pick =
            |name: &str| -> Option<i64> { onvif::find_text(&doc, name).first()?.parse().ok() };
        let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (
            pick("Year"),
            pick("Month"),
            pick("Day"),
            pick("Hour"),
            pick("Minute"),
            pick("Second"),
        ) else {
            return Err(ScanError::Malformed("no UTCDateTime".into()));
        };
        let ts = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
        ts.parse::<jiff::Timestamp>()
            .map_err(|e| ScanError::Malformed(e.to_string()))
    }

    async fn services(&self, xaddr: &str, creds: &Creds) -> Result<Services, ScanError> {
        let (text, status) = self
            .call(
                xaddr,
                r#"<tds:GetCapabilities xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Category>All</tds:Category></tds:GetCapabilities>"#,
                creds,
            )
            .await?;
        if status != 200 {
            return Err(ScanError::Malformed(format!("status {status}")));
        }
        let doc = onvif::parse_doc(&text)?;
        let mut out = Services::default();
        // Media XAddr: <…:Media><…:XAddr>.
        for n in doc.descendants() {
            if n.is_element() && n.tag_name().name() == "Media" {
                if let Some(x) = n
                    .children()
                    .find(|c| c.is_element() && c.tag_name().name() == "XAddr")
                    .and_then(|c| c.text())
                {
                    out.media_xaddr = onvif::rewrite_host_ip(x, self.responder);
                }
            }
            if n.is_element() && n.tag_name().name() == "PTZ" {
                out.ptz = true;
            }
            if n.is_element() && n.tag_name().name() == "Backchannel" && n.text() == Some("true") {
                out.backchannel = true;
            }
        }
        // Fallback: GetServices when capabilities lacked media XAddr.
        if out.media_xaddr.is_none() {
            let (text2, status2) = self
                .call(
                    xaddr,
                    r#"<tds:GetServices xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>"#,
                    creds,
                )
                .await?;
            if status2 == 200 {
                if let Ok(doc2) = onvif::parse_doc(&text2) {
                    for s in doc2.descendants() {
                        if !(s.is_element() && s.tag_name().name() == "Service") {
                            continue;
                        }
                        let ns = s
                            .children()
                            .find(|c| c.is_element() && c.tag_name().name() == "Namespace")
                            .and_then(|c| c.text())
                            .unwrap_or("");
                        let x = s
                            .children()
                            .find(|c| c.is_element() && c.tag_name().name() == "XAddr")
                            .and_then(|c| c.text());
                        if ns.contains("media")
                            && let Some(x) = x
                        {
                            out.media_xaddr = onvif::rewrite_host_ip(x, self.responder);
                        }
                        if ns.contains("ptz") {
                            out.ptz = true;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    async fn profiles(&self, media_xaddr: &str, creds: &Creds) -> Result<Vec<Profile>, ScanError> {
        let (text, status) = self
            .call(
                media_xaddr,
                r#"<trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>"#,
                creds,
            )
            .await?;
        if status != 200 {
            return Err(ScanError::Malformed(format!("status {status}")));
        }
        let doc = onvif::parse_doc(&text)?;
        let mut out = Vec::new();
        for p in doc.descendants() {
            if !(p.is_element() && p.tag_name().name() == "Profiles") {
                continue;
            }
            let token = p.attribute("token").unwrap_or_default().to_owned();
            let mut name = None;
            let mut resolution = None;
            for c in p.descendants() {
                match c.tag_name().name() {
                    "Name" if name.is_none() => name = c.text().map(str::to_owned),
                    "VideoEncoderConfiguration" => {
                        let w = c
                            .descendants()
                            .find(|d| d.tag_name().name() == "Width")
                            .and_then(|d| d.text())
                            .and_then(|t| t.parse().ok());
                        let h = c
                            .descendants()
                            .find(|d| d.tag_name().name() == "Height")
                            .and_then(|d| d.text())
                            .and_then(|t| t.parse().ok());
                        if let (Some(width), Some(height)) = (w, h) {
                            resolution = Some(Resolution { width, height });
                        }
                    }
                    _ => {}
                }
            }
            out.push(Profile {
                token,
                name,
                resolution,
            });
        }
        Ok(out)
    }

    async fn stream_uri(
        &self,
        media_xaddr: &str,
        token: &str,
        creds: &Creds,
    ) -> Result<String, ScanError> {
        let action = format!(
            r#"<trt:GetStreamUri xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:StreamSetup><tt:Stream xmlns:tt="http://www.onvif.org/ver10/schema">RTP-Unicast</tt:Stream><tt:Transport xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>"#,
            onvif::xml_escape(token),
        );
        let (text, status) = self.call(media_xaddr, &action, creds).await?;
        if status != 200 {
            return Err(ScanError::Malformed(format!("status {status}")));
        }
        let doc = onvif::parse_doc(&text)?;
        onvif::find_text(&doc, "Uri")
            .into_iter()
            .next()
            .and_then(|u| onvif::rewrite_host_ip(u, self.responder))
            .ok_or_else(|| ScanError::Malformed("no Uri".into()))
    }
}

/// Enumerate one device through the full chain (V222 order).
pub async fn enumerate(
    client: &mut OnvifHttp,
    device_xaddr: &str,
    creds: &Creds,
) -> Result<(Services, Vec<(Profile, String)>), ScanError> {
    let time = client.device_time(device_xaddr, creds).await;
    if let Ok(t) = time {
        client
            .soap
            .set_clock_offset(t.as_second() - jiff::Timestamp::now().as_second());
    }
    let services = client.services(device_xaddr, creds).await?;
    let mut streams = Vec::new();
    if let Some(media) = &services.media_xaddr {
        let media = media.clone();
        for p in client.profiles(&media, creds).await? {
            if let Ok(uri) = client.stream_uri(&media, &p.token, creds).await {
                streams.push((p, uri));
            }
        }
    }
    Ok((services, streams))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::{Router, body::Body, extract::State, http::Request as AxReq, routing::post};
    use pretty_assertions::assert_eq;
    use std::sync::{Arc, Mutex};

    const TIME_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetSystemDateAndTimeResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:SystemDateAndTime><tt:UTCDateTime xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Time><tt:Hour>12</tt:Hour><tt:Minute>30</tt:Minute><tt:Second>5</tt:Second></tt:Time><tt:Date><tt:Year>2026</tt:Year><tt:Month>1</tt:Month><tt:Day>2</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></s:Body></s:Envelope>"#;

    const CAPS_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Capabilities><tt:Media xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>http://0.0.0.0:{PORT}/onvif/media_service</tt:XAddr><tt:StreamingCapabilities><tt:RTSPStreaming>true</tt:RTSPStreaming></tt:StreamingCapabilities></tt:Media><tt:PTZ xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>http://0.0.0.0/onvif/ptz</tt:XAddr></tt:PTZ></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#;

    const PROFILES_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:Profiles token="p1"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Main</tt:Name><tt:VideoEncoderConfiguration xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Resolution><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration></trt:Profiles><trt:Profiles token="p2"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Sub</tt:Name><tt:VideoEncoderConfiguration xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Resolution><tt:Width>640</tt:Width><tt:Height>360</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration></trt:Profiles></trt:GetProfilesResponse></s:Body></s:Envelope>"#;

    const URI_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetStreamUriResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:MediaUri><tt:Uri xmlns:tt="http://www.onvif.org/ver10/schema">rtsp://0.0.0.0:{PORT}/stream1</tt:Uri></trt:MediaUri></trt:GetStreamUriResponse></s:Body></s:Envelope>"#;

    type Log = Arc<Mutex<Vec<String>>>;

    async fn stub(State((log, port)): State<(Log, u16)>, req: AxReq<Body>) -> String {
        let body = to_bytes(req.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&body).into_owned();
        let action = [
            "GetSystemDateAndTime",
            "GetCapabilities",
            "GetServices",
            "GetProfiles",
            "GetStreamUri",
        ]
        .into_iter()
        .find(|a| text.contains(a))
        .unwrap_or("unknown")
        .to_owned();
        log.lock().unwrap().push(action.clone());
        match action.as_str() {
            "GetSystemDateAndTime" => TIME_RESP.into(),
            "GetCapabilities" => CAPS_RESP.replace("{PORT}", &port.to_string()),
            "GetProfiles" => PROFILES_RESP.into(),
            "GetStreamUri" => URI_RESP.replace("{PORT}", &port.to_string()),
            _ => String::new(),
        }
    }

    async fn spawn_stub() -> (String, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/onvif/device_service", post(stub))
            .route("/onvif/media_service", post(stub))
            .with_state((log.clone(), addr.port()));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/onvif/device_service"), log)
    }

    #[tokio::test]
    async fn chain_order_and_xaddr_rewrite() {
        let (xaddr, log) = spawn_stub().await;
        let mut client = OnvifHttp::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let creds = Creds::default();
        let (services, streams) = enumerate(&mut client, &xaddr, &creds).await.unwrap();
        let calls = log.lock().unwrap().clone();
        assert_eq!(
            vec![
                "GetSystemDateAndTime",
                "GetCapabilities",
                "GetProfiles",
                "GetStreamUri",
                "GetStreamUri"
            ],
            calls.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert!(services.ptz);
        assert_eq!(
            Some(true),
            services
                .media_xaddr
                .map(|u| u.starts_with("http://127.0.0.1:") && u.contains("/onvif/media_service"))
        );
        assert_eq!(2, streams.len());
        assert_eq!("p1", streams[0].0.token);
        assert_eq!(
            Some(Resolution {
                width: 1920,
                height: 1080
            }),
            streams[0].0.resolution
        );
        assert!(streams[1].1.starts_with("rtsp://127.0.0.1:") && streams[1].1.contains("/stream1"));
    }

    #[tokio::test]
    async fn device_time_parsed() {
        let (xaddr, _log) = spawn_stub().await;
        let client = OnvifHttp::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let t = client.device_time(&xaddr, &Creds::default()).await.unwrap();
        assert_eq!("2026-01-02T12:30:05Z", t.to_string());
    }

    #[tokio::test]
    async fn digest_retry_on_401() {
        use axum::http::{HeaderMap, StatusCode, header};
        let attempts: Log = Arc::new(Mutex::new(Vec::new()));
        let log = attempts.clone();
        let app = Router::new().route(
            "/dev",
            post(move |headers: HeaderMap, _body: String| {
                let log = log.clone();
                async move {
                    let has_digest = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.starts_with("Digest"));
                    log.lock().unwrap().push(has_digest.to_string());
                    if has_digest {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "text/xml")],
                            TIME_RESP.to_owned(),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            [(
                                header::WWW_AUTHENTICATE,
                                r#"Digest realm="cam", nonce="n1", qop="auth""#,
                            )],
                            String::new(),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let xaddr = format!("http://{addr}/dev");
        let client = OnvifHttp::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let creds = Creds {
            username: "admin".into(),
            password: "pw".into(),
        };
        client.device_time(&xaddr, &creds).await.unwrap();
        assert_eq!(vec!["false", "true"], *attempts.lock().unwrap());
    }
}
