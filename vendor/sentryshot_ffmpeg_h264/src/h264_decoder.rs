// SPDX-License-Identifier: MPL-2.0+

use crate::error::{unerror, OpaqueFFmpegError, AVERROR_INVALIDDATA};
use libc::{EAGAIN, EOF};
use sentryshot_ffmpeg_h264_sys::{
    c_decoder_allocate, c_decoder_free, c_decoder_open, c_decoder_recieve_frame,
    c_decoder_send_packet, CH264Decoder, C_DECODER_CODEC_H264, C_DECODER_CODEC_HEVC,
};
use sentryshot_padded_bytes::PaddedBytes;
use sentryshot_util::{
    pixfmt::{frame_raw_height, PixelFormat, TryFromPixelFormatError},
    ColorRange, Frame, InvalidColorRange, Packet, NUM_DATA_POINTERS,
};
use std::{
    marker::PhantomData,
    num::NonZeroU16,
    ops::Deref,
    os::raw::{c_int, c_uint},
    ptr::NonNull,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum H264BuilderError {
    //#[error("decode extradata: ")]
    //DecodeExtradata(#[from] DecodeExtradataError),
    //
    #[error("allocate decoder")]
    AllocateDecoder,

    #[error("open decoder: {0}")]
    OpenDecoder(#[from] H264DecoderOpenError),
}

pub struct H264DecoderBuilder;

#[allow(clippy::new_without_default)]
impl H264DecoderBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }

    /// Create and initialize a decoder with AVCC extradata.
    // The extradata is owned in case ffmpeg decides to modifiy it.
    pub fn avcc(&self, extradata: PaddedBytes) -> Result<H264Decoder<Ready>, H264BuilderError> {
        Ok(H264Decoder {
            inner: unsafe { new_c_decoder(C_DECODER_CODEC_H264, Some(extradata))? },
            marker: PhantomData {},
        })
    }

    /// Create a uninitialized ANNEXB decoder.
    pub fn annex_b(&self) -> Result<H264Decoder<Ready>, H264BuilderError> {
        Ok(H264Decoder {
            inner: unsafe { new_c_decoder(C_DECODER_CODEC_H264, None)? },
            marker: PhantomData {},
        })
    }
}

pub struct HevcDecoderBuilder;

#[allow(clippy::new_without_default)]
impl HevcDecoderBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }

    /// Create and initialize a decoder with HVCC extradata.
    // The extradata is owned in case ffmpeg decides to modifiy it.
    pub fn hvcc(&self, extradata: PaddedBytes) -> Result<HevcDecoder<Ready>, H264BuilderError> {
        Ok(HevcDecoder {
            inner: unsafe { new_c_decoder(C_DECODER_CODEC_HEVC, Some(extradata))? },
            marker: PhantomData {},
        })
    }

    /// Create a uninitialized ANNEXB decoder.
    pub fn annex_b(&self) -> Result<HevcDecoder<Ready>, H264BuilderError> {
        Ok(HevcDecoder {
            inner: unsafe { new_c_decoder(C_DECODER_CODEC_HEVC, None)? },
            marker: PhantomData {},
        })
    }
}

unsafe fn new_c_decoder(
    codec: c_int,
    extradata: Option<PaddedBytes>,
) -> Result<Wrapper, H264BuilderError> {
    use H264BuilderError::*;

    let raw = c_decoder_allocate();
    let Some(decoder) = NonNull::new(raw) else {
        return Err(AllocateDecoder);
    };

    if let Some(extradata) = extradata {
        let mut extradata = extradata.to_vec();
        let ret = c_decoder_open(
            decoder.as_ptr(),
            codec,
            extradata.as_mut_ptr(),
            extradata.len(),
        );
        check_decoder_open_err(ret)?;
    } else {
        let ret = c_decoder_open(decoder.as_ptr(), codec, std::ptr::null_mut(), 0);
        check_decoder_open_err(ret)?;
    }

    Ok(Wrapper(decoder))
}

#[derive(Debug, Error)]
pub enum H264DecoderOpenError {
    #[error("find decoder")]
    FindDecoder,

    #[error("allocate codec context")]
    AllocateCodecContext,

    #[error("allocate parameters")]
    AllocateParams,

    #[error("allocate frame")]
    AllocateFrame,

    #[error("allocate packet")]
    AllocatePacket,

    #[error("{0}")]
    FFmpeg(OpaqueFFmpegError),
}

