//! Extracting or testing one ZIP archive. CPU threads and the GPU share the Deflate entries
//! (nvCOMP takes batches of the smallest, the CPU threads the largest, until they meet; in auto
//! mode nvCOMP starts only if the CPU threads, timed on their first entries, would be slower
//! alone), writer threads save nvCOMP's results, gpuinflate decodes the largest entries one at a
//! time when that beats a CPU thread, and 7-Zip takes the entries none of them reads.

use crate::gpu::{self, Api, Decoder, MAX_BATCH_ENTRIES};
use crate::inflate;
use crate::sevenzip;
use crate::zip::{self, DEFLATED, Entry, Route, STORED};
use crate::{Mode, Opts};
use crossbeam_channel::{Sender, bounded, unbounded};
use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;
const WRITE_CHUNK: usize = 64; // GPU results go to the writer threads this many files at a time
const STREAM_ABOVE: u64 = 512 * MIB; // larger entries are decoded in pieces, not in one buffer
// Rough speeds for giving large entries to gpuinflate: an entry goes to it when the GPU, after
// starting up and finishing the entries it already has, would be done before a CPU thread.
const BIG_MIN: u64 = 32 * MIB;
const BIG_MIN_GPU_MODE: u64 = MIB; // gpu mode: nvCOMP keeps only the small ones
const CPU_RATE: f64 = 0.6e9; // bytes/s, one thread on a large entry of text or a ROM image
const GPU_START: f64 = 0.15; // s, the driver and gpuinflate
// gpuinflate on one stream (RTX 5080): text and the like at 3.5 GB/s, but streams that hardly
// compress (ROM images, data compressed before) only at about 1 GB/s; in between, by the ratio
const GI_RATE: f64 = 3.5e9; // bytes/s, compressed to half its size or better
const GI_RATE_FLAT: f64 = 1.0e9; // bytes/s, compressed to 0.9 of its size or worse
const GI_STREAM: f64 = 0.04; // s, each stream's own start-up
// auto mode starts nvCOMP only when it and the CPU threads together would finish sooner than the
// CPU threads alone, start-up included. The CPU threads' speed is measured on the first entries
// they decode; the GPU's figures are rough (RTX 5080).
const NVCOMP_RATE: f64 = 2.5e9; // bytes/s, batches of small entries
const NVCOMP_START: f64 = 0.15; // s, the driver, nvCOMP and the pinned buffers
const GPU_EXIT: f64 = 0.05; // s, what a loaded CUDA driver adds when the program ends
const MEASURE: Duration = Duration::from_millis(5); // CPU decode time to see before deciding
const MEASURE_MAX: Duration = Duration::from_millis(20); // decide anyway after this long
const CPU_RATE_BUSY: f64 = 0.5e9; // bytes/s, one thread while every thread decodes, if not measured

/// Rough time gpuinflate takes for one entry.
fn gpuinflate_seconds(size: u64, csize: u64) -> f64 {
    let ratio = csize as f64 / size.max(1) as f64;
    let rate = GI_RATE - (GI_RATE - GI_RATE_FLAT) * ((ratio - 0.5) / 0.4).clamp(0.0, 1.0);
    GI_STREAM + size as f64 / rate
}

/// auto mode: the largest Deflate entries for gpuinflate, largest first, taken out of `deflate`
/// (sorted by size). Each goes to it if the GPU, started and done with the entries it already
/// has, would finish it before a CPU thread could. A CPU thread decodes one entry at a time, so
/// with more large entries than threads the later ones wait their turn.
fn pick_for_gpuinflate(entries: &[Entry], deflate: &mut Vec<usize>, threads: usize) -> VecDeque<usize> {
    let (mut big, mut keep) = (VecDeque::new(), Vec::new());
    let mut gpu_done = GPU_START + GPU_EXIT;
    while deflate.last().is_some_and(|&i| entries[i].size >= BIG_MIN) {
        let i = deflate.pop().unwrap();
        let e = &entries[i];
        let gpu = gpu_done + gpuinflate_seconds(e.size, e.csize);
        let cpu = (keep.len() / threads + 1) as f64 * e.size as f64 / CPU_RATE;
        if gpu < cpu {
            gpu_done = gpu;
            big.push_back(i);
        } else {
            keep.push(i);
        }
    }
    deflate.extend(keep.into_iter().rev()); // still smallest first
    big
}

