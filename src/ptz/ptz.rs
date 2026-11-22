// SPDX-License-Identifier: GPL-2.0-or-later

//! ONVIF PTZ client — per-command session: endpoint resolution →
//! device time → service discovery → profile token → one PTZ op,
//! all inside a shared chain deadline.

use async_trait::async_trait;
use common::monitor::PtzConfig;
use std::time::{Duration, Instant};
use thiserror::Error;
use url::Url;

/// Whole chain (time + services + profiles + op) bound from dispatch.
const CHAIN_LIMIT: Duration = Duration::from_secs(5);
/// Per-op budget cap — the op also draws down the chain clock.
const OP_LIMIT: Duration = Duration::from_secs(2);
/// Camera-side runaway bound carried in `ContinuousMove`.
const CAMERA_TIMEOUT: &str = "PT2S";

/// Failures the route maps to `502` — everything here is a
/// camera/transport fault, never a client error.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Transport(#[from] onvif::Error),

    #[error("cannot derive ONVIF endpoint")]
    NoEndpoint,

    #[error("no media service advertised")]
    NoMediaService,

    #[error("no PTZ service advertised")]
    NoPtzService,

    #[error("no media profiles")]
    NoProfile,

    #[error("no profile with PTZ configuration")]
    NoPtzProfile,

    #[error("camera fault: {0}")]
    Fault(String),

    #[error("velocity out of range −1.0..=1.0")]
    BadVelocity,
}

/// PTZ command surface — mockable seam for route tests.
#[async_trait]
pub trait PtzClient: Send + Sync {
    /// Continuous move; absent velocities leave that axis untouched.
    async fn move_(
        &self,
        pan: Option<f64>,
        tilt: Option<f64>,
        zoom: Option<f64>,
    ) -> Result<(), Error>;

    /// Stop all axes.
    async fn stop(&self) -> Result<(), Error>;

    /// Move to the camera's home position.
    async fn goto_home(&self) -> Result<(), Error>;
}

/// Resolve the ONVIF device-service endpoint: a valid stored
/// `onvifAddr` verbatim, otherwise derived from the main-stream host.
/// An invalid stored value is treated as empty per config rules.
pub fn resolve_endpoint(main_stream: &str, onvif_addr: &str) -> Result<String, Error> {
    if !onvif_addr.is_empty() && PtzConfig::new(false, onvif_addr.to_owned()).onvif_addr_ok() {
        return Ok(onvif_addr.to_owned());
    }
    let url = Url::parse(main_stream).map_err(|_| Error::NoEndpoint)?;
    let host = url.host_str().ok_or(Error::NoEndpoint)?;
    Ok(format!("http://{host}:80/onvif/device_service"))
}

/// ONVIF PTZ client over the shared SOAP transport.
pub struct OnvifPtz {
    soap: onvif::SoapClient,
    device: String,
    user: String,
    pass: String,
}

/// Resolved session: discovered endpoints + profile token.
struct Session {
    ptz_xaddr: String,
    profile_token: String,
    deadline: Instant,
}

impl OnvifPtz {
    #[must_use]
    pub fn new(device: String, creds: (String, String)) -> Self {
        Self {
            soap: onvif::SoapClient::new(),
            device,
            user: creds.0,
            pass: creds.1,
        }
    }

    fn creds(&self) -> Option<(&str, &str)> {
        if self.user.is_empty() && self.pass.is_empty() {
            None
        } else {
            Some((&self.user, &self.pass))
        }
    }

    /// Re-anchor a device-reported `XAddr` to the connected endpoint
    /// host. Relative URLs resolve against the device URL; non-`http`
    /// schemes are skipped.
    fn anchor(&self, xaddr: &str) -> Option<String> {
        let device = Url::parse(&self.device).ok()?;
        let mut url = Url::parse(xaddr).ok().or_else(|| device.join(xaddr).ok())?;
        if url.scheme() != "http" {
            return None;
        }
        url.set_host(device.host_str()).ok()?;
        Some(url.to_string())
    }

    async fn call(
        &self,
        xaddr: &str,
        body: &str,
        deadline: Instant,
    ) -> Result<(String, u16), Error> {
        Ok(self
            .soap
            .soap_call_deadline(xaddr, body, self.creds(), deadline)
            .await?)
    }

