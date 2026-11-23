// SPDX-License-Identifier: GPL-2.0-or-later

//! Wire and seam types for the camera scan.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// `POST /api/scan` request body.
///
/// `host` absent or empty = LAN multicast scan; present = per-IP probe.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRequest {
    /// Single IPv4 literal. Empty string is treated as absent.
    #[serde(default)]
    pub host: String,

    /// Optional camera credentials, request-scoped.
    #[serde(default)]
    pub username: String,

    /// Optional camera credentials, request-scoped.
    #[serde(default)]
    pub password: String,
}

impl ScanRequest {
    /// Parse `host` into a per-IP target.
    ///
    /// # Errors
    /// [`ScanError::InvalidHost`] when the string is not an IPv4 literal.
    pub fn target(&self) -> Result<Option<Ipv4Addr>, crate::ScanError> {
        if self.host.is_empty() {
            return Ok(None);
        }
        self.host
            .parse()
            .map(Some)
            .map_err(|_| crate::ScanError::InvalidHost(self.host.clone()))
    }
}

/// `POST /api/scan` response body.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// Discovered devices, including zero-stream devices.
    pub cameras: Vec<DiscoveredCamera>,

    /// `true` when the deadline cut the scan short — results are partial,
    /// completed devices only.
    pub truncated: bool,
}

/// A discovered camera as returned to the FE.
#[allow(clippy::struct_excessive_bools)] // serde DTO — flags stay flat on the wire
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredCamera {
    /// Responder IPv4 address.
    pub ip: Ipv4Addr,

    /// Device name from WS-Discovery scopes, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Hardware/model from WS-Discovery scopes, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,

    /// Device service addresses, host rewritten to the responder IP.
    pub xaddrs: Vec<String>,

    /// Candidate RTSP streams.
    pub streams: Vec<ScanStream>,

    /// Capability flag, record-only.
    pub ptz: bool,

    /// Capability flag, record-only.
    pub backchannel: bool,

    /// VIGI `:8800` private-talk probe outcome flag, record-only —
    /// `true` records a succeeded handshake, ⊥ capability proof
    /// (bad creds ∨ a busy talker also read `false`).
    pub vigi_talk: bool,

    /// Any enumerated surface answered `401`/`NotAuthorized`.
    pub needs_auth: bool,
}

/// A candidate RTSP stream.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStream {
    /// ONVIF profile name, when profile-sourced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,

    /// Profile resolution, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,

    /// `rtsp://` URL, always `RtspUrl`-parseable; may carry userinfo.
    pub rtsp_url: String,

    /// Suggested monitor stream slot.
    pub role: ScanRole,

    /// Auth scheme token recorded when a surface answered `401`
    /// (`basic`/`digest`) or ONVIF `NotAuthorized` (`wsse`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,

    /// SDP video `encoding_name` from the DESCRIBE probe (`h264`,
    /// `h265`, …). `"none"` when the SDP carries no video stream,
    /// `"unknown"` when the probe did not complete.
    pub video_codec: String,

    /// `false` when the post-dedupe PLAY probe saw zero media inside
    /// its window — a proven-dead stream. `true` when probed alive or
    /// never probed (DESCRIBE-failed retained, deadline expired).
    pub live: bool,
}

/// Pixel resolution of a profile.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// Suggested monitor stream slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanRole {
    Main,
    Sub,
    Extra,
}

/// Raw WS-Discovery `ProbeMatch`, pre-assembly.
#[derive(Clone, Debug)]
pub struct DiscoveredDevice {
    /// EPR `urn:uuid` for dedupe.
    pub epr: String,

    /// `XAddrs` as reported (rewritten to responder IP).
    pub xaddrs: Vec<String>,

    /// Source IPv4 of the reply.
    pub ip: Ipv4Addr,

    /// `onvif://…/name/…` scope value, when present.
    pub name: Option<String>,

    /// `onvif://…/hardware/…` scope value, when present.
    pub hardware: Option<String>,
}

/// ONVIF profile, pre-assembly.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Profile token used by `GetStreamUri`.
    pub token: String,

    /// Display name.
    pub name: Option<String>,

    /// Video resolution when advertised.
    pub resolution: Option<Resolution>,
}

