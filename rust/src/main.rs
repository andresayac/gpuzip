//! gpuzip: extract ZIP and gzip archives on an NVIDIA GPU, with CPU fallback. Small ZIP
//! entries go to nvCOMP in batches; gzip streams to gpuinflate, which decodes one stream in
//! parallel. RAR, 7z and other tar.* archives go to 7-Zip.

mod extract;
mod gpu;
mod gz;
mod inflate;
mod sevenzip;
mod zip;

use clap::{Args, Parser, Subcommand, ValueEnum};
use extract::{Count, Outcome};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use zip::{Entry, Route};

const EPILOG: &str = r"examples:
  gpuzip x photos.zip               extract into .\photos
  gpuzip x *.zip -o D:\out          extract each archive into D:\out\<name>
  gpuzip x logs.tar.gz              .gz and .tar.gz/.tgz are decoded on the GPU too
  gpuzip x backup.rar               RAR, 7z and other tar.* go to 7-Zip
  gpuzip t backup.zip               check every file's CRC, write nothing
  gpuzip l backup.zip               list entries and where each is decoded
  gpuzip doctor                     check the GPU, nvCOMP and 7-Zip

exit codes: 0 ok · 1 missing file or setup problem · 2 some entries failed";

#[derive(Parser)]
#[command(
    name = "gpuzip",
    version,
    about = "Extract ZIP and gzip archives on an NVIDIA GPU, with CPU fallback. RAR, 7z and other tar.* archives go to 7-Zip.",
    after_help = EPILOG
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract archives
    #[command(visible_alias = "extract")]
    X {
        /// Archive files; wildcards like *.zip work
        #[arg(required = true, value_name = "ARCHIVE")]
        archives: Vec<String>,
        /// Output folder (default: the archive's name, in the current folder); with several archives, each goes into DIR\<name>
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
        #[command(flatten)]
        opts: Opts,
    },
    /// Decompress and check every CRC, write nothing
    #[command(visible_alias = "test")]
    T {
        #[arg(required = true, value_name = "ARCHIVE")]
        archives: Vec<String>,
        #[command(flatten)]
        opts: Opts,
    },
    /// List entries and where each would be decoded
    #[command(visible_alias = "list")]
    L {
        #[arg(required = true, value_name = "ARCHIVE")]
        archives: Vec<String>,
        #[command(flatten)]
        opts: Opts,
    },
    /// Check the GPU, nvCOMP and 7-Zip setup
    Doctor {
        #[command(flatten)]
        opts: Opts,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Auto,
    Gpu,
    Cpu,
}

#[derive(Args, Clone)]
pub struct Opts {
    /// auto: GPU and CPU share the work · gpu: all Deflate on the GPU · cpu: no GPU
    #[arg(long, value_enum, default_value_t = Mode::Auto)]
    pub mode: Mode,
    /// CPU decode threads (default: all)
    #[arg(long)]
    pub threads: Option<usize>,
    /// Threads writing GPU results (default: all, at most 32)
    #[arg(long)]
    pub io_threads: Option<usize>,
    /// Max uncompressed MB per GPU batch
    #[arg(long, default_value_t = 256)]
    pub batch_mb: u64,
    /// auto: larger entries stay on the CPU
    #[arg(long, default_value_t = 1024)]
    pub gpu_max_entry_kb: u64,
    /// auto: skip the GPU below this much small-entry data
    #[arg(long, default_value_t = 64)]
    pub gpu_min_mb: u64,
    /// Password for encrypted entries (passed to 7-Zip)
    #[arg(long)]
    pub password: Option<String>,
    /// Path to 7z.exe, if not in the usual place
    #[arg(long, value_name = "PATH")]
    pub sevenzip: Option<PathBuf>,
    /// Print one JSON report per archive
    #[arg(long)]
    pub json: bool,
    /// Also show GPU details and why work went where
    #[arg(short, long)]
    pub verbose: bool,
    /// Print only failures
    #[arg(short, long)]
    pub quiet: bool,
}

pub fn cpu_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

pub fn human(n: u64) -> String {
    let mut x = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if x < 1000.0 || unit == "GB" {
            return if unit == "B" { format!("{x:.0} B") } else { format!("{x:.1} {unit}") };
        }
        x /= 1000.0;
    }
    unreachable!()
}

pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// JSON with every non-ASCII character escaped, like Python's json.dumps.
fn ascii_json(v: &serde_json::Value) -> String {
    let mut out = String::new();
    for c in v.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Auto => "auto",
        Mode::Gpu => "gpu",
        Mode::Cpu => "cpu",
    }
}

