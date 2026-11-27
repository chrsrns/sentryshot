// SPDX-License-Identifier: GPL-2.0-or-later

//! G.711 µ-law/A-law companders — the canonical `g711.c`
//! reference algorithms; the dep-eval rejected whole codec crates
//! for these two tables.

use crate::Codec;

const BIAS: i32 = 0x84;
const CLIP: i32 = 8159;
// 14-bit µ-law / 16-bit A-law segment ends (audioop conventions).
const SEG_UEND: [i32; 8] = [0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF];
const SEG_AEND: [i32; 8] = [0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF, 0x3FFF, 0x7FFF];

fn search(val: i32, table: &[i32; 8]) -> Option<u32> {
    table
        .iter()
        .position(|&end| val <= end)
        .and_then(|i| u32::try_from(i).ok())
}

/// µ-law byte → linear PCM sample.
#[must_use]
pub fn ulaw_to_linear(u: u8) -> i16 {
    let u = !u;
    let mut t = (i32::from(u & 0x0F) << 3) + BIAS;
    t <<= (i32::from(u) & 0x70) >> 4;
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let v = if u & 0x80 != 0 { BIAS - t } else { t - BIAS } as i16;
    v
}

/// Linear PCM sample → µ-law byte.
#[must_use]
pub fn linear_to_ulaw(sample: i16) -> u8 {
    let pcm = i32::from(sample) >> 2;
    let (pcm, mask) = if pcm < 0 { (-pcm, 0x7F) } else { (pcm, 0xFF) };
    let pcm = (pcm.min(CLIP)) + (BIAS >> 2);
    let Some(seg) = search(pcm, &SEG_UEND) else {
        return 0x7F ^ mask;
    };
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let uval = ((seg << 4) | ((pcm >> (seg + 1)) & 0xF) as u32) as u8;
    uval ^ mask
}

/// A-law byte → linear PCM sample.
#[must_use]
pub fn alaw_to_linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let seg = i32::from(a & 0x70) >> 4;
    let mut t = i32::from(a & 0x0F) << 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let v = if a & 0x80 != 0 { t } else { -t } as i16;
    v
}

/// Linear PCM sample → A-law byte.
#[must_use]
pub fn linear_to_alaw(sample: i16) -> u8 {
    let mag = i32::from(sample);
    let (mag, mask) = if mag >= 0 {
        (mag, 0xD5)
    } else {
        (-mag - 1, 0x55)
    };
    let Some(seg) = search(mag, &SEG_AEND) else {
        return 0x7F ^ mask;
    };
    let mant = if seg < 2 { mag >> 4 } else { mag >> (seg + 3) };
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let aval = ((seg << 4) | (mant & 0x0F) as u32) as u8;
    aval ^ mask
}

/// Decode a G.711 frame to PCM.
#[must_use]
pub fn decode(codec: Codec, data: &[u8]) -> Vec<i16> {
    match codec {
        Codec::Pcmu => data.iter().map(|&b| ulaw_to_linear(b)).collect(),
        Codec::Pcma => data.iter().map(|&b| alaw_to_linear(b)).collect(),
    }
}

/// Encode PCM samples to a G.711 frame.
#[must_use]
pub fn encode(codec: Codec, pcm: &[i16]) -> Vec<u8> {
    match codec {
        Codec::Pcmu => pcm.iter().map(|&s| linear_to_ulaw(s)).collect(),
        Codec::Pcma => pcm.iter().map(|&s| linear_to_alaw(s)).collect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // Golden vectors generated against Python `audioop`
    // (lin2ulaw/lin2alaw/ulaw2lin/alaw2lin) — covers every segment,
    // boundary flips, and both clip rails.
    const ALAW_VEC: &[(i16, u8)] = &[
        (0, 0xd5),
        (8, 0xd5),
        (16, 0xd4),
        (64, 0xd1),
        (128, 0xdd),
        (256, 0xc5),
        (512, 0xf5),
        (1000, 0xfa),
        (2048, 0x95),
        (4096, 0x85),
        (8192, 0xb5),
        (16384, 0xa5),
        (30000, 0xa8),
        (32767, 0xaa),
        (-1, 0x55),
        (-16, 0x55),
        (-17, 0x54),
        (-256, 0x5a),
        (-264, 0x45),
        (-513, 0x75),
        (-30000, 0x28),
        (-32768, 0x2a),
    ];
    const ULAW_VEC: &[(i16, u8)] = &[
        (0, 0xff),
        (1, 0xff),
        (4, 0xfe),
        (8, 0xfe),
        (16, 0xfd),
        (64, 0xf7),
        (124, 0xef),
        (128, 0xef),
        (256, 0xe7),
        (512, 0xdb),
        (1000, 0xce),
        (2048, 0xbe),
        (4096, 0xaf),
        (8192, 0x9f),
        (30000, 0x82),
        (32767, 0x80),
        (-1, 0x7e),
        (-3, 0x7e),
        (-81, 0x74),
        (-124, 0x6f),
        (-30000, 0x02),
        (-32768, 0x00),
    ];

    #[test]
    fn alaw_encode_oracle() {
        for &(i, want) in ALAW_VEC {
            assert_eq!(linear_to_alaw(i), want, "linear_to_alaw({i})");
        }
    }

    #[test]
    fn ulaw_encode_oracle() {
        for &(i, want) in ULAW_VEC {
            assert_eq!(linear_to_ulaw(i), want, "linear_to_ulaw({i})");
        }
    }

    #[test]
    fn decode_oracle() {
        let v: &[(u8, i16, i16)] = &[
            // byte, alaw→pcm, ulaw→pcm
            (0xd5, 8, 716),
            (0x55, -8, -716),
            (0xce, 440, 988),
            (0xa8, 30208, 5884),
            (0x2a, -32256, -5372),
            (0xff, 848, 0),
            (0x7f, -848, 0),
            (0x00, -5504, -32124),
            (0x0e, -7040, -17788),
        ];
        for &(c, awant, uwant) in v {
            assert_eq!(alaw_to_linear(c), awant, "alaw decode {c:#04x}");
            assert_eq!(ulaw_to_linear(c), uwant, "ulaw decode {c:#04x}");
        }
    }

    #[test]
    fn decode_encode_slices() {
        let pcm = vec![0_i16, 100, -100, 1000];
        let mu = encode(Codec::Pcmu, &pcm);
        assert_eq!(mu.len(), 4);
        let back = decode(Codec::Pcmu, &mu);
        assert_eq!(back.len(), 4);
        let al = encode(Codec::Pcma, &pcm);
        assert_eq!(decode(Codec::Pcma, &al).len(), 4);
    }
}
