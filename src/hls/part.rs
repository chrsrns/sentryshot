use crate::error::{GeneratePartError, PartFinalizeError};
use bytes::Bytes;
use common::{
    VideoSample,
    time::{DurationH264, UnixH264, UnixNano},
};
use mp4::{ImmutableBox, ImmutableBoxSync};
use std::{io::Cursor, sync::Arc, task::Poll};
use tokio::io::AsyncRead;

fn fragment_samples(video_samples: &[VideoSample]) -> Vec<mp4::FragmentSample> {
    video_samples
        .iter()
        .map(|sample| mp4::FragmentSample {
            pts: *sample.pts,
            cto: *sample.dts_offset,
            duration: *sample.duration,
            size: sample.avcc.len(),
            non_sync: !sample.random_access_present,
        })
        .collect()
}

fn generate_part(
    muxer_start_time: UnixH264,
    video_samples: Arc<Vec<VideoSample>>,
) -> Result<Bytes, GeneratePartError> {
    /*
       moof
       - mfhd
       - traf (video)
         - tfhd
         - tfdt
         - trun
       mdat
    */

    let mut buf = mp4::generate_moof(*muxer_start_time, &fragment_samples(&video_samples))?;

    let mdat = mp4::Boxes::new(MyMdat(video_samples));
    buf.reserve(mdat.size());
    mdat.marshal(&mut buf)?;

    Ok(Bytes::from(buf))
}

struct MyMdat(Arc<Vec<VideoSample>>);

impl ImmutableBox for MyMdat {
    fn box_type(&self) -> mp4::BoxType {
        mp4::TYPE_MDAT
    }

    fn size(&self) -> usize {
        self.0.iter().map(|sample| sample.avcc.len()).sum()
    }
}

impl ImmutableBoxSync for MyMdat {
    fn marshal(&self, w: &mut dyn std::io::Write) -> Result<(), mp4::Mp4Error> {
        for sample in self.0.iter() {
            w.write_all(&sample.avcc)?;
        }
        Ok(())
    }
}

impl From<MyMdat> for Box<dyn ImmutableBoxSync> {
    fn from(value: MyMdat) -> Self {
        Box::new(value)
    }
}

// fmp4 part.
#[derive(Clone)]
#[allow(clippy::module_name_repetitions)]
pub struct MuxerPart {
    pub id: u64,
    pub muxer_start_time: UnixNano,
    pub is_independent: bool,
    pub video_samples: Vec<VideoSample>,
}

impl std::fmt::Debug for MuxerPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {}",
            self.id,
            self.is_independent,
            self.video_samples.len()
        )
    }
}

impl MuxerPart {
    pub fn new(id: u64, muxer_start_time: UnixNano) -> Self {
        Self {
            id,
            muxer_start_time,
            is_independent: false,
            video_samples: Vec::new(),
        }
    }

    pub fn duration(&self) -> Option<DurationH264> {
        let mut total = DurationH264::new(0);
        for e in &self.video_samples {
            total = total.checked_add(e.duration)?;
        }
        Some(total)
    }

    pub fn finalize(self) -> Result<PartFinalized, PartFinalizeError> {
        let rendered_duration = self.duration().ok_or(PartFinalizeError::Duration)?;
        let video_samples = Arc::new(self.video_samples);
        let rendered_content = if video_samples.is_empty() {
            None
        } else {
            Some(generate_part(
                self.muxer_start_time.into(),
                video_samples.clone(),
            )?)
        };

        Ok(PartFinalized {
            id: self.id,
            is_independent: self.is_independent,
            video_samples: video_samples.clone(),
            rendered_duration,
            rendered_content,
        })
    }

    pub fn write_h264(&mut self, sample: VideoSample) {
        if sample.random_access_present {
            self.is_independent = true;
        }
        self.video_samples.push(sample);
    }
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::module_name_repetitions)]
pub struct PartFinalized {
    pub id: u64,

    pub is_independent: bool,
    pub video_samples: Arc<Vec<VideoSample>>,
    pub rendered_content: Option<Bytes>,
    pub rendered_duration: DurationH264,
}

impl PartFinalized {
    pub fn name(&self) -> String {
        part_name(self.id)
    }

    pub fn reader(&self) -> Box<dyn AsyncRead + Send + Unpin> {
        let Some(rendered_content) = &self.rendered_content else {
            return Box::new(Cursor::new(Vec::new()));
        };
        Box::new(Cursor::new(rendered_content.clone()))
    }
}

#[must_use]
#[allow(clippy::module_name_repetitions)]
pub fn part_name(id: u64) -> String {
    ["part", &id.to_string()].join("")
}

pub struct PartsReader {
    parts: Vec<Arc<PartFinalized>>,
    cur_part: usize,
    cur_pos: usize,
}

impl PartsReader {
    #[must_use]
    pub fn new(parts: Vec<Arc<PartFinalized>>) -> Self {
        Self {
            parts,
            cur_part: 0,
            cur_pos: 0,
        }
    }
}

impl AsyncRead for PartsReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut n = 0;
        let buf_len = buf.remaining();

        loop {
            if self.cur_part >= self.parts.len() {
                // EOF.
                return Poll::Ready(Ok(()));
            }

            let Some(part) = &self.parts[self.cur_part].rendered_content else {
                panic!("expected part to exist");
            };

            let part_len = part.len();

            let start = self.cur_pos;
            let amt = std::cmp::min(part.len() - start, buf.remaining());
            let end = start + amt;

            buf.put_slice(&part[start..end]);

            self.cur_pos += amt;
            n += amt;

            if self.cur_pos == part_len {
                self.cur_part += 1;
                self.cur_pos = 0;
            }

            // If buffer is full.
            if n == buf_len {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use sentryshot_padded_bytes::PaddedBytes;

    // The moof byte layout is covered by mp4's byte-parity tests; this
    // verifies the sample mapping and that the mdat payload is appended.
    #[test]
    fn test_generate_part_appends_mdat_payload() {
        let samples = vec![VideoSample {
            avcc: Arc::new(PaddedBytes::new(b"abcd".to_vec())),
            ..Default::default()
        }];

        let got = generate_part(UnixH264::new(0), Arc::new(samples)).unwrap();

        let mut want = mp4::generate_moof(0, &fragment_samples(&[VideoSample {
            avcc: Arc::new(PaddedBytes::new(b"abcd".to_vec())),
            ..Default::default()
        }]))
        .unwrap();
        want.extend([0, 0, 0, 0xc, b'm', b'd', b'a', b't', b'a', b'b', b'c', b'd']);
        assert_eq!(want, got);
    }
}
