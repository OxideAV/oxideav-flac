//! Runtime libavcodec interop for the FLAC cross-decode fuzz oracle.
//!
//! The shared library is loaded via `dlopen` at first call — there is
//! no `ffmpeg-sys` / `rusty_ffmpeg`-style build-script dep that would
//! pull libavcodec source into the workspace's cargo dep tree. The
//! oracle harness checks `libavcodec::available()` up front and
//! `eprintln!`s + `return`s early when the shared library isn't
//! installed, so fuzz binaries built on a host without ffmpeg still
//! run (the per-sample equality assertion just doesn't fire).
//!
//! Workspace policy bars consulting libavcodec / libFLAC source; we
//! only inspect the public C headers (`<libavcodec/avcodec.h>`,
//! `<libavutil/frame.h>`, `<libavutil/samplefmt.h>`) for function
//! signatures + the documented `AV_CODEC_ID_FLAC == 86028` constant.
//!
//! Install on Debian / Ubuntu via `apt-get install -y ffmpeg` (which
//! pulls `libavcodec61` or whichever is current). On macOS use
//! `brew install ffmpeg`.

#![allow(unsafe_code)]

pub mod libavcodec {
    use libloading::{Library, Symbol};
    use std::ffi::c_void;
    use std::sync::OnceLock;

    /// `AV_CODEC_ID_FLAC` — documented value `86028` (`0x15002`) in the
    /// public `enum AVCodecID` (libavcodec/codec_id.h). Stable across
    /// every libavcodec major version we target (58 through 62).
    pub const AV_CODEC_ID_FLAC: i32 = 86028;

    /// `AVERROR(EAGAIN)` — `-EAGAIN` per the documented AVERROR macro.
    /// `EAGAIN == 11` on Linux/glibc; on macOS it's `35`. We only test
    /// against this in CI (Linux runner), so the Linux value is fine.
    /// Misidentifying it just means we treat EAGAIN as a hard error,
    /// which the harness already tolerates as "skip this iteration".
    const AVERROR_EAGAIN: i32 = -11;

    /// AVSampleFormat values per `<libavutil/samplefmt.h>`. FLAC always
    /// decodes to one of the planar signed integer formats (S16P or
    /// S32P depending on STREAMINFO bps). We only oracle-compare those
    /// two; other formats (FLT/DBL/U8/...) cause the harness to skip
    /// the equality assertion for that input.
    const AV_SAMPLE_FMT_S16P: i32 = 6;
    const AV_SAMPLE_FMT_S32P: i32 = 8;

    /// Conventional libavcodec shared-object names the loader will try
    /// in order. Covers Debian / Ubuntu (versioned `.so.NN`), the
    /// unversioned `-dev` symlink, and macOS (`.dylib`).
    const CANDIDATES: &[&str] = &[
        "libavcodec.so.62",
        "libavcodec.so.61",
        "libavcodec.so.60",
        "libavcodec.so.59",
        "libavcodec.so.58",
        "libavcodec.so",
        "libavcodec.dylib",
    ];

