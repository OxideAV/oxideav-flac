//! FLAC codec registration.

use oxideav_core::{CodecCapabilities, CodecId, CodecParameters, CodecTag, Result};
use oxideav_core::{CodecInfo, CodecRegistry, Decoder, Encoder};

pub fn register(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::audio("flac_sw")
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_channels(8)
        .with_max_sample_rate(655_350);
    // AVI / WAVEFORMATEX tag: 0xF1AC — the non-standard but widely
    // recognised FLAC-in-WAV/AVI marker.
    reg.register(
        CodecInfo::new(CodecId::new(super::CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .tag(CodecTag::wave_format(0xF1AC)),
    );
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    super::decoder::make_decoder(params)
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    super::encoder::make_encoder(params)
}
