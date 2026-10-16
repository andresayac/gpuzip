//! gpuzip: extract ZIP and gzip archives on an NVIDIA GPU, with CPU fallback. Small ZIP
//! entries go to nvCOMP in batches; gzip streams to gpuinflate, which decodes one stream in
//! parallel. RAR, 7z and other tar.* archives go to 7-Zip.

mod extract;
mod gpu;
mod gz;
mod inflate;
mod rescue;
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
  gpuzip rescue broken.tar.gz       recover what a damaged .gz or .tar.gz still holds
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
    /// Recover what a damaged .gz or .tar.gz still holds
    Rescue {
        #[arg(required = true, value_name = "FILE")]
        archives: Vec<String>,
        /// Output folder (default: the archive's name, in the current folder; a plain .gz goes into the current folder)
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
        /// Byte written where the data cannot be known
        #[arg(long, default_value_t = 0)]
        fill: u8,
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

/// `gpuzip rescue`: recover a damaged .gz (one file) or .tar.gz (its files, skipping what was destroyed)
fn rescue_one(path: &str, out_root: PathBuf, fill: u8, opts: &Opts) -> i32 {
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
    if zip::find_eocd(data).is_some() {
        eprintln!("{path}: a ZIP file; rescue reads gzip files (.gz, .tar.gz, .tgz) for now");
        return 1;
    }
    let lib = match rescue::get() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let tar = gz::is_tar_name(&archive);
    let t0 = std::time::Instant::now();
    let mut failed: Vec<(String, String)> = Vec::new();
    let (report, tar_out, target) = if tar {
        let mut out = Vec::with_capacity(data.len().saturating_mul(4));
        let mut mask = Vec::with_capacity(data.len().saturating_mul(4));
        let report = lib.rescue(data, fill, |d, m| {
            out.extend_from_slice(d);
            mask.extend_from_slice(m);
            true
        });
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{path}: {e}");
                return 1;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&out_root) {
            eprintln!("{}: {e}", out_root.display());
            return 1;
        }
        let mut t = rescue::untar(&out, &mask, Some(&out_root), true);
        failed.extend(t.failed.iter().cloned());
        // a checksum failure with no block to blame: the damage is somewhere no decoder can see
        // it (raw stored data, a flipped bit that stayed in step), so no file can be vouched for
        if report.stats.crc_check == 0 && report.stats.damaged == 0 {
            t.unverified = t.intact;
            t.intact = 0;
        }
        (report, Some(t), None)
    } else {
        let target = out_root.join(gz::output_name(&archive));
        if let Err(e) = std::fs::create_dir_all(&out_root) {
            eprintln!("{}: {e}", out_root.display());
            return 1;
        }
        let mut f = match std::fs::File::create(&target) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{}: {e}", target.display());
                return 1;
            }
        };
        let mut write_err = None;
        let report = lib.rescue(data, fill, |d, _| match f.write_all(d) {
            Ok(()) => true,
            Err(e) => {
                write_err = Some(e.to_string());
                false
            }
        });
        if let Some(e) = write_err {
            eprintln!("{}: {e}", target.display());
            return 1;
        }
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{path}: {e}");
                return 1;
            }
        };
        (report, None, Some(target))
    };
    let secs = t0.elapsed().as_secs_f64();
    let s = &report.stats;
    let total = s.output_size.max(1) as f64;
    let pct = |x: u64| 100.0 * x as f64 / total;
    let clean = report.clean();
    let damaged: Vec<&rescue::Piece> = report.pieces.iter().filter(|p| p.suspect).collect();
    if opts.json {
        let v = json!({
            "archive": archive, "command": "rescue", "format": if tar { "tar.gz" } else { "gz" },
            "input_bytes": s.input_size, "output_bytes": s.output_size,
            "exact_bytes": s.exact_bytes, "suspect_bytes": s.suspect_bytes, "unknown_bytes": s.unknown_bytes,
            "block_headers": s.headers, "damaged_blocks": s.damaged, "crc_check": s.crc_check, "length_off": s.length_off,
            "truncated": s.truncated != 0, "header_damaged": s.header_damaged != 0, "clean": clean,
            "seconds": (secs * 1e4).round() / 1e4,
            "damaged": damaged.iter().map(|p| json!({
                "input_byte": p.in_byte_start, "input_byte_end": p.in_byte_end, "output_offset": p.out_offset, "length": p.length,
                "decoded": p.decoded, "known": p.known, "blocks": p.blocks, "symbols": p.symbols, "why": p.why,
            })).collect::<Vec<_>>(),
            "notes": report.notes,
            "output": target.as_ref().map(|t| t.display().to_string()),
            "tar": tar_out.as_ref().map(|t| json!({
                "intact": t.intact, "unverified": t.unverified, "suspect": t.suspect, "partial": t.partial, "repaired_headers": t.repaired,
                "skipped": t.skipped.iter().map(|(o, n)| json!({"offset": o, "bytes": n})).collect::<Vec<_>>(),
            })),
            "failed": failed.iter().map(|(n, r)| json!({"name": n, "reason": r})).collect::<Vec<_>>(),
            "implementation": "rust",
        });
        println!("{}", ascii_json(&v));
        return if failed.is_empty() && clean { 0 } else { 2 };
    }
    if opts.quiet && clean && failed.is_empty() {
        return 0;
    }
    let name = Path::new(&archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(t) = &tar_out {
        println!(
            "Rescued {name} in {secs:.2} s: {} files intact, {} possibly damaged, {} partial, {} region{} unreadable ({}) · data {:.2}% exact, {:.2}% unknown",
            commas(t.intact),
            commas(t.suspect.len() as u64),
            commas(t.partial.len() as u64),
            t.skipped.len(),
            if t.skipped.len() == 1 { "" } else { "s" },
            human(t.skipped.iter().map(|(_, n)| n).sum()),
            pct(s.exact_bytes),
            pct(s.unknown_bytes)
        );
        if t.unverified > 0 {
            println!(
                "  {} files extracted but unverifiable: the archive's checksum fails and the damage could not be located (it lies in raw stored data, such as an already compressed or random file, or is a flipped bit that kept the decoder in step); any of them may be the damaged one",
                commas(t.unverified)
            );
        }
        for n in t.partial.iter().take(20) {
            println!("  Partial: {n}");
        }
        if t.partial.len() > 20 {
            println!("  ... and {} more partial", t.partial.len() - 20);
        }
        for n in t.suspect.iter().take(20) {
            println!("  Possibly damaged: {n}");
        }
        if t.suspect.len() > 20 {
            println!("  ... and {} more possibly damaged", t.suspect.len() - 20);
        }
        if t.repaired > 0 {
            println!("  Note: {} tar headers had unknown bytes and were made whole from the header before them (checksums confirm)", t.repaired);
        }
    } else if let Some(t) = &target {
        println!(
            "Rescued {} in {secs:.2} s: {} written, {:.2}% exact, {:.2}% unknown, {:.2}% in {} damaged block{}{}",
            t.display(),
            human(s.output_size),
            pct(s.exact_bytes),
            pct(s.unknown_bytes),
            pct(s.suspect_bytes),
            s.damaged,
            if s.damaged == 1 { "" } else { "s" },
            if clean { " · intact" } else { "" }
        );
    }
    for p in damaged.iter().take(20) {
        println!("  Damaged block at input byte {} (output offset {}): {}", commas(p.in_byte_start), commas(p.out_offset), p.why);
    }
    if damaged.len() > 20 {
        println!("  ... and {} more damaged blocks", damaged.len() - 20);
    }
    if s.crc_check == 0 {
        println!("  Note: the CRC does not match: an error the decoder could not place remains somewhere");
    }
    for n in &report.notes {
        println!("  Note: {n}");
    }
    for (n, reason) in failed.iter().take(20) {
        println!("  Failed: {n}: {reason}");
    }
    if failed.is_empty() && clean { 0 } else { 2 }
}

