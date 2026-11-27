// SPDX-License-Identifier: GPL-2.0-or-later

//! Merge ONVIF streams, vendor fast-path hits and path-probe hits into
//! `DiscoveredCamera` rows: filter, dedupe, role-assign, creds-embed.

use crate::{
    rtsp::PathHit,
    types::{Creds, DiscoveredCamera, Profile, Resolution, ScanRole, ScanStream},
    vendor::VendorHit,
};
use std::net::Ipv4Addr;

/// Everything the assembler needs for one device.
pub struct Input {
    /// ONVIF `(profile, stream-uri)` pairs from enumeration.
    pub onvif: Vec<(Profile, String)>,
    /// Whether ONVIF enumeration required credentials.
    pub onvif_auth: bool,
    /// Vendor fast-path hits.
    pub vendor: Vec<VendorHit>,
    /// RTSP path-probe hits.
    pub paths: Vec<PathHit>,
    /// User-supplied credentials (request-scoped).
    pub creds: Creds,
    /// Any enumerated surface answered 401/NotAuthorized (including
    /// unauthed scans that could not enumerate).
    pub needs_auth: bool,
}

struct Cand {
    url: String,
    profile_name: Option<String>,
    resolution: Option<Resolution>,
    hint: ScanRole,
    auth: Option<String>,
    video_codec: Option<String>,
}

fn vendor_role(path: &str) -> ScanRole {
    if path.contains("subtype=0") {
        return ScanRole::Main;
    }
    if path.contains("subtype=1") {
        return ScanRole::Sub;
    }
    // Hikvision ids are {channel}0{subtype}: x01 main, x02 sub.
    if let Some(id) = path.rsplit('/').next().and_then(|t| t.parse::<u32>().ok()) {
        match id % 100 {
            1 => return ScanRole::Main,
            2 => return ScanRole::Sub,
            _ => {}
        }
    }
    ScanRole::Extra
}

/// Normalize for dedupe: creds-free, explicit default port.
fn normalize(url: &str) -> Option<(url::Url, String)> {
    let mut u = url::Url::parse(url).ok()?;
    if u.scheme() != "rtsp" {
        return None;
    }
    let _ = u.set_username("");
    let _ = u.set_password(None);
    let key = format!(
        "{}://{}:{}{}",
        u.scheme(),
        u.host_str()?,
        u.port().unwrap_or(554),
        u.path()
    );
    Some((u, key))
}

/// Build candidates from all probe sources; returns them plus the
/// auth-required flag folded in.
fn collect_cands(ip: Ipv4Addr, input: &Input) -> (Vec<Cand>, bool) {
    let mut cands = Vec::new();
    let mut needs_auth = input.needs_auth;

    for (profile, uri) in &input.onvif {
        cands.push(Cand {
            url: uri.clone(),
            profile_name: profile.name.clone(),
            resolution: profile.resolution,
            hint: ScanRole::Extra,
            auth: input.onvif_auth.then(|| "wsse".to_owned()),
            video_codec: None,
        });
    }
    if input.onvif_auth {
        needs_auth = true;
    }
    for hit in &input.vendor {
        if hit.path.is_empty() {
            needs_auth = true;
            continue;
        }
        cands.push(Cand {
            url: format!("rtsp://{ip}:554/{}", hit.path),
            profile_name: None,
            resolution: None,
            hint: vendor_role(&hit.path),
            auth: hit.auth.clone(),
            video_codec: None,
        });
    }
    for hit in &input.paths {
        if hit.auth.is_some() {
            needs_auth = true;
        }
        cands.push(Cand {
            url: format!("rtsp://{ip}:554/{}", hit.path),
            profile_name: None,
            resolution: None,
            hint: hit.role,
            auth: hit.auth.clone(),
            video_codec: hit.codec.clone(),
        });
    }
    (cands, needs_auth)
}

