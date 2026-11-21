//! Whole files as outside tools write them: every metadata block, block
//! chains in unusual orders, tags before and after the stream, seeking
//! with and without a SEEKTABLE, damaged and cut streams.
//!
//! The files come from the black-box `flac` / `metaflac` tools, or are
//! rearranged from such a file by the test and first shown to the
//! black-box decoder as a control. Skips when the tools are missing.

mod common;

use common::*;
use oxideav_core::{Error, Frame};
use oxideav_flac::verify::{VerifyOutcome, verify_stream};

const RATE: u32 = 44_100;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Encode `planes` (16-bit) with the black-box encoder and extra
/// arguments; returns the file image.
fn flac_file(
    scratch: &Scratch,
    planes: &[Vec<i32>],
    wav_chunks: &[u8],
    args: &[String],
) -> Result<Vec<u8>, String> {
    let wav = scratch.path("in.wav");
    let out = scratch.path("out.flac");
    std::fs::write(&wav, wav_bytes(planes, 16, RATE, wav_chunks)).map_err(|e| e.to_string())?;
    let mut all = strings(&["-s", "-f", "--channel-map=none"]);
    all.extend(args.iter().cloned());
    all.extend(["-o".to_string(), s(&out), s(&wav)]);
    run("flac", &all)?;
    std::fs::read(&out).map_err(|e| e.to_string())
}

/// The metadata blocks of a file image as `(type, payload)`, and the
/// offset of the first frame.
fn blocks(file: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
    assert_eq!(&file[..4], b"fLaC");
    let mut at = 4;
    let mut out = Vec::new();
    loop {
        let last = file[at] & 0x80 != 0;
        let kind = file[at] & 0x7F;
        let len = u32::from_be_bytes([0, file[at + 1], file[at + 2], file[at + 3]]) as usize;
        out.push((kind, file[at + 4..at + 4 + len].to_vec()));
        at += 4 + len;
        if last {
            return (out, at);
        }
    }
}

fn assemble(blocks: &[(u8, Vec<u8>)], frames: &[u8]) -> Vec<u8> {
    let mut file = b"fLaC".to_vec();
    for (i, (kind, payload)) in blocks.iter().enumerate() {
        let last = if i + 1 == blocks.len() { 0x80 } else { 0 };
        file.push(last | kind);
        file.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        file.extend_from_slice(payload);
    }
    file.extend_from_slice(frames);
    file
}

/// The file decodes to `planes`, here and by the MD5.
fn decodes_exactly(file: &[u8], planes: &[Vec<i32>]) -> Result<(), String> {
    let ours = decode_file(file)?;
    if ours.pcm != pack(planes, 16) {
        return Err(format!(
            "decoded {} PCM bytes, expected {} (or other content)",
            ours.pcm.len(),
            planes[0].len() * planes.len() * 2
        ));
    }
    match verify_stream(file) {
        Ok(VerifyOutcome::Match) => Ok(()),
        other => Err(format!("MD5 check: {other:?}")),
    }
}

fn tool_accepts(scratch: &Scratch, file: &[u8]) -> bool {
    let path = scratch.path("control.flac");
    std::fs::write(&path, file).is_ok() && tool_test(&path).is_ok()
}

