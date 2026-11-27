// SPDX-License-Identifier: MPL-2.0+

#include "wrapper.h"

#include <libavcodec/avcodec.h>
#include <libavutil/avutil.h>
#include <libavutil/imgutils.h>

struct CH264Decoder {
  // Decoder context.
  AVCodecContext *ctx;

  // avcodec_send_packet() buffer.
  AVPacket *pkt;

  // avcodec_receive_frame() buffer.
  AVFrame *frame;
};

CH264Decoder *c_decoder_allocate() {
  //
  return malloc(sizeof(CH264Decoder));
}

int c_decoder_open(CH264Decoder *d, int codec_sel, uint8_t *extradata,
                   size_t extradata_size) {
#define ERROR_FIND_DECODER 10000;
#define ERROR_ALLOCATE_CODEC_CONTEXT 10001;
#define ERROR_ALLOCATE_PARAMS 10002;
#define ERROR_ALLOCATE_FRAME 10003;
#define ERROR_ALLOCATE_PACKET 10004;

  av_log_set_level(AV_LOG_QUIET);

  enum AVCodecID codec_id;
  switch (codec_sel) {
  case C_DECODER_CODEC_H264:
    codec_id = AV_CODEC_ID_H264;
    break;
  case C_DECODER_CODEC_HEVC:
    codec_id = AV_CODEC_ID_HEVC;
    break;
  default:
    return ERROR_FIND_DECODER;
  }

  const AVCodec *codec = avcodec_find_decoder(codec_id);
  if (!codec) {
    return ERROR_FIND_DECODER;
  }

  // Create the codec context and initialize it with codec parameters
  d->ctx = avcodec_alloc_context3(codec);
  if (!d->ctx) {
    return ERROR_ALLOCATE_CODEC_CONTEXT;
  }

  // Create codec parameters.
  AVCodecParameters *params = avcodec_parameters_alloc();
  if (!params) {
    return ERROR_ALLOCATE_PARAMS;
  }
  params->extradata = extradata;
  params->extradata_size = extradata_size;

  int ret = avcodec_parameters_to_context(d->ctx, params);

  // The extradata is owned by caller.
  params->extradata = NULL;
  avcodec_parameters_free(&params);

  if (ret < 0) {
    return ret;
  }

  if ((ret = avcodec_open2(d->ctx, codec, NULL)) < 0) {
    return ret;
  }

  d->frame = av_frame_alloc();
  if (!d->frame) {
    return ERROR_ALLOCATE_FRAME;
  }

  d->pkt = av_packet_alloc();
  if (!d->pkt) {
    return ERROR_ALLOCATE_PACKET;
  }

  return 0;
}

int c_decoder_send_packet(CH264Decoder *d, uint8_t const *data,
                          size_t data_size, int64_t pts) {

  d->pkt->data = (uint8_t *)data;
  d->pkt->size = data_size;
  d->pkt->pts = pts;

  // `avcodec_send_packet()` is not allowed to modify the packet.
  int ret = avcodec_send_packet(d->ctx, d->pkt);

  // Clear dangling pointer.
  d->pkt->data = NULL;

  return ret;
}

int c_decoder_recieve_frame(CH264Decoder *d,
                            uint8_t *data[AV_NUM_DATA_POINTERS],
                            int *data_linesize[AV_NUM_DATA_POINTERS],
                            int *width, int *height, int64_t *pts, int *pix_fmt,
                            unsigned int *color_range) {
  int ret, i;

  if ((ret = avcodec_receive_frame(d->ctx, d->frame)) != 0) {
    return ret;
  };

  for (i = 0; i < AV_NUM_DATA_POINTERS; i++) {
    data[i] = d->frame->data[i];
  }
  *data_linesize = d->frame->linesize;
  *width = d->frame->width;
  *height = d->frame->height;
  *pts = d->frame->pts;
  *pix_fmt = d->frame->format;
  *color_range = d->frame->color_range;

  return 0;
}

// Free the decoder and everything associated with it.
void c_decoder_free(CH264Decoder *d) {
  avcodec_free_context(&d->ctx);
  av_packet_free(&d->pkt);
  av_frame_free(&d->frame);

  free(d);

  // av_image_get_buffer_size();
  // av_image_copy_to_buffer;
}