/// auto mode: whether nvCOMP joining the CPU threads would finish sooner than they would alone.
/// `small`: bytes nvCOMP could take, `rest`: bytes only the CPU takes, `cpu`: bytes/s of all the
/// CPU threads together.
fn nvcomp_pays(small: f64, rest: f64, cpu: f64) -> bool {
    let all = small + rest;
    // from the moment it is running the GPU takes its share at NVCOMP_RATE, but no more than the
    // small entries; the CPU threads take everything else
    let shared = (rest / cpu).max((all + NVCOMP_RATE * NVCOMP_START) / (cpu + NVCOMP_RATE));
    shared + GPU_EXIT < all / cpu
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Gpu = 0,
    Cpu = 1,
    SevenZip = 2,
}

#[derive(Default, Clone, Copy)]
pub struct Count {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Default)]
pub struct GpuInfo {
    pub device: Option<String>,
    pub batches: u64,
    pub busy_seconds: f64,
    pub startup_seconds: Option<f64>,
}

pub struct Outcome {
    pub seconds: f64,
    pub counts: [Count; 3],
    pub failed: Vec<(String, String)>,
    pub notes: Vec<String>,
    pub info: Vec<String>,
    pub gpu: Option<GpuInfo>,
    pub retried: usize,
    /// gzip files: (compressed, decompressed) bytes
    pub stream: Option<(u64, u64)>,
}

/// The driver and nvCOMP, loaded once per process on first use.
pub fn gpu_api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(Api::load).as_ref().map_err(|e| e.clone())
}

/// A pointer into a GPU slot's pinned output buffer, handed to writer threads. The GPU
/// thread does not reuse the slot until every writer has reported the batch done.
struct SlotPtr(*const u8);
unsafe impl Send for SlotPtr {}
unsafe impl Sync for SlotPtr {}

struct GpuBatch {
    slot: usize,
    base: SlotPtr,
    entries: Vec<usize>,
    offsets: Vec<usize>,
    ok: Vec<bool>,
    remaining: AtomicUsize,
    free: Sender<usize>,
}

/// What gpuinflate made of one entry
enum Inflated {
    Done,
    /// the output fails the size or CRC check
    Wrong,
    /// gpuinflate found the data invalid: the CPU will say how
    Refused,
    /// a GPU error: gpuinflate is not used again in this run
    GpuError(String),
}

struct WriteJob {
    batch: Arc<GpuBatch>,
    range: std::ops::Range<usize>,
}

pub struct Run<'a> {
    opts: &'a Opts,
    archive: &'a str,
    data: &'a [u8],
    entries: &'a [Entry],
    out_root: Option<PathBuf>,
    pending: Mutex<VecDeque<usize>>, // Deflate entries by size: the GPU eats from the front
    big: Mutex<VecDeque<usize>>,     // the largest Deflate entries, for gpuinflate, largest first
    cpu_queue: Mutex<VecDeque<usize>>,
    counts: Mutex<[Count; 3]>,
    failed: Mutex<Vec<(String, String)>>,
    retry: Mutex<Vec<usize>>, // nvCOMP's failures, redone once the GPU is done
    redone: AtomicUsize,      // entries the GPU failed on, redone on the CPU
    wrong: AtomicUsize,       // ... of them because the output failed the CRC check
    refused: AtomicUsize,     // ... because the GPU decoder found the data invalid
    notes: Mutex<Vec<String>>,
    info: Mutex<Vec<String>>,
    gpu: Mutex<GpuInfo>,
    gpu_max_entry: u64,
    slot_bytes: usize,
    use_gpu: bool,
    steal: bool, // in gpu mode the CPU only takes what the GPU cannot
    threads: usize,
    cpu_bytes: AtomicU64, // Deflate entries the CPU threads decoded, and the time they took
    cpu_nanos: AtomicU64,
}