#[test]
fn metadata_blocks_from_the_black_box_tools() {
    if !have_flac() || !have_metaflac() {
        eprintln!("skip: black-box `flac` / `metaflac` not installed");
        return;
    }
    let scratch = Scratch::new("streams-meta");
    let planes = signal(Signal::Tone, 16, 2, 5 * RATE as usize, 1);
    let mut tally = Tally::default();

    // Files the test hands to the tools.
    let picture: Vec<u8> = (0..3000u32).map(|i| (i * 31 % 251) as u8).collect();
    let big_picture: Vec<u8> = (0..3_000_000u32).map(|i| (i * 17 % 253) as u8).collect();
    let pic_path = scratch.path("cover.png");
    let big_pic_path = scratch.path("big.png");
    std::fs::write(&pic_path, &picture).unwrap();
    std::fs::write(&big_pic_path, &big_picture).unwrap();
    let huge_value = "lorem ipsum dolor ".repeat(3000); // about 54 KiB
    let huge_path = scratch.path("huge.txt");
    std::fs::write(&huge_path, &huge_value).unwrap();
    let cue_path = scratch.path("disc.cue");
    std::fs::write(
        &cue_path,
        "FILE \"in.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  \
         TRACK 02 AUDIO\n    INDEX 00 00:01:50\n    INDEX 01 00:02:00\n  \
         TRACK 03 AUDIO\n    INDEX 01 00:03:37\n",
    )
    .unwrap();

    // --- padding and seek table shapes --------------------------------
    for args in [
        vec!["--no-padding"],
        vec!["-P", "0"],
        vec!["-P", "1"],
        vec!["-P", "1000000"],
        vec!["-P", "16777000"],
        vec!["--no-seektable"],
        vec!["--no-seektable", "--no-padding"],
        vec!["-S", "1x"],
        vec!["-S", "500x"],
        vec!["-S", "0.1s"],
        vec!["-S", "X", "-S", "X", "-S", "X"],
        vec!["-S", "0", "-S", "4096", "-S", "100000", "-S", "X"],
        vec!["-b", "256", "-S", "0.01s"],
    ] {
        let name = format!("flac {}", args.join(" "));
        let outcome = flac_file(&scratch, &planes, &[], &strings(&args))
            .and_then(|file| decodes_exactly(&file, &planes));
        tally.record(&name, outcome);
    }

    // --- tags --------------------------------------------------------------
    let tagged = flac_file(
        &scratch,
        &planes,
        &[],
        &[
            "-T".into(),
            "TITLE=A title".into(),
            "-T".into(),
            "ARTIST=Zoë Ünïcode — 音楽".into(),
            "-T".into(),
            "artist=second artist".into(),
            "-T".into(),
            "COMMENT=has = signs = inside".into(),
            format!("--tag-from-file=LYRICS={}", s(&huge_path)),
        ],
    );
    tally.record(
        "tags",
        tagged.and_then(|file| {
            decodes_exactly(&file, &planes)?;
            let demux = open_demuxer(file)?;
            let meta = demux.metadata();
            let has = |k: &str, v: &str| meta.iter().any(|(a, b)| a == k && b == v);
            if !has("title", "A title")
                || !has("artist", "Zoë Ünïcode — 音楽")
                || !has("artist", "second artist")
                || !has("comment", "has = signs = inside")
                || !has("lyrics", huge_value.trim())
            {
                return Err("a tag is missing or altered".into());
            }
            Ok(())
        }),
    );

    // --- pictures ----------------------------------------------------------
    let pictured = flac_file(
        &scratch,
        &planes,
        &[],
        &[
            format!(
                "--picture=3|image/png|front cover|32x32x24|{}",
                s(&pic_path)
            ),
            format!(
                "--picture=4|image/png|back — ü|640x480x24/0|{}",
                s(&big_pic_path)
            ),
        ],
    );
    tally.record(
        "pictures",
        pictured.and_then(|file| {
            decodes_exactly(&file, &planes)?;
            let demux = open_demuxer(file)?;
            let pics = demux.attached_pictures();
            if pics.len() != 2 {
                return Err(format!("{} pictures surfaced", pics.len()));
            }
            if pics[0].data != picture || pics[1].data != big_picture {
                return Err("picture bytes differ".into());
            }
            if pics[0].mime_type != "image/png"
                || pics[0].description != "front cover"
                || pics[1].description != "back — ü"
            {
                return Err("picture fields differ".into());
            }
            Ok(())
        }),
    );

    // --- cue sheet -----------------------------------------------------------
    let cued = flac_file(
        &scratch,
        &planes,
        &[],
        &[format!("--cuesheet={}", s(&cue_path))],
    );
    tally.record(
        "cuesheet",
        cued.and_then(|file| {
            decodes_exactly(&file, &planes)?;
            let demux = open_demuxer(file)?;
            let starts: Vec<i64> = demux.chapters().iter().map(|c| c.start.value).collect();
            let ends: Vec<i64> = demux.chapters().iter().map(|c| c.end.value).collect();
            // mm:ss:ff with 75 frames per second of 588 samples each.
            // Track 2 opens with a pre-gap (index 0) at 00:01:50 and
            // begins at its index 1; track 1 ends where that gap starts.
            let want = [0, 2 * 75 * 588, (3 * 75 + 37) * 588];
            let gap = (75 + 50) * 588;
            if starts != want || ends != [gap, want[2], planes[0].len() as i64] {
                return Err(format!("chapters {starts:?} .. {ends:?}"));
            }
            Ok(())
        }),
    );

    // --- application blocks (foreign metadata kept from the WAVE) -----------
    let mut list = b"LIST".to_vec();
    let info = b"INFOINAM\x06\x00\x00\x00title\x00";
    list.extend_from_slice(&(info.len() as u32).to_le_bytes());
    list.extend_from_slice(info);
    let foreign = flac_file(
        &scratch,
        &planes,
        &list,
        &strings(&["--keep-foreign-metadata"]),
    );
    tally.record(
        "application blocks",
        foreign.and_then(|file| {
            let (chain, _) = blocks(&file);
            if !chain.iter().any(|(kind, _)| *kind == 2) {
                return Err("the tool wrote no APPLICATION block".into());
            }
            decodes_exactly(&file, &planes)
        }),
    );

    // --- everything at once, then edited in place ------------------------------
    let full = flac_file(
        &scratch,
        &planes,
        &list,
        &[
            "--keep-foreign-metadata".into(),
            "-T".into(),
            "TITLE=all blocks".into(),
            format!("--picture=3|image/png|c|32x32x24|{}", s(&pic_path)),
            format!("--cuesheet={}", s(&cue_path)),
            "-S".into(),
            "10x".into(),
            "-P".into(),
            "5000".into(),
        ],
    );
    match full {
        Err(e) => tally.record("all blocks", Err(e)),
        Ok(full) => {
            tally.record("all blocks", decodes_exactly(&full, &planes));
            let path = scratch.path("edit.flac");
            for ops in [
                vec!["--add-padding=1234".to_string()],
                vec!["--add-padding=10".into(), "--add-padding=20".into()],
                vec!["--remove".into(), "--block-type=PADDING".into()],
                vec!["--remove-all".into()],
                vec![
                    "--remove".into(),
                    "--block-type=SEEKTABLE".into(),
                    "--dont-use-padding".into(),
                ],
                vec!["--add-seekpoint=1000x".into()],
                vec!["--set-tag=EXTRA=1".into(), "--dont-use-padding".into()],
                vec![format!(
                    "--import-picture-from=6|image/png|more|8x8x24|{}",
                    s(&big_pic_path)
                )],
                vec!["--merge-padding".into()],
                vec!["--sort-padding".into()],
            ] {
                let name = format!("metaflac {}", ops.join(" "));
                std::fs::write(&path, &full).unwrap();
                let mut args = ops.clone();
                args.push(s(&path));
                let outcome = run("metaflac", &args)
                    .and_then(|_| std::fs::read(&path).map_err(|e| e.to_string()))
                    .and_then(|file| decodes_exactly(&file, &planes));
                tally.record(&name, outcome);
            }

            // Rearranged by the test; the black-box decoder is the
            // control for whether the result is still a FLAC file.
            let (chain, first_frame) = blocks(&full);
            let frames = &full[first_frame..];
            let mut variants: Vec<(String, Vec<u8>)> = Vec::new();
            let mut reversed = chain.clone();
            reversed[1..].reverse();
            variants.push(("blocks reversed".into(), assemble(&reversed, frames)));
            let mut unknown = chain.clone();
            unknown.insert(1, (50, vec![0xAB; 77]));
            unknown.push((126, Vec::new()));
            variants.push(("reserved block types".into(), assemble(&unknown, frames)));
            let mut empties = chain.clone();
            empties.insert(1, (1, Vec::new()));
            empties.push((2, b"zero".to_vec()));
            variants.push(("empty blocks".into(), assemble(&empties, frames)));
            let mut many = chain.clone();
            for i in 0..600u32 {
                many.push((1, vec![0; (i % 7) as usize]));
            }
            variants.push(("600 padding blocks".into(), assemble(&many, frames)));
            // A Vorbis comment block near the 16 MiB block limit.
            let mut huge = chain.clone();
            huge.retain(|(kind, _)| *kind != 4);
            let value = "x".repeat(16_000_000);
            let mut comment = Vec::new();
            comment.extend_from_slice(&6u32.to_le_bytes());
            comment.extend_from_slice(b"vendor");
            comment.extend_from_slice(&2u32.to_le_bytes());
            comment.extend_from_slice(&(value.len() as u32 + 4).to_le_bytes());
            comment.extend_from_slice(b"BIG=");
            comment.extend_from_slice(value.as_bytes());
            comment.extend_from_slice(&7u32.to_le_bytes());
            comment.extend_from_slice(b"AFTER=1");
            huge.push((4, comment));
            let huge = assemble(&huge, frames);
            if tool_accepts(&scratch, &huge) {
                let outcome = decodes_exactly(&huge, &planes).and_then(|()| {
                    let demux = open_demuxer(huge)?;
                    let meta = demux.metadata();
                    let big = meta.iter().find(|(k, _)| k == "big").map(|(_, v)| v.len());
                    let after = meta.iter().any(|(k, v)| k == "after" && v == "1");
                    if big != Some(value.len()) || !after {
                        return Err("the 16 MB tag did not come through".into());
                    }
                    Ok(())
                });
                tally.record("16 MB Vorbis comment", outcome);
            } else {
                eprintln!("control: the black-box decoder does not accept the 16 MB tag");
            }
            let only = vec![chain[0].clone()];
            variants.push(("STREAMINFO only".into(), assemble(&only, frames)));

            for (name, file) in variants {
                if !tool_accepts(&scratch, &file) {
                    eprintln!("control: the black-box decoder does not accept `{name}`");
                    continue;
                }
                tally.record(&name, decodes_exactly(&file, &planes));
            }

            // Tags of another format around the stream. The black-box
            // decoder reads through an ID3v2 tag in front with a
            // warning; bytes behind the last frame make it (and the
            // second outside decoder) report an error after all the
            // audio came out. Here all of them leave the audio intact.
            let plain = assemble(&chain, frames);
            let mut id3v2 = b"ID3\x04\x00\x00".to_vec();
            id3v2.extend_from_slice(&[0, 0, 0x0F, 0x50]); // 2000 bytes, synchsafe
            id3v2.extend_from_slice(&[0u8; 2000]);
            let mut prefixed = id3v2.clone();
            prefixed.extend_from_slice(&plain);
            let mut id3v1 = b"TAG".to_vec();
            id3v1.resize(128, b' ');
            let mut suffixed = plain.clone();
            suffixed.extend_from_slice(&id3v1);
            let mut both = prefixed.clone();
            both.extend_from_slice(&id3v1);
            let mut zeros = plain.clone();
            zeros.extend_from_slice(&[0u8; 5000]);
            let mut noise = plain.clone();
            let mut rng = Rng(4242);
            noise.extend((0..20_000).map(|_| rng.next() as u8));
            for (name, file) in [
                ("ID3v2 tag in front", prefixed),
                ("ID3v1 tag behind", suffixed),
                ("ID3 tags at both ends", both),
                ("zero bytes behind", zeros),
                ("random bytes behind", noise),
            ] {
                tally.record(name, decodes_exactly(&file, &planes));
            }
        }
    }
    tally.finish("metadata");
}

