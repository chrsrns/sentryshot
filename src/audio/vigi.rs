// SPDX-License-Identifier: GPL-2.0-or-later

//! VIGI private talk fallback — `POST :8800/stream` multipart writer
//! per go2rtc `pkg/tapo` (R46): Digest with `securityEncode`-obfuscated
//! password, `{"talk":{"mode":"aec"}}` session open, `audio/mp2t`
//! parts carrying an MPEG-TS PCMA elementary stream. The device's
//! inbound multipart is drained and discarded (V259 — listen rides
//! the RTSP session; undrained back-pressure stalls the writer).

use std::fmt::Write as _;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;

use crate::{Codec, Error, g711};

/// `:8800` probe + negotiate budget — sits inside the caller's open
/// deadline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

const KEY_SHORT: &[u8] = b"RDpbLfCPsJZ7fiv";
#[rustfmt::skip]
const KEY_LONG: &[u8] = b"yLwVl0zKqws7LgKPRQ84Mdt708T1qQ3Ha7xv3H7NyU84p21BriUWBU43odz3iP4rBL3cD02KZciXTysVXiV8ngg6vL48rPJyAUw0HurW20xqxv9aYb4M9wK1Ae0wlro510qXeU07kV57fQMc8L6aLgMLwygtc0F10a0Dg70TOoouyFhdysuRMO51yY5ZlOZZLEal1h0t9YQW0Ko7oBwmCAHoic4HYbUyVeU3sfQ1xtXcPcf1aT303wAQhv66qzW";

const CLIENT_BOUNDARY: &str = "----client-stream-boundary--";
const DEVICE_BOUNDARY: &str = "--device-stream-boundary--";

const TALK_REQUEST: &str =
    r#"{"params":{"talk":{"mode":"aec"},"method":"get"},"seq":3,"type":"request"}"#;

/// go2rtc `securityEncode` — per-position XOR of password and
/// `keyShort` indexes `keyLong`; pads to 15 chars via `keyShort`.
#[must_use]
fn security_encode(pw: &str) -> String {
    let s = pw.as_bytes();
    let n = s.len().max(KEY_SHORT.len());
    let mut out = String::with_capacity(n);
    for i in 0..n {
        let (mut c1, mut c2) = (187_usize, 187_usize);
        if i >= s.len() {
            c1 = usize::from(KEY_SHORT[i]);
        } else if i >= KEY_SHORT.len() {
            c2 = usize::from(s[i]);
        } else {
            c1 = usize::from(KEY_SHORT[i]);
            c2 = usize::from(s[i]);
        }
        out.push(char::from(KEY_LONG[(c1 ^ c2) % KEY_LONG.len()]));
    }
    out
}

/// `:8800` talk session — open TCP + negotiated `session_id`.
pub struct VigiTalk {
    wr: OwnedWriteHalf,
    session_id: String,
    mux: TsMuxer,
    drain: tokio::task::JoinHandle<()>,
}

impl Drop for VigiTalk {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

/// Probe + negotiate outcome — the caller maps to fault classes.
pub enum VigiProbe {
    /// `session_id` negotiated; writer ready.
    Ready(VigiTalk),
    /// TCP refused / connect timeout — transport, session-only.
    Unreachable,
    /// Peer answered but refused/failed the exchange (bad creds,
    /// 4xx, malformed multipart) — capability fault, persists.
    Refused,
}

impl VigiTalk {
    /// TCP probe → `POST /stream` (Digest retry on 401) → talk
    /// request part → device `session_id`. Fixed `:8800` (V259).
    pub async fn connect(host: &str, user: &str, pass: &str) -> VigiProbe {
        Self::connect_addr(&format!("{host}:8800"), user, pass).await
    }

