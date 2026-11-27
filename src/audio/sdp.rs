// SPDX-License-Identifier: GPL-2.0-or-later

//! SDP parsing scoped to audio media — `m=audio` sections, their
//! direction attributes, payload maps, and control URLs. Non-audio
//! media is ignored entirely (V257: never SETUP'd).

/// Direction attribute on an `m=audio` line, mapped to local
/// semantics per spec: `sendonly` = the *client* sends (talk),
/// `recvonly` = the client receives (listen), `sendrecv` = both,
/// absent = listen-only (directionless devices emit receive audio
/// directionless — RFC-4566 default deliberately overridden),
/// `inactive` = usable for neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdpDirection {
    /// Server expects client→server audio (talk track).
    Talk,
    /// Server sends audio (listen track).
    Listen,
    /// Both directions usable.
    Duplex,
    /// `a=inactive` — unusable for this session.
    Inactive,
    /// No direction attribute — listen-capable only.
    Directionless,
}

impl SdpDirection {
    /// Whether the track can deliver camera→client audio.
    #[must_use]
    pub fn listens(self) -> bool {
        matches!(self, Self::Listen | Self::Duplex | Self::Directionless)
    }

    /// Whether the track can carry client→camera audio.
    #[must_use]
    pub fn talks(self) -> bool {
        matches!(self, Self::Talk | Self::Duplex)
    }
}

/// Supported audio codecs (V262 — PCMU/PCMA only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// G.711 µ-law, static PT 0.
    Pcmu,
    /// G.711 A-law, static PT 8.
    Pcma,
}

/// One parsed `m=audio` media section.
#[derive(Clone, Debug)]
pub struct AudioMedia {
    /// Direction attribute mapped to local semantics.
    pub direction: SdpDirection,
    /// RTP payload types usable for this track — filtered to
    /// PCMU/PCMA; empty when the line offers none.
    pub payloads: Vec<PayloadMap>,
    /// Media `a=control` resolved to an absolute URL.
    pub control: String,
    /// Remote port from `m=` (informational — transport is TCP).
    pub port: u16,
}

/// One payload type usable for a track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadMap {
    /// RTP payload type number.
    pub pt: u8,
    /// `G.711` encoding.
    pub codec: Codec,
}

/// Parsed DESCRIBE body — session-level fields + audio media only.
#[derive(Clone, Debug)]
pub struct Sdp {
    /// Session `a=control` (usually `*`/base URL) resolved absolute —
    /// keepalive/PLAY target.
    pub control: String,
    /// Audio media sections, in SDP order.
    pub audio: Vec<AudioMedia>,
}

/// In-progress media section — payloads start unresolved; static
/// PTs 0/8 are known, dynamic ones wait for their `a=rtpmap`.
struct MediaAcc {
    m: AudioMedia,
    /// `(pt, rtpmap encoding name)` collected in the section.
    rtpmaps: Vec<(u8, String)>,
}

impl MediaAcc {
    /// Resolve dynamic PTs through `a=rtpmap` and drop every payload
    /// outside PCMU/PCMA (unsupported codecs mark the direction per
    /// V262 — they never reach SETUP).
    fn finish(self) -> AudioMedia {
        let mut m = self.m;
        m.payloads =
            m.payloads
                .into_iter()
                .filter_map(|mut p| {
                    let codec = match p.pt {
                        0 => Some(Codec::Pcmu),
                        8 => Some(Codec::Pcma),
                        pt => self
                            .rtpmaps
                            .iter()
                            .find(|(n, _)| *n == pt)
                            .and_then(|(_, enc)| match enc.as_str() {
                                "PCMU" => Some(Codec::Pcmu),
                                "PCMA" => Some(Codec::Pcma),
                                _ => None,
                            }),
                    }?;
                    p.codec = codec;
                    Some(p)
                })
                .collect();
        m
    }
}