/// After `seek_to(target)` the demuxer stands on the frame that holds
/// the target sample, and what it reads from there is the audio from
/// that frame's first sample on.
fn check_seeks(file: &[u8], planes: &[Vec<i32>], block: usize) -> Result<(), String> {
    let total = planes[0].len();
    let want = pack(planes, 16);
    let stride = 2 * planes.len();
    let last_frame = (total - 1) / block * block;
    let mut targets = vec![
        0,
        1,
        block - 1,
        block,
        block + 1,
        total / 2,
        total / 3 * 2 + 17,
        last_frame.saturating_sub(1),
        last_frame,
        total - 1,
    ];
    let mut rng = Rng(0xC0FFEE);
    targets.extend((0..12).map(|_| rng.range(0, total as i64 - 1) as usize));
    let mut demux = open_demuxer(file.to_vec())?;
    for (target, expect) in targets
        .into_iter()
        .map(|t| (t as i64, t / block * block))
        .chain([
            (-5, 0),
            (total as i64, last_frame),
            (total as i64 + 99_999, last_frame),
            (i64::MAX, last_frame),
        ])
    {
        let landed = demux
            .seek_to(0, target)
            .map_err(|e| format!("seek {target}: {e}"))?;
        if landed != expect as i64 {
            return Err(format!(
                "seek to {target} landed on {landed}, the frame that holds it starts at {expect}"
            ));
        }
        let decoded = drain(demux.as_mut())?;
        if decoded.first_pts != Some(landed) {
            return Err(format!(
                "after seek to {target}: first packet at {:?}",
                decoded.first_pts
            ));
        }
        if decoded.pcm != want[expect * stride..] {
            return Err(format!("after seek to {target}: audio differs"));
        }
    }
    Ok(())
}

