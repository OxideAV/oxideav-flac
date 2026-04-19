//! FLAC codec registration.

use oxideav_codec::{CodecRegistry, Decoder, Encoder};
use oxideav_core::{CodecCapabilities, CodecId, CodecParameters, CodecTag, Result};

pub fn register(reg: &mut CodecRegistry) {
    let cid = CodecId::new(super::CODEC_ID_STR);
    let caps = CodecCapabilities::audio("flac_sw")
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_channels(8)
        .with_max_sample_rate(655_350);
    reg.register_both(cid.clone(), caps, make_decoder, make_encoder);

    // AVI / WAVEFORMATEX tag: 0xF1AC — the non-standard but widely
    // recognised FLAC-in-WAV/AVI marker. Priority 10, no probe.
    reg.claim_tag(cid, CodecTag::wave_format(0xF1AC), 10, None);
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    super::decoder::make_decoder(params)
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    super::encoder::make_encoder(params)
}