fn check_decoder_open_err(e: i32) -> Result<(), H264DecoderOpenError> {
    use H264DecoderOpenError::*;
    if e == 0 {
        return Ok(());
    }
    Err(match e {
        10000 => FindDecoder,
        10001 => AllocateCodecContext,
        10002 => AllocateParams,
        10003 => AllocateFrame,
        10004 => AllocatePacket,
        _ => FFmpeg(OpaqueFFmpegError::new(e)),
    })
}

/// The `H264Decoder` provide an decode API, which decouples the input and output.
///
/// The API works as
/// follows:
/// - Use `H264DecoderBuilder` to create a `H264Decoder`.
/// - Call `send_packet()` to give the decoder raw compressed data.
/// - Call `receive_frame()` in a loop and process the output:
///   On success, it will return an `Frame` containing uncompressed video data.
///   Repeat this call until it returns `EAGAIN` or an error. The `EAGAIN`
///   return value means that new input data is required to return new output.
///   In this case, continue with sending input. For each input packet, the codec
///   will typically return 1 output frame, but it can also be 0 or more than 1.
///
/// At the beginning of decoding the codec might accept
/// multiple input frames/packets without returning a frame,
/// until its internal buffers are filled. This situation is
/// handled transparently if you follow the steps outlined above.
///
/// In theory, sending input can result in EAGAIN - this should happen only if
/// not all output was received. You can use this to structure alternative
/// decode or encode loops other than the one suggested above. For example, you
/// could try sending new input on each iteration, and try to receive output if
/// that returns EAGAIN.
///
/// End of stream situations. These require "flushing" (aka draining) the
/// decoder, as the codec might buffer multiple frames or packets internally
/// for performance or out of necessity (consider B-frames).
/// This is handled as follows:
/// - Call `drain()` to enter draining more.
/// - Call `receive_frame()` in a loop until EOF is returned.
///   `receive_frame()` will not return EAGAIN.
/// - `FFmpeg` does provide an API to restore the decoder to the `Ready` mode,
///   but we do not expose it at this time.
///
/// Using the API as outlined above is highly recommended. But it is also
/// possible to call functions outside of this rigid schema. For example, you
/// can call `send_packet()` repeatedly without calling `receive_frame()`.
/// In this case, `send_packet`() will succeed until the decoders internal buffer
/// has been filled up (which is typically of size 1 per output frame, after
/// initial input), and then reject input with `EAGAIN`. Once it starts
/// rejecting input, you have no choice but to read at least some output.
///
/// The only guarantee is that an `EAGAIN` return value on a send/receive
/// call on one end implies that a receive/send call on the other end will
/// succeed, or at least will not fail with `EAGAIN`.
///
/// The decoder is not allowed to return `EAGAIN` for both sending and receiving.
/// This would be an invalid state, which could put the decoder user into an
/// endless loop. The API has no concept of time either: it cannot happen that
/// trying to do `send_packet()` results in `EAGAIN`, but a repeated call 1
/// second later accepts the packet (with no other receive/drain API calls
/// involved). The API is a strict state machine, and the passage of time is not
/// supposed to influence it. Some timing-dependent behavior might still be
/// deemed acceptable in certain cases. But it must never result in both
/// send/receive returning `EAGAIN` at the same time at any point. It must also
/// absolutely be avoided that the current state is "unstable" and can
/// "flip-flop" between the send/receive APIs allowing progress. For example,
/// it's not allowed that the decoder randomly decides that it actually wants to
/// consume a packet now instead of returning a frame, after it just returned
/// `EAGAIN` on an `send_packet()` call.
pub struct H264Decoder<S: DecoderState> {
    inner: Wrapper,
    marker: PhantomData<S>,
}

// The decoder is codec-agnostic once opened; H264Decoder is kept as the
// canonical name for backward compatibility.
pub type HevcDecoder<S> = H264Decoder<S>;

mod private {
    pub trait DecoderState {}
    pub enum Ready {}
    pub enum Draining {}
}
use private::DecoderState;
pub use private::{Draining, Ready};

impl DecoderState for Ready {}
impl DecoderState for Draining {}

struct Wrapper(NonNull<CH264Decoder>);

impl Drop for Wrapper {
    fn drop(&mut self) {
        unsafe {
            c_decoder_free(self.0.as_ptr());
        }
    }
}

impl Deref for Wrapper {
    type Target = NonNull<CH264Decoder>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

unsafe impl Send for Wrapper {}
unsafe impl Sync for Wrapper {}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum SendPacketError {
    // input is not accepted in the current state - user
    // must read output with `receive_frame()`
    // (once all output is read, the packet should be
    // resent, and the call will not fail with EAGAIN).
    #[error("EAGAIN")]
    Eagain,