#[test]
fn seeking_lands_on_the_frame_that_holds_the_target() {
    if !have_flac() {
        eprintln!("skip: black-box `flac` not installed");
        return;
    }
    let scratch = Scratch::new("streams-seek");
    let planes = signal(Signal::Tone, 16, 2, 400_000, 2);
    let mut tally = Tally::default();
    for block in [256usize, 1024, 4096] {
        for table in [
            vec!["--no-seektable"],
            vec!["-S", "1x"],
            vec!["-S", "3x"],
            vec!["-S", "40x"],
            vec!["-S", "0.5s"],
            vec!["-S", "X", "-S", "X"],
            vec!["-S", "123456"],
        ] {
            let mut args = strings(&["-b", &block.to_string()]);
            args.extend(strings(&table));
            let name = format!("seek, block {block}, {}", table.join(" "));
            let outcome = flac_file(&scratch, &planes, &[], &args)
                .and_then(|file| check_seeks(&file, &planes, block));
            tally.record(&name, outcome);
        }
    }
    // This crate's own muxer, with and without its seek table.
    for mux in [
        oxideav_flac::container::FlacMuxerOptions::default(),
        oxideav_flac::container::FlacMuxerOptions::no_seektable(),
        oxideav_flac::container::FlacMuxerOptions::default().with_seek_point_interval(50_000),
    ] {
        let name = format!("seek, own muxer {mux:?}");
        let options = oxideav_flac::encoder::FlacEncoderOptions::new().with_block_size(1152);
        let outcome = encode_packets(&planes, 16, RATE, options, 5000)
            .and_then(|(extradata, packets)| mux_file(&extradata, 2, 16, RATE, &packets, mux))
            .and_then(|file| check_seeks(&file, &planes, 1152));
        tally.record(&name, outcome);
    }
    tally.finish("seeking");
}

