mod bindings;

pub use bindings::{
    c_decoder_allocate, c_decoder_free, c_decoder_open, c_decoder_recieve_frame,
    c_decoder_send_packet, CH264Decoder, C_DECODER_CODEC_H264, C_DECODER_CODEC_HEVC,
};