fn report(cmd: char, archive: &str, fmt: &str, o: &Outcome, opts: &Opts) {
    let files: u64 = o.counts.iter().map(|c| c.files).sum();
    let bytes: u64 = o.counts.iter().map(|c| c.bytes).sum();
    let gbps = if o.seconds > 0.0 { bytes as f64 / o.seconds / 1e9 } else { 0.0 };
    if opts.json {
        let route = |c: &Count| json!({"files": c.files, "bytes": c.bytes});
        let gpu = o.gpu.as_ref().map(|g| {
            json!({"device": g.device, "batches": g.batches, "busy_seconds": g.busy_seconds, "startup_seconds": g.startup_seconds})
        });
        let failed: Vec<_> = o.failed.iter().map(|(n, r)| json!({"name": n, "reason": r})).collect();
        let v = json!({
            "archive": archive, "command": cmd.to_string(), "mode": mode_name(opts.mode), "format": fmt,
            "files": files, "bytes": bytes, "seconds": (o.seconds * 1e4).round() / 1e4,
            "gbps": (gbps * 1e3).round() / 1e3,
            "routes": {"gpu": route(&o.counts[0]), "cpu": route(&o.counts[1]), "7z": route(&o.counts[2])},
            "gpu": gpu, "retried": o.retried, "failed": failed, "notes": o.notes, "info": o.info,
            "implementation": "rust",
        });
        println!("{}", ascii_json(&v));
        return;
    }
    if opts.quiet && o.failed.is_empty() {
        return;
    }
    let verb = if cmd == 'x' { "Extracted" } else { "Tested" };
    if let Some((_, unpacked)) = o.stream {
        let rate = if o.seconds > 0.0 { unpacked as f64 / o.seconds / 1e9 } else { 0.0 };
        let on = match (o.counts[0].bytes > 0, o.counts[1].bytes > 0) {
            (true, false) => "on the GPU",
            (true, true) => "on the GPU and CPU",
            _ => "on the CPU",
        };
        let what = if fmt == "tar.gz" {
            format!("{} files ({})", commas(files), human(bytes.min(u64::MAX)))
        } else {
            Path::new(archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        };
        println!(
            "{verb} {what} in {:.2} s · {} decompressed at {rate:.2} GB/s {on} · {} failed",
            o.seconds,
            human(unpacked),
            o.failed.len()
        );
    } else if fmt != "zip" {
        let name = Path::new(archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        println!("{verb} {name} with 7-Zip on the CPU in {:.2} s · {} failed", o.seconds, o.failed.len());
    } else {
        println!(
            "{verb} {} files ({}) in {:.2} s · {gbps:.2} GB/s · {} on GPU, {} on CPU, {} with 7-Zip · {} failed",
            commas(files),
            human(bytes),
            o.seconds,
            commas(o.counts[0].files),
            commas(o.counts[1].files),
            commas(o.counts[2].files),
            o.failed.len()
        );
    }
    if opts.verbose {
        if let Some(g) = o.gpu.as_ref().filter(|g| g.device.is_some()) {
            println!(
                "  GPU: {} · {} batches · busy {:.2} s · ready after {:.2} s",
                g.device.as_deref().unwrap_or(""),
                g.batches,
                g.busy_seconds,
                g.startup_seconds.unwrap_or(0.0)
            );
        }
    }
    let info: &[String] = if opts.verbose { &o.info } else { &[] };
    for n in o.notes.iter().chain(info) {
        println!("  Note: {n}");
    }
    for (name, reason) in o.failed.iter().take(20) {
        println!("  Failed: {name}: {reason}");
    }
    if o.failed.len() > 20 {
        println!("  ... and {} more failed", o.failed.len() - 20);
    }
}

fn list_entries(entries: &[Entry], opts: &Opts) {
    let gpu_max = if opts.mode == Mode::Auto { opts.gpu_max_entry_kb * 1024 } else { u64::MAX };
    let mut counts: Vec<(&str, u64)> = Vec::new();
    println!("{:>9} {:>12} {:>12}  route  name", "method", "size", "packed");
    for e in entries {
        let route = match e.route {
            Route::Deflate if opts.mode == Mode::Cpu || e.size > gpu_max => "cpu",
            Route::Deflate => "gpu",
            Route::Cpu => "cpu",
            Route::SevenZip => "7z",
            Route::Dir => "dir",
            Route::Unsafe => "unsafe",
            Route::Dup => "dup",
        };
        match counts.iter_mut().find(|(r, _)| *r == route) {
            Some((_, n)) => *n += 1,
            None => counts.push((route, 1)),
        }
        println!("{:>9} {:>12} {:>12}  {route:<5}  {}", zip::method_name(e.method), commas(e.size), commas(e.csize), e.name);
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let parts: Vec<String> = counts.iter().map(|(r, n)| format!("{} {r}", commas(*n))).collect();
    println!("  {}", parts.join(" · "));
}

fn archive_stem(path: &str) -> String {
    let base = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = base.to_lowercase();
    for suf in sevenzip::TAR_SUFFIXES.iter().chain([".zip", ".rar", ".7z", ".tar", ".gz", ".xz", ".bz2", ".zst"].iter()) {
        if lower.ends_with(suf) {
            let stem = &base[..base.len() - suf.len()];
            return if stem.is_empty() { "out".into() } else { stem.into() };
        }
    }
    let stem = Path::new(&base).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{stem}_out")
}

fn run_7z_archive(cmd: char, archive: &str, out_root: Option<PathBuf>, opts: &Opts, why: Option<String>) -> i32 {
    let Some(exe) = sevenzip::find(opts.sevenzip.as_deref()) else {
        eprintln!("{archive}: this format needs 7-Zip (https://www.7-zip.org); pass --sevenzip PATH if it is installed somewhere else");
        return 1;
    };
    if cmd == 'l' {
        return sevenzip::list(&exe, archive);
    }
    let t0 = std::time::Instant::now();
    let rc = sevenzip::run_archive(&exe, archive, out_root.as_deref(), opts.password.as_deref(), opts.verbose && !opts.json);
    let lower = Path::new(archive).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = lower.split_once('.').map(|(_, e)| e.to_string()).unwrap_or(lower.clone());
    let outcome = Outcome {
        seconds: t0.elapsed().as_secs_f64(),
        counts: [Count::default(); 3],
        failed: if rc != 0 { vec![(archive.to_string(), format!("7-Zip exit code {rc}"))] } else { Vec::new() },
        notes: why.into_iter().collect(),
        info: vec!["not a ZIP file: handed to 7-Zip on the CPU".into()],
        gpu: None,
        retried: 0,
        stream: None,
    };
    report(cmd, archive, &ext, &outcome, opts);
    if rc != 0 { 2 } else { 0 }
}

fn process(cmd: char, path: &str, out_root: Option<PathBuf>, opts: &Opts) -> i32 {
    if !Path::new(path).is_file() {
        eprintln!("No such file: {path}");
        return 1;
    }
    let archive = std::path::absolute(path).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string());
    let file = match std::fs::File::open(&archive) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            return 1;
        }
    };
    let map = unsafe { memmap2::Mmap::map(&file) };
    let data: &[u8] = map.as_deref().unwrap_or(&[]);
    if gz::is_gzip(data) {
        let fmt = if gz::is_tar_name(&archive) { "tar.gz" } else { "gz" };
        if cmd == 'l' && fmt == "gz" {
            println!("{:>14} {:>14}  name", "packed", "size (trailer)");
            let isize = u32::from_le_bytes(data[data.len().max(4) - 4..].try_into().unwrap_or([0; 4]));
            println!("{:>14} {:>14}  {}", commas(data.len() as u64), commas(isize as u64), gz::output_name(&archive));
            return 0;
        }
        let outcome = gz::run(cmd, &archive, data, out_root, opts);
        if cmd != 'l' {
            report(cmd, &archive, fmt, &outcome, opts);
        } else {
            for (name, reason) in &outcome.failed {
                eprintln!("Failed: {name}: {reason}");
            }
        }
        return if outcome.failed.is_empty() { 0 } else { 2 };
    }
    if zip::find_eocd(data).is_none() {
        return run_7z_archive(cmd, &archive, out_root, opts, None);
    }
    let entries = match zip::read_entries(data) {
        Ok(e) => e,
        Err(e) => {
            let why = format!("could not read the ZIP directory ({e}); handed to 7-Zip");
            return run_7z_archive(cmd, &archive, out_root, opts, Some(why));
        }
    };
    if cmd == 'l' {
        list_entries(&entries, opts);
        return 0;
    }
    let outcome = extract::Run::new(opts, &archive, data, &entries, out_root).run();
    report(cmd, &archive, "zip", &outcome, opts);
    if outcome.failed.is_empty() { 0 } else { 2 }
}