    /// `connect` with an explicit `host:port` — stub-server tests
    /// can't bind :8800.
    pub(crate) async fn connect_addr(addr: &str, user: &str, pass: &str) -> VigiProbe {
        let Ok(Ok(stream)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await
        else {
            return VigiProbe::Unreachable;
        };
        let (rd, mut wr) = stream.into_split();
        let mut rd = tokio::io::BufReader::new(rd);

        let mut auth: Option<String> = None;
        let mut answered = false;
        // Up to two POSTs: probe, then Digest-answered retry.
        for _ in 0..2 {
            let mut req = String::with_capacity(256);
            let _ = write!(
                req,
                "POST /stream HTTP/1.1\r\nHost: {addr}\r\nContent-Type: multipart/mixed; boundary=--client-stream-boundary--\r\nContent-Length: 0\r\n"
            );
            if let Some(a) = &auth {
                let _ = write!(req, "Authorization: {a}\r\n");
            }
            req.push_str("\r\n");
            if wr.write_all(req.as_bytes()).await.is_err() {
                return VigiProbe::Unreachable;
            }
            let Ok(Ok(head)) = tokio::time::timeout(CONNECT_TIMEOUT, read_head(&mut rd)).await
            else {
                return VigiProbe::Unreachable;
            };
            let Some(head) = head else {
                return VigiProbe::Unreachable;
            };
            let status = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            match status {
                200 => {
                    answered = true;
                    break;
                }
                401 => {
                    let www = head
                        .lines()
                        .find_map(|l| l.strip_prefix("WWW-Authenticate: "))
                        .unwrap_or("")
                        .trim();
                    let Some(challenge) = onvif::parse_digest_challenge(www) else {
                        return VigiProbe::Refused;
                    };
                    auth = Some(onvif::digest_authorization(
                        &challenge,
                        user,
                        &security_encode(pass),
                        "/stream",
                        "POST",
                    ));
                }
                _ => return VigiProbe::Refused,
            }
        }
        if !answered {
            return VigiProbe::Refused;
        }
        // Talk session open — JSON part then read device parts until
        // the session_id arrives.
        let part = format!(
            "{CLIENT_BOUNDARY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{TALK_REQUEST}\r\n",
            TALK_REQUEST.len()
        );
        // Mid-POST fault after the peer answered — persists (V259).
        if wr.write_all(part.as_bytes()).await.is_err() {
            return VigiProbe::Refused;
        }
        let session_id = match tokio::time::timeout(CONNECT_TIMEOUT, read_session_id(&mut rd)).await
        {
            // EOF/malformed mid-POST — the peer disproved the
            // protocol; persists (V259 boundary).
            Ok(Some(id)) => id,
            Ok(None) => return VigiProbe::Refused,
            // Answer-then-stall is a transport fault — session-only.
            Err(_) => return VigiProbe::Unreachable,
        };
        // Muxer header is the first audio part (PAT+PMT).
        let mut talk = Self {
            wr,
            session_id,
            mux: TsMuxer::new(),
            drain: tokio::spawn(async move {
                let mut buf = [0_u8; 4096];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut rd, &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }),
        };
        let hdr = talk.mux.header();
        if talk.send_part(&hdr).await.is_err() {
            return VigiProbe::Refused;
        }
        VigiProbe::Ready(talk)
    }

    /// One `audio/mp2t` multipart part onto the wire.
    async fn send_part(&mut self, body: &[u8]) -> std::io::Result<()> {
        let mut part = String::with_capacity(160);
        let _ = write!(
            part,
            "{CLIENT_BOUNDARY}\r\nContent-Type: audio/mp2t\r\nX-If-Encrypt: 0\r\nX-Session-Id: {}\r\nContent-Length: {}\r\n\r\n",
            self.session_id,
            body.len()
        );
        self.wr.write_all(part.as_bytes()).await?;
        self.wr.write_all(body).await
    }

    /// One talk chunk → `audio/mp2t` part. `ts8k` is the running RTP
    /// timestamp (8 kHz clock); converted to 90 kHz PTS (R48).
    pub async fn send(&mut self, pcm: &[i16], ts8k: u32) -> Result<(), Error> {
        let pcma = g711::encode(Codec::Pcma, pcm);
        let pts = u64::from(ts8k) * 90000 / 8000;
        let body = self
            .mux
            .payload(u32::try_from(pts).unwrap_or(u32::MAX), &pcma);
        self.send_part(&body).await.map_err(Error::Transport)
    }
}

/// Read one response head (`\r\n\r\n`-terminated); `None` on EOF.
async fn read_head(
    rd: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> std::io::Result<Option<String>> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if line == "\r\n" {
            return Ok(Some(head));
        }
        head.push_str(&line);
    }
}

/// Consume device multipart parts until a JSON part carries
/// `params.session_id`; `None` on EOF/malformed.
async fn read_session_id(
    rd: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Option<String> {
    let mut at_boundary = false;
    let mut content_len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if rd.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line == DEVICE_BOUNDARY || line == format!("{DEVICE_BOUNDARY}--") {
            at_boundary = true;
            content_len = None;
            continue;
        }
        if !at_boundary {
            continue;
        }
        if line.is_empty() {
            let len = content_len?;
            let mut body = vec![0_u8; len];
            tokio::io::AsyncReadExt::read_exact(rd, &mut body)
                .await
                .ok()?;
            let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
            return v
                .pointer("/params/session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_len = rest.trim().parse().ok();
        }
    }
}

// ── Minimal MPEG-TS muxer — one PCMA elementary stream ──────────────
// go2rtc `pkg/mpegts`: PAT (pid 0) + PMT (pid 0x1000) + PES (pid
// 0x100, stream type 0x90 `PCMATapo`, stream id 0xC0), 188 B packets.

const TS_SIZE: usize = 188;
const PID_PAT: u16 = 0;
const PID_PMT: u16 = 0x1000;
const PID_PES: u16 = 0x100;
const STREAM_TYPE_PCMA_TAPO: u8 = 0x90;
const STREAM_ID_AUDIO: u8 = 0xC0;

/// PAT/PMT + PES packetizer; continuity counters per PID.
pub struct TsMuxer {
    counters: [u8; 3], // PAT, PMT, PES
}

impl TsMuxer {
    #[must_use]
    pub fn new() -> Self {
        Self { counters: [0; 3] }
    }

    /// PAT + PMT pair — sent once as the first part body.
    #[must_use]
    pub fn header(&mut self) -> Vec<u8> {
        let mut out = self.pat();
        out.extend_from_slice(&self.pmt());
        out
    }

    /// One PES → 188 B TS packet(s). `pts` is in the 90 kHz domain.
    #[must_use]
    pub fn payload(&mut self, pts: u32, payload: &[u8]) -> Vec<u8> {
        let mut pes = Vec::with_capacity(14 + payload.len());
        pes.extend_from_slice(&[0, 0, 1, STREAM_ID_AUDIO]);
        let size = u16::try_from(3 + 5 + payload.len()).unwrap_or(u16::MAX);
        pes.extend_from_slice(&size.to_be_bytes());
        pes.extend_from_slice(&[0x80, 0x80, 5]);
        write_pts(&mut pes, pts);
        pes.extend_from_slice(payload);
        let mut out = Vec::with_capacity(TS_SIZE * 2);
        let mut pusi = true;
        while !pes.is_empty() {
            let take = pes.len().min(TS_SIZE - 4);
            out.push(0x47);
            out.push((if pusi { 0x40 } else { 0x00 }) | u8::try_from(PID_PES >> 8).unwrap_or(0));
            out.push(u8::try_from(PID_PES & 0xFF).unwrap_or(0));
            pusi = false;
            if take < TS_SIZE - 4 {
                let pad = TS_SIZE - 4 - 1 - take;
                out.push(0x30 | (self.counters[2] & 0xF)); // adaptation + payload
                out.push(u8::try_from(pad).unwrap_or(0));
                out.resize(out.len() + pad, 0);
            } else {
                out.push(0x10 | (self.counters[2] & 0xF)); // payload only
            }
            self.counters[2] = self.counters[2].wrapping_add(1);
            out.extend_from_slice(&pes[..take]);
            pes.drain(..take);
        }
        out
    }

    fn pat(&mut self) -> Vec<u8> {
        // program 1 → PMT pid, little-endian CRC32 per go2rtc table.
        let mut section = Vec::with_capacity(9);
        psi_header(&mut section, 0, 4);
        section.extend_from_slice(&1_u16.to_be_bytes()); // program
        section.extend_from_slice(&[
            0xE0 | u8::try_from(PID_PMT >> 8).unwrap_or(0),
            u8::try_from(PID_PMT & 0xFF).unwrap_or(0),
        ]);
        let crc = checksum(&section[1..]);
        section.extend_from_slice(&crc.to_le_bytes());
        self.psi_packet(PID_PAT, 0, &section)
    }

    fn pmt(&mut self) -> Vec<u8> {
        let mut section = Vec::with_capacity(18);
        psi_header(&mut section, 2, 4 + 5);
        section.extend_from_slice(&[0xE0 | 0x1F, 0xFF]); // PCR pid unused
        section.extend_from_slice(&[0xF0, 0x00]); // program info len 0
        section.push(STREAM_TYPE_PCMA_TAPO);
        section.extend_from_slice(&[
            0xE0 | u8::try_from(PID_PES >> 8).unwrap_or(0),
            u8::try_from(PID_PES & 0xFF).unwrap_or(0),
        ]);
        section.extend_from_slice(&[0xF0, 0x00]); // ES info len 0
        let crc = checksum(&section[1..]);
        section.extend_from_slice(&crc.to_le_bytes());
        self.psi_packet(PID_PMT, 1, &section)
    }

    fn psi_packet(&mut self, pid: u16, slot: usize, section: &[u8]) -> Vec<u8> {
        debug_assert!(section.len() < TS_SIZE - 4);
        let mut out = vec![0x47];
        out.push(0x40 | u8::try_from(pid >> 8).unwrap_or(0)); // PUSI
        out.push(u8::try_from(pid & 0xFF).unwrap_or(0));
        out.push(0x30 | (self.counters[slot] & 0xF)); // adaptation + payload
        let pad = TS_SIZE - 4 - 1 - section.len();
        out.push(u8::try_from(pad).unwrap_or(0));
        out.resize(out.len() + pad, 0xFF); // stuffing
        self.counters[slot] = self.counters[slot].wrapping_add(1);
        out.extend_from_slice(section);
        debug_assert_eq!(out.len(), TS_SIZE);
        out
    }
}

fn psi_header(out: &mut Vec<u8>, table_id: u8, size: u16) {
    out.push(0); // pointer field
    out.push(table_id);
    // syntax 1, private 0, reserved 11, len unused 00, len 10b
    let len = 5 + size + 4;
    out.push(0xB0 | u8::try_from(len >> 8).unwrap_or(0));
    out.push(u8::try_from(len & 0xFF).unwrap_or(0));
    out.extend_from_slice(&1_u16.to_be_bytes()); // table id ext
    out.push(0xC1); // reserved 11 + version 0 + current/next 1
    out.push(0); // section number
    out.push(0); // last section number
}

fn write_pts(out: &mut Vec<u8>, t: u32) {
    out.push(0x20 | u8::try_from(t >> 29).unwrap_or(0) << 1 | 1);
    out.push(u8::try_from(t >> 22).unwrap_or(0));
    out.push(u8::try_from(t >> 14).unwrap_or(0) << 1 | 1);
    out.push(u8::try_from(t >> 7).unwrap_or(0));
    out.push(u8::try_from(t << 1).unwrap_or(0) | 1);
}

// Verbatim go2rtc table — kept unseparated for diff parity.
#[allow(clippy::unreadable_literal)]
const CRC_TABLE: [u32; 256] = [
    0x00000000, 0xB71DC104, 0x6E3B8209, 0xD926430D, 0xDC760413, 0x6B6BC517, 0xB24D861A, 0x0550471E,
    0xB8ED0826, 0x0FF0C922, 0xD6D68A2F, 0x61CB4B2B, 0x649B0C35, 0xD386CD31, 0x0AA08E3C, 0xBDBD4F38,
    0x70DB114C, 0xC7C6D048, 0x1EE09345, 0xA9FD5241, 0xACAD155F, 0x1BB0D45B, 0xC2969756, 0x758B5652,
    0xC836196A, 0x7F2BD86E, 0xA60D9B63, 0x11105A67, 0x14401D79, 0xA35DDC7D, 0x7A7B9F70, 0xCD665E74,
    0xE0B62398, 0x57ABE29C, 0x8E8DA191, 0x39906095, 0x3CC0278B, 0x8BDDE68F, 0x52FBA582, 0xE5E66486,
    0x585B2BBE, 0xEF46EABA, 0x3660A9B7, 0x817D68B3, 0x842D2FAD, 0x3330EEA9, 0xEA16ADA4, 0x5D0B6CA0,
    0x906D32D4, 0x2770F3D0, 0xFE56B0DD, 0x494B71D9, 0x4C1B36C7, 0xFB06F7C3, 0x2220B4CE, 0x953D75CA,
    0x28803AF2, 0x9F9DFBF6, 0x46BBB8FB, 0xF1A679FF, 0xF4F63EE1, 0x43EBFFE5, 0x9ACDBCE8, 0x2DD07DEC,
    0x77708634, 0xC06D4730, 0x194B043D, 0xAE56C539, 0xAB068227, 0x1C1B4323, 0xC53D002E, 0x7220C12A,
    0xCF9D8E12, 0x78804F16, 0xA1A60C1B, 0x16BBCD1F, 0x13EB8A01, 0xA4F64B05, 0x7DD00808, 0xCACDC90C,
    0x07AB9778, 0xB0B6567C, 0x69901571, 0xDE8DD475, 0xDBDD936B, 0x6CC0526F, 0xB5E61162, 0x02FBD066,
    0xBF469F5E, 0x085B5E5A, 0xD17D1D57, 0x6660DC53, 0x63309B4D, 0xD42D5A49, 0x0D0B1944, 0xBA16D840,
    0x97C6A5AC, 0x20DB64A8, 0xF9FD27A5, 0x4EE0E6A1, 0x4BB0A1BF, 0xFCAD60BB, 0x258B23B6, 0x9296E2B2,
    0x2F2BAD8A, 0x98366C8E, 0x41102F83, 0xF60DEE87, 0xF35DA999, 0x4440689D, 0x9D662B90, 0x2A7BEA94,
    0xE71DB4E0, 0x500075E4, 0x892636E9, 0x3E3BF7ED, 0x3B6BB0F3, 0x8C7671F7, 0x555032FA, 0xE24DF3FE,
    0x5FF0BCC6, 0xE8ED7DC2, 0x31CB3ECF, 0x86D6FFCB, 0x8386B8D5, 0x349B79D1, 0xEDBD3ADC, 0x5AA0FBD8,
    0xEEE00C69, 0x59FDCD6D, 0x80DB8E60, 0x37C64F64, 0x3296087A, 0x858BC97E, 0x5CAD8A73, 0xEBB04B77,
    0x560D044F, 0xE110C54B, 0x38368646, 0x8F2B4742, 0x8A7B005C, 0x3D66C158, 0xE4408255, 0x535D4351,
    0x9E3B1D25, 0x2926DC21, 0xF0009F2C, 0x471D5E28, 0x424D1936, 0xF550D832, 0x2C769B3F, 0x9B6B5A3B,
    0x26D61503, 0x91CBD407, 0x48ED970A, 0xFFF0560E, 0xFAA01110, 0x4DBDD014, 0x949B9319, 0x2386521D,
    0x0E562FF1, 0xB94BEEF5, 0x606DADF8, 0xD7706CFC, 0xD2202BE2, 0x653DEAE6, 0xBC1BA9EB, 0x0B0668EF,
    0xB6BB27D7, 0x01A6E6D3, 0xD880A5DE, 0x6F9D64DA, 0x6ACD23C4, 0xDDD0E2C0, 0x04F6A1CD, 0xB3EB60C9,
    0x7E8D3EBD, 0xC990FFB9, 0x10B6BCB4, 0xA7AB7DB0, 0xA2FB3AAE, 0x15E6FBAA, 0xCCC0B8A7, 0x7BDD79A3,
    0xC660369B, 0x717DF79F, 0xA85BB492, 0x1F467596, 0x1A163288, 0xAD0BF38C, 0x742DB081, 0xC3307185,
    0x99908A5D, 0x2E8D4B59, 0xF7AB0854, 0x40B6C950, 0x45E68E4E, 0xF2FB4F4A, 0x2BDD0C47, 0x9CC0CD43,
    0x217D827B, 0x9660437F, 0x4F460072, 0xF85BC176, 0xFD0B8668, 0x4A16476C, 0x93300461, 0x242DC565,
    0xE94B9B11, 0x5E565A15, 0x87701918, 0x306DD81C, 0x353D9F02, 0x82205E06, 0x5B061D0B, 0xEC1BDC0F,
    0x51A69337, 0xE6BB5233, 0x3F9D113E, 0x8880D03A, 0x8DD09724, 0x3ACD5620, 0xE3EB152D, 0x54F6D429,
    0x7926A9C5, 0xCE3B68C1, 0x171D2BCC, 0xA000EAC8, 0xA550ADD6, 0x124D6CD2, 0xCB6B2FDF, 0x7C76EEDB,
    0xC1CBA1E3, 0x76D660E7, 0xAFF023EA, 0x18EDE2EE, 0x1DBDA5F0, 0xAAA064F4, 0x738627F9, 0xC49BE6FD,
    0x09FDB889, 0xBEE0798D, 0x67C63A80, 0xD0DBFB84, 0xD58BBC9A, 0x62967D9E, 0xBBB03E93, 0x0CADFF97,
    0xB110B0AF, 0x060D71AB, 0xDF2B32A6, 0x6836F3A2, 0x6D66B4BC, 0xDA7B75B8, 0x035D36B5, 0xB440F7B1,
];

fn checksum(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = CRC_TABLE[usize::from(b ^ crc.to_le_bytes()[0])] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc as tmpsc;

    // Python-recomputed `securityEncode` goldens (go2rtc algorithm).
    #[test]
    fn security_encode_vectors() {
        let v: &[(&str, &str)] = &[
            ("admin", "WaQ7xbhc9TefbwK"),
            ("password123", "xHVQ3wiB3A0fbwK"),
            ("", "tyWcQbhc9TefbwK"),
            (
                "a-very-long-password-over-15-chars",
                "W0zK4H9iq3PTtq090CYyMCKUYM5MMim4Y9",
            ),
            ("xxxxxxxxxxxxxxx", "8iqTB33y7UxV3QK"),
        ];
        for &(pw, want) in v {
            assert_eq!(security_encode(pw), want, "security_encode({pw:?})");
        }
    }

    #[test]
    fn ts_mux_structure() {
        let mut m = TsMuxer::new();
        let hdr = m.header();
        assert_eq!(hdr.len(), 2 * TS_SIZE);
        assert_eq!((hdr[0], hdr[TS_SIZE]), (0x47, 0x47));
        // PAT → pid 0 + PUSI; PMT → pid 0x1000 + PUSI.
        assert_eq!(hdr[1] & 0x7F, 0x40);
        assert_eq!(hdr[TS_SIZE + 1] & 0x7F, 0x50);
        // PMT advertises stream type 0x90 on elementary pid 0x100.
        let pmt = &hdr[TS_SIZE..];
        assert!(
            pmt.windows(3).any(|w| w == [0x90, 0xE1, 0x00]),
            "PMT stream entry"
        );

        // One 160-byte PCMA payload → exactly one TS packet.
        let ts = m.payload(90000, &[0xAA; 160]);
        assert_eq!(ts.len(), TS_SIZE);
        assert_eq!(ts[0], 0x47);
        assert_eq!(u16::from_be_bytes([ts[1] & 0x3F, ts[2]]), PID_PES);
        assert_eq!(ts[1] & 0x40, 0x40); // PUSI
        // PES start code + stream id follow the adaptation field.
        let ad_len = 4 + 1 + usize::from(ts[4]);
        let pes_off = ad_len;
        assert_eq!(&ts[pes_off..pes_off + 4], &[0, 0, 1, STREAM_ID_AUDIO]);
        // PTS marker bits.
        assert_eq!(ts[pes_off + 6], 0x80);
        assert_eq!(ts[pes_off + 7], 0x80);
        // Tail payload is the PCMA bytes.
        assert_eq!(&ts[TS_SIZE - 160..], &[0xAA; 160]);
    }

    /// Read one request head; `None` on EOF.
    async fn req_head(
        rd: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    ) -> Option<String> {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if rd.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            if line == "\r\n" {
                return Some(head);
            }
            head.push_str(&line);
        }
    }

    /// Stub `:8800`: canned status path → `session_id` → log parts.
    /// `tx` ships every client→device byte block for assertions.
    async fn stub_vigi(mode: &'static str) -> (String, tmpsc::Receiver<Vec<u8>>) {
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = lis.local_addr().unwrap().to_string();
        let (tx, rx) = tmpsc::channel::<Vec<u8>>(32);
        tokio::spawn(async move {
            let Ok((stream, _)) = lis.accept().await else {
                return;
            };
            let (rd, mut wr) = stream.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            let Some(head) = req_head(&mut rd).await else {
                return;
            };
            tx.send(head.into_bytes()).await.unwrap();
            match mode {
                "401" => {
                    wr.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\n\
                          WWW-Authenticate: Digest realm=\"vigi\", nonce=\"abc123\", qop=\"auth\"\r\n\
                          Content-Length: 0\r\n\r\n"
                            .as_slice(),
                    )
                    .await
                    .unwrap();
                    let Some(head) = req_head(&mut rd).await else {
                        return;
                    };
                    tx.send(head.into_bytes()).await.unwrap();
                }
                "refuse" => {
                    wr.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    return;
                }
                "malformed" => {
                    wr.write_all(b"not-http-at-all\r\n\r\n").await.unwrap();
                    return;
                }
                _ => {}
            }
            wr.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed\r\n\r\n")
                .await
                .unwrap();
            // Talk-request JSON part, then the session_id reply part.
            let mut buf = vec![0_u8; 4096];
            let n = rd.read(&mut buf).await.unwrap();
            tx.send(buf[..n].to_vec()).await.unwrap();
            let body = r#"{"params":{"session_id":"sess42"}}"#;
            let sid = format!(
                "{DEVICE_BOUNDARY}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            wr.write_all(sid.as_bytes()).await.unwrap();
            loop {
                let n = rd.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    return;
                }
            }
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn connect_digest_retry_then_header_part() {
        let (addr, mut rx) = stub_vigi("401").await;
        let probe = VigiTalk::connect_addr(&addr, "admin", "admin").await;
        let mut talk = match probe {
            VigiProbe::Ready(t) => t,
            other => panic!("expected Ready, got {}", probe_name(&other)),
        };
        let r1 = String::from_utf8(rx.recv().await.unwrap()).unwrap();
        assert!(r1.starts_with("POST /stream HTTP/1.1"));
        assert!(!r1.contains("Authorization:"), "{r1}");
        let r2 = String::from_utf8(rx.recv().await.unwrap()).unwrap();
        let auth = r2
            .lines()
            .find_map(|l| l.strip_prefix("Authorization: "))
            .unwrap()
            .trim();
        // Verify the response against a fresh MD5 computation.
        let enc = security_encode("admin");
        let ha1 = format!("{:x}", md5::compute(format!("admin:vigi:{enc}")));
        let ha2 = format!("{:x}", md5::compute("POST:/stream"));
        let cnonce = auth
            .split("cnonce=\"")
            .nth(1)
            .and_then(|r| r.split('\"').next())
            .unwrap();
        let want = format!(
            "{:x}",
            md5::compute(format!("{ha1}:abc123:00000001:{cnonce}:auth:{ha2}"))
        );
        assert!(auth.contains(&format!("response=\"{want}\"")), "{auth}");
        assert!(auth.contains("qop=auth, nc=00000001"), "{auth}");

        // Talk-request JSON part.
        let part = String::from_utf8(rx.recv().await.unwrap()).unwrap();
        assert!(part.contains(CLIENT_BOUNDARY), "{part}");
        assert!(part.contains(r#""talk":{"mode":"aec"}"#), "{part}");

        // The muxer header part: audio/mp2t + 2×188 B TS. TCP
        // fragments arbitrarily — accumulate until the part's
        // Content-Length is satisfied.
        let mut buf = rx.recv().await.unwrap();
        let (body_off, want) = part_shape(&buf);
        while buf.len() < body_off + want {
            buf.extend_from_slice(&rx.recv().await.unwrap());
        }
        let hdr_s = String::from_utf8_lossy(&buf[..body_off]);
        assert!(hdr_s.contains("Content-Type: audio/mp2t"), "{hdr_s}");
        assert!(hdr_s.contains("X-Session-Id: sess42"), "{hdr_s}");
        assert_eq!(buf[body_off], 0x47);
        assert_eq!(want, 2 * TS_SIZE);
        buf.drain(..body_off + want);

        // A talk chunk → PCMA PES in one part.
        talk.send(&[1000_i16; 160], 0).await.unwrap();
        while buf.len() < TS_SIZE + 4 {
            buf.extend_from_slice(&rx.recv().await.unwrap());
        }
        let (_off, want) = part_shape(&buf);
        assert_eq!(want, TS_SIZE);
    }

    /// Locate the `\r\n\r\n` boundary and read the part's
    /// declared `Content-Length`.
    fn part_shape(buf: &[u8]) -> (usize, usize) {
        let off = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&buf[..off]);
        let want = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap();
        (off, want)
    }

    fn probe_name(p: &VigiProbe) -> &'static str {
        match p {
            VigiProbe::Ready(_) => "Ready",
            VigiProbe::Unreachable => "Unreachable",
            VigiProbe::Refused => "Refused",
        }
    }

    #[tokio::test]
    async fn unreachable_is_transport() {
        // Bind + immediately drop a listener for a dead port.
        let lis = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = lis.local_addr().unwrap().to_string();
        drop(lis);
        let probe = VigiTalk::connect_addr(&addr, "a", "b").await;
        assert!(matches!(probe, VigiProbe::Unreachable));
    }

    #[tokio::test]
    async fn refused_on_4xx() {
        let (addr, mut rx) = stub_vigi("refuse").await;
        let probe = VigiTalk::connect_addr(&addr, "a", "b").await;
        assert!(matches!(probe, VigiProbe::Refused));
        let _ = rx.recv().await;
    }

    #[tokio::test]
    async fn refused_on_malformed() {
        let (addr, mut rx) = stub_vigi("malformed").await;
        let probe = VigiTalk::connect_addr(&addr, "a", "b").await;
        assert!(matches!(probe, VigiProbe::Refused));
        let _ = rx.recv().await;
    }
}