impl<'a> Run<'a> {
    pub fn new(opts: &'a Opts, archive: &'a str, data: &'a [u8], entries: &'a [Entry], out_root: Option<PathBuf>) -> Self {
        let mut deflate: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].route == Route::Deflate).collect();
        deflate.sort_by_key(|&i| entries[i].size);
        let threads = opts.threads.unwrap_or_else(crate::cpu_threads).max(1);
        let mut big = VecDeque::new();
        let mut info = Vec::new();
        if opts.mode == Mode::Gpu && inflate::available() {
            while deflate.last().is_some_and(|&i| entries[i].size >= BIG_MIN_GPU_MODE) {
                big.push_back(deflate.pop().unwrap());
            }
        } else if opts.mode == Mode::Auto && inflate::available() {
            let large = deflate.iter().filter(|&&i| entries[i].size >= BIG_MIN).count();
            big = pick_for_gpuinflate(entries, &mut deflate, threads);
            if large > big.len() {
                info.push(format!(
                    "gpuinflate takes {} of the {large} entries of {} MB or more: the CPU threads would finish the others first",
                    big.len(),
                    BIG_MIN / MIB
                ));
            }
        }
        let gpu_max_entry = if opts.mode == Mode::Auto { opts.gpu_max_entry_kb * 1024 } else { u64::MAX };
        let eligible: u64 = deflate
            .iter()
            .filter(|&&i| entries[i].size <= gpu_max_entry)
            .map(|&i| gpu::align(entries[i].size as usize) as u64)
            .sum();
        let slot_bytes = (opts.batch_mb * MIB).min(gpu::align(eligible as usize) as u64).max(MIB) as usize;
        // nvCOMP: only for what gpuinflate did not take
        let use_gpu = (opts.mode == Mode::Gpu && !deflate.is_empty()) || (opts.mode == Mode::Auto && eligible >= opts.gpu_min_mb * MIB);
        if opts.mode == Mode::Auto && !use_gpu && !deflate.is_empty() {
            info.push(format!(
                "GPU skipped: only {} of small Deflate data, the CPU is faster below {} MB",
                crate::human(eligible),
                opts.gpu_min_mb
            ));
        }
        Run {
            opts,
            archive,
            data,
            entries,
            out_root,
            cpu_queue: Mutex::new((0..entries.len()).filter(|&i| entries[i].route == Route::Cpu).collect()),
            pending: Mutex::new(deflate.into()),
            big: Mutex::new(big),
            counts: Mutex::new([Count::default(); 3]),
            failed: Mutex::new(Vec::new()),
            retry: Mutex::new(Vec::new()),
            redone: AtomicUsize::new(0),
            wrong: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            notes: Mutex::new(Vec::new()),
            info: Mutex::new(info),
            gpu: Mutex::new(GpuInfo::default()),
            gpu_max_entry,
            slot_bytes,
            use_gpu,
            steal: opts.mode != Mode::Gpu,
            threads,
            cpu_bytes: AtomicU64::new(0),
            cpu_nanos: AtomicU64::new(0),
        }
    }

    fn fail(&self, e: &Entry, reason: impl Into<String>) {
        self.failed.lock().unwrap().push((e.name.clone(), reason.into()));
    }

    fn note(&self, s: String) {
        self.notes.lock().unwrap().push(s);
    }

    fn add(&self, via: Via, e: &Entry) {
        let mut c = self.counts.lock().unwrap();
        c[via as usize].files += 1;
        c[via as usize].bytes += e.size;
    }

    fn target(&self, e: &Entry) -> Option<PathBuf> {
        self.out_root.as_ref().map(|r| r.join(e.path.as_ref().unwrap()))
    }

    /// Check size and CRC, then write the file. Ok(false) means the data is wrong.
    fn deliver(&self, e: &Entry, data: &[u8], via: Via) -> Result<bool, String> {
        if data.len() as u64 != e.size || crc32fast::hash(data) != e.crc {
            return Ok(false);
        }
        if let Some(path) = self.target(e) {
            let mut f = File::create(&path).map_err(|x| x.to_string())?;
            f.write_all(data).map_err(|x| x.to_string())?;
            if let Some(t) = zip::dos_time(e.dos_date, e.dos_time) {
                let _ = f.set_modified(t);
            }
        }
        self.add(via, e);
        Ok(true)
    }

    /// Very large Deflate entries: decode and write in pieces instead of one buffer.
    fn deliver_streamed(&self, e: &Entry) -> Result<bool, String> {
        let src = &self.data[e.data_offset as usize..(e.data_offset + e.csize) as usize];
        let mut dec = flate2::read::DeflateDecoder::new(src);
        let mut out = match self.target(e) {
            Some(p) => Some(File::create(&p).map_err(|x| x.to_string())?),
            None => None,
        };
        let (mut crc, mut total, mut buf) = (crc32fast::Hasher::new(), 0u64, vec![0u8; 4 << 20]);
        loop {
            let n = dec.read(&mut buf).map_err(|x| x.to_string())?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            total += n as u64;
            if let Some(f) = out.as_mut() {
                f.write_all(&buf[..n]).map_err(|x| x.to_string())?;
            }
        }
        if total != e.size || crc.finalize() != e.crc {
            return Ok(false);
        }
        if let (Some(f), Some(t)) = (out.as_ref(), zip::dos_time(e.dos_date, e.dos_time)) {
            let _ = f.set_modified(t);
        }
        self.add(Via::Cpu, e);
        Ok(true)
    }

    /// A large Deflate entry on gpuinflate, written as it comes.
    fn deliver_inflate(&self, g: &inflate::Gpu, e: &Entry) -> Result<Inflated, String> {
        let src = &self.data[e.data_offset as usize..(e.data_offset + e.csize) as usize];
        let mut out = match self.target(e) {
            Some(p) => Some(File::create(&p).map_err(|x| x.to_string())?),
            None => None,
        };
        let (mut crc, mut total, mut write_err) = (crc32fast::Hasher::new(), 0u64, None);
        let r = g.inflate(src, e.size, |c| {
            crc.update(c);
            total += c.len() as u64;
            match out.as_mut().map(|f| f.write_all(c)) {
                Some(Err(x)) => {
                    write_err = Some(x.to_string());
                    false
                }
                _ => total <= e.size, // more than the directory says: stop, the CPU will tell
            }
        });
        if let Some(x) = write_err {
            return Err(x);
        }
        match r {
            Err(inflate::Error::Gpu(x)) => return Ok(Inflated::GpuError(x)),
            Err(inflate::Error::Data(_) | inflate::Error::Truncated(_)) => return Ok(Inflated::Refused),
            Err(inflate::Error::Aborted) => return Ok(Inflated::Wrong), // more than the directory says
            Ok(_) if total != e.size || crc.finalize() != e.crc => return Ok(Inflated::Wrong),
            Ok(_) => {}
        }
        if let (Some(f), Some(t)) = (out.as_ref(), zip::dos_time(e.dos_date, e.dos_time)) {
            let _ = f.set_modified(t);
        }
        self.add(Via::Gpu, e);
        Ok(Inflated::Done)
    }

    /// Entries for the CPU threads to take next, ahead of everything else.
    fn to_cpu_now(&self, todo: impl IntoIterator<Item = usize>) {
        let mut q = self.cpu_queue.lock().unwrap();
        for i in todo {
            q.push_front(i);
        }
    }

    fn big_thread(&self) {
        let t0 = Instant::now();
        let g = match inflate::get() {
            Ok(g) => g,
            Err(x) => {
                let msg = format!("gpuinflate unavailable ({x}); large entries used the CPU");
                if self.opts.mode == Mode::Gpu { self.note(msg) } else { self.info.lock().unwrap().push(msg) }
                let rest: Vec<usize> = self.big.lock().unwrap().drain(..).collect();
                self.to_cpu_now(rest.into_iter().rev());
                return;
            }
        };
        {
            let mut info = self.gpu.lock().unwrap();
            info.device.get_or_insert_with(|| g.device.clone());
            info.startup_seconds.get_or_insert(t0.elapsed().as_secs_f64());
        }
        let (mut n, mut bytes, t) = (0u64, 0u64, Instant::now());
        loop {
            let Some(i) = self.big.lock().unwrap().pop_front() else { break };
            let e = &self.entries[i];
            match self.deliver_inflate(g, e) {
                Ok(Inflated::Done) => {
                    n += 1;
                    bytes += e.size;
                    self.gpu.lock().unwrap().batches += 1;
                    continue;
                }
                Ok(Inflated::Wrong) => self.wrong.fetch_add(1, Ordering::Relaxed),
                Ok(Inflated::Refused) => self.refused.fetch_add(1, Ordering::Relaxed),
                Ok(Inflated::GpuError(x)) => {
                    // the GPU may be in a bad state now: the CPU threads take the rest too
                    let rest: Vec<usize> = self.big.lock().unwrap().drain(..).collect();
                    self.note(format!(
                        "gpuinflate failed on {} ({x}); it and the {} large entries after it were decoded on the CPU",
                        e.name,
                        rest.len()
                    ));
                    self.to_cpu_now(rest.into_iter().rev());
                    0
                }
                Err(x) => {
                    self.fail(e, x);
                    continue;
                }
            };
            self.redone.fetch_add(1, Ordering::Relaxed);
            self.to_cpu_now([i]);
        }
        if n > 0 {
            let secs = t.elapsed().as_secs_f64();
            self.gpu.lock().unwrap().busy_seconds += secs;
            self.info.lock().unwrap().push(format!(
                "gpuinflate: {n} large entries, {} in {secs:.2} s ({:.2} GB/s)",
                crate::human(bytes),
                bytes as f64 / secs / 1e9
            ));
        }
    }

    fn run_cpu(&self, i: usize, d: &mut libdeflater::Decompressor) {
        let e = &self.entries[i];
        let start = e.data_offset as usize;
        let result = match e.method {
            STORED => self.deliver(e, &self.data[start..start + e.size as usize], Via::Cpu),
            DEFLATED if e.size > STREAM_ABOVE => self.deliver_streamed(e),
            DEFLATED => {
                let t = Instant::now();
                let mut out = vec![0u8; e.size as usize];
                match d.deflate_decompress(&self.data[start..start + e.csize as usize], &mut out) {
                    Ok(n) if n == out.len() => {
                        self.cpu_nanos.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        self.cpu_bytes.fetch_add(e.size, Ordering::Relaxed);
                        self.deliver(e, &out, Via::Cpu)
                    }
                    Ok(_) => Ok(false),
                    Err(x) => Err(format!("Deflate error: {x:?}")),
                }
            }
            m => Err(format!("method {m} is not read here")),
        };
        match result {
            Ok(true) => {}
            Ok(false) => self.fail(e, "CRC mismatch"),
            Err(x) => self.fail(e, x),
        }
    }

    fn take_cpu(&self) -> Option<usize> {
        if let Some(i) = self.cpu_queue.lock().unwrap().pop_front() {
            return Some(i);
        }
        if self.steal { self.pending.lock().unwrap().pop_back() } else { None }
    }

    fn cpu_worker(&self) {
        let mut d = libdeflater::Decompressor::new();
        while let Some(i) = self.take_cpu() {
            self.run_cpu(i, &mut d);
        }
    }

    fn take_gpu_batch(&self) -> Vec<usize> {
        let mut p = self.pending.lock().unwrap();
        let (mut batch, mut used_u, mut used_c) = (Vec::new(), 0usize, 0usize);
        while let Some(&i) = p.front() {
            let e = &self.entries[i];
            let (u, c) = (gpu::align(e.size as usize), gpu::align(e.csize as usize));
            if batch.len() == MAX_BATCH_ENTRIES
                || e.size > self.gpu_max_entry
                || used_u + u > self.slot_bytes
                || used_c + c > self.slot_bytes
            {
                break;
            }
            batch.push(p.pop_front().unwrap());
            used_u += u;
            used_c += c;
        }
        batch
    }

    /// auto mode: watch the CPU threads for a moment, then say whether starting nvCOMP pays off.
    fn nvcomp_worth_starting(&self) -> bool {
        let t0 = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(1));
            if self.pending.lock().unwrap().is_empty() {
                self.info.lock().unwrap().push("GPU not started: the CPU threads took every entry first".into());
                return false;
            }
            if Duration::from_nanos(self.cpu_nanos.load(Ordering::Relaxed)) >= MEASURE || t0.elapsed() >= MEASURE_MAX {
                break;
            }
        }
        let (bytes, nanos) = (self.cpu_bytes.load(Ordering::Relaxed), self.cpu_nanos.load(Ordering::Relaxed));
        // every thread still on its first entry (large ones come first): assume the usual speed
        let (per_thread, how) = if bytes == 0 { (CPU_RATE_BUSY, "about ") } else { (bytes as f64 * 1e9 / nanos as f64, "") };
        let cpu = per_thread * self.threads as f64;
        let (mut small, mut rest) = (0u64, 0u64);
        for &i in self.pending.lock().unwrap().iter() {
            let size = self.entries[i].size;
            if size <= self.gpu_max_entry { small += size } else { rest += size }
        }
        let go = nvcomp_pays(small as f64, rest as f64, cpu);
        self.info.lock().unwrap().push(format!(
            "GPU {}: the CPU threads decode at {how}{:.1} GB/s and would need {:.2} s for the {} left",
            if go { "started" } else { "not started, the CPU is faster" },
            cpu / 1e9,
            (small + rest) as f64 / cpu,
            crate::human(small + rest)
        ));
        go
    }

    fn gpu_thread(&self, jobs: Sender<WriteJob>) {
        if self.steal && !self.nvcomp_worth_starting() {
            return;
        }
        let t0 = Instant::now();
        let api = match gpu_api() {
            Ok(a) => a,
            Err(x) => {
                if self.opts.mode == Mode::Gpu {
                    self.note(format!("GPU unavailable ({x}); used the CPU instead"));
                } else {
                    self.info.lock().unwrap().push(format!("GPU unavailable ({x}); used the CPU"));
                }
                return;
            }
        };
        if self.steal && self.pending.lock().unwrap().is_empty() {
            self.info.lock().unwrap().push("GPU not used: the CPU took every entry before the GPU was ready".into());
            return;
        }
        let max_entry = (self.gpu_max_entry as usize).min(self.slot_bytes);
        let mut dec = match Decoder::new(api, self.slot_bytes, max_entry, 2) {
            Ok(d) => d,
            Err(x) => {
                self.note(format!("GPU unavailable ({x}); used the CPU instead"));
                return;
            }
        };
        {
            let mut g = self.gpu.lock().unwrap();
            g.device = Some(api.device.clone());
            g.startup_seconds = Some(t0.elapsed().as_secs_f64());
        }
        let (free_tx, free_rx) = bounded(2);
        free_tx.send(0).unwrap();
        free_tx.send(1).unwrap();
        loop {
            let batch = self.take_gpu_batch();
            if batch.is_empty() {
                break;
            }
            let slot = free_rx.recv().unwrap();
            let spans: Vec<(u64, u64, u64)> = batch
                .iter()
                .map(|&i| (self.entries[i].data_offset, self.entries[i].csize, self.entries[i].size))
                .collect();
            let t = Instant::now();
            match dec.decode(slot, self.data, &spans) {
                Ok(res) => {
                    {
                        let mut g = self.gpu.lock().unwrap();
                        g.batches += 1;
                        g.busy_seconds += t.elapsed().as_secs_f64();
                    }
                    let n = batch.len();
                    let chunks = n.div_ceil(WRITE_CHUNK);
                    let b = Arc::new(GpuBatch {
                        slot,
                        base: SlotPtr(dec.slots[slot].h_out),
                        entries: batch,
                        offsets: res.out_offsets,
                        ok: res.ok,
                        remaining: AtomicUsize::new(chunks),
                        free: free_tx.clone(),
                    });
                    for c in 0..chunks {
                        let range = c * WRITE_CHUNK..((c + 1) * WRITE_CHUNK).min(n);
                        jobs.send(WriteJob { batch: b.clone(), range }).unwrap();
                    }
                }
                Err(x) => {
                    self.note(format!("GPU batch failed ({x}); redone on the CPU"));
                    self.redone.fetch_add(batch.len(), Ordering::Relaxed);
                    self.retry.lock().unwrap().extend(batch);
                    free_tx.send(slot).unwrap();
                }
            }
        }
        // the writers still read the slots; wait for both before freeing them
        for _ in 0..2 {
            free_rx.recv().unwrap();
        }
        if self.gpu.lock().unwrap().batches == 0 {
            self.info.lock().unwrap().push("GPU not used: the CPU took every entry before the GPU was ready".into());
        }
    }

    fn writer(&self, jobs: crossbeam_channel::Receiver<WriteJob>) {
        for job in jobs.iter() {
            let b = &job.batch;
            for k in job.range.clone() {
                let i = b.entries[k];
                let e = &self.entries[i];
                if !b.ok[k] {
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    self.redone.fetch_add(1, Ordering::Relaxed);
                    self.retry.lock().unwrap().push(i);
                    continue;
                }
                let data = unsafe { std::slice::from_raw_parts(b.base.0.add(b.offsets[k]), e.size as usize) };
                match self.deliver(e, data, Via::Gpu) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.wrong.fetch_add(1, Ordering::Relaxed);
                        self.redone.fetch_add(1, Ordering::Relaxed);
                        self.retry.lock().unwrap().push(i);
                    }
                    Err(x) => self.fail(e, x),
                }
            }
            if b.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                let _ = b.free.send(b.slot);
            }
        }
    }

    // A line every 250 ms until stop, which run() sets and then wakes this thread for: a sleep
    // there held every `x` and `t` in a terminal up to 250 ms after its work was done (measured:
    // 259 ms against 8 ms with stderr not a terminal, for a ZIP of one entry)
    fn progress(&self, stop: &AtomicBool) {
        let total: u64 = self.entries.iter().filter(|e| matches!(e.route, Route::Deflate | Route::Cpu)).map(|e| e.size).sum();
        let t0 = Instant::now();
        let mut err = std::io::stderr();
        let mut next = t0;
        loop {
            next += Duration::from_millis(250);
            // parked until the next line is due or run() wakes it (it may also wake for nothing)
            while !stop.load(Ordering::Acquire) && Instant::now() < next {
                std::thread::park_timeout(next.saturating_duration_since(Instant::now()));
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            let c = *self.counts.lock().unwrap();
            let done = c[0].bytes + c[1].bytes;
            let rate = done as f64 / t0.elapsed().as_secs_f64() / 1e9;
            let _ = write!(
                err,
                "\r  {:5.1}% · {} files · {} · {:.2} GB/s · GPU {} · CPU {}   ",
                100.0 * done as f64 / total.max(1) as f64,
                crate::commas(c[0].files + c[1].files),
                crate::human(done),
                rate,
                crate::commas(c[0].files),
                crate::commas(c[1].files)
            );
        }
        let _ = write!(err, "\r{}\r", " ".repeat(96));
    }

    fn run_7z_entries(&self) {
        let todo: Vec<&Entry> = self.entries.iter().filter(|e| e.route == Route::SevenZip).collect();
        if todo.is_empty() {
            return;
        }
        let Some(exe) = sevenzip::find(self.opts.sevenzip.as_deref()) else {
            for e in todo {
                self.fail(e, format!("{} or encrypted entry needs 7-Zip, not found", zip::method_name(e.method)));
            }
            return;
        };
        let names: Vec<&str> = todo.iter().map(|e| e.name.as_str()).collect();
        let rc = sevenzip::run_entries(&exe, self.archive, &names, self.out_root.as_deref(), self.opts.password.as_deref(), self.opts.verbose && !self.opts.json);
        for e in todo {
            if rc == 0 {
                self.add(Via::SevenZip, e);
            } else {
                self.fail(e, format!("7-Zip exit code {rc}"));
            }
        }
    }

    pub fn run(self) -> Outcome {
        let t0 = Instant::now();
        if let Some(root) = &self.out_root {
            let mut dirs = BTreeSet::new();
            dirs.insert(root.clone());
            for e in self.entries {
                match e.route {
                    Route::Dir => {
                        dirs.insert(root.join(e.path.as_ref().unwrap()));
                    }
                    Route::Deflate | Route::Cpu | Route::SevenZip => {
                        if let Some(parent) = root.join(e.path.as_ref().unwrap()).parent() {
                            dirs.insert(parent.to_path_buf());
                        }
                    }
                    _ => {}
                }
            }
            for d in dirs {
                if let Err(x) = std::fs::create_dir_all(&d) {
                    self.note(format!("could not create {}: {x}", d.display()));
                }
            }
        }
        let threads = self.threads;
        let io_threads = self.opts.io_threads.unwrap_or_else(|| crate::cpu_threads().min(32)).max(1);
        let stop = &AtomicBool::new(false);
        let show_progress = !(self.opts.quiet || self.opts.json) && std::io::stderr().is_terminal();
        let this = &self;
        std::thread::scope(|sc| {
            let progress = show_progress.then(|| sc.spawn(move || this.progress(stop)));
            let (jobs_tx, jobs_rx) = unbounded::<WriteJob>();
            let writers: Vec<_> = if this.use_gpu {
                (0..io_threads)
                    .map(|_| {
                        let rx = jobs_rx.clone();
                        sc.spawn(move || this.writer(rx))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            drop(jobs_rx);
            let gpu = this.use_gpu.then(|| {
                let tx = jobs_tx.clone();
                sc.spawn(move || this.gpu_thread(tx))
            });
            drop(jobs_tx);
            let big = (!this.big.lock().unwrap().is_empty()).then(|| sc.spawn(move || this.big_thread()));
            let cpus: Vec<_> = (0..threads).map(|_| sc.spawn(move || this.cpu_worker())).collect();
            for h in cpus {
                h.join().unwrap();
            }
            if let Some(h) = gpu {
                h.join().unwrap();
            }
            if let Some(h) = big {
                h.join().unwrap();
            }
            for h in writers {
                h.join().unwrap();
            }
            // what the GPU could not take or got wrong (should be nothing) is redone on the CPU
            let (wrong, refused) = (this.wrong.load(Ordering::Relaxed), this.refused.load(Ordering::Relaxed));
            if wrong > 0 {
                this.note(format!("{wrong} GPU results failed the CRC check and were redone on the CPU"));
            }
            if refused > 0 {
                this.note(format!("the GPU found {refused} entries invalid; they were decoded again on the CPU"));
            }
            let rest: Vec<usize> = this.pending.lock().unwrap().drain(..).collect();
            let mut q = this.cpu_queue.lock().unwrap();
            q.extend(rest);
            q.extend(this.retry.lock().unwrap().iter().copied());
            drop(q);
            let again: Vec<_> = (0..threads).map(|_| sc.spawn(move || this.cpu_worker())).collect();
            for h in again {
                h.join().unwrap();
            }
            this.run_7z_entries();
            stop.store(true, Ordering::Release);
            if let Some(h) = progress {
                h.thread().unpark();
                h.join().unwrap();
            }
        });
        for e in self.entries {
            match e.route {
                Route::Unsafe => self.fail(e, "path leaves the output folder; skipped"),
                Route::Dup => self.note(format!("duplicate path, kept the last copy: {}", e.name)),
                _ if self.out_root.is_some() && zip::device_renamed(&e.name) => self.note(format!(
                    "{} uses a name this Windows takes for a device (what is written to one is lost): written as {}",
                    e.name,
                    e.path.as_ref().unwrap().display()
                )),
                _ => {}
            }
        }
        let retried = self.redone.load(Ordering::Relaxed);
        Outcome {
            seconds: t0.elapsed().as_secs_f64(),
            counts: *self.counts.lock().unwrap(),
            failed: std::mem::take(&mut *self.failed.lock().unwrap()),
            notes: std::mem::take(&mut *self.notes.lock().unwrap()),
            info: std::mem::take(&mut *self.info.lock().unwrap()),
            gpu: (self.use_gpu || self.gpu.lock().unwrap().device.is_some())
                .then(|| std::mem::take(&mut *self.gpu.lock().unwrap())),
            retried,
            stream: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, Route, nvcomp_pays, pick_for_gpuinflate};

    fn entries(sizes: &[(u64, f64)]) -> Vec<Entry> {
        let entry = |&(size, ratio): &(u64, f64)| Entry {
            name: String::new(),
            raw_name: Vec::new(),
            path: None,
            method: 8,
            flags: 0,
            crc: 0,
            csize: (size as f64 * ratio) as u64,
            size,
            data_offset: 0,
            dos_time: 0,
            dos_date: 0,
            route: Route::Deflate,
        };
        sizes.iter().map(entry).collect()
    }

    fn picked(sizes: &[(u64, f64)], threads: usize) -> usize {
        let e = entries(sizes);
        let mut deflate: Vec<usize> = (0..e.len()).collect();
        deflate.sort_by_key(|&i| e[i].size);
        let big = pick_for_gpuinflate(&e, &mut deflate, threads);
        assert_eq!(big.len() + deflate.len(), e.len());
        assert!(deflate.windows(2).all(|w| e[w[0]].size <= e[w[1]].size));
        big.len()
    }

    #[test]
    fn gpuinflate_for_what_it_finishes_first() {
        const MB: u64 = 1 << 20;
        // one 2 GB text entry: the GPU (0.8 s) beats a CPU thread (2 s)
        assert_eq!(picked(&[(2048 * MB, 0.4)], 16), 1);
        // eight 128 MB entries, one CPU thread each: the CPU
        assert_eq!(picked(&[(128 * MB, 0.4); 8], 16), 0);
        // with 2 threads the CPU would do them four at a time per thread: the GPU takes some
        assert!(picked(&[(128 * MB, 0.4); 8], 2) >= 2);
        // nine 750 MB ROM image parts that hardly compress (about 1.6 GB/s on the GPU): it takes
        // two, a thread each does the rest; with 4 threads it takes more
        assert_eq!(picked(&[(750 * MB, 0.8); 9], 16), 2);
        assert!(picked(&[(750 * MB, 0.8); 9], 4) >= 3);
    }

    #[test]
    fn nvcomp_only_when_the_cpu_is_slow() {
        // 1 GB of small entries: 16 Zen 5 threads (6 GB/s) finish before the GPU helps, 2 do not
        assert!(!nvcomp_pays(1e9, 0.0, 6e9));
        assert!(nvcomp_pays(1e9, 0.0, 1.2e9));
        // too little the GPU could take: the CPU has the large entries to do anyway
        assert!(!nvcomp_pays(0.02e9, 2e9, 1.2e9));
    }
}
