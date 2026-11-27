// SPDX-License-Identifier: GPL-2.0-or-later

//! Scan orchestrator: single-flight, 15 s shared deadline, ≤8 fan-out.

use crate::{
    ScanError, assemble,
    onvif::{OnvifHttp, enumerate},
    rtsp,
    types::{Creds, DiscoveredCamera, DiscoveredDevice, ScanRequest, ScanResult, ScanStream},
    vendor, wsd,
};
use std::{
    collections::HashSet,
    net::Ipv4Addr,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

const DEADLINE: Duration = Duration::from_secs(15);
const MCAST_WINDOW: Duration = Duration::from_secs(5);
const UNICAST_WINDOW: Duration = Duration::from_secs(2);
const FANOUT: usize = 8;
const ENUM_BUDGET: Duration = Duration::from_secs(6);
const VIGI_PROBE_BUDGET: Duration = Duration::from_secs(5);
const MIN_PROBE_BUDGET: Duration = Duration::from_millis(500);

/// Injectable VIGI `:8800` probe — `(target ip, creds, budget)` →
/// the private talk handshake succeeded. Tests stub the seam; the
/// production fn runs the real `VigiTalk::connect` handshake.
type VigiProbeFn = Arc<
    dyn Fn(Ipv4Addr, Creds, Duration) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync,
>;

fn default_vigi_probe() -> VigiProbeFn {
    Arc::new(|ip: Ipv4Addr, creds: Creds, budget: Duration| {
        Box::pin(async move {
            let host = ip.to_string();
            matches!(
                tokio::time::timeout(
                    budget,
                    audio::VigiTalk::connect(&host, &creds.username, &creds.password)
                )
                .await,
                Ok(audio::VigiProbe::Ready(_) | audio::VigiProbe::Busy)
            )
        })
    })
}

/// The V270 gate — `Some(target)` when the `:8800` probe should
/// run: ONVIF yielded ⊥ backchannel, creds present, budget left.
/// Per-IP scans probe the REQUESTED host; mcast scans probe the
/// responder IP — a stray `ProbeMatch` ⊥ retargets creds.
fn probe_target(
    backchannel: bool,
    creds: &Creds,
    per_ip: Option<Ipv4Addr>,
    dev_ip: Ipv4Addr,
    remaining: Duration,
) -> Option<Ipv4Addr> {
    if backchannel || creds.is_empty() || remaining < MIN_PROBE_BUDGET {
        return None;
    }
    Some(per_ip.unwrap_or(dev_ip))
}

type Outcome = Arc<Result<ScanResult, ScanError>>;
type Slot = Option<(ScanRequest, watch::Receiver<Option<Outcome>>)>;

enum Act {
    Leader(watch::Sender<Option<Outcome>>),
    Join(watch::Receiver<Option<Outcome>>),
}

/// Single-flight scan coordinator — identical parsed requests join the
/// in-flight scan; different requests get [`ScanError::Busy`].
#[derive(Default)]
pub struct Scanner {
    in_flight: Mutex<Slot>,
}

impl Scanner {
    /// Run or join a scan.
    ///
    /// # Errors
    /// [`ScanError::Busy`] when a different request is in flight,
    /// [`ScanError::InvalidHost`] on a bad literal, or the inner scan
    /// error.
    pub async fn scan(&self, req: ScanRequest) -> Result<ScanResult, ScanError> {
        req.target()?; // validate before join: identical errors for all callers
        let act = {
            let mut g = self.in_flight.lock().await;
            match g.as_ref() {
                Some((r, rx)) if *r == req => Act::Join(rx.clone()),
                Some(_) => return Err(ScanError::Busy),
                None => {
                    let (tx, rx) = watch::channel(None);
                    *g = Some((req.clone(), rx));
                    Act::Leader(tx)
                }
            }
        };
        match act {
            Act::Join(mut rx) => {
                if rx.changed().await.is_err() {
                    return Err(ScanError::Busy);
                }
                match rx.borrow().as_ref().map(Arc::as_ref) {
                    Some(Ok(r)) => Ok(r.clone()),
                    Some(Err(_)) => Err(ScanError::Malformed("scan failed".into())),
                    None => Err(ScanError::Busy),
                }
            }
            Act::Leader(tx) => {
                let out = run(&req).await;
                let shared: Outcome = Arc::new(match &out {
                    Ok(r) => Ok(r.clone()),
                    Err(_) => Err(ScanError::Malformed("scan failed".into())),
                });
                let _ = tx.send(Some(shared));
                let mut g = self.in_flight.lock().await;
                *g = None;
                out
            }
        }
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

async fn run(req: &ScanRequest) -> Result<ScanResult, ScanError> {
    let deadline = Instant::now() + DEADLINE;
    let creds = Creds {
        username: req.username.clone(),
        password: req.password.clone(),
    };
    let per_ip = req.target()?;
    let mut devices: Vec<DiscoveredDevice> = match per_ip {
        Some(ip) => {
            let w = UNICAST_WINDOW.min(remaining(deadline));
            let mut d = wsd::probe_unicast(ip, w).await.unwrap_or_default();
            if d.is_empty() {
                // Directed WSD dead — still try ONVIF default + probes.
                d.push(DiscoveredDevice {
                    epr: String::new(),
                    xaddrs: vec![format!("http://{ip}/onvif/device_service")],
                    ip,
                    name: None,
                    hardware: None,
                });
            }
            d
        }
        None => wsd::probe_and_collect(MCAST_WINDOW.min(remaining(deadline))).await?,
    };

    let mut set = tokio::task::JoinSet::new();
    let mut cams = Vec::new();
    let mut truncated = false;
    let probe = default_vigi_probe();
    let probed = Arc::new(Mutex::new(HashSet::new()));
    while !devices.is_empty() || !set.is_empty() {
        while set.len() < FANOUT && !devices.is_empty() {
            if remaining(deadline).is_zero() {
                truncated = true;
                break;
            }
            let dev = devices.remove(0);
            let creds = creds.clone();
            let probe = probe.clone();
            let probed = probed.clone();
            set.spawn(async move { scan_one(dev, creds, per_ip, deadline, probe, probed).await });
        }
        if truncated {
            break;
        }
        if let Some(Ok(cam)) = set.join_next().await {
            cams.push(cam);
        }
        if remaining(deadline).is_zero() && (!devices.is_empty() || !set.is_empty()) {
            truncated = true;
        }
    }
    if truncated {
        set.abort_all();
        while set.join_next().await.is_some() {}
    }
    Ok(ScanResult {
        cameras: cams,
        truncated,
    })
}

async fn scan_one(
    dev: DiscoveredDevice,
    creds: Creds,
    per_ip: Option<std::net::Ipv4Addr>,
    deadline: Instant,
    probe: VigiProbeFn,
    probed: Arc<Mutex<HashSet<Ipv4Addr>>>,
) -> DiscoveredCamera {
    let mut input = assemble::Input {
        onvif: Vec::new(),
        onvif_auth: false,
        vendor: Vec::new(),
        paths: Vec::new(),
        creds: creds.clone(),
        needs_auth: false,
    };
    let mut ptz = false;
    let mut backchannel = false;

    if let Some(xaddr) = dev.xaddrs.first() {
        let mut client = OnvifHttp::new(dev.ip.into());
        let budget = remaining(deadline).min(ENUM_BUDGET);
        match tokio::time::timeout(budget, enumerate(&mut client, xaddr, &creds)).await {
            Ok(Ok((services, streams))) => {
                ptz = services.ptz;
                backchannel = services.backchannel;
                input.onvif = streams;
                input.onvif_auth = client.auth_used();
            }
            _ => input.needs_auth = client.auth_used(),
        }
    }

    // VIGI private-audio probe (V270) — before vendor/path/codec
    // probes can exhaust the deadline, on both scan_one exits.
    let mut vigi_talk = false;
    if let Some(target) = probe_target(backchannel, &creds, per_ip, dev.ip, remaining(deadline))
        && probed.lock().await.insert(target)
    {
        vigi_talk = probe(
            target,
            creds.clone(),
            remaining(deadline).min(VIGI_PROBE_BUDGET),
        )
        .await;
    }

    // Per-IP probes target the REQUESTED host — a stray or spoofed
    // unicast reply must never retarget probing or creds.
    let Some(probe_ip) = per_ip else {
        let mut cam = assemble::assemble(
            dev.ip,
            dev.name,
            dev.hardware,
            dev.xaddrs,
            ptz,
            backchannel,
            vigi_talk,
            &input,
        );
        fill_codecs(&mut cam, &creds, deadline).await;
        return cam;
    };
    if !remaining(deadline).is_zero() {
        if remaining(deadline) > vendor_timeout() {
            input
                .vendor
                .extend(vendor::isapi_channels(probe_ip, &creds).await);
        }
        if remaining(deadline) > vendor_timeout() {
            input
                .vendor
                .extend(vendor::dahua_channels(probe_ip, &creds).await);
        }
        // Shortlist skipped once enumeration already yielded streams.
        if input.onvif.is_empty() && input.vendor.iter().all(|h| h.path.is_empty()) {
            let server = rtsp::fingerprint(probe_ip).await;
            let hits = rtsp::probe_paths(deadline, probe_ip, &creds, server.as_deref()).await;
            // Retina-verify answered candidates; hits that fail are
            // dropped as false positives. Auth-flagged hits stay with
            // codec unknown — the challenge blocks the DESCRIBE.
            for mut hit in hits {
                if hit.auth.is_none()
                    && !remaining(deadline).is_zero()
                    && let Ok(u) = url::Url::parse(&format!("rtsp://{}:554/{}", probe_ip, hit.path))
                {
                    match rtsp::describe_codec(u, &creds, remaining(deadline)).await {
                        Ok(codec) => hit.codec = Some(codec),
                        Err(_) => continue,
                    }
                }
                input.paths.push(hit);
            }
        }
    }

    let mut cam = assemble::assemble(
        dev.ip,
        dev.name,
        dev.hardware,
        dev.xaddrs,
        ptz,
        backchannel,
        vigi_talk,
        &input,
    );
    fill_codecs(&mut cam, &creds, deadline).await;
    cam
}

/// DESCRIBE+PLAY-probe every emitted stream once post-dedupe: fill
/// codec when still unknown and mark `live = false` when the PLAY
/// window sees zero media. A SETUP/PLAY rejection yields an
/// undetermined liveness verdict — codec still fills, `live` keeps
/// the enumeration value. Enumeration-evidenced URIs stay on
/// DESCRIBE failure (codec `"unknown"`, `live` untouched);
/// evidence-less path candidates were already dropped pre-assembly.
async fn fill_codecs(cam: &mut DiscoveredCamera, creds: &Creds, deadline: Instant) {
    for s in &mut cam.streams {
        if remaining(deadline).is_zero() {
            break;
        }
        let Ok(mut u) = url::Url::parse(&s.rtsp_url) else {
            continue;
        };
        let _ = u.set_username("");
        let _ = u.set_password(None);
        let Ok(outcome) =
            rtsp::describe_play_probe(u, creds, remaining(deadline), PLAY_PROBE_WINDOW).await
        else {
            continue;
        };
        apply_outcome(s, &outcome);
    }
}

/// Merge one probe outcome into an emitted stream: codec fills when
/// still `"unknown"`; `live` only moves on a definitive verdict —
/// `None` (SETUP/PLAY rejected) leaves enumeration evidence standing.
fn apply_outcome(s: &mut ScanStream, o: &rtsp::ProbeOutcome) {
    if s.video_codec == "unknown" {
        s.video_codec.clone_from(&o.codec);
    }
    if let Some(live) = o.live {
        s.live = live;
    }
}

const PLAY_PROBE_WINDOW: Duration = Duration::from_secs(5);

fn vendor_timeout() -> Duration {
    Duration::from_millis(1500)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn single_flight_busy_then_join() {
        // TEST-NET-1 is unroutable: unicast window keeps the scan alive.
        let s = Arc::new(Scanner::default());
        let req1 = ScanRequest {
            host: "192.0.2.1".into(),
            ..Default::default()
        };
        let s2 = s.clone();
        let r1c = req1.clone();
        let leader = tokio::spawn(async move { s2.scan(r1c).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Different request → Busy.
        let other = s
            .scan(ScanRequest {
                host: "192.0.2.2".into(),
                ..Default::default()
            })
            .await;
        assert!(matches!(other, Err(ScanError::Busy)));

        // Identical request joins and shares the result.
        let joined = tokio::time::timeout(Duration::from_secs(20), s.scan(req1))
            .await
            .unwrap()
            .unwrap();
        let leader_out = leader.await.unwrap().unwrap();
        assert_eq!(leader_out, joined);
    }

    fn creds() -> Creds {
        Creds {
            username: "u".into(),
            password: "p".into(),
        }
    }

    fn dev(ip: &str, xaddr: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            epr: "urn:uuid:t".into(),
            xaddrs: vec![xaddr.into()],
            ip: ip.parse().unwrap(),
            name: None,
            hardware: None,
        }
    }

    fn recording_probe(hits: Arc<Mutex<Vec<Ipv4Addr>>>, out: bool) -> VigiProbeFn {
        Arc::new(move |ip, _c, _b| {
            let hits = hits.clone();
            Box::pin(async move {
                hits.lock().await.push(ip);
                out
            })
        })
    }

    // `:1` refuses instantly — enumerate fails fast, so the probe
    // path under test is the enumerate-failed branch.
    fn refused_dev() -> DiscoveredDevice {
        dev("127.0.0.1", "http://127.0.0.1:1/onvif/device_service")
    }

    #[test]
    fn probe_target_gates() {
        let ip: Ipv4Addr = "192.0.2.9".parse().unwrap();
        let req: Ipv4Addr = "192.0.2.7".parse().unwrap();
        let budget = Duration::from_secs(3);

        // ONVIF backchannel already advertised → no private probe.
        assert_eq!(None, probe_target(true, &creds(), None, ip, budget));
        // Empty creds → skip (V219 posture).
        assert_eq!(
            None,
            probe_target(false, &Creds::default(), None, ip, budget)
        );
        // Starved deadline → skip rather than borrow codec budget.
        assert_eq!(
            None,
            probe_target(false, &creds(), None, ip, Duration::from_millis(10))
        );
        // Per-IP scans probe the REQUESTED host, never the responder.
        assert_eq!(
            Some(req),
            probe_target(false, &creds(), Some(req), ip, budget)
        );
        // Multicast scans probe the responder IP.
        assert_eq!(Some(ip), probe_target(false, &creds(), None, ip, budget));
    }

    #[tokio::test]
    async fn vigi_probe_targets_requested_host_per_ip() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let req: Ipv4Addr = "192.0.2.7".parse().unwrap();
        let cam = scan_one(
            refused_dev(),
            creds(),
            Some(req),
            Instant::now() + DEADLINE,
            recording_probe(hits.clone(), true),
            Arc::new(Mutex::new(HashSet::new())),
        )
        .await;
        assert_eq!(vec![req], *hits.lock().await);
        assert!(cam.vigi_talk);
    }

    #[tokio::test]
    async fn vigi_probe_targets_responder_in_mcast() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let cam = scan_one(
            refused_dev(),
            creds(),
            None,
            Instant::now() + DEADLINE,
            recording_probe(hits.clone(), true),
            Arc::new(Mutex::new(HashSet::new())),
        )
        .await;
        assert_eq!(
            vec!["127.0.0.1".parse::<Ipv4Addr>().unwrap()],
            *hits.lock().await
        );
        assert!(cam.vigi_talk);
    }

    #[tokio::test]
    async fn vigi_probe_dedupes_across_devices() {
        // A second stray responder for the same target must not
        // re-probe — the shared set suppresses it.
        let hits = Arc::new(Mutex::new(Vec::new()));
        let probed = Arc::new(Mutex::new(HashSet::new()));
        let req: Ipv4Addr = "192.0.2.7".parse().unwrap();
        for _ in 0..2 {
            let cam = scan_one(
                refused_dev(),
                creds(),
                Some(req),
                Instant::now() + DEADLINE,
                recording_probe(hits.clone(), true),
                probed.clone(),
            )
            .await;
            assert!(cam.vigi_talk || hits.lock().await.len() == 1);
        }
        assert_eq!(1, hits.lock().await.len());
    }

    #[test]
    fn bad_host_rejected() {
        let r = ScanRequest {
            host: "not-an-ip".into(),
            ..Default::default()
        };
        assert!(matches!(r.target(), Err(ScanError::InvalidHost(_))));
        let _: Ipv4Addr = "192.0.2.1".parse().unwrap();
    }

    fn stream(codec: &str, live: bool) -> ScanStream {
        ScanStream {
            profile_name: None,
            resolution: None,
            rtsp_url: "rtsp://127.0.0.1:554/x".into(),
            role: crate::types::ScanRole::Main,
            auth: None,
            video_codec: codec.into(),
            live,
        }
    }

    #[test]
    fn apply_outcome_fills_codec_and_dead_verdict() {
        let mut s = stream("unknown", true);
        apply_outcome(
            &mut s,
            &rtsp::ProbeOutcome {
                codec: "h264".into(),
                live: Some(false),
            },
        );
        assert_eq!("h264", s.video_codec);
        assert!(!s.live);
    }

    #[test]
    fn apply_outcome_keeps_live_when_verdict_undetermined() {
        // PLAY-rejected (e.g. 429 on an in-use stream): codec still
        // fills, enumeration's `live` stands.
        for prior in [true, false] {
            let mut s = stream("unknown", prior);
            apply_outcome(
                &mut s,
                &rtsp::ProbeOutcome {
                    codec: "h264".into(),
                    live: None,
                },
            );
            assert_eq!("h264", s.video_codec);
            assert_eq!(prior, s.live);
        }
    }

    #[test]
    fn apply_outcome_does_not_overwrite_known_codec() {
        let mut s = stream("h265", true);
        apply_outcome(
            &mut s,
            &rtsp::ProbeOutcome {
                codec: "h264".into(),
                live: Some(true),
            },
        );
        assert_eq!("h265", s.video_codec);
        assert!(s.live);
    }
}