/// Parse an SDP body against a base URL for `a=control` resolution.
/// Errors are malformed-SDP only; absent audio is an empty `audio`.
#[must_use]
pub fn parse(body: &str, base: &url::Url) -> Sdp {
    let mut session_control: Option<String> = None;
    let mut audio = Vec::new();
    let mut cur: Option<MediaAcc> = None;
    // `a=` lines under ANY `m=` belong to that media — session-level
    // attrs exist only before the first media line.
    let mut in_media = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(m) = line.strip_prefix("m=") {
            in_media = true;
            if let Some(acc) = cur.take() {
                audio.push(acc.finish());
            }
            let mut it = m.split_whitespace();
            let kind = it.next().unwrap_or("");
            let port: u16 = it
                .next()
                .and_then(|p| p.split('/').next()?.parse().ok())
                .unwrap_or(0);
            let _proto = it.next();
            cur = kind.eq_ignore_ascii_case("audio").then(|| MediaAcc {
                m: AudioMedia {
                    direction: SdpDirection::Directionless,
                    payloads: it
                        .filter_map(|tok| {
                            tok.parse::<u8>().ok().map(|pt| PayloadMap {
                                pt,
                                codec: Codec::Pcmu,
                            })
                        })
                        .collect(),
                    control: String::new(),
                    port,
                },
                rtpmaps: Vec::new(),
            });
            continue;
        }
        if let Some(acc) = cur.as_mut() {
            match line {
                "a=sendonly" => acc.m.direction = SdpDirection::Talk,
                "a=recvonly" => acc.m.direction = SdpDirection::Listen,
                "a=sendrecv" => acc.m.direction = SdpDirection::Duplex,
                "a=inactive" => acc.m.direction = SdpDirection::Inactive,
                l if l.starts_with("a=rtpmap:") => {
                    if let Some((pt_s, enc)) = l["a=rtpmap:".len()..].split_once(' ') {
                        if let Ok(pt) = pt_s.trim().parse::<u8>() {
                            acc.rtpmaps.push((
                                pt,
                                enc.split('/').next().unwrap_or("").to_ascii_uppercase(),
                            ));
                        }
                    }
                }
                l if l.starts_with("a=control:") => {
                    acc.m.control = resolve_control(&l["a=control:".len()..], base);
                }
                _ => {}
            }
        } else if !in_media {
            if let Some(v) = line.strip_prefix("a=control:") {
                session_control = Some(resolve_control(v, base));
            }
        }
    }
    if let Some(acc) = cur.take() {
        audio.push(acc.finish());
    }
    Sdp {
        control: session_control.unwrap_or_else(|| base.to_string()),
        audio,
    }
}

