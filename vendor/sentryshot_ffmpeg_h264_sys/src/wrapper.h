// SPDX-License-Identifier: MPL-2.0+

// Run "../../make.sh bindgen" after modifying this file!
#include <stddef.h>
#include <stdint.h>

#define AV_NUM_DATA_POINTERS 8

// Codec selectors for c_decoder_open(). Kept as plain integers so the
// Rust side never depends on the AVCodecID enum values of the linked
// FFmpeg version.
#define C_DECODER_CODEC_H264 0
#define C_DECODER_CODEC_HEVC 1

typedef struct CH264Decoder CH264Decoder;

// Allocate an CH264Decoder.
// The caller must check if the returned value is NULL.
// The resulting struct must be freed with c_decoder_free().
CH264Decoder *c_decoder_allocate(void);

// Initialize the decoder for the given codec selector
// (C_DECODER_CODEC_H264|C_DECODER_CODEC_HEVC) with optional borrowed
// extradata.
int c_decoder_open(CH264Decoder *d, int codec, uint8_t *extradata,
                   size_t extradata_size);

// Supply raw packet data as input to the decoder.
int c_decoder_send_packet(CH264Decoder *d, uint8_t const *data,
                          size_t data_size, int64_t pts);

// Return decoded output data from a decoder.
int c_decoder_recieve_frame(CH264Decoder *d,
                            uint8_t *data[AV_NUM_DATA_POINTERS],
                            int *linesize[AV_NUM_DATA_POINTERS], int *width,
                            int *height, int64_t *pts, int *pix_fmt,
                            unsigned int *color_range);

// Free the decoder and everything associated with it.
void c_decoder_free(CH264Decoder *pd);