/// Windows shells pass *.zip through unexpanded, so expand wildcards here.
fn expand(patterns: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in patterns {
        let mut matches: Vec<String> = if p.contains(['*', '?', '[']) {
            glob::glob(p).map(|g| g.flatten().map(|m| m.display().to_string()).collect()).unwrap_or_default()
        } else {
            Vec::new()
        };
        matches.sort();
        if matches.is_empty() { out.push(p.clone()) } else { out.extend(matches) }
    }
    out
}

fn run_all(cmd: char, patterns: &[String], output: Option<PathBuf>, opts: &Opts) -> i32 {
    let archives = expand(patterns);
    let mut rc = 0;
    for path in &archives {
        // a plain .gz holds one file: it goes straight into the output folder, like gunzip
        let plain_gz = path.to_lowercase().ends_with(".gz") && !gz::is_tar_name(path);
        let out_root = (cmd == 'x').then(|| {
            let dir = match &output {
                Some(o) if plain_gz => o.clone(),
                None if plain_gz => PathBuf::from("."),
                Some(o) if archives.len() > 1 => o.join(archive_stem(path)),
                Some(o) => o.clone(),
                None => PathBuf::from(archive_stem(path)),
            };
            std::path::absolute(&dir).unwrap_or(dir)
        });
        if archives.len() > 1 && !(opts.quiet || opts.json) {
            println!("{path}:");
        }
        rc = rc.max(process(cmd, path, out_root, opts));
    }
    rc
}