/// Service/capability endpoint set.
#[derive(Clone, Debug, Default)]
pub struct Services {
    /// Media service `XAddr` (host rewritten to responder).
    pub media_xaddr: Option<String>,

    /// PTZ support flag.
    pub ptz: bool,

    /// Backchannel (two-way audio) support flag.
    pub backchannel: bool,
}

/// Camera credentials as supplied in [`ScanRequest`].
#[derive(Clone, Debug, Default)]
pub struct Creds {
    pub username: String,
    pub password: String,
}

impl Creds {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.username.is_empty() && self.password.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn scan_request_camel_case() {
        let req: ScanRequest =
            serde_json::from_str(r#"{"host":"192.168.1.10","username":"u","password":"p"}"#)
                .unwrap();
        assert_eq!("192.168.1.10", req.host);
        let req2: ScanRequest = serde_json::from_str(r"{}").unwrap();
        assert_eq!("", req2.host);
    }

    #[test]
    fn scan_request_target() {
        let req = ScanRequest::default();
        assert_eq!(None, req.target().unwrap());
        let req = ScanRequest {
            host: "192.168.1.10".into(),
            ..Default::default()
        };
        assert_eq!(Some(Ipv4Addr::new(192, 168, 1, 10)), req.target().unwrap());
        let req = ScanRequest {
            host: "cam.local".into(),
            ..Default::default()
        };
        assert!(matches!(
            req.target(),
            Err(crate::ScanError::InvalidHost(_))
        ));
        let req = ScanRequest {
            host: "::1".into(),
            ..Default::default()
        };
        assert!(matches!(
            req.target(),
            Err(crate::ScanError::InvalidHost(_))
        ));
    }

    #[test]
    fn scan_result_camel_case() {
        let res = ScanResult {
            cameras: vec![DiscoveredCamera {
                ip: Ipv4Addr::new(10, 0, 0, 1),
                name: Some("Cam".into()),
                hardware: None,
                xaddrs: vec![],
                streams: vec![ScanStream {
                    profile_name: None,
                    resolution: Some(Resolution {
                        width: 1920,
                        height: 1080,
                    }),
                    rtsp_url: "rtsp://10.0.0.1/x".into(),
                    role: ScanRole::Main,
                    auth: Some("digest".into()),
                    video_codec: "h264".into(),
                    live: true,
                }],
                ptz: true,
                backchannel: false,
                vigi_talk: true,
                needs_auth: true,
            }],
            truncated: true,
        };
        let json = serde_json::to_value(&res).unwrap();
        assert_eq!(true, json["truncated"]);
        let cam = &json["cameras"][0];
        assert_eq!("10.0.0.1", cam["ip"]);
        assert_eq!(true, cam["needsAuth"]);
        assert_eq!(true, cam["vigiTalk"]);
        let stream = &cam["streams"][0];
        assert_eq!("rtsp://10.0.0.1/x", stream["rtspUrl"]);
        assert_eq!("main", stream["role"]);
        assert_eq!("digest", stream["auth"]);
        assert_eq!(1920, stream["resolution"]["width"]);
        assert!(stream.get("profileName").is_none());
        assert!(cam.get("hardware").is_none());
    }

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!("\"main\"", serde_json::to_string(&ScanRole::Main).unwrap());
        assert_eq!("\"sub\"", serde_json::to_string(&ScanRole::Sub).unwrap());
        assert_eq!(
            "\"extra\"",
            serde_json::to_string(&ScanRole::Extra).unwrap()
        );
    }
}

/// Rewrite a URI's host to `ip`, preserving port and path. Unparseable
/// input returns unchanged.
#[must_use]
pub fn rewrite_ip_host(uri: &str, ip: std::net::IpAddr) -> String {
    url::Url::parse(uri).map_or_else(
        |_| uri.to_owned(),
        |mut u| {
            if u.set_ip_host(ip).is_ok() {
                u.to_string()
            } else {
                uri.to_owned()
            }
        },
    )
}
