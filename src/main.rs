//! gpuzip: extract ZIP and gzip archives on an NVIDIA GPU, with CPU fallback. Small ZIP
//! entries go to nvCOMP in batches; gzip streams to gpuinflate, which decodes one stream in
//! parallel. RAR, 7z and other tar.* archives go to 7-Zip.

mod dospages;
mod dylib;
mod extract;
mod gpu;
mod gz;
mod inflate;
mod officefix;
mod outfile;
mod rescue;
mod sevenzip;
mod sheetfix;
mod ui;
mod zip;
mod ziprescue;

use clap::{Args, Parser, Subcommand, ValueEnum};
use extract::{Count, Outcome};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use ui::{BAD, BOLD, DIM, GOOD, INFO, WARN, paint, say};
use zip::{Entry, Route};

const EPILOG: &str = r"examples:
  gpuzip x photos.zip               extract into ./photos
  gpuzip x *.zip -o out             extract each archive into out/<name>
  gpuzip x logs.tar.gz              .gz and .tar.gz/.tgz are decoded on the GPU too
  gpuzip x backup.rar               RAR, 7z and other tar.* go to 7-Zip
  gpuzip t backup.zip               check every file's CRC, write nothing
  gpuzip l backup.zip               list entries and where each is decoded
  gpuzip rescue broken.tar.gz       recover what a damaged .gz, .tar.gz, .zip or Office file still holds
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
        /// Output folder (default: the archive's name, in the current folder); with several archives, each goes into DIR/<name>
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
    /// Recover what a damaged .gz, .tar.gz, .zip or Office document still holds
    Rescue {
        #[arg(required = true, value_name = "FILE")]
        archives: Vec<String>,
        /// Output folder (default: the archive's name, in the current folder; a plain .gz goes into the current folder)
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,
        /// Byte written where the data cannot be known
        #[arg(long, default_value_t = 0)]
        fill: u8,
        /// Stop when a file's output would be larger than this (a decompression bomb): bytes, or
        /// with K, M, G or T; 0 for no limit [default: 100 times the input, at least 4G]
        #[arg(long, value_name = "SIZE", value_parser = parse_size)]
        max_output: Option<u64>,
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
    /// Threads writing files: GPU results, a tar's small files (default: all, at most 32)
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
    /// Path to 7-Zip (7z.exe, 7z or 7zz), if not in the usual place
    #[arg(long, value_name = "PATH")]
    pub sevenzip: Option<PathBuf>,
    /// Code page of ZIP names not marked UTF-8: 932 (Japanese), 936 (Chinese), 949 (Korean),
    /// 950 (Traditional Chinese), 866 (Russian), 850 (Western Europe), 852... [default: the
    /// system's OEM one on Windows, as Explorer's; 437 elsewhere]
    #[arg(long, value_name = "CODEPAGE")]
    pub codepage: Option<String>,
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

/// A size: bytes, or a number with K, M, G or T after it (powers of 1024)
fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (num, mult) = match t.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M') => (&t[..t.len() - 1], 1 << 20),
        Some('G') => (&t[..t.len() - 1], 1 << 30),
        Some('T') => (&t[..t.len() - 1], 1 << 40),
        _ => (t, 1),
    };
    let v: f64 = num.trim().parse().map_err(|_| format!("not a size: {s} (a number of bytes, or 64G, 1.5T, ...)"))?;
    if !(v >= 0.0) {
        return Err(format!("not a size: {s}"));
    }
    Ok((v * mult as f64) as u64)
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
        say!("{}", ascii_json(&v));
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
            format!("{} file{} ({})", commas(files), if files == 1 { "" } else { "s" }, human(bytes.min(u64::MAX)))
        } else {
            Path::new(archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        };
        say!(
            "{} {what} in {} · {} decompressed at {} {on} · {}",
            paint(BOLD, verb),
            paint(BOLD, format!("{:.2} s", o.seconds)),
            human(unpacked),
            paint(INFO, format!("{rate:.2} GB/s")),
            ui::failed(o.failed.len())
        );
    } else if fmt != "zip" {
        let name = Path::new(archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        say!("{} {name} with 7-Zip on the CPU in {:.2} s · {}", paint(BOLD, verb), o.seconds, ui::failed(o.failed.len()));
    } else {
        let routes: Vec<String> = [(0, "on the GPU"), (1, "on the CPU"), (2, "with 7-Zip")]
            .iter()
            .filter(|(i, _)| o.counts[*i].files > 0)
            .map(|(i, how)| format!("{} {how}", commas(o.counts[*i].files)))
            .collect();
        say!(
            "{} {} file{} ({}) in {} · {} · {} · {}",
            paint(BOLD, verb),
            commas(files),
            if files == 1 { "" } else { "s" },
            human(bytes),
            paint(BOLD, format!("{:.2} s", o.seconds)),
            paint(INFO, format!("{gbps:.2} GB/s")),
            if routes.is_empty() { "nothing to decode".to_string() } else { routes.join(", ") },
            ui::failed(o.failed.len())
        );
    }
    if opts.verbose {
        if let Some(g) = o.gpu.as_ref().filter(|g| g.device.is_some()) {
            say!(
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
        say!("  {}", paint(DIM, format!("Note: {n}")));
    }
    for (name, reason) in o.failed.iter().take(20) {
        say!("  {} {name}: {reason}", paint(BAD, "Failed:"));
    }
    if o.failed.len() > 20 {
        say!("  ... and {} more failed", o.failed.len() - 20);
    }
}

fn list_entries(entries: &[Entry], opts: &Opts) {
    let gpu_max = if opts.mode == Mode::Auto { opts.gpu_max_entry_kb * 1024 } else { u64::MAX };
    let mut counts: Vec<(&str, u64)> = Vec::new();
    say!("{:>9} {:>12} {:>12}  route  name", "method", "size", "packed");
    for e in entries {
        let route = match e.route {
            Route::Deflate if opts.mode == Mode::Cpu || e.size > gpu_max => "cpu",
            Route::Deflate => "gpu",
            Route::Cpu => "cpu",
            Route::SevenZip => "7z",
            Route::Dir => "dir",
            Route::Unsafe => "unsafe",
            Route::Dup => "dup",
            Route::Overlap => "overlap",
        };
        match counts.iter_mut().find(|(r, _)| *r == route) {
            Some((_, n)) => *n += 1,
            None => counts.push((route, 1)),
        }
        say!("{:>9} {:>12} {:>12}  {route:<5}  {}", zip::method_name(e.method), commas(e.size), commas(e.csize), e.name);
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let parts: Vec<String> = counts.iter().map(|(r, n)| format!("{} {r}", commas(*n))).collect();
    say!("  {}", parts.join(" · "));
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
            say!("{:>14} {:>14}  name", "packed", "size (trailer)");
            let isize = u32::from_le_bytes(data[data.len().max(4) - 4..].try_into().unwrap_or([0; 4]));
            say!("{:>14} {:>14}  {}", commas(data.len() as u64), commas(isize as u64), gz::output_name(&archive));
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
    zip::codepage_hint(); // forget names an archive before this one left
    let entries = match zip::read_entries(data) {
        Ok(e) => e,
        Err(e) => {
            let why = format!("could not read the ZIP directory ({e}); handed to 7-Zip");
            return run_7z_archive(cmd, &archive, out_root, opts, Some(why));
        }
    };
    let hint = zip::codepage_hint();
    if cmd == 'l' {
        list_entries(&entries, opts);
        if let Some(h) = hint {
            say!("  {}", paint(DIM, format!("Note: {h}")));
        }
        return 0;
    }
    let mut outcome = extract::Run::new(opts, &archive, data, &entries, out_root).run();
    outcome.notes.extend(hint);
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
            say!("{path}:");
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
    let mapped: &[u8] = map.as_deref().unwrap_or(&[]);
    let lib = match rescue::get() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // A file sent through FTP in ASCII mode from Unix to Windows has a CR before every LF it
    // ever held. Compressed data is close to random, where a CR precedes an LF one time in 256;
    // when nearly every LF has one, the conversion is undone exactly, before anything else.
    // A ZIP whose central directory ends exactly where its end record says has had no bytes
    // inserted: not converted, even if most of its LFs have a CR (Windows text stored as is).
    let zip_in_place = zip::find_eocd(mapped).map_or(false, |e| {
        let (size, offset) = (u32::from_le_bytes(mapped[e + 12..e + 16].try_into().unwrap()), u32::from_le_bytes(mapped[e + 16..e + 20].try_into().unwrap()));
        size == u32::MAX || offset == u32::MAX || offset as u64 + size as u64 == e as u64
    });
    let unconverted = if zip_in_place { None } else { rescue::undo_lf_to_crlf(mapped) };
    let data: &[u8] = unconverted.as_deref().unwrap_or(mapped);
    rescue::set_ascii_note(unconverted.is_some());
    if ziprescue::is_office_name(&archive) || (!gz::is_gzip(data) && ziprescue::looks_like_zip(data)) {
        // the reverse transfer removed CRs: a ZIP's entries are too short to tell on their own
        rescue::set_cr_lost(unconverted.is_none() && rescue::looks_cr_stripped(data));
        return rescue_zip_file(&archive, data, lib, out_root, fill, opts);
    }
    // a gzip file is one stream: gzrescue tells from all of it, where a few dozen CR LF pairs are due
    rescue::set_cr_lost(false);
    let tar = gz::is_tar_name(&archive);
    let t0 = std::time::Instant::now();
    let mut failed: Vec<(String, String)> = Vec::new();
    let (report, tar_out, target) = if tar {
        // the files are written as the output comes, none of it held
        let mut reader = rescue::TarReader::new(Some(&out_root), true);
        let report = lib.rescue(data, fill, |d, m| {
            reader.feed(d, m);
            true
        });
        let mut report = match report {
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
        let mut t = reader.finish();
        failed.extend(t.failed.iter().cloned());
        report.notes.extend(zip::device_note(&t.renamed));
        report.notes.extend(gz::case_note(&t.case_pairs));
        report.notes.extend(t.link_notes.iter().cloned());
        // a checksum failure with no block to blame: the damage is somewhere no decoder can see
        // it (raw stored data, a flipped bit that stayed in step), so no file can be vouched for
        if report.stats.crc_check == 0 && report.stats.damaged == 0 {
            t.unverified = t.intact;
            t.intact = 0;
        }
        // Damage that kept the decoder in step was found (a flipped bit, a lost CR, a zeroed
        // sector), and no checksum vouches for the rest: the same damage may lie where it changed
        // nothing a decoder sees, in any file (measured: 57 wrong files of Python-3.14.0.tgz with 8
        // flipped bits would have been called intact, the 3 blocks they showed in flagged)
        if report.stats.hidden_damage != 0 {
            t.unverified += t.intact;
            t.intact = 0;
        }
        (report, Some(t), None)
    } else {
        let (out_name, renamed) = gz::output_file(&archive);
        let target = out_root.join(out_name);
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
        let mut report = match report {
            Ok(r) => r,
            Err(e) => {
                // nothing, or not all of it, was written (no Deflate data, a decompression bomb)
                drop(f);
                let _ = std::fs::remove_file(&target);
                eprintln!("{path}: {e}");
                return 1;
            }
        };
        report.notes.extend(renamed);
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
            "exact_bytes": s.exact_bytes, "inferred_bytes": s.inferred_bytes, "crs_restored": s.cr_restored, "suspect_bytes": s.suspect_bytes, "unknown_bytes": s.unknown_bytes,
            "block_headers": s.headers, "damaged_blocks": s.damaged, "crc_check": s.crc_check, "hidden_damage": s.hidden_damage != 0, "length_off": s.length_off,
            "truncated": s.truncated != 0, "header_damaged": s.header_damaged != 0, "clean": clean,
            "seconds": (secs * 1e4).round() / 1e4,
            "damaged": damaged.iter().map(|p| json!({
                "input_byte": p.in_byte_start, "input_byte_end": p.in_byte_end, "output_offset": p.out_offset, "length": p.length,
                "decoded": p.decoded, "known": p.known, "blocks": p.blocks, "symbols": p.symbols, "why": p.why,
            })).collect::<Vec<_>>(),
            "notes": report.notes, "ascii_transfer_undone": rescue::ascii_undone(),
            "output": target.as_ref().map(|t| t.display().to_string()),
            "tar": tar_out.as_ref().map(|t| json!({
                "intact": t.intact, "unverified": t.unverified, "inferred": t.inferred, "suspect": t.suspect, "partial": t.partial, "repaired_headers": t.repaired,
                "skipped": t.skipped.iter().map(|(o, n)| json!({"offset": o, "bytes": n})).collect::<Vec<_>>(),
            })),
            "failed": failed.iter().map(|(n, r)| json!({"name": n, "reason": r})).collect::<Vec<_>>(),
            "implementation": "rust",
        });
        say!("{}", ascii_json(&v));
        return if failed.is_empty() && clean { 0 } else { 2 };
    }
    if opts.quiet && clean && failed.is_empty() {
        return 0;
    }
    let name = Path::new(&archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let into = target.clone().unwrap_or_else(|| out_root.clone());
    say!("{} {name} in {:.2} s into {}", paint(BOLD, "Rescued"), secs, ui::shown_path(&into));
    let share = |x: u64| x as f64 / total;
    say!(
        "  {}  {}",
        ui::bar(&[(share(s.exact_bytes), GOOD, '█'), (share(s.inferred_bytes), INFO, '▓'), (share(s.suspect_bytes), WARN, '▒'), (share(s.unknown_bytes), BAD, '░')], 40),
        if clean && rescue::ascii_undone() {
            paint(GOOD, "every byte recovered, the CRC checks")
        } else if clean {
            paint(GOOD, "intact: every byte checked")
        } else if s.crc_check == 0 {
            paint(WARN, format!("{:.1}% decoded cleanly, but the CRC does not match: some of it is wrong", pct(s.exact_bytes)))
        } else if s.hidden_damage != 0 {
            format!("{} of the data decoded cleanly; the checksum cannot confirm it", paint(BOLD, format!("{:.1}%", pct(s.exact_bytes))))
        } else {
            format!("{} of the data exact", paint(BOLD, format!("{:.1}%", pct(s.exact_bytes))))
        }
    );
    if let Some(t) = &tar_out {
        // files that decoded cleanly but nothing confirms come first, plainly, when no file is
        // confirmed; they are written like every other
        let unconfirmed = (
            t.unverified,
            INFO,
            "decoded cleanly",
            if s.hidden_damage != 0 {
                "but the archive's checksum cannot confirm them: some damage did not stop the decoder".to_string()
            } else {
                "but the archive's checksum fails and cannot say which of them is wrong".to_string()
            },
        );
        let intact = (t.intact, GOOD, "files intact", String::new());
        let (first, second) = if t.unverified > 0 && t.intact == 0 { (unconfirmed, intact) } else { (intact, unconfirmed) };
        let rows: Vec<(u64, ui::Style, &str, String)> = vec![
            first,
            second,
            (t.inferred.len() as u64, INFO, "with inferred bytes", "likely right, not certain".into()),
            (t.suspect.len() as u64, WARN, "possibly damaged", "inside a damaged block or after a repaired header".into()),
            (t.partial.len() as u64, BAD, "partial", format!("unknown bytes written as {fill:#04x}")),
            (t.skipped.len() as u64, BAD, "regions unreadable", format!("{} skipped, the files there lost", human(t.skipped.iter().map(|(_, n)| n).sum()))),
        ];
        rescue_rows(&rows);
        let limit = if opts.verbose { usize::MAX } else { 3 };
        for (label, style, list) in [("partial", BAD, &t.partial), ("possibly damaged", WARN, &t.suspect), ("inferred bytes", INFO, &t.inferred)] {
            for n in list.iter().take(limit) {
                say!("    {} {n}", paint(style, format!("{label}:")));
            }
            if list.len() > limit {
                say!("    {}", paint(DIM, format!("... and {} more {label} (-v lists them)", commas((list.len() - limit) as u64))));
            }
        }
        if t.repaired > 0 {
            let note = if t.repaired == 1 {
                "1 tar header had unknown bytes and was made whole from the one before it (its checksum confirms)".to_string()
            } else {
                format!("{} tar headers had unknown bytes and were made whole from the ones before them (their checksums confirm)", t.repaired)
            };
            say!("  {}", paint(DIM, format!("Note: {note}")));
        }
    } else {
        say!(
            "  {} written: {:.2}% {}, {:.2}% inferred, {:.2}% unknown, {:.2}% in {} damaged block{}",
            human(s.output_size),
            pct(s.exact_bytes),
            if s.hidden_damage != 0 { "decoded cleanly (unconfirmed)" } else { "exact" },
            pct(s.inferred_bytes),
            pct(s.unknown_bytes),
            pct(s.suspect_bytes),
            s.damaged,
            if s.damaged == 1 { "" } else { "s" }
        );
    }
    if opts.verbose {
        for p in &damaged {
            say!("  {} at input byte {} (output offset {}): {}", paint(WARN, "Damaged block"), commas(p.in_byte_start), commas(p.out_offset), p.why);
        }
    }
    if rescue::ascii_undone() {
        say!("  {}", paint(INFO, "The file had gone through an ASCII-mode transfer (every LF made CR LF): undone"));
    }
    if s.crc_check == 0 {
        say!("  {}", paint(WARN, "The CRC does not match: an error the decoder could not place remains somewhere"));
    } else if s.hidden_damage != 0 {
        say!("  {}", paint(DIM, "Some of the damage did not stop the decoder (a flipped bit, a lost CR, a zeroed sector it read through), so more of it may lie where nothing shows, and the checksum, which could rule that out, cannot be checked. What decoded cleanly is written; compare it with another copy if there is one"));
    } else if s.crc_check < 0 && s.damaged > 0 {
        say!("  {}", paint(DIM, "With damaged blocks the checksum cannot be checked: \"exact\" and \"intact\" mean every byte decoded cleanly"));
    } else if s.crc_check < 0 && s.truncated != 0 {
        say!("  {}", paint(WARN, "The file ends inside the compressed data, so the checksum cannot be checked: \"exact\" means every byte decoded cleanly, not that it is right"));
    }
    if opts.verbose {
        for n in &report.notes {
            say!("  {}", paint(DIM, format!("Note: {n}")));
        }
    }
    for (n, reason) in failed.iter().take(20) {
        say!("  {} {n}: {reason}", paint(BAD, "Failed:"));
    }
    if !clean && !opts.verbose {
        say!("  {}", paint(DIM, format!("-v adds every file, damaged block and note ({}); --json gives the whole report", report.notes.len())));
    }
    if failed.is_empty() && clean { 0 } else { 2 }
}

/// A rescue's counts, one per line, the number right-aligned; empty ones left out but the first
fn rescue_rows(rows: &[(u64, ui::Style, &str, String)]) {
    let width = rows.iter().map(|r| commas(r.0).len()).max().unwrap_or(1);
    let label_width = rows.iter().filter(|r| r.0 > 0).map(|r| r.2.len()).max().unwrap_or(0);
    for (i, (n, style, label, detail)) in rows.iter().enumerate() {
        if *n == 0 && i > 0 {
            continue;
        }
        let shown = if *n == 0 { paint(DIM, format!("{:>width$}", 0)) } else { paint(*style, format!("{:>width$}", commas(*n))) };
        if detail.is_empty() {
            say!("  {shown} {label}");
        } else {
            say!("  {shown} {label:<label_width$}  {}", paint(DIM, detail));
        }
    }
}

/// `gpuzip rescue` on a ZIP or an Office document
fn rescue_zip_file(archive: &str, data: &[u8], lib: &rescue::Rescue, out_root: PathBuf, fill: u8, opts: &Opts) -> i32 {
    let t0 = std::time::Instant::now();
    if let Err(e) = std::fs::create_dir_all(&out_root) {
        eprintln!("{}: {e}", out_root.display());
        return 1;
    }
    // Office parts are kept in memory to rebuild the document; a docx starts with its content types
    let keep_parts = ziprescue::is_office_name(archive) || data.len() > 50 && &data[30..30 + 19.min(data.len() - 30)] == b"[Content_Types].xml";
    zip::codepage_hint(); // forget names an archive before this one left
    let o = ziprescue::rescue_zip(data, lib, Some(&out_root), fill, keep_parts);
    let hint = zip::codepage_hint();
    let secs = t0.elapsed().as_secs_f64();
    use ziprescue::Status;
    let count = |s: Status| o.entries.iter().filter(|e| e.status == s).count() as u64;
    let (intact, partial, unverified, lost, unsupported) =
        (count(Status::Intact), count(Status::Partial), count(Status::Unverified), count(Status::Lost), count(Status::Unsupported));
    let from_scan = o.entries.iter().filter(|e| e.from_scan).count();
    let clean = o.cd_ok && o.entries.iter().all(|e| e.status == Status::Intact) && o.failed.is_empty();
    let fmt = o.office.as_ref().map(|f| f.kind.clone()).unwrap_or_else(|| "zip".into());
    let name = Path::new(archive).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if opts.json {
        let status_name = |s: Status| match s {
            Status::Intact => "intact",
            Status::Partial => "partial",
            Status::Unverified => "unverified",
            Status::Lost => "lost",
            Status::Unsupported => "unsupported",
        };
        let v = json!({
            "archive": archive, "command": "rescue", "format": fmt, "clean": clean,
            "directory_ok": o.cd_ok, "directory_error": o.cd_error, "scanned_headers": o.scanned_headers, "from_scan": from_scan,
            "counts": {"intact": intact, "partial": partial, "unverified": unverified, "lost": lost, "unsupported": unsupported},
            "entries": o.entries.iter().map(|e| json!({"name": e.name, "status": status_name(e.status), "detail": e.detail, "size": e.size, "from_scan": e.from_scan})).collect::<Vec<_>>(),
            "office": o.office.as_ref().map(|f| json!({
                "kind": f.kind, "repaired_parts": f.repaired_parts, "text_file": f.text_file.as_ref().map(|p| p.display().to_string()),
                "text_chars": f.text_chars, "rebuilt": f.rebuilt.as_ref().map(|p| p.display().to_string()),
                "package_fixes": f.package_fixes,
            })),
            "notes": o.notes, "codepage_hint": hint, "ascii_transfer_undone": rescue::ascii_undone(), "crs_restored": o.crs_restored, "failed": o.failed.iter().map(|(n, r)| json!({"name": n, "reason": r})).collect::<Vec<_>>(),
            "seconds": (secs * 1e4).round() / 1e4, "implementation": "rust",
        });
        say!("{}", ascii_json(&v));
        return if clean { 0 } else { 2 };
    }
    if opts.quiet && clean {
        return 0;
    }
    say!("{} {name} in {:.2} s into {}", paint(BOLD, "Rescued"), secs, ui::shown_path(&out_root));
    let total = (o.entries.len() as f64).max(1.0);
    say!(
        "  {}  {}",
        ui::bar(&[(intact as f64 / total, GOOD, '█'), (unverified as f64 / total, WARN, '▒'), (partial as f64 / total, BAD, '░')], 40),
        if clean && (o.notes.is_empty() && o.crs_restored == 0 && !rescue::ascii_undone()) {
            paint(GOOD, "intact: every entry's CRC checked")
        } else if clean {
            paint(GOOD, "every entry recovered, each CRC checks")
        } else {
            format!("{} of {} entries intact", paint(BOLD, commas(intact)), commas(o.entries.len() as u64))
        }
    );
    let rows: Vec<(u64, ui::Style, &str, String)> = vec![
        (intact, GOOD, "intact", String::new()),
        (unverified, WARN, "unverified", "no CRC to check them against".into()),
        (partial, BAD, "partial", format!("unknown bytes written as {fill:#04x}")),
        (lost, BAD, "lost", String::new()),
        (unsupported, WARN, "not readable here", "a method gpuzip rescue does not decode".into()),
    ];
    rescue_rows(&rows);
    say!(
        "  {}",
        paint(
            DIM,
            if o.cd_ok {
                format!("directory read{}", if from_scan > 0 { format!(", plus {from_scan} entries it did not list") } else { String::new() })
            } else {
                format!("directory lost: {} entries found by scanning the file", commas(o.scanned_headers as u64))
            }
        )
    );
    let limit = if opts.verbose { usize::MAX } else { 3 };
    for (label, style, status) in [("partial", BAD, Status::Partial), ("unverified", WARN, Status::Unverified), ("lost", BAD, Status::Lost), ("not readable here", WARN, Status::Unsupported)] {
        let list: Vec<&ziprescue::Recovered> = o.entries.iter().filter(|e| e.status == status).collect();
        for e in list.iter().take(limit) {
            say!("    {} {}{}", paint(style, format!("{label}:")), e.name, if e.detail.is_empty() { String::new() } else { format!(" ({})", e.detail) });
        }
        if list.len() > limit {
            say!("    {}", paint(DIM, format!("... and {} more {label} (-v lists them)", commas((list.len() - limit) as u64))));
        }
    }
    if let Some(f) = &o.office {
        if let Some(r) = &f.rebuilt {
            say!("  {} {}", paint(GOOD, "Rebuilt document:"), ui::shown_path(r));
        }
        if let Some(t) = &f.text_file {
            say!("  {} {} ({} characters)", paint(INFO, "Text recovered:"), ui::shown_path(t), commas(f.text_chars as u64));
        }
        if !f.repaired_parts.is_empty() {
            say!("  {}", paint(DIM, format!("{} damaged XML part{} made well-formed again: {}", f.repaired_parts.len(), if f.repaired_parts.len() == 1 { "" } else { "s" }, f.repaired_parts.join(", "))));
        }
        for fx in &f.package_fixes {
            say!("  {}", paint(DIM, fx));
        }
    }
    if rescue::ascii_undone() {
        say!("  {}", paint(INFO, "The file had gone through an ASCII-mode transfer (every LF made CR LF): undone"));
    }
    if o.crs_restored > 0 {
        say!("  {}", paint(INFO, format!("{} CRs lost to an ASCII-mode transfer put back, each entry's CRC confirming them", commas(o.crs_restored as u64))));
    }
    if let Some(h) = &hint {
        say!("  {}", paint(INFO, format!("Note: {h}")));
    }
    if opts.verbose {
        for n in &o.notes {
            say!("  {}", paint(DIM, format!("Note: {n}")));
        }
    }
    for (n, reason) in o.failed.iter().take(20) {
        say!("  {} {n}: {reason}", paint(BAD, "Failed:"));
    }
    if !opts.verbose && !o.notes.is_empty() {
        say!("  {}", paint(DIM, format!("-v adds every entry and note ({}); --json gives the whole report", o.notes.len())));
    }
    if clean { 0 } else { 2 }
}

fn run_rescue(patterns: &[String], output: Option<PathBuf>, fill: u8, opts: &Opts) -> i32 {
    let archives = expand(patterns);
    let mut rc = 0;
    for path in &archives {
        let lower = path.to_lowercase();
        let plain_gz = lower.ends_with(".gz") && !gz::is_tar_name(path);
        let dir = match &output {
            Some(o) if plain_gz => o.clone(),
            None if plain_gz => PathBuf::from("."),
            Some(o) if archives.len() > 1 => o.join(archive_stem(path)),
            Some(o) => o.clone(),
            None => PathBuf::from(archive_stem(path)),
        };
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        if archives.len() > 1 && !(opts.quiet || opts.json) {
            say!("{path}:");
        }
        rc = rc.max(rescue_one(path, dir, fill, opts));
    }
    rc
}

fn doctor(opts: &Opts) -> i32 {
    say!("gpuzip {} (Rust) · {} CPU threads · libdeflate", env!("CARGO_PKG_VERSION"), cpu_threads());
    match sevenzip::find(opts.sevenzip.as_deref()) {
        Some(exe) => say!("7-Zip:  {} at {}", sevenzip::version(&exe), exe.display()),
        None => say!("7-Zip:  not found; needed for RAR, 7z, tar.* and encrypted entries (https://www.7-zip.org)"),
    }
    match rescue::get() {
        Ok(r) => say!("rescue: gzrescue loaded from {}", r.path),
        Err(e) => say!(
            "rescue: not available ({e}); `gpuzip rescue` needs {} next to {}",
            dylib::file_name("gzrescue"),
            dylib::exe_name()
        ),
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
            say!("gzip:   {} · gpuinflate test decode {}", g.device, if good { "ok" } else { "WRONG OUTPUT" });
            say!("        gpuinflate loaded from {}", g.path);
        }
        Err(e) => {
            say!("gzip:   decoded on the CPU ({e})");
            let places: Vec<String> = inflate::candidates().iter().map(|p| p.display().to_string()).collect();
            if !places.is_empty() {
                say!("        gpuinflate was looked for in: {}", places.join(", "));
            }
        }
    }
    let api = match extract::gpu_api() {
        Ok(a) => a,
        Err(e) => {
            say!("GPU:    not usable ({e}); ZIP files are extracted on the CPU");
            let places: Vec<String> = gpu::nvcomp_candidates().iter().map(|p| p.display().to_string()).collect();
            if !places.is_empty() {
                say!("        nvCOMP was looked for in: {}", places.join(", "));
            }
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
            say!(
                "GPU:    {} · nvCOMP {} · test decode {}",
                api.device,
                api.nvcomp_version,
                if good { "ok" } else { "WRONG OUTPUT" }
            );
            say!("        nvCOMP loaded from {}", api.nvcomp_path);
            if good { 0 } else { 1 }
        }
        Err(e) => {
            say!("GPU:    {} found, but the test decode failed: {e}", api.device);
            1
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let opts = match &cli.command {
        Command::Doctor { opts } | Command::X { opts, .. } | Command::T { opts, .. } | Command::L { opts, .. } | Command::Rescue { opts, .. } => opts,
    };
    match &opts.codepage {
        Some(cp) => {
            if let Err(e) = zip::set_codepage(cp) {
                eprintln!("--codepage: {e}");
                std::process::exit(1);
            }
        }
        None => zip::use_system_codepage(),
    }
    let rc = match &cli.command {
        Command::Doctor { opts } => doctor(opts),
        Command::X { archives, output, opts } => run_all('x', archives, output.clone(), opts),
        Command::T { archives, opts } => run_all('t', archives, None, opts),
        Command::L { archives, opts } => run_all('l', archives, None, opts),
        Command::Rescue { archives, output, fill, max_output, opts } => {
            rescue::set_max_output(*max_output);
            run_rescue(archives, output.clone(), *fill, opts)
        }
    };
    std::process::exit(rc);
}