    fn lib() -> Option<&'static Library> {
        static LIB: OnceLock<Option<Library>> = OnceLock::new();
        LIB.get_or_init(|| {
            for name in CANDIDATES {
                // SAFETY: `Library::new` is documented as unsafe because
                // the loaded library may run code at load time. We
                // accept that risk for fuzz tooling — libavcodec is a
                // well-behaved shared library distributed by distros.
                if let Ok(l) = unsafe { Library::new(name) } {
                    return Some(l);
                }
            }
            None
        })
        .as_ref()
    }

    /// True iff a libavcodec shared library was successfully loaded.
    /// Cross-decode fuzz harnesses early-return when this is false so
    /// the binary still runs without an oracle (the assertions just
    /// don't fire).
    pub fn available() -> bool {
        lib().is_some()
    }

    /// Per-channel decoded samples. For FLAC the libavcodec output is
    /// always planar signed integer (`S16P` for ≤16 bps, `S32P` for
    /// 17..=32 bps); we normalise both to `i32` so the harness can
    /// per-sample-compare against our decoder regardless of bps.
    pub struct DecodedAudio {
        pub channels: usize,
        pub nb_samples: usize,
        /// Per-channel planar samples, length == channels. Each inner
        /// vec has length == nb_samples.
        pub planes: Vec<Vec<i32>>,
        /// libavcodec's reported sample format (AVSampleFormat). The
        /// harness only per-sample-compares S16P / S32P; other formats
        /// are treated as "skip equality, just check non-panic".
        pub sample_fmt: i32,
    }

    /// Outcome of asking libavcodec to decode one FLAC packet.
    pub enum OracleResult {
        /// Library not loadable / decoder factory not registered.
        Unavailable,
        /// libavcodec rejected the input (any error at any step).
        /// Our decoder is allowed to either accept or reject in this
        /// case — the contract only asserts equality when libavcodec
        /// accepts.
        Rejected,
        /// libavcodec produced a frame.
        Frame(DecodedAudio),
    }

    /// Open a fresh libavcodec FLAC decoder context configured with
    /// `extradata` (the FLAC METADATA_BLOCK_STREAMINFO + header bytes
    /// emitted by our encoder / demuxer). Returns the AVCodecContext
    /// on success, or `None` if libavcodec isn't available or the
    /// open failed. The caller MUST free via `free_decoder`.
    ///
    /// Lifetime contract: the returned context borrows from `lib()`'s
    /// `OnceLock<Library>` which lives for the program lifetime, so
    /// the symbol pointers we cached for free remain valid.
    pub fn open_decoder(extradata: &[u8]) -> Option<*mut c_void> {
        type FindDecoderFn = unsafe extern "C" fn(i32) -> *const c_void;
        type AllocContext3Fn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
        type Open2Fn = unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32;
        type AvMallocFn = unsafe extern "C" fn(usize) -> *mut c_void;

        let l = lib()?;
        unsafe {
            let find_decoder: Symbol<FindDecoderFn> = l.get(b"avcodec_find_decoder").ok()?;
            let alloc_context3: Symbol<AllocContext3Fn> = l.get(b"avcodec_alloc_context3").ok()?;
            let open2: Symbol<Open2Fn> = l.get(b"avcodec_open2").ok()?;
            let av_malloc: Symbol<AvMallocFn> = l.get(b"av_malloc").ok()?;

            let codec = find_decoder(AV_CODEC_ID_FLAC);
            if codec.is_null() {
                return None;
            }
            let ctx = alloc_context3(codec);
            if ctx.is_null() {
                return None;
            }

            // Wire extradata into the AVCodecContext if any was
            // supplied. AVCodecContext layout for the prefix we touch
            // is **not** byte-stable across libavcodec major versions,
            // so we skip extradata wiring entirely — libavcodec's FLAC
            // decoder happily parses an inline `fLaC` magic +
            // STREAMINFO METADATA_BLOCK at the head of the first
            // packet (the demuxer-less path), which is exactly what
            // our encoder emits before the first audio frame anyway.
            // Skipping the extradata pointer keeps this wrapper
            // version-agnostic and matches the way distros' standalone
            // `flac` CLI passes data through.
            let _ = (extradata, av_malloc);

            if open2(ctx, codec, std::ptr::null_mut()) < 0 {
                let mut ctx_mut = ctx;
                if let Ok(free_context) =
                    l.get::<unsafe extern "C" fn(*mut *mut c_void)>(b"avcodec_free_context")
                {
                    free_context(&mut ctx_mut);
                }
                return None;
            }
            Some(ctx)
        }
    }

    /// Free an AVCodecContext returned by `open_decoder`. Safe to call
    /// with a NULL pointer (no-op).
    pub fn free_decoder(mut ctx: *mut c_void) {
        if ctx.is_null() {
            return;
        }
        let Some(l) = lib() else {
            return;
        };
        unsafe {
            if let Ok(free_context) =
                l.get::<unsafe extern "C" fn(*mut *mut c_void)>(b"avcodec_free_context")
            {
                free_context(&mut ctx);
            }
        }
    }

    /// Send one packet (a complete FLAC frame, optionally prefixed by
    /// the `fLaC` magic + STREAMINFO when this is the first packet)
    /// then drain a single frame. Returns `Rejected` on any error or
    /// EOF-without-frame; `Frame(...)` on the first picture produced.
    pub fn decode_packet(ctx: *mut c_void, data: &[u8]) -> OracleResult {
        type PacketAllocFn = unsafe extern "C" fn() -> *mut c_void;
        type FrameAllocFn = unsafe extern "C" fn() -> *mut c_void;
        type SendPacketFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> i32;
        type ReceiveFrameFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
        type PacketFreeFn = unsafe extern "C" fn(*mut *mut c_void);
        type FrameFreeFn = unsafe extern "C" fn(*mut *mut c_void);

        let Some(l) = lib() else {
            return OracleResult::Unavailable;
        };
        if ctx.is_null() {
            return OracleResult::Unavailable;
        }
        unsafe {
            let packet_alloc: Symbol<PacketAllocFn> = match l.get(b"av_packet_alloc") {
                Ok(s) => s,
                Err(_) => return OracleResult::Unavailable,
            };
            let frame_alloc: Symbol<FrameAllocFn> = match l.get(b"av_frame_alloc") {
                Ok(s) => s,
                Err(_) => return OracleResult::Unavailable,
            };
            let send_packet: Symbol<SendPacketFn> = match l.get(b"avcodec_send_packet") {
                Ok(s) => s,
                Err(_) => return OracleResult::Unavailable,
            };
            let receive_frame: Symbol<ReceiveFrameFn> = match l.get(b"avcodec_receive_frame") {
                Ok(s) => s,
                Err(_) => return OracleResult::Unavailable,
            };
            let packet_free: Symbol<PacketFreeFn> = match l.get(b"av_packet_free") {
                Ok(s) => s,
                Err(_) => return OracleResult::Unavailable,
            };
            let frame_free: Symbol<FrameFreeFn> = match l.get(b"av_frame_free") {
                Ok(s) => s,
                Err(_) => return OracleResult::Unavailable,
            };

            let mut pkt = packet_alloc();
            if pkt.is_null() {
                return OracleResult::Unavailable;
            }
            let mut frame = frame_alloc();
            if frame.is_null() {
                packet_free(&mut pkt);
                return OracleResult::Unavailable;
            }

            let result = (|| -> OracleResult {
                // AVPacket prefix layout (stable since libavcodec 57):
                //   off  0  *AVBufferRef  buf
                //   off  8  i64           pts
                //   off 16  i64           dts
                //   off 24  *u8           data
                //   off 32  i32           size
                // We only set data + size; buf stays NULL so libavcodec
                // copies the bytes itself before send_packet returns.
                const OFF_DATA: usize = 24;
                const OFF_SIZE: usize = 32;
                let pkt_bytes = pkt as *mut u8;
                (pkt_bytes.add(OFF_DATA) as *mut *const u8).write_unaligned(data.as_ptr());
                (pkt_bytes.add(OFF_SIZE) as *mut i32).write_unaligned(data.len() as i32);

                let sp = send_packet(ctx, pkt);
                if sp < 0 && sp != AVERROR_EAGAIN {
                    return OracleResult::Rejected;
                }
                // Send EOF to flush.
                let _ = send_packet(ctx, std::ptr::null());

                let rc = receive_frame(ctx, frame);
                if rc < 0 {
                    return OracleResult::Rejected;
                }

                // AVFrame prefix layout (stable since libavutil 55):
                //   off   0  *u8 x AV_NUM_DATA_POINTERS  data[8]
                //   off  64  i32 x AV_NUM_DATA_POINTERS  linesize[8]
                //   off  96  **u8                          extended_data
                //   off 104  i32                           width
                //   off 108  i32                           height
                //   off 112  i32                           nb_samples
                //   off 116  i32                           format
                // For audio, format is an AVSampleFormat. width/height
                // are zero. AV_NUM_DATA_POINTERS == 8 since libavutil
                // 51 — pre-dates everything we support.
                const OFF_F_EXTENDED_DATA: usize = 96;
                const OFF_F_NB_SAMPLES: usize = 112;
                const OFF_F_FORMAT: usize = 116;
                let f_bytes = frame as *const u8;
                let nb_samples = (f_bytes.add(OFF_F_NB_SAMPLES) as *const i32).read_unaligned();
                let sample_fmt = (f_bytes.add(OFF_F_FORMAT) as *const i32).read_unaligned();
                if nb_samples <= 0 {
                    return OracleResult::Rejected;
                }

                // Per-channel data lives in extended_data[ch] (which
                // aliases data[ch] for ≤8 channels — FLAC tops out at
                // 8 so we can read either; we use extended_data to
                // be future-proof). Channel count isn't directly on
                // AVFrame in older versions; we count non-NULL planes
                // up to the AV_NUM_DATA_POINTERS == 8 ceiling.
                let extended_data =
                    (f_bytes.add(OFF_F_EXTENDED_DATA) as *const *const *const u8).read_unaligned();
                if extended_data.is_null() {
                    return OracleResult::Rejected;
                }
                let mut planes: Vec<Vec<i32>> = Vec::new();
                let n = nb_samples as usize;
                // Walk extended_data[0..8]; stop at the first NULL plane
                // (planar formats always pack channels contiguously).
                for ch in 0..8 {
                    let plane_ptr = extended_data.add(ch).read_unaligned();
                    if plane_ptr.is_null() {
                        break;
                    }
                    let mut plane = Vec::with_capacity(n);
                    match sample_fmt {
                        x if x == AV_SAMPLE_FMT_S16P => {
                            for i in 0..n {
                                let v = (plane_ptr.add(i * 2) as *const i16).read_unaligned();
                                plane.push(v as i32);
                            }
                        }
                        x if x == AV_SAMPLE_FMT_S32P => {
                            for i in 0..n {
                                let v = (plane_ptr.add(i * 4) as *const i32).read_unaligned();
                                plane.push(v);
                            }
                        }
                        _ => {
                            // Unsupported format for our oracle — return
                            // an empty plane so the harness skips
                            // equality. We still report nb_samples for
                            // the non-equality assertion.
                            return OracleResult::Frame(DecodedAudio {
                                channels: 0,
                                nb_samples: n,
                                planes: Vec::new(),
                                sample_fmt,
                            });
                        }
                    }
                    planes.push(plane);
                }
                let channels = planes.len();
                OracleResult::Frame(DecodedAudio {
                    channels,
                    nb_samples: n,
                    planes,
                    sample_fmt,
                })
            })();

            frame_free(&mut frame);
            packet_free(&mut pkt);
            result
        }
    }
}