fn run_rescue(patterns: &[String], output: Option<PathBuf>, fill: u8, opts: &Opts) -> i32 {
    let archives = expand(patterns);
    let mut rc = 0;
    for path in &archives {
        let plain_gz = !gz::is_tar_name(path);
        let dir = match &output {
            Some(o) if plain_gz => o.clone(),
            None if plain_gz => PathBuf::from("."),
            Some(o) if archives.len() > 1 => o.join(archive_stem(path)),
            Some(o) => o.clone(),
            None => PathBuf::from(archive_stem(path)),
        };
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        if archives.len() > 1 && !(opts.quiet || opts.json) {
            println!("{path}:");
        }
        rc = rc.max(rescue_one(path, dir, fill, opts));
    }
    rc
}

fn doctor(opts: &Opts) -> i32 {
    println!("gpuzip {} (Rust) · {} CPU threads · libdeflate", env!("CARGO_PKG_VERSION"), cpu_threads());
    match sevenzip::find(opts.sevenzip.as_deref()) {
        Some(exe) => println!("7-Zip:  {} at {}", sevenzip::version(&exe), exe.display()),
        None => println!("7-Zip:  not found; needed for RAR, 7z, tar.* and encrypted entries (https://www.7-zip.org)"),
    }
    match rescue::get() {
        Ok(r) => println!("rescue: gzrescue loaded from {}", r.path),
        Err(e) => println!("rescue: not available ({e}); `gpuzip rescue` needs gzrescue.dll next to gpuzip.exe"),
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
        Command::Rescue { archives, output, fill, opts } => run_rescue(archives, output.clone(), *fill, opts),
    };
    std::process::exit(rc);
}
