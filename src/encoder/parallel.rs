//! Frame-parallel encoding.
//!
//! FLAC frames are independent: a frame is its header, one subframe per
//! channel and a CRC-16 footer (RFC 9639 §9), and "FLAC frames are
//! completely independent of each other" (§10) — no predictor or
//! entropy-coder state crosses a frame boundary. The encoder therefore
//! codes blocks on a fixed set of worker threads, each owning its own
//! search scratch, and hands the coded frames back in stream order.
//!
//! The caller thread keeps block cutting, frame numbering and the
//! min/max frame-size and total-sample statistics (folded in as frames
//! come back in order).
//!
//! The STREAMINFO MD5 (§8.2) is one sequential digest over the whole
//! input, so it cannot be split per frame. Each worker serialises its
//! block into the digest's byte order and deposits it with the
//! [`Md5Stage`], which feeds the digest strictly in frame order. The
//! caller thread hashes one ready block per block it submits and
//! everything ready whenever it would otherwise wait, which keeps the
//! digest off the workers when the frame search is the bottleneck; a
//! worker takes over as soon as [`WORKER_HASH_BACKLOG`] blocks are
//! waiting, which keeps the caller free to cut blocks when the digest is
//! the bottleneck. Either way the digest sees the blocks in order, so it comes
//! out exactly as the serial path computes it.
//!
//! Memory is bounded by a fixed window ([`FramePool::window`], two blocks
//! per worker): at most that many blocks are with the workers, and at
//! most that many are submitted but not yet hashed. A submit that would
//! exceed either bound first waits (hashing in the meantime when it can).

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use oxideav_core::{Error, Result};

use super::{FlacEncoderOptions, FrameShape, build_scratch, encode_frame, verify_encoded_frame};
use crate::md5::Md5;

/// Ready-but-unhashed blocks at which a worker, rather than the caller,
/// feeds the digest. Chosen by measurement on 44.1 kHz stereo at levels
/// 0, 5 and 8 with 2, 4 and 8 workers: 2 was at or near the fastest
/// setting everywhere (1 starves the caller's share at 2 workers; the
/// worker count leaves the digest behind at 4–8).
const WORKER_HASH_BACKLOG: usize = 2;

/// One block to code.
struct Job {
    /// Frame number (fixed-blocking: the block index, RFC 9639 §9.1.5).
    frame_number: u64,
    /// Interchannel samples in the block.
    samples: u32,
    /// Per-channel samples, `channels.len()` planes of `samples` each.
    channels: Vec<Vec<i32>>,
}

/// A coded block coming back from a worker.
pub(super) struct Done {
    /// Frame number of the block.
    pub frame_number: u64,
    /// Interchannel samples in the block.
    pub samples: u32,
    /// The coded frame, or the error coding (or verifying) it produced.
    pub result: Result<Vec<u8>>,
    /// The block's sample planes (taken by the pool for reuse).
    channels: Vec<Vec<i32>>,
}

/// The STREAMINFO MD5, fed in frame order by whichever thread is free.
struct Md5Stage {
    queue: Mutex<Md5Queue>,
    /// Signalled whenever the hasher stops or advances.
    progress: Condvar,
    /// Only the thread that set [`Md5Queue::hashing`] touches it, so the
    /// lock is never contended; it exists to make the hand-over sound.
    md5: Mutex<Md5>,
    /// Ready backlog at which a worker takes over the hashing.
    worker_threshold: usize,
}

struct Md5Queue {
    /// Frame number of the next block the digest needs.
    next: u64,
    /// Serialised blocks waiting for their turn, indexed by
    /// `frame_number - next`.
    ready: VecDeque<Option<Vec<u8>>>,
    /// Number of `Some` entries in `ready`.
    deposited: usize,
    /// A thread is feeding the digest right now.
    hashing: bool,
    /// Byte buffers of hashed blocks, for reuse.
    spare: Vec<Vec<u8>>,
}

