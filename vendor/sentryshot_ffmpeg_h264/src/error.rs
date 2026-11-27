// SPDX-License-Identifier: MPL-2.0+

use thiserror::Error;

//#define MKTAG(a,b,c,d)   ((a) | ((b) << 8) | ((c) << 16) | ((unsigned)(d) << 24))
const fn mktag(a: u8, b: u8, c: u8, d: u8) -> i32 {
    i32::from_le_bytes([a, b, c, d])
}

//#define FFERRTAG(a, b, c, d) (-(int)MKTAG(a, b, c, d))
const fn fferrtag(a: u8, b: u8, c: u8, d: u8) -> i32 {
    -(mktag(a, b, c, d))
}
// Bitstream filter not found
const AVERROR_BSF_NOT_FOUND: i32 = fferrtag(0xF8, b'B', b'S', b'F');
// Internal bug, also see AVERROR_BUG2
const AVERROR_BUG: i32 = fferrtag(b'B', b'U', b'G', b'!');
// Buffer too small
const AVERROR_BUFFER_TOO_SMALL: i32 = fferrtag(b'B', b'U', b'F', b'S');
// Decoder not found
const AVERROR_DECODER_NOT_FOUND: i32 = fferrtag(0xF8, b'D', b'E', b'C');
// Demuxer not found
const AVERROR_DEMUXER_NOT_FOUND: i32 = fferrtag(0xF8, b'D', b'E', b'M');
// Encoder not found
const AVERROR_ENCODER_NOT_FOUND: i32 = fferrtag(0xF8, b'E', b'N', b'C');
// End of file
const AVERROR_EOF: i32 = fferrtag(b'E', b'O', b'F', b' ');
// Immediate exit was requested; the called function should not be restarted
const AVERROR_EXIT: i32 = fferrtag(b'E', b'X', b'I', b'T');
// Generic error in an external library
const AVERROR_EXTERNAL: i32 = fferrtag(b'E', b'X', b'T', b' ');
// Filter not found
const AVERROR_FILTER_NOT_FOUND: i32 = fferrtag(0xF8, b'F', b'I', b'L');
// Invalid data found when processing input
pub(crate) const AVERROR_INVALIDDATA: i32 = fferrtag(b'I', b'N', b'D', b'A');
// Muxer not found
const AVERROR_MUXER_NOT_FOUND: i32 = fferrtag(0xF8, b'M', b'U', b'X');
// Option not found
const AVERROR_OPTION_NOT_FOUND: i32 = fferrtag(0xF8, b'O', b'P', b'T');
// Not yet implemented in FFmpeg, patches welcome
const AVERROR_PATCHWELCOME: i32 = fferrtag(b'P', b'A', b'W', b'E');
// Protocol not found
const AVERROR_PROTOCOL_NOT_FOUND: i32 = fferrtag(0xF8, b'P', b'R', b'O');
// Stream not found
const AVERROR_STREAM_NOT_FOUND: i32 = fferrtag(0xF8, b'S', b'T', b'R');

// This is semantically identical to AVERROR_BUG
// it has been introduced in Libav after our AVERROR_BUG and with a modified value.
const AVERROR_BUG2: i32 = fferrtag(b'B', b'U', b'G', b' ');
// Unknown error, typically from an external library
const AVERROR_UNKNOWN: i32 = fferrtag(b'U', b'N', b'K', b'N');
// Requested feature is flagged experimental. Set `strict_std_compliance` if you really want to use it.
const AVERROR_EXPERIMENTAL: i32 = -0x2bb2_afa8;
// Input changed between calls. Reconfiguration is required. (can be OR-ed with `AVERROR_OUTPUT_CHANGED`)
const AVERROR_INPUT_CHANGED: i32 = -0x636e_6701;
// Output changed between calls. Reconfiguration is required. (can be OR-ed with `AVERROR_INPUT_CHANGED`)
const AVERROR_OUTPUT_CHANGED: i32 = -0x636e_6702;
// HTTP & RTSP errors
const AVERROR_HTTP_BAD_REQUEST: i32 = fferrtag(0xF8, b'4', b'0', b'0');
const AVERROR_HTTP_UNAUTHORIZED: i32 = fferrtag(0xF8, b'4', b'0', b'1');
const AVERROR_HTTP_FORBIDDEN: i32 = fferrtag(0xF8, b'4', b'0', b'3');
const AVERROR_HTTP_NOT_FOUND: i32 = fferrtag(0xF8, b'4', b'0', b'4');
const AVERROR_HTTP_TOO_MANY_REQUESTS: i32 = fferrtag(0xF8, b'4', b'2', b'9');
const AVERROR_HTTP_OTHER_4XX: i32 = fferrtag(0xF8, b'4', b'X', b'X');
const AVERROR_HTTP_SERVER_ERROR: i32 = fferrtag(0xF8, b'5', b'X', b'X');