    /// Runs the full resolution chain and returns the session.
    /// The chain clock starts here — `Instant::now() + CHAIN_LIMIT`.
    async fn session(&self) -> Result<Session, Error> {
        let deadline = Instant::now() + CHAIN_LIMIT;
        self.device_time(deadline).await;
        let (media_xaddr, ptz_xaddr) = self.services(deadline).await?;
        let profile_token = self.ptz_profile_token(&media_xaddr, deadline).await?;
        Ok(Session {
            ptz_xaddr,
            profile_token,
            deadline,
        })
    }

    /// Best-effort device-time sync for WSSE `Created`.
    /// Any failure leaves the clock offset at 0.
    async fn device_time(&self, deadline: Instant) {
        let body =
            r#"<tds:GetSystemDateAndTime xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>"#;
        let Ok((text, 200)) = self.call(&self.device.clone(), body, deadline).await else {
            return;
        };
        let Ok(doc) = onvif::parse_doc(&text) else {
            return;
        };
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
            return;
        };
        let Ok(device_ts) =
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z").parse::<jiff::Timestamp>()
        else {
            return;
        };
        let offset = device_ts.as_second() - jiff::Timestamp::now().as_second();
        self.soap.set_clock_offset(offset);
    }

    /// `GetCapabilities` → media + PTZ `XAddr`s; missing or faulty
    /// results fall back to `GetServices`.
    async fn services(&self, deadline: Instant) -> Result<(String, String), Error> {
        let mut media = None;
        let mut ptz = None;

        let caps = self
            .call(
                &self.device.clone(),
                r#"<tds:GetCapabilities xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Category>All</tds:Category></tds:GetCapabilities>"#,
                deadline,
            )
            .await;
        if let Ok((text, 200)) = caps
            && let Ok(doc) = onvif::parse_doc(&text)
        {
            for n in doc.descendants() {
                if !n.is_element() {
                    continue;
                }
                let xaddr = n
                    .children()
                    .find(|c| c.is_element() && c.tag_name().name() == "XAddr")
                    .and_then(|c| c.text())
                    .and_then(|x| self.anchor(x));
                match n.tag_name().name() {
                    "Media" => media = xaddr,
                    "PTZ" => ptz = xaddr,
                    _ => {}
                }
            }
        }

        if media.is_none() || ptz.is_none() {
            self.services_fallback(&mut media, &mut ptz, deadline).await;
        }

        Ok((
            media.ok_or(Error::NoMediaService)?,
            ptz.ok_or(Error::NoPtzService)?,
        ))
    }

    /// `GetServices` fallback — fills whichever `XAddr`s are missing.
    async fn services_fallback(
        &self,
        media: &mut Option<String>,
        ptz: &mut Option<String>,
        deadline: Instant,
    ) {
        let Ok((text, 200)) = self
            .call(
                &self.device.clone(),
                r#"<tds:GetServices xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>"#,
                deadline,
            )
            .await
        else {
            return;
        };
        let Ok(doc) = onvif::parse_doc(&text) else {
            return;
        };
        for s in doc.descendants() {
            if !(s.is_element() && s.tag_name().name() == "Service") {
                continue;
            }
            let ns = s
                .children()
                .find(|c| c.is_element() && c.tag_name().name() == "Namespace")
                .and_then(|c| c.text())
                .unwrap_or("");
            let Some(xaddr) = s
                .children()
                .find(|c| c.is_element() && c.tag_name().name() == "XAddr")
                .and_then(|c| c.text())
                .and_then(|x| self.anchor(x))
            else {
                continue;
            };
            if media.is_none() && ns.contains("media") {
                *media = Some(xaddr);
            } else if ptz.is_none() && ns.contains("ptz") {
                *ptz = Some(xaddr);
            }
        }
    }

    /// `GetProfiles` → token of the first profile carrying a
    /// `PTZConfiguration` element.
    async fn ptz_profile_token(
        &self,
        media_xaddr: &str,
        deadline: Instant,
    ) -> Result<String, Error> {
        let (text, status) = self
            .call(
                media_xaddr,
                r#"<trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>"#,
                deadline,
            )
            .await?;
        if status != 200 {
            return Err(fault_or(&text, status));
        }
        let doc = onvif::parse_doc(&text)?;
        let mut saw_profile = false;
        for p in doc.descendants() {
            if !(p.is_element() && p.tag_name().name() == "Profiles") {
                continue;
            }
            saw_profile = true;
            let has_ptz = p
                .descendants()
                .any(|d| d.is_element() && d.tag_name().name() == "PTZConfiguration");
            if has_ptz {
                return Ok(p.attribute("token").unwrap_or_default().to_owned());
            }
        }
        Err(if saw_profile {
            Error::NoPtzProfile
        } else {
            Error::NoProfile
        })
    }

    /// Session + one op inside its own `min(OP_LIMIT, remaining)`
    /// budget. Returns the reply text for ops that parse one.
    async fn op(&self, body_fn: impl Fn(&Session) -> String) -> Result<String, Error> {
        let session = self.session().await?;
        let remaining = session.deadline.saturating_duration_since(Instant::now());
        let dl = Instant::now() + remaining.min(OP_LIMIT);
        let (text, status) = self
            .call(&session.ptz_xaddr, &body_fn(&session), dl)
            .await?;
        if status != 200 {
            return Err(fault_or(&text, status));
        }
        Ok(text)
    }
}