/// Resolve a `a=control` value — `*`, absolute, or relative to the
/// DESCRIBE base URL. Relative controls join *under* the base
/// (`rtsp://h/p` + `trackID=1` → `rtsp://h/p/trackID=1`) per
/// RFC 2326 §C.3 — not RFC-3986 last-segment replacement.
fn resolve_control(v: &str, base: &url::Url) -> String {
    let v = v.trim();
    if v == "*" || v.is_empty() {
        return base.to_string();
    }
    if let Ok(u) = url::Url::parse(v) {
        return u.to_string();
    }
    if let Some(abs) = v.strip_prefix('/') {
        let mut origin = base.clone();
        origin.set_path("/");
        origin.set_query(None);
        return format!("{origin}{abs}");
    }
    format!("{}/{v}", base.as_str().trim_end_matches('/'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const BASE: &str = "rtsp://cam.local:554/live";

    fn base() -> url::Url {
        url::Url::parse(BASE).unwrap()
    }

    #[test]
    fn duplex_backchannel_sdp() {
        let sdp = parse(
            "v=0\r\n\
             o=- 1 1 IN IP4 0.0.0.0\r\n\
             s=Session\r\n\
             t=0 0\r\n\
             a=control:*\r\n\
             m=video 0 RTP/AVP 96\r\n\
             a=rtpmap:96 H264/90000\r\n\
             a=control:trackID=0\r\n\
             m=audio 0 RTP/AVP 8\r\n\
             a=recvonly\r\n\
             a=control:trackID=1\r\n\
             m=audio 0 RTP/AVP 0\r\n\
             a=sendonly\r\n\
             a=control:trackID=2\r\n",
            &base(),
        );
        assert_eq!(sdp.audio.len(), 2);
        assert_eq!(sdp.audio[0].direction, SdpDirection::Listen);
        assert_eq!(
            sdp.audio[0].payloads,
            vec![PayloadMap {
                pt: 8,
                codec: Codec::Pcma
            }]
        );
        assert_eq!(sdp.audio[0].control, format!("{BASE}/trackID=1"));
        assert_eq!(sdp.audio[1].direction, SdpDirection::Talk);
        assert_eq!(
            sdp.audio[1].payloads,
            vec![PayloadMap {
                pt: 0,
                codec: Codec::Pcmu
            }]
        );
        assert_eq!(sdp.audio[1].control, format!("{BASE}/trackID=2"));
        assert_eq!(sdp.control, BASE);
    }

    #[test]
    fn recvonly_only_sdp() {
        let sdp = parse(
            "v=0\r\nm=video 0 RTP/AVP 96\r\na=control:video\r\n\
             m=audio 0 RTP/AVP 0\r\na=recvonly\r\na=control:audio\r\n",
            &base(),
        );
        assert_eq!(sdp.audio.len(), 1);
        assert!(sdp.audio[0].direction.listens());
        assert!(!sdp.audio[0].direction.talks());
    }

    #[test]
    fn directionless_audio_is_listen_only() {
        // VIGI/directionless posture — no direction attribute.
        let sdp = parse("v=0\r\nm=audio 0 RTP/AVP 8\r\na=control:a\r\n", &base());
        assert_eq!(sdp.audio[0].direction, SdpDirection::Directionless);
        assert!(sdp.audio[0].direction.listens());
        assert!(!sdp.audio[0].direction.talks());
    }

    #[test]
    fn sendrecv_is_duplex() {
        let sdp = parse(
            "v=0\r\nm=audio 0 RTP/AVP 0 8\r\na=sendrecv\r\na=control:a\r\n",
            &base(),
        );
        assert_eq!(sdp.audio[0].direction, SdpDirection::Duplex);
        assert_eq!(
            sdp.audio[0].payloads,
            vec![
                PayloadMap {
                    pt: 0,
                    codec: Codec::Pcmu
                },
                PayloadMap {
                    pt: 8,
                    codec: Codec::Pcma
                },
            ]
        );
    }

    #[test]
    fn inactive_unusable() {
        let sdp = parse(
            "v=0\r\nm=audio 0 RTP/AVP 0\r\na=inactive\r\na=control:a\r\n",
            &base(),
        );
        assert_eq!(sdp.audio[0].direction, SdpDirection::Inactive);
        assert!(!sdp.audio[0].direction.listens());
        assert!(!sdp.audio[0].direction.talks());
    }

    #[test]
    fn dynamic_pt_resolves_via_rtpmap() {
        let sdp = parse(
            "v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 PCMA/8000\r\na=recvonly\r\na=control:a\r\n",
            &base(),
        );
        assert_eq!(
            sdp.audio[0].payloads,
            vec![PayloadMap {
                pt: 96,
                codec: Codec::Pcma
            }]
        );
    }

    #[test]
    fn unsupported_codec_pt_dropped() {
        // AAC offer — no usable payloads left.
        let sdp = parse(
            "v=0\r\nm=audio 0 RTP/AVP 97\r\na=rtpmap:97 mpeg4-generic/16000\r\na=recvonly\r\na=control:a\r\n",
            &base(),
        );
        assert_eq!(sdp.audio.len(), 1);
        assert!(sdp.audio[0].payloads.is_empty());
    }

    #[test]
    fn non_audio_ignored() {
        let sdp = parse("v=0\r\nm=video 0 RTP/AVP 96\r\na=control:v\r\n", &base());
        assert!(sdp.audio.is_empty());
    }
}
