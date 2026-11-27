// SPDX-License-Identifier: GPL-2.0-or-later

use common::{
    VideoSample,
    time::{DtsOffset, DurationH264, UnixH264},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Sample flags.
const FLAG_RANDOM_ACCESS_PRESENT: u8 = 0b1000_0000;

const SAMPLE_SIZE_U8: u8 = 25;
#[allow(clippy::as_conversions)]
pub const SAMPLE_SIZE: usize = SAMPLE_SIZE_U8 as usize;

// Sample .
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub random_access_present: bool,

    pub pts: UnixH264,         // Presentation time in 90khz since the unix epoch.
    pub dts_offset: DtsOffset, // Display time offset.
    pub duration: DurationH264,
    pub data_size: u32,
    pub data_offset: u32,
}

impl Sample {
    #[must_use]
    pub fn from_bytes(b: &[u8; SAMPLE_SIZE]) -> Self {
        let flags = b[0];
        Self {
            random_access_present: flags & FLAG_RANDOM_ACCESS_PRESENT != 0,
            pts: UnixH264::new(i64::from_be_bytes([
                b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8],
            ])),
            dts_offset: DtsOffset::new(i32::from_be_bytes([b[9], b[10], b[11], b[12]])),
            duration: DurationH264::new(u32::from_be_bytes([b[13], b[14], b[15], b[16]]).into()),
            data_offset: u32::from_be_bytes([b[17], b[18], b[19], b[20]]),
            data_size: u32::from_be_bytes([b[21], b[22], b[23], b[24]]),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, std::num::TryFromIntError> {
        let mut flags: u8 = 0;
        if self.random_access_present {
            flags |= FLAG_RANDOM_ACCESS_PRESENT;
        }

        let mut out = Vec::with_capacity(SAMPLE_SIZE);

        out.push(flags);
        out.extend_from_slice(&self.pts.to_be_bytes());
        out.extend_from_slice(&self.dts_offset.to_be_bytes());
        out.extend_from_slice(&self.duration.as_u32()?.to_be_bytes());
        out.extend_from_slice(&self.data_offset.to_be_bytes());
        out.extend_from_slice(&self.data_size.to_be_bytes());
        Ok(out)
    }

    #[must_use]
    pub fn dts(&self) -> Option<UnixH264> {
        self.pts.checked_sub(self.dts_offset.into())
    }

    #[must_use]
    pub fn end(&self) -> Option<UnixH264> {
        self.pts.checked_add(self.duration.into())
    }
}

// Writes videos in our custom format.
#[allow(clippy::module_name_repetitions)]
pub struct VideoWriter<'a, W: AsyncWrite + Unpin> {
    meta: &'a mut W, // Output file.
    mdat: &'a mut W, // Output file.

    mdat_pos: u32,
}