/// Decode what can be decoded: `(first sample, PCM)` of every packet the
/// decoder accepts.
fn decode_surviving(file: &[u8]) -> Result<Vec<(i64, Vec<u8>)>, String> {
    let mut demux = open_demuxer(file.to_vec())?;
    let params = demux.streams()[0].params.clone();
    let mut dec = oxideav_flac::decoder::make_decoder(&params).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    loop {
        let pkt = match demux.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => return Ok(out),
            Err(e) => return Err(e.to_string()),
        };
        dec.send_packet(&pkt).map_err(|e| e.to_string())?;
        if let Ok(Frame::Audio(a)) = dec.receive_frame() {
            out.push((pkt.pts.unwrap_or(-1), a.data[0].clone()));
        }
    }
}

/// A stream that was cut — at the end, or at the start so that it begins
/// in the middle of a frame — still yields every whole frame, each at
/// its own sample position, and nothing else. The black-box decoder,
/// told to decode through errors, recovers the same audio.
#[test]
fn cut_and_damaged_streams_yield_their_whole_frames() {
    if !have_flac() {
        eprintln!("skip: black-box `flac` not installed");
        return;
    }
    let scratch = Scratch::new("streams-cut");
    let planes = signal(Signal::Tone, 16, 2, 60_000, 3);
    let want = pack(&planes, 16);
    let mut tally = Tally::default();
    let file = flac_file(&scratch, &planes, &[], &strings(&["-b", "1024"])).unwrap();
    let (_, first_frame) = blocks(&file);
    let body = file.len() - first_frame;

    let tool_through_errors = |image: &[u8]| -> Result<Vec<u8>, String> {
        let path = scratch.path("cut.flac");
        let wav = scratch.path("cut.wav");
        std::fs::write(&path, image).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&wav);
        let _ = std::process::Command::new("flac")
            .args(["-s", "-d", "-F", "-f", "-o"])
            .arg(&wav)
            .arg(&path)
            .output();
        let bytes = std::fs::read(&wav).map_err(|e| e.to_string())?;
        Ok(wav_data(&bytes).unwrap_or(&[]).to_vec())
    };

    let mut rng = Rng(77);
    let mut cuts: Vec<usize> = (0..25)
        .map(|_| rng.range(1, body as i64 - 1) as usize)
        .collect();
    cuts.extend([1, 2, 5, 16, body - 1, body - 2, body - 3]);
    for cut in cuts {
        // Cut at the end.
        let image = &file[..first_frame + cut];
        let outcome = decode_surviving(image).and_then(|frames| {
            let mut pcm = Vec::new();
            for (pts, data) in &frames {
                if *pts as usize * 4 != pcm.len() {
                    return Err(format!("frame at {pts} out of sequence"));
                }
                pcm.extend_from_slice(data);
            }
            if pcm != want[..pcm.len()] {
                return Err("surviving audio differs from the input".into());
            }
            let theirs = tool_through_errors(image)?;
            // The tool pads the lost tail with silence up to the length
            // STREAMINFO states; its audio is what precedes that.
            if theirs.len() < pcm.len() || theirs[..pcm.len()] != pcm[..] {
                return Err("the black-box decoder recovered other audio".into());
            }
            if theirs[pcm.len()..].iter().any(|&b| b != 0) {
                return Err(format!(
                    "the black-box decoder recovered more: {} bytes here",
                    pcm.len()
                ));
            }
            Ok(())
        });
        tally.record(&format!("cut after {cut} frame bytes"), outcome);

        // Cut at the start: the frames begin in the middle of one.
        let mut image = file[..first_frame].to_vec();
        image.extend_from_slice(&file[first_frame + cut..]);
        let outcome = decode_surviving(&image).and_then(|frames| {
            let mut pcm = Vec::new();
            let Some(&(first, _)) = frames.first() else {
                // Too little is left to hold a frame; the black-box
                // decoder must find none either.
                let theirs = tool_through_errors(&image)?;
                return if theirs.iter().all(|&b| b == 0) {
                    Ok(())
                } else {
                    Err("nothing survived here, but the black-box decoder found audio".into())
                };
            };
            for (pts, data) in &frames {
                if (*pts - first) as usize * 4 != pcm.len() {
                    return Err(format!("frame at {pts} out of sequence"));
                }
                pcm.extend_from_slice(data);
            }
            if pcm != want[first as usize * 4..] {
                return Err("surviving audio differs from the input".into());
            }
            let theirs = tool_through_errors(&image)?;
            if !theirs.ends_with(&pcm) || theirs[..theirs.len() - pcm.len()].iter().any(|&b| b != 0)
            {
                return Err(format!(
                    "the black-box decoder recovered {} bytes, here {}",
                    theirs.len(),
                    pcm.len()
                ));
            }
            Ok(())
        });
        tally.record(&format!("cut before {cut} frame bytes"), outcome);
    }

    // One flipped bit anywhere in the frames costs exactly one frame.
    for _ in 0..25 {
        let at = first_frame + rng.range(0, body as i64 - 1) as usize;
        let mut image = file.clone();
        image[at] ^= 1 << rng.range(0, 7);
        let outcome = decode_surviving(&image).and_then(|frames| {
            let good: usize = frames.iter().map(|(_, d)| d.len()).sum();
            if good + 1024 * 4 < want.len() {
                return Err(format!(
                    "{} of {} PCM bytes survived one flipped bit",
                    good,
                    want.len()
                ));
            }
            for (pts, data) in &frames {
                let at = *pts as usize * 4;
                if want.get(at..at + data.len()) != Some(&data[..]) {
                    return Err(format!("frame at {pts} differs from the input"));
                }
            }
            Ok(())
        });
        tally.record(&format!("bit flipped at byte {at}"), outcome);
    }
    tally.finish("cut and damaged streams");
}
