// SPDX-License-Identifier: GPL-2.0-or-later

//! WS-Discovery (UDP multicast) camera probe.
//!
//! Probe envelope is hand-rolled with the April-2005 discovery namespace
//! (`http://schemas.xmlsoap.org/ws/2005/04/discovery`) — the one ONVIF
//! devices answer. Reply parsing is local-name tolerant and size-capped.

use crate::{ScanError, types::DiscoveredDevice};
use roxmltree::Document;
use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};
use tokio::net::UdpSocket;

/// ONVIF WS-Discovery multicast group.
pub const MULTICAST_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 3702);

/// April-2005 discovery namespace ONVIF devices answer.
pub const DISCOVERY_NS: &str = "http://schemas.xmlsoap.org/ws/2005/04/discovery";

/// Replies larger than this are dropped (untrusted surface).
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Build a WS-Discovery Probe SOAP envelope with a unique `MessageID`.
#[must_use]
pub fn build_probe(message_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="{DISCOVERY_NS}" xmlns:dn="http://www.onvif.org/ver10/network/wsdl" xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><s:Header><a:Action s:mustUnderstand="1">{DISCOVERY_NS}/Probe</a:Action><a:MessageID>{message_id}</a:MessageID><a:To s:mustUnderstand="1">urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To></s:Header><s:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types></d:Probe></s:Body></s:Envelope>"#
    )
}

/// Fresh `urn:uuid:` `MessageID` from random bytes.
#[must_use]
pub fn new_message_id() -> String {
    let b: [u8; 16] = rand::random();
    format!(
        "urn:uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

/// A single parsed `ProbeMatch` reply (pre-dedupe).
#[derive(Clone, Debug)]
pub struct ProbeMatch {
    /// EPR `urn:uuid` (or any EPR address).
    pub epr: String,
    /// `XAddrs` URIs as reported.
    pub xaddrs: Vec<String>,
    /// `Scopes` URIs as reported.
    pub scopes: Vec<String>,
}

/// Parse a `ProbeMatches` datagram. Empty vec on malformed input.
#[must_use]
pub fn parse_probe_matches(data: &[u8]) -> Vec<ProbeMatch> {
    if data.len() > MAX_REPLY_BYTES {
        return Vec::new();
    }
    // Some firmware prepends garbage before the XML.
    let Some(start) = data.windows(5).position(|w| w == b"<?xml").or(data
        .windows(2)
        .position(|w| w == b"<s" || w == b"<e" || w == b"<d"))
    else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(&data[start..]) else {
        return Vec::new();
    };
    let Ok(doc) = Document::parse(text) else {
        return Vec::new();
    };

    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "ProbeMatch")
        .map(|m| {
            let epr = m
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "Address")
                .and_then(|n| n.text())
                .unwrap_or_default()
                .to_owned();
            let xaddrs = m
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "XAddrs")
                .and_then(|n| n.text())
                .map(|t| t.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default();
            let scopes = m
                .descendants()
                .find(|n| n.is_element() && n.tag_name().name() == "Scopes")
                .and_then(|n| n.text())
                .map(|t| t.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default();
            ProbeMatch {
                epr,
                xaddrs,
                scopes,
            }
        })
        .collect()
}

/// Extract `…/name/<value>` and `…/hardware/<value>` scope tails.
#[must_use]
pub fn parse_scopes(scopes: &[String]) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut hardware = None;
    for scope in scopes {
        for (marker, slot) in [("/name/", &mut name), ("/hardware/", &mut hardware)] {
            if let Some(pos) = scope.find(marker) {
                let value = &scope[pos + marker.len()..];
                if !value.is_empty() {
                    *slot = Some(value.to_owned());
                }
            }
        }
    }
    (name, hardware)
}

