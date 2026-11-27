// SPDX-License-Identifier: GPL-2.0-or-later

use common::{TrackParameters, VideoSample, time::UnixH264};
use mp4::{ImmutableBox, ImmutableBoxSync};

pub fn generate_init(params: &TrackParameters) -> Result<Vec<u8>, mp4::Mp4Error> {
    mp4::generate_init(
        params.width,
        params.height,
        params.extra_data.clone(),
        &params.codec,
    )
}

pub(crate) fn generate_moof_and_empty_mdat(
    muxer_start_time: UnixH264,
    frames: &[&VideoSample],
) -> Result<Vec<u8>, mp4::GenerateMoofError> {
    /*
       moof
       - mfhd
       - traf (video)
         - tfhd
         - tfdt
         - trun
       mdat (empty)
    */

    let samples: Vec<mp4::FragmentSample> = frames
        .iter()
        .map(|sample| mp4::FragmentSample {
            pts: *sample.pts,
            cto: *sample.dts_offset,
            duration: *sample.duration,
            size: sample.avcc.len(),
            non_sync: !sample.random_access_present,
        })
        .collect();

    let mut buf = mp4::generate_moof(*muxer_start_time, &samples)?;

    // Empty mdat — payload is appended by the caller.
    let mdat_size = frames.iter().map(|v| v.avcc.len()).sum();
    let mdat = mp4::Boxes::new(EmptyMdat(mdat_size));
    buf.reserve(mdat.size());
    mdat.marshal(&mut buf)?;

    Ok(buf)
}

struct EmptyMdat(usize);

impl ImmutableBox for EmptyMdat {
    fn box_type(&self) -> mp4::BoxType {
        mp4::TYPE_MDAT
    }

    fn size(&self) -> usize {
        self.0
    }
}

impl ImmutableBoxSync for EmptyMdat {
    fn marshal(&self, _: &mut dyn std::io::Write) -> Result<(), mp4::Mp4Error> {
        Ok(())
    }
}

impl From<EmptyMdat> for Box<dyn ImmutableBoxSync> {
    fn from(value: EmptyMdat) -> Self {
        Box::new(value)
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use common::time::UnixH264;
    use pretty_assertions::assert_eq;
    use sentryshot_padded_bytes::PaddedBytes;
    use std::sync::Arc;

    // The moof byte layout is covered by mp4's byte-parity tests; this
    // verifies the empty mdat header is appended without payload.
    #[test]
    fn test_generate_moof_and_empty_mdat() {
        let samples = [&VideoSample {
            avcc: Arc::new(PaddedBytes::new(b"abcd".to_vec())),
            ..Default::default()
        }];

        let got = generate_moof_and_empty_mdat(UnixH264::new(0), &samples).unwrap();

        assert_eq!(
            &got[got.len() - 8..],
            &[0, 0, 0, 0xc, b'm', b'd', b'a', b't']
        );
    }
}