    #[error("invalid data found when processing input")]
    Invaliddata,

    #[error(transparent)]
    FFmpeg(OpaqueFFmpegError),
}

#[derive(Debug, Error)]
pub enum DrainError {
    #[error("{0}")]
    FFmpeg(OpaqueFFmpegError),
}

// Operations that are valid only in Ready state.
impl H264Decoder<Ready> {
    /// Supply raw packet data as input to a decoder. See `H264Decoder` documentation.
    //
    // The data will be padded by `INPUT_BUFFER_PADDING_SIZE`
    // because some optimized bitstream readers read 32 or 64
    // bits at once and could read over the end.
    //
    // The data will usually be a single video frame.
    // The decoder may copy the data. Unlike with older APIs,
    // the packet is always fully consumed, and if it contains multiple frames,
    // will require you to call `receive_frame()` multiple times afterwards before you can send a new packet.
    //
    // Will return EAGAIN if input is not accepted in
    // the current state - user must read output with
    // avcodec_receive_frame() (once all output is read,
    // the packet should be resent, and the call will not
    // fail with EAGAIN).
    pub fn send_packet(&mut self, pkt: &Packet) -> Result<(), SendPacketError> {
        let ret = unsafe {
            c_decoder_send_packet(
                self.inner.as_ptr(),
                pkt.data().as_ptr(),
                pkt.data().len(),
                pkt.pts(),
            )
        };
        if unerror(ret) == EAGAIN {
            Err(SendPacketError::Eagain)
        } else if ret == AVERROR_INVALIDDATA {
            Err(SendPacketError::Invaliddata)
        } else if ret != 0 {
            Err(SendPacketError::FFmpeg(OpaqueFFmpegError::new(ret)))
        } else {
            Ok(())
        }
    }

    /// Enter draining mode. See `H264Decoder` documentation.
    pub fn drain(self) -> Result<H264Decoder<Draining>, DrainError> {
        let ret = unsafe { c_decoder_send_packet(self.inner.as_ptr(), std::ptr::null(), 0, 0) };
        if ret != 0 {
            return Err(DrainError::FFmpeg(OpaqueFFmpegError::new(ret)));
        }

        Ok(H264Decoder::<Draining> {
            inner: self.inner,
            marker: PhantomData,
        })
    }
}

#[derive(Debug, Error)]
pub enum ReceiveFrameError {
    #[error("EAGAIN")]
    Eagain,

    #[error("EOF")]
    Eof,

    #[error("linesize pointer is null")]
    LinesizeNull,

    #[error("width is zero")]
    WidthZero,

    #[error("height is zero")]
    HeightZero,

    #[error("linesize '{0}', doesn't fit u32")]
    LinesizeToBig(c_int),

    #[error("width '{0}' doesn't fit u16")]
    WidthToBig(c_int),

    #[error("height '{0}' doesn't fit u16")]
    HeightToBig(c_int),