/// Assemble one `DiscoveredCamera` — pure, no I/O.
#[must_use]
pub fn assemble(
    ip: Ipv4Addr,
    name: Option<String>,
    hardware: Option<String>,
    xaddrs: Vec<String>,
    ptz: bool,
    backchannel: bool,
    input: &Input,
) -> DiscoveredCamera {
    let (cands, needs_auth) = collect_cands(ip, input);

    // filter + dedupe by normalized creds-free URL; a duplicate
    // carrying a probed codec fills it in on the kept candidate.
    let mut seen = std::collections::HashMap::new();
    let mut uniq: Vec<(url::Url, Cand)> = Vec::new();
    for c in cands {
        let Some((u, key)) = normalize(&c.url) else {
            continue;
        };
        match seen.entry(key) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(uniq.len());
                uniq.push((u, c));
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                let i = *o.get();
                if uniq[i].1.video_codec.is_none() {
                    uniq[i].1.video_codec = c.video_codec;
                }
            }
        }
    }

    // Role order: explicit main-hints first, then resolution desc,
    // then url for a deterministic tiebreak.
    uniq.sort_by(|(ua, a), (ub, b)| {
        let rank = |r: &ScanRole| match r {
            ScanRole::Main => 0,
            ScanRole::Sub => 1,
            ScanRole::Extra => 2,
        };
        rank(&a.hint)
            .cmp(&rank(&b.hint))
            .then(b.resolution.cmp(&a.resolution))
            .then(ua.as_str().cmp(ub.as_str()))
    });

    let streams = uniq
        .into_iter()
        .enumerate()
        .map(|(i, (mut u, c))| {
            let role = match i {
                0 => ScanRole::Main,
                1 => ScanRole::Sub,
                _ => ScanRole::Extra,
            };
            // creds embed: only streams whose enumeration needed auth
            if c.auth.is_some() && !input.creds.is_empty() {
                let _ = u.set_username(&input.creds.username);
                let _ = u.set_password(Some(&input.creds.password));
            }
            ScanStream {
                profile_name: c.profile_name,
                resolution: c.resolution,
                rtsp_url: u.to_string(),
                role,
                auth: c.auth,
                video_codec: c.video_codec.unwrap_or_else(|| "unknown".to_owned()),
                live: true,
            }
        })
        .collect();

    DiscoveredCamera {
        ip,
        name,
        hardware,
        xaddrs,
        streams,
        ptz,
        backchannel,
        needs_auth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::net::Ipv4Addr;

    fn ip() -> Ipv4Addr {
        "192.168.1.9".parse().unwrap()
    }

    fn profile(tok: &str, w: u32, h: u32) -> (Profile, String) {
        (
            Profile {
                token: tok.into(),
                name: Some(format!("prof-{tok}")),
                resolution: Some(Resolution {
                    width: w,
                    height: h,
                }),
            },
            format!("rtsp://u:p@{}/{tok}", ip()),
        )
    }

    fn empty() -> Input {
        Input {
            onvif: Vec::new(),
            onvif_auth: false,
            vendor: Vec::new(),
            paths: Vec::new(),
            creds: Creds::default(),
            needs_auth: false,
        }
    }

    #[test]
    fn roles_by_resolution_then_deterministic() {
        let mut i = empty();
        i.onvif = vec![profile("sub", 640, 360), profile("main", 1920, 1080)];
        let cam = assemble(ip(), None, None, vec![], false, false, &i);
        assert_eq!(ScanRole::Main, cam.streams[0].role);
        assert_eq!(
            Some(Resolution {
                width: 1920,
                height: 1080
            }),
            cam.streams[0].resolution
        );
        assert_eq!(ScanRole::Sub, cam.streams[1].role);
        // creds-free: userinfo stripped
        assert_eq!(format!("rtsp://{}/main", ip()), cam.streams[0].rtsp_url);
    }

    #[test]
    fn dedupes_across_sources() {
        let mut i = empty();
        i.onvif = vec![profile("x", 1920, 1080)];
        i.paths = vec![PathHit {
            path: "x".into(),
            role: ScanRole::Main,
            auth: None,
            codec: None,
        }];
        let cam = assemble(ip(), None, None, vec![], false, false, &i);
        assert_eq!(1, cam.streams.len());
    }

    #[test]
    fn creds_embed_only_on_auth_required() {
        let mut i = empty();
        i.creds = Creds {
            username: "admin".into(),
            password: "p@ss".into(),
        };
        i.paths = vec![
            PathHit {
                path: "a".into(),
                role: ScanRole::Main,
                auth: Some("digest".into()),
                codec: None,
            },
            PathHit {
                path: "b".into(),
                role: ScanRole::Sub,
                auth: None,
                codec: None,
            },
        ];
        let cam = assemble(ip(), None, None, vec![], false, false, &i);
        assert!(cam.needs_auth);
        assert!(cam.streams[0].rtsp_url.contains("admin"));
        assert!(cam.streams[0].rtsp_url.contains("p%40ss"));
        assert!(!cam.streams[1].rtsp_url.contains('@'));
    }

    #[test]
    fn non_rtsp_and_zero_streams() {
        let mut i = empty();
        i.onvif = vec![(
            Profile {
                token: "h".into(),
                name: None,
                resolution: None,
            },
            "http://x/video".into(),
        )];
        let cam = assemble(ip(), Some("cam".into()), None, vec![], true, false, &i);
        assert!(cam.streams.is_empty());
        assert!(cam.ptz);
    }

    #[test]
    fn codec_from_path_hit_and_dedupe_merge() {
        let mut i = empty();
        // Same URL via ONVIF (no codec) and path-probe (verified h264):
        // dedupe keeps the ONVIF cand but merges the probed codec.
        i.onvif = vec![profile("x", 1920, 1080)];
        i.paths = vec![PathHit {
            path: "x".into(),
            role: ScanRole::Main,
            auth: None,
            codec: Some("h264".into()),
        }];
        let cam = assemble(ip(), None, None, vec![], false, false, &i);
        assert_eq!(1, cam.streams.len());
        assert_eq!("h264", cam.streams[0].video_codec);
    }

    #[test]
    fn onvif_streams_default_unknown_codec() {
        let mut i = empty();
        i.onvif = vec![profile("x", 1920, 1080)];
        let cam = assemble(ip(), None, None, vec![], false, false, &i);
        assert_eq!("unknown", cam.streams[0].video_codec);
    }

    #[test]
    fn vendor_hint_roles() {
        let mut i = empty();
        i.vendor = vec![
            VendorHit {
                path: "Streaming/Channels/102".into(),
                auth: None,
            },
            VendorHit {
                path: "Streaming/Channels/101".into(),
                auth: None,
            },
        ];
        let cam = assemble(ip(), None, None, vec![], false, false, &i);
        assert_eq!(ScanRole::Main, cam.streams[0].role);
        assert_eq!(ScanRole::Sub, cam.streams[1].role);
    }
}