fn doctor(opts: &Opts) -> i32 {
    println!("gpuzip {} (Rust) · {} CPU threads · libdeflate", env!("CARGO_PKG_VERSION"), cpu_threads());
    match sevenzip::find(opts.sevenzip.as_deref()) {
        Some(exe) => println!("7-Zip:  {} at {}", sevenzip::version(&exe), exe.display()),
        None => println!("7-Zip:  not found; needed for RAR, 7z, tar.* and encrypted entries (https://www.7-zip.org)"),
    }
    match inflate::get() {
        Ok(g) => {
            let data: Vec<u8> = b"gpuzip doctor checks one stream decoded in parallel on the GPU. ".repeat(200_000);
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
            let _ = enc.write_all(&data);
            let comp = enc.finish().unwrap_or_default();
            let mut out = Vec::with_capacity(data.len());
            let good = g
                .inflate(&comp, data.len() as u64, |c| {
                    out.extend_from_slice(c);
                    true
                })
                .is_ok()
                && out == data;
            println!("gzip:   {} · gpuinflate test decode {}", g.device, if good { "ok" } else { "WRONG OUTPUT" });
            println!("        gpuinflate loaded from {}", g.path);
        }
        Err(e) => {
            println!("gzip:   decoded on the CPU ({e})");
            let places: Vec<String> = inflate::candidates().iter().map(|p| p.display().to_string()).collect();
            println!("        gpuinflate was looked for in: {}", places.join(", "));
        }
    }
    let api = match extract::gpu_api() {
        Ok(a) => a,
        Err(e) => {
            println!("GPU:    not usable ({e}); ZIP files are extracted on the CPU");
            let places: Vec<String> = gpu::nvcomp_candidates().iter().map(|p| p.display().to_string()).collect();
            println!("        nvCOMP was looked for in: {}", places.join(", "));
            return 1;
        }
    };
    let data: Vec<u8> = b"gpuzip doctor checks a decode on the GPU. ".repeat(40_000);
    let mut comp = vec![0u8; libdeflater::Compressor::new(libdeflater::CompressionLvl::default()).deflate_compress_bound(data.len())];
    let n = libdeflater::Compressor::new(libdeflater::CompressionLvl::default()).deflate_compress(&data, &mut comp).unwrap();
    comp.truncate(n);
    let result = gpu::Decoder::new(api, 4 << 20, 4 << 20, 1).and_then(|mut dec| {
        let res = dec.decode(0, &comp, &[(0, comp.len() as u64, data.len() as u64)])?;
        let out = unsafe { std::slice::from_raw_parts(dec.slots[0].h_out, data.len()) };
        Ok(res.ok[0] && out == data.as_slice())
    });
    match result {
        Ok(good) => {
            println!(
                "GPU:    {} · nvCOMP {} · test decode {}",
                api.device,
                api.nvcomp_version,
                if good { "ok" } else { "WRONG OUTPUT" }
            );
            println!("        nvCOMP loaded from {}", api.nvcomp_path);
            if good { 0 } else { 1 }
        }
        Err(e) => {
            println!("GPU:    {} found, but the test decode failed: {e}", api.device);
            1
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let rc = match &cli.command {
        Command::Doctor { opts } => doctor(opts),
        Command::X { archives, output, opts } => run_all('x', archives, output.clone(), opts),
        Command::T { archives, opts } => run_all('t', archives, None, opts),
        Command::L { archives, opts } => run_all('l', archives, None, opts),
    };
    std::process::exit(rc);
}