    #[error("{0}")]
    TryFromPixelFormat(#[from] TryFromPixelFormatError),

    #[error("{0}")]
    InvalidColorRange(#[from] InvalidColorRange),

    #[error("data pointer to plane '{0}' is null")]
    DataNull(u8),

    #[error("{0}")]
    FFmpeg(OpaqueFFmpegError),
}

// Operations that are valid in both states.
impl<S> H264Decoder<S>
where
    S: DecoderState,
{
    /// Writes decoded output data from the decoder into a frame. See `H264Decoder` documentation.
    pub fn receive_frame(&mut self, frame: &mut Frame) -> Result<(), ReceiveFrameError> {
        use ReceiveFrameError::*;
        unsafe {
            // Construct an array of null pointers.
            const LEN: usize = NUM_DATA_POINTERS;
            let mut data: [*mut u8; LEN] = [std::ptr::null_mut(); LEN];

            // Construct a pointer to an array of null pointers.
            let linesize_ptr: *mut *mut c_int = &mut [0; LEN].as_mut_ptr();
            let mut width: c_int = 0;
            let mut height: c_int = 0;
            let mut pts: i64 = 0;
            let mut pix_fmt: c_int = 0;
            let mut color_range: c_uint = 0;

            // Pass the pointers to the decoder.
            let ret = c_decoder_recieve_frame(
                self.inner.as_ptr(),
                data.as_mut_ptr(),
                linesize_ptr,
                &mut width,
                &mut height,
                &mut pts,
                &mut pix_fmt,
                &mut color_range,
            );
            if ret != 0 {
                return Err(match unerror(ret) {
                    EAGAIN => Eagain,
                    EOF => Eof,
                    _ => FFmpeg(OpaqueFFmpegError::new(ret)),
                });
            }

            // Check if any of the pointers are NULL.
            if linesize_ptr.is_null() || (*linesize_ptr).is_null() {
                return Err(LinesizeNull);
            }

            // Dereference linesize and copy it to the output frame.
            let linesize = std::slice::from_raw_parts(*linesize_ptr, LEN);
            #[allow(clippy::needless_range_loop)]
            for i in 0..8 {
                frame.linesize_mut()[i] =
                    usize::try_from(linesize[i]).map_err(|_| LinesizeToBig(linesize[i]))?;
            }

            // Parse values.
            let width = NonZeroU16::new(u16::try_from(width).map_err(|_| WidthToBig(width))?)
                .ok_or(WidthZero)?;
            let height = NonZeroU16::new(u16::try_from(height).map_err(|_| HeightToBig(height))?)
                .ok_or(HeightZero)?;
            let pix_fmt = PixelFormat::try_from(pix_fmt)?;
            let color_range = ColorRange::try_from(color_range)?;

            // Set values.
            frame.set_width(width);
            frame.set_height(height);
            frame.set_pts(pts);
            frame.set_pix_fmt(pix_fmt);
            frame.set_color_range(color_range);

            let frame_data = frame.data_mut();
            for i in 0..pix_fmt.num_comps() {
                let h = frame_raw_height(pix_fmt, height, i);

                // Check if data is NULL.
                let i2 = usize::from(i);
                if data[i2].is_null() {
                    return Err(DataNull(i));
                }

                // Dereference data.
                let plane = std::slice::from_raw_parts(
                    data[i2],
                    usize::try_from(linesize[i2]).expect("linesize to fit usize") * usize::from(h),
                );

                // Copy plane to the output frame.
                frame_data[i2].clear();
                frame_data[i2].extend_from_slice(plane);
            }

            Ok(())
        }
    }
}

// Operations that are valid only in Draining state.
impl H264Decoder<Draining> {}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use pretty_hex::pretty_hex;

    const EXTRADATA: &[u8] = include_bytes!("../../testdata/extradata.bin");
    const AVCC_FRAME: &[u8] = include_bytes!("../../testdata/avcc_frame.bin");
    const ANNEXB_FRAME: &[u8] = include_bytes!("../../testdata/annexb_64x64.bin");
    const YUV444P_FRAME: &[u8] = include_bytes!("../../testdata/yuv444p_64x64.bin");

    #[test]
    fn test_avcc() {
        let mut decoder = H264DecoderBuilder::new()
            .avcc(PaddedBytes::new(EXTRADATA.to_owned()))
            .unwrap();

        decoder
            .send_packet(&Packet::new(&PaddedBytes::new(AVCC_FRAME.to_owned())))
            .unwrap();

        let mut decoder = decoder.drain().unwrap();

        let mut frame = Frame::new();
        frame.set_pix_fmt(PixelFormat::GBRP);

        decoder.receive_frame(&mut frame).unwrap();

        assert_eq!(650, frame.width().get());
        assert_eq!(450, frame.height().get());
        assert_eq!(PixelFormat::YUV420P, frame.pix_fmt());
        assert_eq!(ColorRange::UNSPECIFIED, frame.color_range());
        assert_eq!(&[768, 384, 384, 0, 0, 0, 0, 0], frame.linesize());
    }

    #[test]
    fn test_annexb() {
        let mut decoder = H264DecoderBuilder::new().annex_b().unwrap();

        let data = &PaddedBytes::new(ANNEXB_FRAME.to_owned());
        let pkt = Packet::new(data).with_pts(5);
        decoder.send_packet(&pkt).unwrap();

        let mut decoder = decoder.drain().unwrap();

        let mut frame = Frame::new();
        decoder.receive_frame(&mut frame).unwrap();

        assert_eq!(5, frame.pts());
        assert_eq!(64, frame.width().get());
        assert_eq!(64, frame.height().get());
        assert_eq!(PixelFormat::YUVJ444P, frame.pix_fmt());
        assert_eq!(ColorRange::JPEG, frame.color_range());
        assert_eq!(&[64, 64, 64, 0, 0, 0, 0, 0], frame.linesize());

        let mut buf = Vec::new();

        frame.copy_to_buffer(&mut buf, 1).unwrap();

        assert_eq!(12288, buf.len());
        assert_eq!(pretty_hex(&YUV444P_FRAME), pretty_hex(&buf));
    }
}
