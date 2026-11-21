// SPDX-License-Identifier: GPL-2.0-or-later

//! Scan orchestrator: single-flight, 15 s shared deadline, ≤8 fan-out.

use crate::{
    ScanError, assemble,
    onvif::{OnvifHttp, enumerate},
    rtsp,
    types::{Creds, DiscoveredCamera, DiscoveredDevice, ScanRequest, ScanResult},
    vendor, wsd,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

const DEADLINE: Duration = Duration::from_secs(15);
const MCAST_WINDOW: Duration = Duration::from_secs(5);
const UNICAST_WINDOW: Duration = Duration::from_secs(2);
const FANOUT: usize = 8;
const ENUM_BUDGET: Duration = Duration::from_secs(6);

type Outcome = Arc<Result<ScanResult, ScanError>>;
type Slot = Option<(ScanRequest, watch::Receiver<Option<Outcome>>)>;

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
        enum Act {
            Leader(watch::Sender<Option<Outcome>>),
            Join(watch::Receiver<Option<Outcome>>),
        }
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
    while !devices.is_empty() || !set.is_empty() {
        while set.len() < FANOUT && !devices.is_empty() {
            if remaining(deadline).is_zero() {
                truncated = true;
                break;
            }
            let dev = devices.remove(0);
            let creds = creds.clone();
            set.spawn(async move { scan_one(dev, creds, per_ip.is_some(), deadline).await });
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
    per_ip: bool,
    deadline: Instant,
) -> DiscoveredCamera {
    let mut input = assemble::Input {
        onvif: Vec::new(),
        onvif_auth: false,
        vendor: Vec::new(),
        paths: Vec::new(),
        creds: creds.clone(),
    };
    let mut ptz = false;
    let mut backchannel = false;

    if let Some(xaddr) = dev.xaddrs.first() {
        let mut client = OnvifHttp::new(dev.ip.into());
        let budget = remaining(deadline).min(ENUM_BUDGET);
        if let Ok(Ok((services, streams))) =
            tokio::time::timeout(budget, enumerate(&mut client, xaddr, &creds)).await
        {
            ptz = services.ptz;
            backchannel = services.backchannel;
            input.onvif = streams;
            input.onvif_auth = client.auth_used();
        }
    }

    if per_ip && !remaining(deadline).is_zero() {
        if remaining(deadline) > vendor_timeout() {
            input
                .vendor
                .extend(vendor::isapi_channels(dev.ip, &creds).await);
        }
        if remaining(deadline) > vendor_timeout() {
            input
                .vendor
                .extend(vendor::dahua_channels(dev.ip, &creds).await);
        }
        let _server = rtsp::fingerprint(dev.ip).await;
        let hits = rtsp::probe_paths(deadline, dev.ip, &creds).await;
        // Retina-verify answered candidates; hits that fail are dropped
        // as false positives. Auth-pending hits stay unverified.
        for hit in hits {
            if hit.auth.is_none()
                && !remaining(deadline).is_zero()
                && let Ok(u) = url::Url::parse(&format!("rtsp://{}:554/{}", dev.ip, hit.path))
                && rtsp::verify(u, &creds, remaining(deadline)).await.is_err()
            {
                continue;
            }
            input.paths.push(hit);
        }
    }

    assemble::assemble(
        dev.ip,
        dev.name,
        dev.hardware,
        dev.xaddrs,
        ptz,
        backchannel,
        input,
    )
}

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

    #[test]
    fn bad_host_rejected() {
        let r = ScanRequest {
            host: "not-an-ip".into(),
            ..Default::default()
        };
        assert!(matches!(r.target(), Err(ScanError::InvalidHost(_))));
        let _: Ipv4Addr = "192.0.2.1".parse().unwrap();
    }
}
