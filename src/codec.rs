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
    //
    // Payload magics (RFC 9639):
    //   * `\x7fFLAC` — the first packet of a FLAC-in-Ogg logical
    //     bitstream opens with bytes 0x7F 0x46 0x4C 0x41 0x43
    //     (§10.1 Table 24, as also defined by RFC 5334). Ogg carries
    //     no codec tag; demuxers identify the mapped codec purely by
    //     this first-packet prefix.
    //   * `fLaC` — the stream signature that heads every raw
    //     (container-less) FLAC stream, immediately before the
    //     streaminfo metadata block. Lets tag-less elementary-stream
    //     consumers resolve the codec from the payload head alone.
    reg.register(
        CodecInfo::new(CodecId::new(super::CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .tag(CodecTag::wave_format(0xF1AC))
            .payload_magic(b"\x7fFLAC")
            .payload_magic(b"fLaC"),
    );
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    super::decoder::make_decoder(params)
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    super::encoder::make_encoder(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> CodecRegistry {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        reg
    }

    /// The first packet of a FLAC-in-Ogg logical bitstream (RFC 9639
    /// §10.1 Table 24): 0x7F + "FLAC" + mapping version 1.0 + header
    /// packet count + the fLaC signature + streaminfo block header.
    /// Only the prefix matters for resolution; the tail here is the
    /// genuine Table 24 layout so the fixture doubles as
    /// documentation.
    fn ogg_flac_first_packet_head() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&[0x7F, 0x46, 0x4C, 0x41, 0x43]); // 0x7F "FLAC"
        p.extend_from_slice(&[0x01, 0x00]); // mapping version 1.0
        p.extend_from_slice(&[0x00, 0x01]); // 1 following header packet
        p.extend_from_slice(b"fLaC"); // stream signature
        p.extend_from_slice(&[0x00, 0x00, 0x00, 0x22]); // streaminfo header (34 bytes)
        p
    }

    #[test]
    fn ogg_flac_first_packet_magic_resolves() {
        let reg = registry();
        let id = reg
            .resolve_payload_magic_ref(&ogg_flac_first_packet_head())
            .expect("\\x7fFLAC prefix must resolve");
        assert_eq!(id.as_str(), crate::CODEC_ID_STR);
    }

    #[test]
    fn raw_stream_flac_signature_resolves() {
        let reg = registry();
        // Raw stream head: fLaC signature + streaminfo block header.
        let head = b"fLaC\x00\x00\x00\x22";
        let id = reg
            .resolve_payload_magic_ref(head)
            .expect("fLaC signature must resolve");
        assert_eq!(id.as_str(), crate::CODEC_ID_STR);
    }

    #[test]
    fn payload_magic_is_prefix_matched_not_exact() {
        let reg = registry();
        // Bare 5-byte prefix with no tail at all still resolves —
        // resolution is starts_with, not exact-length.
        assert!(reg.resolve_payload_magic_ref(b"\x7fFLAC").is_some());
        // A truncated prefix must NOT resolve.
        assert!(reg.resolve_payload_magic_ref(b"\x7fFLA").is_none());
        assert!(reg.resolve_payload_magic_ref(b"fLa").is_none());
    }

    #[test]
    fn foreign_payload_magic_does_not_resolve() {
        let reg = registry();
        assert!(reg.resolve_payload_magic_ref(b"OpusHead").is_none());
        assert!(reg.resolve_payload_magic_ref(b"\x01vorbis").is_none());
        // Case matters: the signature is fLaC, not FLAC / flac.
        assert!(reg.resolve_payload_magic_ref(b"FLAC").is_none());
        assert!(reg.resolve_payload_magic_ref(b"flac").is_none());
    }

    #[test]
    fn resolves_through_dyn_codec_resolver() {
        // The demuxer-facing path: an Ogg demuxer holds a
        // `&dyn CodecResolver`, not a concrete registry, and hands it
        // the first packet of each logical stream.
        let reg = registry();
        let resolver: &dyn oxideav_core::CodecResolver = &reg;
        let id = resolver
            .resolve_payload_magic(&ogg_flac_first_packet_head())
            .expect("dyn resolver must resolve the Ogg-FLAC first packet");
        assert_eq!(id.as_str(), crate::CODEC_ID_STR);
    }

    #[test]
    fn both_magics_enumerate_in_registration_order() {
        let reg = registry();
        let magics: Vec<(&[u8], &str)> = reg
            .all_payload_magic_registrations()
            .map(|(m, id)| (m, id.as_str()))
            .collect();
        assert_eq!(
            magics,
            vec![
                (b"\x7fFLAC".as_slice(), crate::CODEC_ID_STR),
                (b"fLaC".as_slice(), crate::CODEC_ID_STR),
            ],
        );
    }
}
