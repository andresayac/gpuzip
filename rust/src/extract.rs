//! Extracting or testing one ZIP archive. CPU threads and the GPU share the Deflate entries
//! (nvCOMP takes batches of the smallest, the CPU threads the largest, until they meet),
//! writer threads save nvCOMP's results, gpuinflate decodes the largest entries one at a time
//! when that beats a CPU thread, and 7-Zip takes the entries none of them reads.

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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;
const WRITE_CHUNK: usize = 64; // GPU results go to the writer threads this many files at a time
const STREAM_ABOVE: u64 = 512 * MIB; // larger entries are decoded in pieces, not in one buffer
// Rough speeds for giving large entries to gpuinflate: an entry goes to it when the GPU, after
// starting up and finishing the entries it already has, would be done before one CPU thread.
const BIG_MIN: u64 = 32 * MIB;
const BIG_MIN_GPU_MODE: u64 = MIB; // gpu mode: nvCOMP keeps only the small ones
const CPU_RATE: f64 = 1.0e9; // bytes/s, one thread with libdeflate
const GPU_RATE: f64 = 4.0e9; // bytes/s, gpuinflate on one stream
const GPU_START: f64 = 0.15; // s

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
    retry: Mutex<Vec<usize>>,
    notes: Mutex<Vec<String>>,
    info: Mutex<Vec<String>>,
    gpu: Mutex<GpuInfo>,
    gpu_max_entry: u64,
    slot_bytes: usize,
    use_gpu: bool,
    steal: bool, // in gpu mode the CPU only takes what the GPU cannot
}

impl<'a> Run<'a> {
    pub fn new(opts: &'a Opts, archive: &'a str, data: &'a [u8], entries: &'a [Entry], out_root: Option<PathBuf>) -> Self {
        let mut deflate: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].route == Route::Deflate).collect();
        deflate.sort_by_key(|&i| entries[i].size);
        let mut big = VecDeque::new();
        if opts.mode != Mode::Cpu && inflate::available() {
            let mut queued = 0.0;
            while let Some(&i) = deflate.last() {
                let size = entries[i].size as f64;
                let take = match opts.mode {
                    Mode::Gpu => entries[i].size >= BIG_MIN_GPU_MODE,
                    _ => entries[i].size >= BIG_MIN && GPU_START + (queued + size) / GPU_RATE < size / CPU_RATE,
                };
                if !take {
                    break; // smaller entries would not qualify either
                }
                queued += size;
                big.push_back(deflate.pop().unwrap());
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
        let mut info = Vec::new();
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
            notes: Mutex::new(Vec::new()),
            info: Mutex::new(info),
            gpu: Mutex::new(GpuInfo::default()),
            gpu_max_entry,
            slot_bytes,
            use_gpu,
            steal: opts.mode != Mode::Gpu,
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

    /// A large Deflate entry on gpuinflate, written as it comes. Ok(false): redo it on the CPU.
    fn deliver_inflate(&self, g: &inflate::Gpu, e: &Entry) -> Result<bool, String> {
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
        if r.is_err() || total != e.size || crc.finalize() != e.crc {
            return Ok(false);
        }
        if let (Some(f), Some(t)) = (out.as_ref(), zip::dos_time(e.dos_date, e.dos_time)) {
            let _ = f.set_modified(t);
        }
        self.add(Via::Gpu, e);
        Ok(true)
    }

    fn big_thread(&self) {
        let t0 = Instant::now();
        let g = match inflate::get() {
            Ok(g) => g,
            Err(x) => {
                let msg = format!("gpuinflate unavailable ({x}); large entries used the CPU");
                if self.opts.mode == Mode::Gpu { self.note(msg) } else { self.info.lock().unwrap().push(msg) }
                self.retry_all_big();
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
                Ok(true) => {
                    n += 1;
                    bytes += e.size;
                    self.gpu.lock().unwrap().batches += 1;
                }
                Ok(false) => self.retry.lock().unwrap().push(i),
                Err(x) => self.fail(e, x),
            }
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

    fn retry_all_big(&self) {
        let rest: Vec<usize> = self.big.lock().unwrap().drain(..).collect();
        self.retry.lock().unwrap().extend(rest);
    }

    fn run_cpu(&self, i: usize, d: &mut libdeflater::Decompressor) {
        let e = &self.entries[i];
        let start = e.data_offset as usize;
        let result = match e.method {
            STORED => self.deliver(e, &self.data[start..start + e.size as usize], Via::Cpu),
            DEFLATED if e.size > STREAM_ABOVE => self.deliver_streamed(e),
            DEFLATED => {
                let mut out = vec![0u8; e.size as usize];
                match d.deflate_decompress(&self.data[start..start + e.csize as usize], &mut out) {
                    Ok(n) if n == out.len() => self.deliver(e, &out, Via::Cpu),
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

    fn gpu_thread(&self, jobs: Sender<WriteJob>) {
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
                    self.retry.lock().unwrap().push(i);
                    continue;
                }
                let data = unsafe { std::slice::from_raw_parts(b.base.0.add(b.offsets[k]), e.size as usize) };
                match self.deliver(e, data, Via::Gpu) {
                    Ok(true) => {}
                    Ok(false) => self.retry.lock().unwrap().push(i),
                    Err(x) => self.fail(e, x),
                }
            }
            if b.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                let _ = b.free.send(b.slot);
            }
        }
    }

    fn progress(&self, stop: &AtomicBool) {
        let total: u64 = self.entries.iter().filter(|e| matches!(e.route, Route::Deflate | Route::Cpu)).map(|e| e.size).sum();
        let t0 = Instant::now();
        let mut err = std::io::stderr();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
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
        let threads = self.opts.threads.unwrap_or_else(crate::cpu_threads).max(1);
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
            let retried = this.retry.lock().unwrap().len();
            if retried > 0 {
                this.note(format!("{retried} GPU results failed the CRC check and were redone on the CPU"));
            }
            let mut rest: VecDeque<usize> = this.pending.lock().unwrap().drain(..).collect();
            rest.extend(this.retry.lock().unwrap().iter().copied());
            *this.cpu_queue.lock().unwrap() = rest;
            let again: Vec<_> = (0..threads).map(|_| sc.spawn(move || this.cpu_worker())).collect();
            for h in again {
                h.join().unwrap();
            }
            this.run_7z_entries();
            stop.store(true, Ordering::Relaxed);
            if let Some(h) = progress {
                h.join().unwrap();
            }
        });
        for e in self.entries {
            match e.route {
                Route::Unsafe => self.fail(e, "path leaves the output folder; skipped"),
                Route::Dup => self.note(format!("duplicate path, kept the last copy: {}", e.name)),
                _ => {}
            }
        }
        let retried = self.retry.lock().unwrap().len();
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
