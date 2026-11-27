// SPDX-License-Identifier: GPL-2.0-or-later

// Codec-keyed dispatch over the vendored FFmpeg decoder shim.
// `{h264,h265}` ≡ `{avc*,hvc*|hev*}` prefix matching, consistent with
// the box-type mapping in mp4::sample_entry_tags.

use sentryshot_ffmpeg_h264::{
    DrainError, Draining, Frame, H264BuilderError, H264Decoder, H264DecoderBuilder, HevcDecoder,
    HevcDecoderBuilder, Packet, PaddedBytes, Ready, ReceiveFrameError, SendPacketError,
};
use thiserror::Error;

pub enum VideoDecoder {
    H264(H264Decoder<Ready>),
    Hevc(HevcDecoder<Ready>),
}

pub enum DrainingVideoDecoder {
    H264(H264Decoder<Draining>),
    Hevc(HevcDecoder<Draining>),
}

#[derive(Debug, Error)]
pub enum BuildVideoDecoderError {
    #[error("new decoder: {0}")]
    Builder(#[from] H264BuilderError),

    #[error("unsupported codec: {0}")]
    UnsupportedCodec(String),
}

pub fn build_video_decoder(
    codec: &str,
    extradata: PaddedBytes,
) -> Result<VideoDecoder, BuildVideoDecoderError> {
    if codec.starts_with("avc") {
        Ok(VideoDecoder::H264(
            H264DecoderBuilder::new().avcc(extradata)?,
        ))
    } else if codec.starts_with("hvc") || codec.starts_with("hev") {
        Ok(VideoDecoder::Hevc(
            HevcDecoderBuilder::new().hvcc(extradata)?,
        ))
    } else {
        Err(BuildVideoDecoderError::UnsupportedCodec(codec.to_owned()))
    }
}

impl VideoDecoder {
    pub fn send_packet(&mut self, pkt: &Packet) -> Result<(), SendPacketError> {
        let d = match self {
            Self::H264(d) | Self::Hevc(d) => d,
        };
        d.send_packet(pkt)
    }

    pub fn receive_frame(&mut self, frame: &mut Frame) -> Result<(), ReceiveFrameError> {
        let d = match self {
            Self::H264(d) | Self::Hevc(d) => d,
        };
        d.receive_frame(frame)
    }

    pub fn drain(self) -> Result<DrainingVideoDecoder, DrainError> {
        Ok(match self {
            Self::H264(d) => DrainingVideoDecoder::H264(d.drain()?),
            Self::Hevc(d) => DrainingVideoDecoder::Hevc(d.drain()?),
        })
    }
}

impl DrainingVideoDecoder {
    pub fn receive_frame(&mut self, frame: &mut Frame) -> Result<(), ReceiveFrameError> {
        let d = match self {
            Self::H264(d) | Self::Hevc(d) => d,
        };
        d.receive_frame(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_codec() {
        let got = build_video_decoder("mjpeg", PaddedBytes::new(Vec::new()));
        assert!(matches!(
            got,
            Err(BuildVideoDecoderError::UnsupportedCodec(_))
        ));
    }
}