impl Md5Stage {
    fn new(md5: Md5, first_frame: u64, worker_threshold: usize) -> Self {
        Md5Stage {
            queue: Mutex::new(Md5Queue {
                next: first_frame,
                ready: VecDeque::new(),
                deposited: 0,
                hashing: false,
                spare: Vec::new(),
            }),
            progress: Condvar::new(),
            md5: Mutex::new(md5),
            worker_threshold,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Md5Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Frame number of the next block the digest needs.
    fn next(&self) -> u64 {
        self.lock().next
    }

    /// A buffer to serialise a block into.
    fn buffer(&self) -> Vec<u8> {
        self.lock().spare.pop().unwrap_or_default()
    }

    /// Deposit frame `frame_number`'s serialised samples (from a worker).
    /// When the ready backlog has reached the worker threshold, also hash.
    fn deposit(&self, frame_number: u64, bytes: Vec<u8>) {
        let mut q = self.lock();
        let slot = (frame_number - q.next) as usize;
        if q.ready.len() <= slot {
            q.ready.resize_with(slot + 1, || None);
        }
        q.ready[slot] = Some(bytes);
        q.deposited += 1;
        // The caller may be waiting for exactly this block.
        self.progress.notify_all();
        if q.deposited >= self.worker_threshold {
            self.hash(q, usize::MAX);
        }
    }

    /// Hash up to `max_blocks` consecutive ready blocks unless another
    /// thread is hashing. Returns how many were hashed.
    fn hash_ready(&self, max_blocks: usize) -> usize {
        self.hash(self.lock(), max_blocks)
    }

    /// Become the hasher (unless there is one) and feed the digest up to
    /// `max_blocks` consecutive ready blocks, starting with the one it
    /// needs next. The hasher re-checks the queue under the lock before it
    /// stops, so a block deposited meanwhile is never stranded: either the
    /// hasher picks it up, or a later `hash` call finds the stage idle.
    fn hash<'a>(&'a self, mut q: MutexGuard<'a, Md5Queue>, max_blocks: usize) -> usize {
        if q.hashing {
            return 0;
        }
        q.hashing = true;
        let mut hashed = 0;
        while hashed < max_blocks {
            let Some(Some(_)) = q.ready.front() else {
                break;
            };
            let mut bytes = q.ready.pop_front().flatten().expect("checked");
            q.next += 1;
            q.deposited -= 1;
            drop(q);
            self.md5
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .update(&bytes);
            hashed += 1;
            q = self.lock();
            if q.spare.len() < 64 {
                bytes.clear();
                q.spare.push(bytes);
            }
            self.progress.notify_all();
        }
        q.hashing = false;
        self.progress.notify_all();
        hashed
    }

    /// Block until the digest has moved past `next`, or the block it needs
    /// is ready and nobody is hashing it.
    fn wait_progress(&self, next: u64) {
        let mut q = self.lock();
        while q.next == next && (q.hashing || !matches!(q.ready.front(), Some(Some(_)))) {
            q = self.progress.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Fixed set of frame-coding workers plus the reorder buffer that turns
/// their completions back into stream order.
pub(super) struct FramePool {
    jobs: Option<Sender<Job>>,
    done: Receiver<Done>,
    workers: Vec<JoinHandle<()>>,
    /// Completions waiting for their turn, indexed by
    /// `frame_number - next`.
    parked: VecDeque<Option<Done>>,
    /// Frame number of the next block to hand back.
    next: u64,
    /// Blocks submitted but not yet handed back.
    in_flight: usize,
    /// Most blocks with the workers, and most blocks submitted but not
    /// hashed, at once.
    window: usize,
    /// Recycled sample planes from handed-back blocks.
    spare: Vec<Vec<Vec<i32>>>,
    /// The digest over every block submitted to this pool.
    md5: Arc<Md5Stage>,
    /// Set when the pool is dropped with blocks still queued: workers skip
    /// the rest instead of coding frames nobody will read.
    cancel: Arc<AtomicBool>,
}

impl FramePool {
    /// Start `threads` workers coding frames of `shape` with the search
    /// that `options` selects; the first block submitted must be frame
    /// number `first_frame`, and `md5` is the digest of every sample
    /// before it. [`finish`](Self::finish) hands the digest back.
    pub(super) fn new(
        threads: usize,
        options: &FlacEncoderOptions,
        shape: FrameShape,
        verify: bool,
        first_frame: u64,
        md5: Md5,
    ) -> Self {
        let threads = threads.max(1);
        let md5 = Arc::new(Md5Stage::new(md5, first_frame, WORKER_HASH_BACKLOG));
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let cancel = Arc::new(AtomicBool::new(false));
        let workers = (0..threads)
            .map(|i| {
                let job_rx = Arc::clone(&job_rx);
                let done_tx = done_tx.clone();
                let options = options.clone();
                let md5 = Arc::clone(&md5);
                let cancel = Arc::clone(&cancel);
                std::thread::Builder::new()
                    .name(format!("flac-enc-{i}"))
                    .spawn(move || {
                        worker(&job_rx, &done_tx, &md5, &cancel, &options, shape, verify)
                    })
                    .expect("spawn FLAC encoder worker")
            })
            .collect();
        FramePool {
            jobs: Some(job_tx),
            done: done_rx,
            workers,
            parked: VecDeque::new(),
            next: first_frame,
            in_flight: 0,
            window: 2 * threads,
            spare: Vec::new(),
            md5,
            cancel,
        }
    }

    /// Number of worker threads.
    pub(super) fn threads(&self) -> usize {
        self.workers.len()
    }

    /// Blocks submitted and not yet handed back.
    pub(super) fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Frame number the next submitted block must carry.
    fn end(&self) -> u64 {
        self.next + self.in_flight as u64
    }

    /// `n_ch` empty sample planes with capacity for at least `samples`
    /// each, reusing a handed-back block's planes when one is available.
    pub(super) fn planes(&mut self, n_ch: usize, samples: usize) -> Vec<Vec<i32>> {
        match self.spare.pop() {
            Some(mut planes) if planes.len() == n_ch => {
                for p in &mut planes {
                    p.clear();
                    p.reserve(samples);
                }
                planes
            }
            _ => (0..n_ch).map(|_| Vec::with_capacity(samples)).collect(),
        }
    }

    /// Queue block `frame_number` (consecutive from the pool's
    /// `first_frame`). Waits first while the window is full, handing every
    /// block that comes back in order to `sink`; afterwards hashes one
    /// ready block and hands on whatever else has come back.
    pub(super) fn submit(
        &mut self,
        frame_number: u64,
        samples: u32,
        channels: Vec<Vec<i32>>,
        sink: &mut dyn FnMut(Done) -> Result<()>,
    ) -> Result<()> {
        debug_assert_eq!(frame_number, self.end());
        self.make_room(sink)?;
        let job = Job {
            frame_number,
            samples,
            channels,
        };
        self.jobs
            .as_ref()
            .and_then(|tx| tx.send(job).ok())
            .ok_or_else(worker_gone)?;
        self.in_flight += 1;
        self.md5.hash_ready(1);
        self.poll(sink)
    }

    /// Hand every block that has come back, in order, to `sink` without
    /// waiting.
    pub(super) fn poll(&mut self, sink: &mut dyn FnMut(Done) -> Result<()>) -> Result<()> {
        while let Some(done) = self.next_done(false)? {
            sink(done)?;
        }
        Ok(())
    }

    /// Wait for every submitted block, handing each to `sink` in order.
    /// Blocks after one `sink` rejects are still waited for (and dropped),
    /// so the digest stays whole; the first error is returned.
    pub(super) fn drain(&mut self, sink: &mut dyn FnMut(Done) -> Result<()>) -> Result<()> {
        let mut first_err = None;
        while self.in_flight > 0 {
            match self.next_done(true) {
                Ok(Some(done)) => {
                    if first_err.is_none()
                        && let Err(e) = sink(done)
                    {
                        first_err = Some(e);
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Stop the workers and return the digest, after hashing whatever is
    /// still queued. Every submitted block must have been handed back
    /// ([`drain`](Self::drain)): a worker deposits its block with the
    /// digest before handing it back, so all of them are deposited by then.
    pub(super) fn finish(mut self) -> Md5 {
        debug_assert_eq!(self.in_flight, 0);
        self.stop();
        self.md5.hash_ready(usize::MAX);
        debug_assert_eq!(self.md5.next(), self.end());
        let mut md5 = self.md5.md5.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *md5)
    }

    /// Wait until another block may be submitted: fewer than `window`
    /// blocks with the workers and fewer than `window` not yet hashed.
    fn make_room(&mut self, sink: &mut dyn FnMut(Done) -> Result<()>) -> Result<()> {
        loop {
            self.poll(sink)?;
            let unhashed = self.end() - self.md5.next();
            if self.in_flight < self.window && unhashed < self.window as u64 {
                return Ok(());
            }
            if self.md5.hash_ready(usize::MAX) > 0 {
                continue;
            }
            if self.in_flight >= self.window {
                // Every worker is busy (or its result is parked behind an
                // earlier block): wait for the next completion.
                if let Some(done) = self.next_done(true)? {
                    sink(done)?;
                }
            } else {
                // The digest is behind: another thread is hashing, or the
                // block it needs is still being coded.
                self.md5.wait_progress(self.md5.next());
            }
        }
    }

    /// The next block in stream order, if it has been coded. With `wait`,
    /// block until it is (returning `None` only when nothing is in
    /// flight). Its sample planes are kept for reuse.
    fn next_done(&mut self, wait: bool) -> Result<Option<Done>> {
        loop {
            if let Some(Some(_)) = self.parked.front() {
                let mut done = self.parked.pop_front().flatten().expect("checked");
                self.next += 1;
                self.in_flight -= 1;
                if self.spare.len() < self.window {
                    self.spare.push(std::mem::take(&mut done.channels));
                }
                return Ok(Some(done));
            }
            if self.in_flight == 0 {
                return Ok(None);
            }
            let done = if wait {
                self.done.recv().map_err(|_| worker_gone())?
            } else {
                match self.done.try_recv() {
                    Ok(d) => d,
                    Err(TryRecvError::Empty) => return Ok(None),
                    Err(TryRecvError::Disconnected) => return Err(worker_gone()),
                }
            };
            let slot = (done.frame_number - self.next) as usize;
            if self.parked.len() <= slot {
                self.parked.resize_with(slot + 1, || None);
            }
            self.parked[slot] = Some(done);
        }
    }

    fn stop(&mut self) {
        // Closing the job channel ends every worker's receive loop.
        self.jobs = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // Blocks still queued belong to an encoder that is going away
        // (dropped mid-stream, or after an error): don't code them.
        self.cancel.store(true, Ordering::Relaxed);
        self.stop();
    }
}

fn worker_gone() -> Error {
    Error::other("FLAC encoder: a frame worker thread exited unexpectedly")
}

/// One worker: pull blocks, code them with this worker's own scratch,
/// optionally verify them, deposit them with the digest and send them
/// back, until the job channel closes.
fn worker(
    jobs: &Mutex<Receiver<Job>>,
    done: &Sender<Done>,
    md5: &Md5Stage,
    cancel: &AtomicBool,
    options: &FlacEncoderOptions,
    shape: FrameShape,
    verify: bool,
) {
    let mut scratch = build_scratch(options);
    loop {
        let job = {
            let rx = jobs.lock().unwrap_or_else(|e| e.into_inner());
            match rx.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let coded = catch_unwind(AssertUnwindSafe(|| {
            let frame = encode_frame(
                job.frame_number,
                job.samples,
                shape.sample_rate,
                shape.bps,
                &job.channels,
                &mut scratch,
            )?;
            if verify {
                verify_encoded_frame(&shape, job.frame_number, &frame, &job.channels)?;
            }
            Ok(frame)
        }));
        let result = coded.unwrap_or_else(|_| {
            // The scratch may be half-updated; start the next block from
            // a fresh one.
            scratch = build_scratch(options);
            Err(Error::other(format!(
                "FLAC encoder: worker panicked coding frame {}",
                job.frame_number
            )))
        });
        // Deposit before handing the block back: once the caller holds
        // every block, the digest has every block too.
        let mut bytes = md5.buffer();
        md5_bytes(&job.channels, shape.bps, &mut bytes);
        md5.deposit(job.frame_number, bytes);
        let out = Done {
            frame_number: job.frame_number,
            samples: job.samples,
            result,
            channels: job.channels,
        };
        if done.send(out).is_err() {
            return;
        }
    }
}

/// Serialise a block for the STREAMINFO MD5 (RFC 9639 §8.2): samples
/// interleaved, each little-endian in the sample width rounded up to whole
/// bytes, two's complement — the same bytes the serial path's `feed_md5`
/// hashes, built here from the per-channel planes.
fn md5_bytes(planes: &[Vec<i32>], bps: u8, out: &mut Vec<u8>) {
    let width = (bps as usize).div_ceil(8);
    let n = planes.first().map_or(0, Vec::len);
    out.clear();
    out.resize(n * planes.len() * width, 0);
    match (planes, width) {
        ([l, r], 2) => {
            for ((o, &a), &b) in out.as_chunks_mut::<4>().0.iter_mut().zip(l).zip(r) {
                let (a, b) = ((a as i16).to_le_bytes(), (b as i16).to_le_bytes());
                *o = [a[0], a[1], b[0], b[1]];
            }
        }
        _ => {
            let stride = planes.len() * width;
            for (c, plane) in planes.iter().enumerate() {
                for (i, &s) in plane.iter().enumerate() {
                    let at = i * stride + c * width;
                    out[at..at + width].copy_from_slice(&s.to_le_bytes()[..width]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_bytes_match_the_interleaved_serialisation() {
        // Same layout `feed_md5` produces from interleaved samples, for
        // every sample width and for the stereo 16-bit fast path.
        for (bps, n_ch) in [(8u8, 1usize), (16, 2), (16, 3), (24, 2), (32, 2), (12, 6)] {
            let lim = 1i64 << (bps - 1);
            let planes: Vec<Vec<i32>> = (0..n_ch)
                .map(|c| {
                    (0..50i64)
                        .map(|i| ((i * 7919 + c as i64 * 104_729) % (2 * lim) - lim) as i32)
                        .collect()
                })
                .collect();
            let interleaved: Vec<i32> = (0..50)
                .flat_map(|i| planes.iter().map(move |p| p[i]))
                .collect();
            let mut want = Md5::new();
            super::super::feed_md5(&mut want, &mut Vec::new(), bps, &interleaved);
            let mut bytes = Vec::new();
            md5_bytes(&planes, bps, &mut bytes);
            let mut got = Md5::new();
            got.update(&bytes);
            assert_eq!(got.finalize(), want.finalize(), "{bps} bps, {n_ch} ch");
        }
    }

    #[test]
    fn md5_stage_hashes_out_of_order_deposits_in_order() {
        let blocks: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i; 3 + i as usize]).collect();
        let mut want = Md5::new();
        for b in &blocks {
            want.update(b);
        }
        let stage = Md5Stage::new(Md5::new(), 7, 3);
        for i in [
            2usize, 0, 5, 1, 3, 4, 9, 8, 7, 6, 19, 10, 11, 12, 13, 14, 15, 16, 17, 18,
        ] {
            stage.deposit(7 + i as u64, blocks[i].clone());
            if i % 4 == 0 {
                stage.hash_ready(1);
            }
        }
        stage.hash_ready(usize::MAX);
        assert_eq!(stage.next(), 27);
        let md5 = std::mem::take(&mut *stage.md5.lock().unwrap());
        assert_eq!(md5.finalize(), want.finalize());
    }
}
