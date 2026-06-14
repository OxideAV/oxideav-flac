#![no_main]

//! Fuzz: focused stress on the FLAC APPLICATION metadata block
//! (`oxideav_flac::metadata::{parse_application, write_application,
//! Application}`, RFC 9639 §8.4).
//!
//! Round 299 (depth-mode fuzz, twelfth target). The typed APPLICATION
//! parser / writer pair was added in round 244 and is the only typed
//! metadata-block surface with **no direct fuzz coverage**:
//!
//!   * `metadata_walker` (round 200, extended r241) round-trips every
//!     other typed block (STREAMINFO / SEEKTABLE / CUESHEET / PICTURE /
//!     VORBIS_COMMENT / PADDING) but its `try_typed_round_trip` arm for
//!     `BlockType::Application` is an explicit no-op — the block survives
//!     the header walk but `parse_application` / `write_application` are
//!     never called. Its Path-2 direct-parser block likewise omits them.
//!   * `metadata_chain` (round 283) reaches `parse_application` only via
//!     `MetadataChain::parse` — and only when the fuzzer first threads a
//!     structurally valid `fLaC`-less chain (STREAMINFO-first, singleton
//!     constraints, valid `last`-flag placement) that an APPLICATION
//!     block happens to sit inside. That gate makes the APPLICATION
//!     arithmetic a vanishingly small fraction of reachable iterations.
//!   * `decode` / `panic_free_decode` never touch `parse_application` at
//!     all (the demuxer projects APPLICATION onto its container surface
//!     without the typed parser).
//!
//! ## What this target drives directly
//!
//! Both directions of the §8.4 wire format, with three complementary
//! entry shapes per iteration:
//!
//! 1. **Raw parse — panic-freedom.** `parse_application(data)` against
//!    arbitrary attacker bytes. The only structural invariant §8.4 puts
//!    on the payload is "at least the 4-byte big-endian application ID"
//!    (a zero-length data run is legal), so the parser MUST return
//!    `Err(Error::invalid)` for a < 4-byte payload and `Ok` for
//!    everything else — never panic, never index out of bounds.
//!
//! 2. **parse → write → parse round-trip.** Every `Ok` parse is fed back
//!    through `write_application` and re-parsed. The contract is
//!    `parse ∘ write ∘ parse == parse` *and* byte-stability: because the
//!    §8.4 layout is fully positional (4-byte ID + opaque tail) the
//!    writer output MUST equal the exact bytes the parser consumed, so
//!    `write_application(parse_application(x)?) == x` for any `x` the
//!    parser accepts. A divergence here would mean the parser accepted a
//!    payload its own writer can't reproduce.
//!
//! 3. **Synthesised-struct round-trip.** A fuzzer-chosen `(id, data_len)`
//!    builds an `Application` directly (id from four input bytes, data a
//!    bounded run) so the *writer* is driven on shapes the parser would
//!    never hand it, then the wire bytes are re-parsed and checked for
//!    structural equality + the `4 + data.len()` wire-length invariant.
//!    The data run is capped at `MAX_SYNTH_DATA` so a single iteration
//!    never approaches the 24-bit `BlockHeader::MAX_LENGTH` ceiling; the
//!    writer's over-ceiling (`4 + data.len() > 2^24 − 1 ⇒ Err`) branch is
//!    left to `metadata::tests`, since reproducing it here would force a
//!    ~16 MiB allocation per iteration and only report a phantom OOM. A
//!    compile-time assertion pins `MAX_SYNTH_DATA` under the ceiling so
//!    phase 3 always lands on the writer's in-range (`Ok`) branch.

use libfuzzer_sys::fuzz_target;
use oxideav_flac::metadata::{parse_application, write_application, Application, BlockHeader};

/// Cap the synthesised `data` run so a fuzz iteration can't be steered
/// into a multi-MiB allocation. The 24-bit ceiling itself is checked
/// arithmetically (see phase 3), not by materialising 16 MiB.
const MAX_SYNTH_DATA: usize = 4096;

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Phase 1 + 2: raw parse (panic-freedom) and, on success, the
    // positional byte-stability round-trip.
    // -----------------------------------------------------------------
    match parse_application(data) {
        Ok(app) => {
            // §8.4 admits a zero-length data run, so any payload of at
            // least 4 bytes parses. The recovered data run is exactly
            // the bytes past the 4-byte ID.
            assert_eq!(
                app.data.len(),
                data.len() - 4,
                "APPLICATION data run length must equal payload len minus the 4-byte ID"
            );

            // The writer must reproduce the exact bytes the parser
            // consumed — the layout is fully positional.
            let written = write_application(&app)
                .expect("parse_application Ok ⇒ write_application must succeed (in-range)");
            assert_eq!(
                written, data,
                "write_application must reproduce the parsed payload byte-for-byte"
            );

            // parse ∘ write ∘ parse == parse: re-parse the writer output
            // and confirm structural equality.
            let re = parse_application(&written)
                .expect("write_application output must re-parse as APPLICATION");
            assert_eq!(re, app, "APPLICATION parse/write/parse round-trip drift");

            // Second-pass determinism guard.
            let written2 = write_application(&re).expect("second write_application must succeed");
            assert_eq!(
                written, written2,
                "write_application is non-deterministic — same input yielded different bytes"
            );
        }
        Err(_) => {
            // The only rejection §8.4 admits is a payload too short to
            // carry the 4-byte ID. A longer payload must never reject.
            assert!(
                data.len() < 4,
                "parse_application rejected a {}-byte payload (>= 4) — only sub-4-byte payloads may reject",
                data.len()
            );
        }
    }

    // -----------------------------------------------------------------
    // Phase 3: synthesised-struct writer drive + 24-bit ceiling guard.
    // Build an `Application` straight from fuzzer bytes so the writer
    // sees shapes the parser would never produce, then exercise the
    // ceiling-arithmetic branch precisely.
    // -----------------------------------------------------------------
    if data.len() >= 4 {
        let id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let body = &data[4..];

        // Bounded data run derived from the input tail. Length picked
        // from a byte so the fuzzer can steer it across the small range.
        let want_len = if body.is_empty() {
            0
        } else {
            (body[0] as usize) % (MAX_SYNTH_DATA + 1)
        };
        let mut payload = Vec::with_capacity(want_len);
        // Fill from the body (cycled) so contents vary; zero-pad the
        // tail when the body is shorter than the requested run.
        for i in 0..want_len {
            payload.push(if body.is_empty() {
                0
            } else {
                body[i % body.len()]
            });
        }

        let app = Application { id, data: payload };
        // 4 + want_len is far below the ceiling, so this must succeed and
        // round-trip.
        let written = write_application(&app).expect("in-range synthesised write must succeed");
        assert_eq!(
            written.len(),
            4 + app.data.len(),
            "synthesised APPLICATION wire length must be 4 + data.len()"
        );
        let re = parse_application(&written).expect("synthesised write output must re-parse");
        assert_eq!(re, app, "synthesised APPLICATION round-trip drift");
    }

    // -----------------------------------------------------------------
    // Static invariant: the synthesised data run must stay strictly
    // under the 24-bit metadata-block-length ceiling so phase 3 always
    // lands on the writer's in-range (Ok) branch. The over-ceiling
    // (Err) branch is checked by `metadata::tests` against the true
    // 2^24 boundary; reproducing it here would force a ~16 MiB
    // allocation on every iteration, which would just report a phantom
    // OOM rather than exercise codec logic.
    const _: () = assert!(4 + MAX_SYNTH_DATA <= BlockHeader::MAX_LENGTH as usize);
});