#[derive(Debug, Error)]
enum FFmpegError {
    #[error("bitstream filter not found")]
    BsfNotFound,
    #[error("internal bug")]
    Bug,
    #[error("buffer too small")]
    BufferTooSmall,
    #[error("decoder not found")]
    DecoderNotFound,
    #[error("demuxer not found")]
    DemuxerNotFound,
    #[error("encoder not found")]
    EncoderNotFound,
    #[error("end of file")]
    Eof,
    #[error("immediate exit was requested; the called function should not be restarted")]
    Exit,
    #[error("generic error in an external library")]
    External,
    #[error("filter not found")]
    FilterNotFound,
    #[error("invalid data found when processing input")]
    Invaliddata,
    #[error("muxer not found")]
    MuxerNotFound,
    #[error("option not found")]
    OptionNotFound,
    #[error("not yet implemented in ffmpeg, patches welcome")]
    Patchwelcome,
    #[error("protocol not found")]
    ProtocolNotFound,
    #[error("stream not found")]
    StreamNotFound,
    #[error("internal bug2")]
    Bug2,
    #[error("unknown error, typically from an external library")]
    Unknown,
    #[error("requested feature is flagged experimental. set strict_std_compliance if you really want to use it.")]
    Experimental,
    #[error("input changed between calls. reconfiguration is required. (can be or-ed with averror_output_changed)")]
    InputChanged,
    #[error("output changed between calls. reconfiguration is required. (can be or-ed with averror_input_changed)")]
    /* http & rtsp errors */
    OutputChanged,
    #[error("http bad request")]
    HttpBadRequest,
    #[error("http unauthorized")]
    HttpUnauthorized,
    #[error("http forbidden")]
    HttpForbidden,
    #[error("http not found")]
    HttpNotFound,
    #[error("http too many requests")]
    HttpTooManyRequests,
    #[error("http other 4xx")]
    HttpOther4xx,
    #[error("http server error")]
    HttpServerError,

    #[error("ERRNO: {0}")]
    Errno(i32),
}

impl FFmpegError {
    fn new(v: i32) -> Self {
        use FFmpegError::*;
        match v {
            AVERROR_BSF_NOT_FOUND => BsfNotFound,
            AVERROR_BUG => Bug,
            AVERROR_BUFFER_TOO_SMALL => BufferTooSmall,
            AVERROR_DECODER_NOT_FOUND => DecoderNotFound,
            AVERROR_DEMUXER_NOT_FOUND => DemuxerNotFound,
            AVERROR_ENCODER_NOT_FOUND => EncoderNotFound,
            AVERROR_EOF => Eof,
            AVERROR_EXIT => Exit,
            AVERROR_EXTERNAL => External,
            AVERROR_FILTER_NOT_FOUND => FilterNotFound,
            AVERROR_INVALIDDATA => Invaliddata,
            AVERROR_MUXER_NOT_FOUND => MuxerNotFound,
            AVERROR_OPTION_NOT_FOUND => OptionNotFound,
            AVERROR_PATCHWELCOME => Patchwelcome,
            AVERROR_PROTOCOL_NOT_FOUND => ProtocolNotFound,
            AVERROR_STREAM_NOT_FOUND => StreamNotFound,
            AVERROR_BUG2 => Bug2,
            AVERROR_UNKNOWN => Unknown,
            AVERROR_EXPERIMENTAL => Experimental,
            AVERROR_INPUT_CHANGED => InputChanged,
            AVERROR_OUTPUT_CHANGED => OutputChanged,
            AVERROR_HTTP_BAD_REQUEST => HttpBadRequest,
            AVERROR_HTTP_UNAUTHORIZED => HttpUnauthorized,
            AVERROR_HTTP_FORBIDDEN => HttpForbidden,
            AVERROR_HTTP_NOT_FOUND => HttpNotFound,
            AVERROR_HTTP_TOO_MANY_REQUESTS => HttpTooManyRequests,
            AVERROR_HTTP_OTHER_4XX => HttpOther4xx,
            AVERROR_HTTP_SERVER_ERROR => HttpServerError,
            _ => Errno(unerror(v)),
        }
    }
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct OpaqueFFmpegError(#[from] FFmpegError);

impl OpaqueFFmpegError {
    pub(crate) fn new(v: i32) -> Self {
        OpaqueFFmpegError(FFmpegError::new(v))
    }
}

// Returns a POSIX error code from a library function error return value.
pub(crate) fn unerror(v: i32) -> i32 {
    -v
}