#[derive(Debug, Error)]
pub enum CreateVideoWriterError {
    #[error("{0}")]
    TryFromInt(#[from] std::num::TryFromIntError),

    #[error("write: {0}")]
    Write(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum WriteSampleError {
    #[error("{0}")]
    TryFromInt(#[from] std::num::TryFromIntError),

    #[error("write: {0}")]
    Write(std::io::Error),

    #[error("flush: {0}")]
    Flush(std::io::Error),
}

impl<'a, W: AsyncWrite + Unpin> VideoWriter<'a, W> {
    // Creates a new writer and writes the header.
    pub async fn new(
        meta: &'a mut W,
        mdat: &'a mut W,
        header: MetaHeader,
    ) -> Result<VideoWriter<'a, W>, CreateVideoWriterError> {
        meta.write_all(&header.marshal()?).await?;
        Ok(Self {
            meta,
            mdat,
            mdat_pos: 0,
        })
    }

    pub async fn write_frames<'b, T: Iterator<Item = &'b VideoSample>>(
        &mut self,
        frames: T,
    ) -> Result<(), WriteSampleError> {
        use WriteSampleError::*;

        for frame in frames {
            self.write_sample(frame).await?;
        }
        self.mdat.flush().await.map_err(Flush)?;
        self.meta.flush().await.map_err(Flush)?;
        Ok(())
    }

    // Writes a single sample in the custom format to the output files.
    pub async fn write_sample(&mut self, sample: &VideoSample) -> Result<(), WriteSampleError> {
        use WriteSampleError::*;

        let s = Sample {
            random_access_present: sample.random_access_present,
            pts: sample.pts,
            dts_offset: sample.dts_offset,
            duration: sample.duration,
            data_offset: self.mdat_pos,
            data_size: u32::try_from(sample.avcc.len())?,
        };

        self.mdat.write_all(&sample.avcc).await.map_err(Write)?;
        self.mdat_pos += u32::try_from(sample.avcc.len())?;

        self.meta.write_all(&s.encode()?).await.map_err(Write)?;

        Ok(())
    }
}

// Reads a single meta file.
#[derive(Debug, Error)]
pub enum ReadMetaError {
    #[error("unmarshal header {0}")]
    UnmarshalHeader(#[from] HeaderFromReaderError),

    #[error("{0}")]
    TryFromInt(#[from] std::num::TryFromIntError),

    #[error("seek: {0}")]
    Seek(std::io::Error),

    #[error("read: {0}")]
    Read(std::io::Error),
}

// Reads header and all samples.
pub async fn read_meta<T>(
    mut file: T,
    file_size: u64,
) -> Result<(MetaHeader, Vec<Sample>), ReadMetaError>
where
    T: AsyncRead + Unpin,
{
    use ReadMetaError::*;

    let header = MetaHeader::from_reader(&mut file).await?;
    let header_size = u64::try_from(header.size())?;
    let sample_count = usize::try_from((file_size - header_size) / u64::from(SAMPLE_SIZE_U8))?;

    let mut buf = [0; SAMPLE_SIZE];
    let mut samples = vec![Sample::default(); sample_count];
    for sample in &mut samples {
        file.read_exact(&mut buf).await.map_err(Read)?;
        *sample = Sample::from_bytes(&buf);
    }

    Ok((header, samples))
}

// Recording meta file header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaHeader {
    // Wire format version. New recordings are always written as v2;
    // v1 recordings are read with codec "avc1".
    pub version: u8,
    pub start_time: UnixH264,
    pub width: u16,
    pub height: u16,
    // RFC 6381 codec string, e.g. "avc1.64001E". v1 headers didn't carry
    // this field and are read as "avc1".
    pub codec: String,
    pub extra_data: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum HeaderFromReaderError {
    #[error("old version, upgrade with this script: TODO")]
    OldVersion,

    #[error("unsupported version")]
    UnsupportedVersion,

    #[error("invalid codec")]
    InvalidCodec,

    #[error("read: {0}")]
    Read(#[from] std::io::Error),

    #[error("{0}")]
    TryFromInt(#[from] std::num::TryFromIntError),
}

impl MetaHeader {
    pub async fn from_reader<R: AsyncRead + Unpin>(
        r: &mut R,
    ) -> Result<Self, HeaderFromReaderError> {
        let mut api_version = [0];
        r.read_exact(&mut api_version).await?;
        let version = api_version[0];
        if version == 0 {
            return Err(HeaderFromReaderError::OldVersion);
        } else if version > 2 {
            return Err(HeaderFromReaderError::UnsupportedVersion);
        }

        // Start time.
        let mut start_time = [0; 8];
        r.read_exact(&mut start_time).await?;
        let start_time = UnixH264::new(i64::from_be_bytes(start_time));

        // Width.
        let mut width = [0; 2];
        r.read_exact(&mut width).await?;
        let width = u16::from_be_bytes(width);

        // Height.
        let mut height = [0; 2];
        r.read_exact(&mut height).await?;
        let height = u16::from_be_bytes(height);

        // Codec. v1 headers predate the field and are always h264.
        let codec = if version == 1 {
            "avc1".to_owned()
        } else {
            let mut codec_len = [0];
            r.read_exact(&mut codec_len).await?;
            let mut codec = vec![0; codec_len[0].into()];
            r.read_exact(&mut codec).await?;
            String::from_utf8(codec)
                .ok()
                .filter(|c| !c.is_empty())
                .ok_or(HeaderFromReaderError::InvalidCodec)?
        };

        // Extra data.
        let mut size_buf = [0; 2];
        r.read_exact(&mut size_buf).await?;

        let size = u16::from_be_bytes(size_buf);

        let mut extra_data = vec![0; size.into()];
        r.read_exact(&mut extra_data).await?;

        Ok(MetaHeader {
            version,
            start_time,
            width,
            height,
            codec,
            extra_data,
        })
    }

    // Marshaled size.
    #[must_use]
    pub fn size(&self) -> usize {
        // v1: 1 version + 8 start + 2 width + 2 height + 2 extra_len.
        // v2: v1 + 1 codec_len + codec_len.
        if self.version == 1 {
            15 + self.extra_data.len()
        } else {
            16 + self.codec.len() + self.extra_data.len()
        }
    }

    pub fn marshal(&self) -> Result<Vec<u8>, std::num::TryFromIntError> {
        let mut out = Vec::with_capacity(self.size());

        out.push(self.version);

        // Start time.
        out.extend_from_slice(&self.start_time.to_be_bytes());

        // Width.
        out.extend_from_slice(&self.width.to_be_bytes());

        // Height.
        out.extend_from_slice(&self.height.to_be_bytes());

        // Codec.
        if self.version >= 2 {
            let codec_len = u8::try_from(self.codec.len())?;
            out.push(codec_len);
            out.extend_from_slice(self.codec.as_bytes());
        }

        // Extra data.
        let extra_data_size = u16::try_from(self.extra_data.len())?;
        out.extend_from_slice(&extra_data_size.to_be_bytes());
        out.extend_from_slice(&self.extra_data);

        Ok(out)
    }

    #[must_use]
    pub fn params(&self) -> TrackParameters {
        TrackParameters {
            width: self.width,
            height: self.height,
            codec: self.codec.clone(),
            extra_data: self.extra_data.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TrackParameters {
    pub width: u16,
    pub height: u16,
    pub codec: String,
    pub extra_data: Vec<u8>,
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use common::time::{DtsOffset, DurationH264, UnixH264};
    use pretty_assertions::assert_eq;
    use pretty_hex::pretty_hex;
    use sentryshot_padded_bytes::PaddedBytes;
    use std::{io::Cursor, sync::Arc};

    #[tokio::test]
    async fn test_video() {
        let mut meta = Vec::new();
        let mut mdat = Vec::new();

        let test_header = MetaHeader {
            version: 2,
            start_time: UnixH264::new(1_000_000_000),
            width: 1920,
            height: 1080,
            codec: "avc1".to_owned(),
            extra_data: vec![0, 1],
        };

        let mut w = VideoWriter::new(&mut meta, &mut mdat, test_header.clone())
            .await
            .unwrap();

        let frames = [
            VideoSample {
                pts: UnixH264::new(100_000_000_000_000_000),
                dts_offset: DtsOffset::new(-1_000_000_000),
                random_access_present: true,
                avcc: Arc::new(PaddedBytes::new(vec![3, 4])),
                duration: DurationH264::new(1_000_000_000),
            },
            VideoSample {
                pts: UnixH264::new(300_000_000_000_000_000),
                dts_offset: DtsOffset::new(-1_000_000_000),
                random_access_present: false,
                avcc: Arc::new(PaddedBytes::new(vec![5, 6, 7])),
                duration: DurationH264::new(1_000_000_000),
            },
        ];
        w.write_frames(frames.iter()).await.unwrap();

        #[rustfmt::skip]
        let want_meta = &[
            2, // Version.
            0, 0, 0, 0, 0x3b, 0x9a, 0xca, 0, // Start time.
            7, 0x80, // Width.
            4, 0x38, // Height.
            4, b'a', b'v', b'c', b'1', // Codec.
            0, 2, // Extra data size.
            0, 1, // Extra data.
            //
            // Sample 1.
            0b1000_0000, // Flags.
            1, 0x63, 0x45, 0x78, 0x5d, 0x8a, 0, 0, // PTS.
            0xc4, 0x65, 0x36, 0, // DTS offset.
            0x3b, 0x9a, 0xca, 0, // Duration.
            0, 0, 0, 0, // Offset.
            0, 0, 0, 2, // Size.
            //
            // Sample 2.
            0b0000_0000, // Flags.
            4, 0x29, 0xd0, 0x69, 0x18, 0x9e, 0, 0, // PTS.
            0xc4, 0x65, 0x36, 0, // Dts offset.
            0x3b, 0x9a, 0xca, 0, // Duration.
            0, 0, 0, 2, // Offset.
            0, 0, 0, 3, // Size.
        ];
        let want_meta_len = u64::try_from(want_meta.len()).unwrap();

        let want_mdat = vec![3, 4, 5, 6, 7];

        assert_eq!(pretty_hex(&want_meta), pretty_hex(&meta));
        assert_eq!(want_mdat, mdat);

        let (header, samples) = read_meta(Cursor::new(want_meta), want_meta_len)
            .await
            .unwrap();
        assert_eq!(test_header, header);

        let want_samples = &[
            Sample {
                random_access_present: true,
                pts: UnixH264::new(100_000_000_000_000_000),
                dts_offset: DtsOffset::new(-1_000_000_000),
                duration: DurationH264::new(1_000_000_000),
                data_size: 2,
                data_offset: 0,
            },
            Sample {
                random_access_present: false,
                pts: UnixH264::new(300_000_000_000_000_000),
                dts_offset: DtsOffset::new(-1_000_000_000),
                duration: DurationH264::new(1_000_000_000),
                data_size: 3,
                data_offset: 2,
            },
        ];

        assert_eq!(want_samples, samples.as_slice());
    }

    #[tokio::test]
    async fn test_meta_header_v2_round_trip() {
        let header = MetaHeader {
            version: 2,
            start_time: UnixH264::new(42),
            width: 1,
            height: 2,
            codec: "hvc1.1.6.L93.B0".to_owned(),
            extra_data: vec![3, 4, 5],
        };

        let marshaled = header.marshal().unwrap();
        let got = MetaHeader::from_reader(&mut Cursor::new(marshaled))
            .await
            .unwrap();
        assert_eq!(header, got);
    }

    #[tokio::test]
    async fn test_meta_header_v1_read() {
        #[rustfmt::skip]
        let v1 = &[
            1, // Version.
            0, 0, 0, 0, 0x3b, 0x9a, 0xca, 0, // Start time.
            7, 0x80, // Width.
            4, 0x38, // Height.
            0, 2, // Extra data size.
            0, 1, // Extra data.
        ];

        let got = MetaHeader::from_reader(&mut Cursor::new(v1)).await.unwrap();
        let want = MetaHeader {
            version: 1,
            start_time: UnixH264::new(1_000_000_000),
            width: 1920,
            height: 1080,
            codec: "avc1".to_owned(),
            extra_data: vec![0, 1],
        };
        assert_eq!(want, got);
    }

    #[tokio::test]
    async fn test_meta_header_version_errors() {
        let v0 = &[0u8; 20];
        let got = MetaHeader::from_reader(&mut Cursor::new(v0)).await;
        assert!(matches!(got, Err(HeaderFromReaderError::OldVersion)));

        let v3 = &[3u8; 20];
        let got = MetaHeader::from_reader(&mut Cursor::new(v3)).await;
        assert!(matches!(
            got,
            Err(HeaderFromReaderError::UnsupportedVersion)
        ));
    }

    #[tokio::test]
    async fn test_meta_header_invalid_codec() {
        // Non-utf8 codec.
        let bad_utf8 = &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0, 0];
        let got = MetaHeader::from_reader(&mut Cursor::new(bad_utf8)).await;
        assert!(matches!(got, Err(HeaderFromReaderError::InvalidCodec)));

        // Empty codec.
        let empty = &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let got = MetaHeader::from_reader(&mut Cursor::new(empty)).await;
        assert!(matches!(got, Err(HeaderFromReaderError::InvalidCodec)));
    }

    #[test]
    fn test_meta_header_size() {
        let v1 = MetaHeader {
            version: 1,
            start_time: UnixH264::new(0),
            width: 0,
            height: 0,
            codec: "avc1".to_owned(),
            extra_data: vec![0; 10],
        };
        assert_eq!(25, v1.size());

        let v2 = MetaHeader {
            version: 2,
            ..v1.clone()
        };
        // 15 base + 1 codec_len + 4 codec + 10 extra.
        assert_eq!(30, v2.size());
    }
}
