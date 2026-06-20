//! Integration tests for the verify-decoder MD5 path (RFC 9639 §9.2.1).
//!
//! Each reference fixture under `docs/audio/flac/fixtures/<name>/input.flac`
//! is a complete `.flac` stream produced by a black-box reference encoder.
//! Its STREAMINFO carries that encoder's MD5 of the *original* PCM. Running
//! `oxideav_flac::verify::verify_stream` over the file decodes every frame
//! with this crate's own decoder, recomputes the MD5 from the reconstructed
//! samples, and compares: a `VerifyOutcome::Match` is independent proof that
//! our decode is sample-exact against an *external* oracle (the reference
//! encoder), not just self-consistent with our own encoder.
//!
//! When the `docs/` submodule is not checked out, the fixtures are absent;
//! the test logs a skip and passes (CI without the private submodule still
//! goes green).

use std::path::PathBuf;

use oxideav_flac::verify::{verify_stream, VerifyOutcome};

fn fixtures_root() -> PathBuf {
    PathBuf::from("../../docs/audio/flac/fixtures")
}

/// Every fixture directory that ships an `input.flac` reference stream.
const FIXTURES: &[&str] = &[
    "constant-subframe-silence",
    "fixed-subframe-low-order",
    "left-right-channel-pair",
    "left-side-channel-pair",
    "lpc-subframe-typical-music",
    "mid-side-channel-pair",
    "mono-16bit-44100-fixed-blocksize",
    "mono-8bit-22050",
    "right-side-channel-pair",
    "stereo-16bit-44100-fixed",
    "stereo-16bit-44100-small-blocksize",
    "stereo-24bit-96000",
    "stereo-32bit-192000",
    "surround-7_1-24bit-48000",
    "verbatim-subframe-noise",
    "with-padding-block",
    "with-picture-block",
    "with-vorbis-comment",
];

/// Whole-stream MD5 verification across the reference corpus. Every
/// reference `input.flac` must verify `Match` (or `NoSignature` if the
/// encoder stored no MD5) — a mismatch means our decode diverged from the
/// original PCM the reference encoder hashed.
#[test]
fn reference_corpus_verifies_md5() {
    let root = fixtures_root();
    if !root.exists() {
        eprintln!("skip reference_corpus_verifies_md5: docs/ submodule absent");
        return;
    }

    let mut checked = 0usize;
    let mut matched = 0usize;
    let mut no_sig = 0usize;
    for name in FIXTURES {
        let path = root.join(name).join("input.flac");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skip {name}: {} not readable", path.display());
            continue;
        };
        checked += 1;
        match verify_stream(&bytes) {
            Ok(VerifyOutcome::Match) => {
                matched += 1;
            }
            Ok(VerifyOutcome::NoSignature) => {
                no_sig += 1;
                eprintln!("note {name}: STREAMINFO has no MD5 signature (unverifiable)");
            }
            Err(e) => panic!("{name}: verify_stream failed: {e}"),
        }
    }

    if checked == 0 {
        eprintln!("skip reference_corpus_verifies_md5: no input.flac fixtures found");
        return;
    }
    eprintln!("verified {checked} fixtures: {matched} Match, {no_sig} NoSignature");
    // Every fixture that ships a signature must verify; at least one of the
    // corpus is expected to carry a real (non-zero) MD5.
    assert!(
        matched + no_sig == checked,
        "all readable fixtures must produce a definite verify outcome",
    );
    assert!(
        matched > 0,
        "at least one reference fixture should carry a real MD5 that verifies Match",
    );
}

/// Flipping a single sample byte inside a frame body must make verification
/// fail — proves the check actually depends on the decoded PCM, not just on
/// metadata parsing succeeding.
#[test]
fn corrupted_sample_fails_verification() {
    let root = fixtures_root();
    if !root.exists() {
        eprintln!("skip corrupted_sample_fails_verification: docs/ submodule absent");
        return;
    }
    // Use a fixture known to carry a real MD5; the mono fixed-blocksize file
    // is a plain mono stream so a body-byte flip lands on real audio.
    let path = root
        .join("mono-16bit-44100-fixed-blocksize")
        .join("input.flac");
    let Ok(mut bytes) = std::fs::read(&path) else {
        eprintln!("skip corrupted_sample_fails_verification: fixture absent");
        return;
    };

    // Confirm the pristine file verifies first; if it has no signature we
    // cannot test corruption detection against it.
    match verify_stream(&bytes) {
        Ok(VerifyOutcome::Match) => {}
        Ok(VerifyOutcome::NoSignature) => {
            eprintln!("skip corrupted_sample_fails_verification: fixture has no MD5");
            return;
        }
        Err(e) => panic!("pristine fixture failed to verify: {e}"),
    }

    // Flip one bit deep in the tail, well past the metadata chain and the
    // first frame header, so we mutate a residual/sample byte rather than a
    // sync code. The per-frame CRC-16 will reject the mutated frame OR the
    // MD5 will mismatch — either way verification must NOT return Match.
    let idx = bytes.len() - 8;
    bytes[idx] ^= 0x01;

    match verify_stream(&bytes) {
        Ok(VerifyOutcome::Match) => {
            panic!("corrupted stream wrongly verified as Match");
        }
        // A non-Match outcome (CRC-16 rejection, MD5 mismatch, or a parse
        // error) is the correct rejection of a corrupted stream.
        Ok(VerifyOutcome::NoSignature) => {
            panic!("corrupted stream unexpectedly reported NoSignature");
        }
        Err(_) => {}
    }
}
