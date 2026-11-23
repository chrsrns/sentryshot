// SPDX-License-Identifier: GPL-2.0-or-later

//! Minimal RTP — 12-byte fixed header + CSRC/extension skip on
//! parse; no SRTP, no RTCP (the TCP-interleaved session never
//! reads RTCP).

/// Parsed RTP packet.
#[derive(Debug)]
pub struct Rtp {
    /// Payload type.
    pub pt: u8,
    /// Sequence number.
    pub seq: u16,
    /// Timestamp (codec clock — 8 kHz for G.711). Sequencing uses
    /// `seq`; kept for future jitter handling.
    #[allow(dead_code)]
    pub ts: u32,
    /// Payload bytes (header stripped).
    pub payload: Vec<u8>,
}

/// Parse one packet; `None` on truncation or bad version.
#[must_use]
pub fn parse(b: &[u8]) -> Option<Rtp> {
    if b.len() < 12 || b[0] >> 6 != 2 {
        return None;
    }
    let cc = usize::from(b[0] & 0x0F);
    let mut off = 12 + cc * 4;
    if b.len() < off {
        return None;
    }
    if b[0] & 0x10 != 0 {
        if b.len() < off + 4 {
            return None;
        }
        let ext = usize::from(u16::from_be_bytes([b[off + 2], b[off + 3]])) * 4;
        off += 4 + ext;
        if b.len() < off {
            return None;
        }
    }
    Some(Rtp {
        pt: b[1] & 0x7F,
        seq: u16::from_be_bytes([b[2], b[3]]),
        ts: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        payload: b[off..].to_vec(),
    })
}

/// Build a packet — CSRC/extensions never emitted.
#[must_use]
pub fn build(pt: u8, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.push(0x80); // V2
    out.push(pt & 0x7F);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&ts.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Out-of-order/duplicate check — `true` when `seq` is newer than
/// `last` under 16-bit wraparound.
#[must_use]
#[allow(clippy::as_conversions, clippy::cast_possible_wrap)] // RFC-1982 wraparound compare
pub fn newer(seq: u16, last: u16) -> bool {
    (seq.wrapping_sub(last) as i16) > 0
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn build_parse_roundtrip() {
        let raw = build(96, 0x1234, 0xA0B0_C0D0, 0xCAFE, &[1, 2, 3]);
        assert_eq!(raw[0], 0x80);
        let p = parse(&raw).unwrap();
        assert_eq!((p.pt, p.seq, p.ts), (96, 0x1234, 0xA0B0_C0D0));
        assert_eq!(p.payload, vec![1, 2, 3]);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse(&[]).is_none());
        assert!(parse(&[0x80; 8]).is_none());
        assert!(parse(&[0x40; 16]).is_none()); // V1
    }

    #[test]
    fn parse_skips_csrc_and_extension() {
        // CC=1 + X=1 extension of 1 word.
        let mut raw = build(0, 1, 2, 0, &[9]);
        raw[0] = 0x80 | 0x10 | 0x01;
        raw.splice(12..12, [0xAA; 4]); // CSRC
        raw.splice(16..16, [0, 0, 0, 1, 0xBB, 0xBB, 0xBB, 0xBB]); // ext
        let p = parse(&raw).unwrap();
        assert_eq!(p.payload, vec![9]);
    }

    #[test]
    fn newer_wraparound() {
        assert!(newer(2, 1));
        assert!(!newer(1, 2));
        assert!(newer(0, 0xFFFF));
        assert!(!newer(0xFFFF, 0));
        assert!(!newer(5, 5));
    }
}