/// Send the probe to [`MULTICAST_ADDR`] on every up non-loopback IPv4
/// interface, then collect replies for `window`.
///
/// # Errors
/// [`ScanError::Io`] on socket bind/send failure.
pub async fn probe_and_collect(window: Duration) -> Result<Vec<DiscoveredDevice>, ScanError> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?;
    let probe = build_probe(&new_message_id());
    let ifaces: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .map(|v| {
            v.iter()
                .filter_map(|i| match &i.addr {
                    if_addrs::IfAddr::V4(a) if !i.is_loopback() => Some(a.ip),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    // Per-NIC: multicast egress must be pinned to each interface.
    if ifaces.is_empty() {
        let _ = socket.send_to(probe.as_bytes(), MULTICAST_ADDR).await;
    }
    let sock_ref = socket2::SockRef::from(&socket);
    for ip in &ifaces {
        sock_ref.set_multicast_if_v4(ip)?;
        let _ = socket.send_to(probe.as_bytes(), MULTICAST_ADDR).await;
    }
    collect(&socket, window).await
}

/// Directed probe to one host on port 3702. Multicast-dead cameras
/// still answer unicast probes. Short window: directed replies are
/// immediate; stragglers are accepted loss.
///
/// # Errors
/// [`ScanError::Io`] on socket bind/send failure.
pub async fn probe_unicast(
    target: Ipv4Addr,
    window: Duration,
) -> Result<Vec<DiscoveredDevice>, ScanError> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?;
    let probe = build_probe(&new_message_id());
    let _ = socket
        .send_to(probe.as_bytes(), SocketAddrV4::new(target, 3702))
        .await;
    collect(&socket, window).await
}

/// Collect `ProbeMatches` for `window`, dedupe by EPR then by IP.
pub async fn collect(
    socket: &UdpSocket,
    window: Duration,
) -> Result<Vec<DiscoveredDevice>, ScanError> {
    let mut devices = Vec::new();
    let mut seen_epr = HashSet::new();
    let mut seen_ip = HashSet::new();
    let mut buf = vec![0_u8; MAX_REPLY_BYTES];
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let recv = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
        match recv {
            Err(_) => break,
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok((n, from))) => {
                let ip = match from.ip() {
                    std::net::IpAddr::V4(ip) => ip,
                    std::net::IpAddr::V6(_) => continue,
                };
                for m in parse_probe_matches(&buf[..n]) {
                    if !m.epr.is_empty() && !seen_epr.insert(m.epr.clone()) {
                        continue;
                    }
                    if !seen_ip.insert(ip) {
                        continue;
                    }
                    let (name, hardware) = parse_scopes(&m.scopes);
                    devices.push(DiscoveredDevice {
                        epr: m.epr,
                        xaddrs: m.xaddrs,
                        ip,
                        name,
                        hardware,
                    });
                }
            }
        }
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::net::SocketAddrV4;

    const PROBE_MATCH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2004/09/discovery">
  <e:Body><d:ProbeMatches><d:ProbeMatch>
    <a:EndpointReference><a:Address>urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee</a:Address></a:EndpointReference>
    <d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
    <d:Scopes>onvif://www.onvif.org/name/HW-01 onvif://www.onvif.org/hardware/HW-MODEL onvif://www.onvif.org/Profile/Streaming</d:Scopes>
    <d:XAddrs>http://192.168.1.66/onvif/device_service</d:XAddrs>
  </d:ProbeMatch></d:ProbeMatches></e:Body>
</e:Envelope>"#;

    const PROBE_MATCH_2: &str = r#"<?xml version="1.0"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2004/09/discovery">
  <e:Body><d:ProbeMatches><d:ProbeMatch>
    <a:EndpointReference><a:Address>urn:uuid:11111111-2222-3333-4444-555555555555</a:Address></a:EndpointReference>
    <d:XAddrs>http://10.0.0.5/onvif/device_service</d:XAddrs>
  </d:ProbeMatch></d:ProbeMatches></e:Body>
</e:Envelope>"#;

    #[test]
    fn probe_envelope_uses_2005_ns_and_unique_ids() {
        let a = build_probe(&new_message_id());
        assert!(a.contains("http://schemas.xmlsoap.org/ws/2005/04/discovery"));
        assert!(a.contains("urn:schemas-xmlsoap-org:ws:2005:04:discovery"));
        assert!(a.contains("dn:NetworkVideoTransmitter"));
        assert!(a.contains("tds:Device"));
        assert_ne!(new_message_id(), new_message_id());
    }

    #[test]
    fn parses_probe_match() {
        let matches = parse_probe_matches(PROBE_MATCH.as_bytes());
        assert_eq!(1, matches.len());
        assert_eq!(
            "urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            matches[0].epr
        );
        assert_eq!(
            vec!["http://192.168.1.66/onvif/device_service".to_owned()],
            matches[0].xaddrs
        );
        let (name, hardware) = parse_scopes(&matches[0].scopes);
        assert_eq!(Some("HW-01".to_owned()), name);
        assert_eq!(Some("HW-MODEL".to_owned()), hardware);
    }

    #[test]
    fn skips_malformed_and_garbage() {
        assert!(parse_probe_matches(b"not xml").is_empty());
        assert!(parse_probe_matches(&[]).is_empty());
        assert!(parse_probe_matches(&vec![0_u8; MAX_REPLY_BYTES + 1]).is_empty());
        // Garbage prefix before XML still parses.
        let mut data = b"\x00\x01\xff".to_vec();
        data.extend_from_slice(PROBE_MATCH.as_bytes());
        assert_eq!(1, parse_probe_matches(&data).len());
    }

    #[test]
    fn collects_and_dedupes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let recv = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let port = recv.local_addr().unwrap().port();
            let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let dest = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
            // Same EPR sent twice + a second device.
            sender.send_to(PROBE_MATCH.as_bytes(), dest).await.unwrap();
            sender.send_to(PROBE_MATCH.as_bytes(), dest).await.unwrap();
            sender
                .send_to(PROBE_MATCH_2.as_bytes(), dest)
                .await
                .unwrap();
            // Malformed datagram must not abort collection.
            sender.send_to(b"junk", dest).await.unwrap();

            let devices = collect(&recv, Duration::from_millis(200)).await.unwrap();
            // Same source IP → secondary dedupe collapses to one device.
            assert_eq!(1, devices.len());
            assert_eq!("HW-01", devices[0].name.as_deref().unwrap());
            assert_eq!("HW-MODEL", devices[0].hardware.as_deref().unwrap());
        });
    }
}
