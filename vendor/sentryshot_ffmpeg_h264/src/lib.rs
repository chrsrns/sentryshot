// SPDX-License-Identifier: MPL-2.0+

mod error;
mod extradata;
mod h264_decoder;

pub use sentryshot_padded_bytes::PaddedBytes;
pub use sentryshot_util::{Frame, Packet};

pub use h264_decoder::{
    DrainError, Draining, H264BuilderError, H264Decoder, H264DecoderBuilder, HevcDecoder,
    HevcDecoderBuilder, Ready, ReceiveFrameError, SendPacketError,
};