#[async_trait]
impl PtzClient for OnvifPtz {
    async fn move_(
        &self,
        pan: Option<f64>,
        tilt: Option<f64>,
        zoom: Option<f64>,
    ) -> Result<(), Error> {
        for v in [pan, tilt, zoom].into_iter().flatten() {
            if !(-1.0..=1.0).contains(&v) {
                return Err(Error::BadVelocity);
            }
        }
        self.op(|s| {
            let pantilt = if pan.is_some() || tilt.is_some() {
                format!(
                    r#"<tt:PanTilt xmlns:tt="http://www.onvif.org/ver10/schema" x="{}" y="{}"/>"#,
                    pan.unwrap_or(0.0),
                    tilt.unwrap_or(0.0),
                )
            } else {
                String::new()
            };
            let zoom_el = zoom.map_or_else(String::new, |z| {
                format!(r#"<tt:Zoom xmlns:tt="http://www.onvif.org/ver10/schema" x="{z}"/>"#)
            });
            let velocity = pantilt + &zoom_el;
            format!(
                r#"<tptz:ContinuousMove xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity>{velocity}</tptz:Velocity><tptz:Timeout>{CAMERA_TIMEOUT}</tptz:Timeout></tptz:ContinuousMove>"#,
                onvif::xml_escape(&s.profile_token),
            )
        })
        .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), Error> {
        self.op(|s| {
            format!(
                r#"<tptz:Stop xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>"#,
                onvif::xml_escape(&s.profile_token),
            )
        })
        .await?;
        Ok(())
    }

    async fn goto_home(&self) -> Result<(), Error> {
        self.op(|s| {
            format!(
                r#"<tptz:GotoHomePosition xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl"><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GotoHomePosition>"#,
                onvif::xml_escape(&s.profile_token),
            )
        })
        .await?;
        Ok(())
    }
}

/// Translate a non-200 reply into the spec'd fault taxonomy:
/// `ter:NoProfile`/`ter:NoPTZProfile` → their 502-class variants,
/// anything else a generic fault.
fn fault_or(text: &str, status: u16) -> Error {
    if let Ok(doc) = onvif::parse_doc(text) {
        for v in onvif::find_text(&doc, "Value") {
            if v.ends_with(":NoProfile") || v == "NoProfile" {
                return Error::NoProfile;
            }
            if v.ends_with(":NoPTZProfile") || v == "NoPTZProfile" {
                return Error::NoPtzProfile;
            }
        }
        if let Some(reason) = onvif::find_text(&doc, "Text").into_iter().next() {
            return Error::Fault(reason.to_owned());
        }
    }
    Error::Fault(format!("status {status}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::{Router, body::Body, extract::Request as AxReq, extract::State, routing::post};
    use pretty_assertions::assert_eq;
    use std::sync::{Arc, Mutex};

    const TIME_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetSystemDateAndTimeResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:SystemDateAndTime><tt:UTCDateTime xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Time><tt:Hour>12</tt:Hour><tt:Minute>30</tt:Minute><tt:Second>5</tt:Second></tt:Time><tt:Date><tt:Year>2026</tt:Year><tt:Month>1</tt:Month><tt:Day>2</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></s:Body></s:Envelope>"#;

    const CAPS_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Capabilities><tt:Media xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>http://0.0.0.0:{PORT}/onvif/media_service</tt:XAddr></tt:Media><tt:PTZ xmlns:tt="http://www.onvif.org/ver10/schema"><tt:XAddr>http://0.0.0.0:{PORT}/onvif/ptz_service</tt:XAddr></tt:PTZ></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#;

    const SERVICES_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetServicesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Service><tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace><tds:XAddr>http://0.0.0.0:{PORT}/onvif/media_service</tds:XAddr></tds:Service><tds:Service><tds:Namespace>http://www.onvif.org/ver20/ptz/wsdl</tds:Namespace><tds:XAddr>http://0.0.0.0:{PORT}/onvif/ptz_service</tds:XAddr></tds:Service></tds:GetServicesResponse></s:Body></s:Envelope>"#;

    const PROFILES_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:Profiles token="plain"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Main</tt:Name></trt:Profiles><trt:Profiles token="ptz1"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Sub</tt:Name><tt:PTZConfiguration xmlns:tt="http://www.onvif.org/ver10/schema"><tt:Name>ptz</tt:Name></tt:PTZConfiguration></trt:Profiles></trt:GetProfilesResponse></s:Body></s:Envelope>"#;

    const PROFILES_NO_PTZ: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><trt:Profiles token="plain"><tt:Name xmlns:tt="http://www.onvif.org/ver10/schema">Main</tt:Name></trt:Profiles></trt:GetProfilesResponse></s:Body></s:Envelope>"#;

    const FAULT_NO_PTZ_PROFILE: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:NoPTZProfile</s:Value></s:Subcode></s:Code><s:Reason><s:Text>ptz profile missing</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#;

    const OK_RESP: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body/></s:Envelope>"#;

    type Log = Arc<Mutex<Vec<String>>>;

    struct Stub {
        log: Log,
        bodies: Log,
        port: std::sync::atomic::AtomicU16,
        caps_faults: bool,
        no_ptz_profile: bool,
        fault_ops: bool,
    }

    async fn handle(
        State(stub): State<Arc<Stub>>,
        req: AxReq<Body>,
    ) -> (axum::http::StatusCode, String) {
        let body = to_bytes(req.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&body).into_owned();
        let action = [
            "GetSystemDateAndTime",
            "GetCapabilities",
            "GetServices",
            "GetProfiles",
            "ContinuousMove",
            "GotoHomePosition",
            "Stop",
        ]
        .into_iter()
        .find(|a| text.contains(a))
        .unwrap_or("unknown")
        .to_owned();
        stub.log.lock().unwrap().push(action.clone());
        stub.bodies.lock().unwrap().push(text);
        match action.as_str() {
            "GetSystemDateAndTime" => (axum::http::StatusCode::OK, TIME_RESP.into()),
            "GetCapabilities" if stub.caps_faults => {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, String::new())
            }
            "GetCapabilities" => (
                axum::http::StatusCode::OK,
                CAPS_RESP.replace(
                    "{PORT}",
                    &stub
                        .port
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .to_string(),
                ),
            ),
            "GetServices" => (
                axum::http::StatusCode::OK,
                SERVICES_RESP.replace(
                    "{PORT}",
                    &stub
                        .port
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .to_string(),
                ),
            ),
            "GetProfiles" if stub.no_ptz_profile => {
                (axum::http::StatusCode::OK, PROFILES_NO_PTZ.into())
            }
            "GetProfiles" => (axum::http::StatusCode::OK, PROFILES_RESP.into()),
            _ if stub.fault_ops => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                FAULT_NO_PTZ_PROFILE.into(),
            ),
            _ => (axum::http::StatusCode::OK, OK_RESP.into()),
        }
    }

    async fn spawn_stub(stub: Arc<Stub>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        stub.port
            .store(addr.port(), std::sync::atomic::Ordering::Relaxed);
        let app = Router::new()
            .route("/onvif/device_service", post(handle))
            .route("/onvif/media_service", post(handle))
            .route("/onvif/ptz_service", post(handle))
            .with_state(stub);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/onvif/device_service")
    }

    fn new_stub() -> Arc<Stub> {
        Arc::new(Stub {
            log: Arc::new(Mutex::new(Vec::new())),
            bodies: Arc::new(Mutex::new(Vec::new())),
            port: std::sync::atomic::AtomicU16::new(0),
            caps_faults: false,
            no_ptz_profile: false,
            fault_ops: false,
        })
    }

    #[test]
    fn resolve_endpoint_cases() {
        // Stored addr used verbatim.
        assert_eq!(
            "http://cam:8080/onvif/device_service",
            resolve_endpoint("rtsp://cam/main", "http://cam:8080/onvif/device_service").unwrap()
        );
        // Empty + invalid stored addr → derive.
        assert_eq!(
            "http://cam:80/onvif/device_service",
            resolve_endpoint("rtsp://cam/main", "").unwrap()
        );
        assert_eq!(
            "http://cam:80/onvif/device_service",
            resolve_endpoint("rtsp://cam/main", "https://cam").unwrap()
        );
        // Unparseable main stream → no endpoint.
        assert!(matches!(
            resolve_endpoint("not a url", ""),
            Err(Error::NoEndpoint)
        ));
    }

    #[tokio::test]
    async fn move_chain_order_and_token() {
        let stub = new_stub();
        let device = spawn_stub(stub.clone()).await;
        let client = OnvifPtz::new(device, (String::new(), String::new()));

        client.move_(Some(0.5), Some(-0.25), None).await.unwrap();

        let log = stub.log.lock().unwrap().clone();
        assert_eq!(
            vec![
                "GetSystemDateAndTime",
                "GetCapabilities",
                "GetProfiles",
                "ContinuousMove",
            ],
            log
        );

        let bodies = stub.bodies.lock().unwrap().clone();
        let mv = bodies.last().unwrap();
        // PTZConfiguration profile wins over the first plain profile.
        assert!(mv.contains("<tptz:ProfileToken>ptz1</tptz:ProfileToken>"));
        assert!(mv.contains(r#"x="0.5" y="-0.25""#));
        assert!(mv.contains("<tptz:Timeout>PT2S</tptz:Timeout>"));
        // Zoom-only absent → no Zoom element.
        assert!(!mv.contains("tt:Zoom"));
    }

    #[tokio::test]
    async fn move_zoom_only_omits_pantilt() {
        let stub = new_stub();
        let device = spawn_stub(stub.clone()).await;
        let client = OnvifPtz::new(device, (String::new(), String::new()));

        client.move_(None, None, Some(0.8)).await.unwrap();

        let bodies = stub.bodies.lock().unwrap().clone();
        let mv = bodies.last().unwrap();
        assert!(!mv.contains("tt:PanTilt"));
        assert!(mv.contains(r#"tt:Zoom xmlns:tt="http://www.onvif.org/ver10/schema" x="0.8""#));
    }

    #[tokio::test]
    async fn stop_is_all_axes() {
        let stub = new_stub();
        let device = spawn_stub(stub.clone()).await;
        let client = OnvifPtz::new(device, (String::new(), String::new()));

        client.stop().await.unwrap();

        let bodies = stub.bodies.lock().unwrap().clone();
        let stop = bodies.last().unwrap();
        assert!(stop.contains("<tptz:PanTilt>true</tptz:PanTilt>"));
        assert!(stop.contains("<tptz:Zoom>true</tptz:Zoom>"));
    }

    #[tokio::test]
    async fn capabilities_fault_falls_back_to_services() {
        let mut stub = new_stub();
        Arc::get_mut(&mut stub).unwrap().caps_faults = true;
        let device = spawn_stub(stub.clone()).await;
        let client = OnvifPtz::new(device, (String::new(), String::new()));

        client.goto_home().await.unwrap();

        let log = stub.log.lock().unwrap().clone();
        assert_eq!(
            vec![
                "GetSystemDateAndTime",
                "GetCapabilities",
                "GetServices",
                "GetProfiles",
                "GotoHomePosition",
            ],
            log
        );
    }

    #[tokio::test]
    async fn no_ptz_profile_errors() {
        let mut stub = new_stub();
        Arc::get_mut(&mut stub).unwrap().no_ptz_profile = true;
        let device = spawn_stub(stub.clone()).await;
        let client = OnvifPtz::new(device, (String::new(), String::new()));

        assert!(matches!(
            client.move_(Some(0.1), None, None).await,
            Err(Error::NoPtzProfile)
        ));
    }

    #[tokio::test]
    async fn op_fault_maps_no_ptz_profile() {
        let mut stub = new_stub();
        Arc::get_mut(&mut stub).unwrap().fault_ops = true;
        let device = spawn_stub(stub.clone()).await;
        let client = OnvifPtz::new(device, (String::new(), String::new()));

        assert!(matches!(client.goto_home().await, Err(Error::NoPtzProfile)));
    }

    #[tokio::test]
    async fn bad_velocity_rejected() {
        let client = OnvifPtz::new(
            "http://x/onvif/device_service".to_owned(),
            (String::new(), String::new()),
        );
        assert!(matches!(
            client.move_(Some(1.5), None, None).await,
            Err(Error::BadVelocity)
        ));
        assert!(matches!(
            client.move_(None, Some(-1.1), None).await,
            Err(Error::BadVelocity)
        ));
    }
}
