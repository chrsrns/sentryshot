// SPDX-License-Identifier: MPL-2.0+

/*use h264_reader::{
    avcc::{AvcDecoderConfigurationRecord, AvccError, ParamSetError},
    nal::sps::{ChromaFormat, SeqParameterSet, SpsError},
    rbsp::{BitReader, ByteReader},
};
use thiserror::Error;
use util::pixfmt::{
    PixelFormat, PIX_FMT_GBRP10, PIX_FMT_GBRP12, PIX_FMT_GBRP14, PIX_FMT_GBRP9, PIX_FMT_YUV420P10,
    PIX_FMT_YUV420P12, PIX_FMT_YUV420P14, PIX_FMT_YUV420P9, PIX_FMT_YUV422P10, PIX_FMT_YUV422P12,
    PIX_FMT_YUV422P14, PIX_FMT_YUV422P9, PIX_FMT_YUV444P10, PIX_FMT_YUV444P12, PIX_FMT_YUV444P14,
    PIX_FMT_YUV444P9,
};

#[derive(Debug, Error)]
pub enum DecodeExtradataError {
    #[error("decode avcC: {0:?}")]
    DecodeAvcC(AvccError),

    #[error("no sps")]
    NoSps,

    #[error("parse sps: {0:?}")]
    ParseSps(ParamSetError),

    #[error("decode sps: {0:?}")]
    DecodeSps(SpsError),

    #[error("pixel dimensions: {0:?}")]
    PixelDimensions(SpsError),

    #[error("unsupported pixel format")]
    UnsupportedPixelFormat,
}

pub fn decode_extradata(buf: &[u8]) -> Result<(u16, u16, PixelFormat), DecodeExtradataError> {
    use DecodeExtradataError::*;
    let decoder = AvcDecoderConfigurationRecord::try_from(buf).map_err(DecodeAvcC)?;

    let raw_sps = decoder
        .sequence_parameter_sets()
        .next()
        .ok_or(NoSps)?
        .map_err(ParseSps)?;

    let r = BitReader::new(ByteReader::new(raw_sps));
    let sps = SeqParameterSet::from_bits(r).map_err(DecodeSps)?;

    let (width, height) = sps.pixel_dimensions().map_err(PixelDimensions)?;

    Ok((width as u16, height as u16, get_pixel_format(&sps)?))
}

fn get_pixel_format(sps: &SeqParameterSet) -> Result<PixelFormat, DecodeExtradataError> {
    let chroma_format = sps.chroma_info.chroma_format;
    let color_space = get_color_space(sps);
    let color_range = get_color_range(sps);

    match sps.chroma_info.bit_depth_luma_minus8 + 8 {
        8 => match chroma_format {
            ChromaFormat::YUV444 => {
                if color_space == ColorSpace::RGB {
                    Ok(PixelFormat::GBRP)
                } else if color_range == ColorRange::JPEG {
                    Ok(PixelFormat::YUVJ444P)
                } else {
                    Ok(PixelFormat::YUV444P)
                }
            }
            ChromaFormat::YUV422 => {
                if color_range == ColorRange::JPEG {
                    Ok(PixelFormat::YUVJ422P)
                } else {
                    Ok(PixelFormat::YUV422P)
                }
            }
            _ => {
                if color_range == ColorRange::JPEG {
                    Ok(PixelFormat::YUVJ420P)
                } else {
                    Ok(PixelFormat::YUV420P)
                }
            }
        },
        9 => match chroma_format {
            ChromaFormat::YUV444 => {
                if color_space == ColorSpace::RGB {
                    Ok(PIX_FMT_GBRP9)
                } else {
                    Ok(PIX_FMT_YUV444P9)
                }
            }
            ChromaFormat::YUV422 => Ok(PIX_FMT_YUV422P9),
            _ => Ok(PIX_FMT_YUV420P9),
        },
        10 => match chroma_format {
            ChromaFormat::YUV444 => {
                if color_space == ColorSpace::RGB {
                    Ok(PIX_FMT_GBRP10)
                } else {
                    Ok(PIX_FMT_YUV444P10)
                }
            }
            ChromaFormat::YUV422 => Ok(PIX_FMT_YUV422P10),
            _ => Ok(PIX_FMT_YUV420P10),
        },
        12 => match chroma_format {
            ChromaFormat::YUV444 => {
                if color_space == ColorSpace::RGB {
                    Ok(PIX_FMT_GBRP12)
                } else {
                    Ok(PIX_FMT_YUV444P12)
                }
            }
            ChromaFormat::YUV422 => Ok(PIX_FMT_YUV422P12),
            _ => Ok(PIX_FMT_YUV420P12),
        },
        14 => match chroma_format {
            ChromaFormat::YUV444 => {
                if color_space == ColorSpace::RGB {
                    Ok(PIX_FMT_GBRP14)
                } else {
                    Ok(PIX_FMT_YUV444P14)
                }
            }
            ChromaFormat::YUV422 => Ok(PIX_FMT_YUV422P14),
            _ => Ok(PIX_FMT_YUV420P14),
        },
        _ => Err(DecodeExtradataError::UnsupportedPixelFormat),
    }
}

// https://github.com/FFmpeg/FFmpeg/blob/1460acc2ac4c5687742b0fdfc9af89f9f0d28029/libavutil/pixfmt.h#L595
// YUV colorspace type.
// These values match the ones defined by ISO/IEC 23091-2_2019 subclause 8.3.
#[derive(Default, PartialEq)]
enum ColorSpace {
    #[default]
    RGB, // = 0,  // order of coefficients is actually GBR, also IEC 61966-2-1 (sRGB), YZX and ST 428-1
    // BT709, // = 1,  // also ITU-R BT1361 / IEC 61966-2-4 xvYCC709 / derived in SMPTE RP 177 Annex B
    // UNSPECIFIED, // = 2,
    // RESERVED, // = 3,  // reserved for future use by ITU-T and ISO/IEC just like 15-255 are
    // FCC,   // = 4,  // FCC Title 47 Code of Federal Regulations 73.682 (a)(20)
    // BT470BG, // = 5,  // also ITU-R BT601-6 625 / ITU-R BT1358 625 / ITU-R BT1700 625 PAL & SECAM / IEC 61966-2-4 xvYCC601
    // SMPTE170M, // = 6,  // also ITU-R BT601-6 525 / ITU-R BT1358 525 / ITU-R BT1700 NTSC / functionally identical to above
    // SMPTE240M, // = 7,  // derived from 170M primaries and D65 white point, 170M is derived from BT470 System M's primaries
    // YCGCO,     // = 8,  // used by Dirac / VC-2 and H.264 FRext, see ITU-T SG16
    // YCOCG,     // = AVCOL_SPC_YCGCO,
    // BT2020NCL, // = 9,  // ITU-R BT2020 non-constant luminance system
    // BT2020CL,  // = 10, // ITU-R BT2020 constant luminance system
    // SMPTE2085, // = 11, // SMPTE 2085, Y'D'zD'x
    // ChromaDerivedNcl, // = 12, // Chromaticity-derived non-constant luminance system
    // ChromaDerivedCl, // = 13, // Chromaticity-derived constant luminance system
    // ICTCP,     // = 14, // ITU-R BT.2100-0, ICtCp
    // NB,
    Unkown(u8),
}

fn get_color_space(sps: &SeqParameterSet) -> ColorSpace {
    let Some(vui_parameters) = &sps.vui_parameters else {
       return ColorSpace::default();
    };

    let Some(video_signal_type) = &vui_parameters.video_signal_type else{
       return ColorSpace::default();
    };

    let Some(colour_description) = &video_signal_type.colour_description else {
       return ColorSpace::default();
    };

    match colour_description.matrix_coefficients {
        0 => ColorSpace::RGB,
        v @ _ => ColorSpace::Unkown(v),
    }
}

// https://github.com/FFmpeg/FFmpeg/blob/1460acc2ac4c5687742b0fdfc9af89f9f0d28029/libavutil/pixfmt.h#L634
// These values are based on definitions that can be found in multiple
// specifications, such as ITU-T BT.709 (3.4 - Quantization of RGB, luminance
// and colour-difference signals), ITU-T BT.2020 (Table 5 - Digital
// Representation) as well as ITU-T BT.2100 (Table 9 - Digital 10- and 12-bit
// integer representation). At the time of writing, the BT.2100 one is
// recommended, as it also defines the full range representation.
//
// Common definitions:
//   - For RGB and luma planes such as Y in YCbCr and I in ICtCp,
//     'E' is the original value in range of 0.0 to 1.0.
//   - For chroma planes such as Cb,Cr and Ct,Cp, 'E' is the original
//     value in range of -0.5 to 0.5.
//   - 'n' is the output bit depth.
//   - For additional definitions such as rounding and clipping to valid n
//     bit unsigned integer range, please refer to BT.2100 (Table 9).
#[derive(Default, PartialEq)]
enum ColorRange {
    #[default]
    UNSPECIFIED, // = 0

    // Narrow or limited range content.
    //
    // - For luma planes:
    //
    //       (219 * E + 16) * 2^(n-8)
    //
    //   F.ex. the range of 16-235 for 8 bits
    //
    // - For chroma planes:
    //
    //       (224 * E + 128) * 2^(n-8)
    //
    //   F.ex. the range of 16-240 for 8 bits
    MPEG, //= 1,

    // Full range content.
    //
    // - For RGB and luma planes:
    //
    //       (2^n - 1) * E
    //
    //   F.ex. the range of 0-255 for 8 bits
    //
    // - For chroma planes:
    //
    //       (2^n - 1) * E + 2^(n - 1)
    //
    //   F.ex. the range of 1-255 for 8 bits
    JPEG, //= 2,
}

fn get_color_range(sps: &SeqParameterSet) -> ColorRange {
    let Some(vui_parameters) = &sps.vui_parameters else {
       return ColorRange::default();
    };

    let Some(video_signal_type) = &vui_parameters.video_signal_type else{
       return ColorRange::default();
    };

    if video_signal_type.video_full_range_flag {
        ColorRange::JPEG
    } else {
        ColorRange::MPEG
    }
}*/
