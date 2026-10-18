//! gzip files (.gz, .tgz, .tar.gz). Each member's Deflate stream is decoded on the GPU by
//! gpuinflate (large members) or on one CPU thread (zlib-rs), its CRC32 and size checked, and
//! the output streamed as it comes: to one file, or for tar, to an extractor running alongside.

use crate::extract::{Count, GpuInfo, Outcome, Via};
use crate::inflate;
use crate::zip;
use crate::{Mode, Opts};
use crossbeam_channel::{Receiver, bounded};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const GPU_MIN: u64 = 8 << 20; // auto: below this much compressed data one CPU thread is faster
const PIPE_CHUNK: usize = 4 << 20; // the tar extractor gets the output in pieces this big

pub fn is_gzip(d: &[u8]) -> bool {
    d.len() >= 3 && d[0] == 0x1f && d[1] == 0x8b && d[2] == 8
}

/// .tar.gz and .tgz hold a tar archive; other .gz files one file
pub fn is_tar_name(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz")
}

/// The file a plain .gz decompresses to: its name without .gz, like gunzip
pub fn output_name(path: &str) -> String {
    let base = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = base.to_lowercase();
    if lower.ends_with(".gz") && base.len() > 3 {
        base[..base.len() - 3].to_string()
    } else {
        format!("{base}.out")
    }
}

/// The name the decompressed file is written under: output_name, with a `_` before it when it
/// names a Windows device (as safe_relpath does), and then a note that says so
pub fn output_file(path: &str) -> (String, Option<String>) {
    let name = output_name(path);
    if cfg!(windows) && crate::zip::is_windows_device(&name) {
        let to = format!("_{name}");
        let note = format!("{name} is a name this Windows takes for a device (what is written to one is lost): written as {to}");
        return (to, Some(note));
    }
    (name, None)
}

struct Header {
    len: usize,
}

/// A member header (RFC 1952): its length, after checking what can be checked
fn parse_header(d: &[u8]) -> Result<Header, String> {
    if d.len() < 10 || d[0] != 0x1f || d[1] != 0x8b {
        return Err("not a gzip member".into());
    }
    if d[2] != 8 {
        return Err(format!("compression method {} is not Deflate", d[2]));
    }
    let flg = d[3];
    if flg & 0xe0 != 0 {
        return Err("reserved header flags are set".into());
    }
    let short = || "the file ends inside a member header".to_string();
    let mut p = 10;
    if flg & 4 != 0 {
        // FEXTRA
        let n = *d.get(p).ok_or_else(short)? as usize | (*d.get(p + 1).ok_or_else(short)? as usize) << 8;
        p += 2 + n;
    }
    for flag in [8u8, 16] {
        // FNAME, FCOMMENT: zero-terminated
        if flg & flag != 0 {
            let z = d.get(p..).and_then(|s| s.iter().position(|&b| b == 0)).ok_or_else(short)?;
            p += z + 1;
        }
    }
    if flg & 2 != 0 {
        // FHCRC: the low 16 bits of the header's CRC32
        let want = d.get(p..p + 2).ok_or_else(short)?;
        if crc32fast::hash(&d[..p]) as u16 != u16::from_le_bytes([want[0], want[1]]) {
            return Err("header CRC mismatch".into());
        }
        p += 2;
    }
    if p > d.len() {
        return Err(short());
    }
    Ok(Header { len: p })
}

pub enum Fail {
    /// the file is damaged
    Corrupt(String),
    /// the GPU failed or its output failed the CRC check: decode again on the CPU
    Retry(String),
    /// the consumer stopped
    Stopped,
}

#[derive(Default)]
pub struct Decoded {
    pub members: u32,
    pub packed: u64,
    pub out: [u64; 2], // bytes decoded on the GPU, on the CPU
    pub gpu: Option<GpuInfo>,
    pub stats: inflate::Stats, // summed over the GPU's members
    pub notes: Vec<String>,
    pub info: Vec<String>,
}

fn cpu_member(body: &[u8], sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<u64, Fail> {
    let mut z = flate2::Decompress::new(false);
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let (i0, o0) = (z.total_in(), z.total_out());
        let st = z
            .decompress(&body[i0 as usize..], &mut buf, flate2::FlushDecompress::None)
            .map_err(|e| Fail::Corrupt(format!("invalid Deflate data: {e}")))?;
        let n = (z.total_out() - o0) as usize;
        if n > 0 && !sink(&buf[..n]) {
            return Err(Fail::Stopped);
        }
        if st == flate2::Status::StreamEnd {
            return Ok(z.total_in());
        }
        if z.total_in() == i0 && n == 0 {
            return Err(Fail::Corrupt("the file ends inside a member".into()));
        }
    }
}

/// Decode every member of the gzip file `data`, passing the output to `sink` in order.
pub fn decode(data: &[u8], mode: Mode, sink: &mut (dyn FnMut(&[u8]) -> bool + Send), d: &mut Decoded) -> Result<(), Fail> {
    let mut pos = 0usize;
    let mut prefer_gpu = mode != Mode::Cpu;
    while pos < data.len() {
        let rest = &data[pos..];
        if pos > 0 && !is_gzip(rest) {
            if rest.iter().all(|&b| b == 0) {
                d.info.push(format!("{} zero bytes after the last member", rest.len()));
            } else {
                d.notes.push(format!("{} bytes after the last member are not gzip data; ignored", rest.len()));
            }
            break;
        }
        let h = parse_header(rest).map_err(Fail::Corrupt)?;
        let body = &rest[h.len..];
        let want_gpu = match mode {
            Mode::Cpu => false,
            Mode::Gpu => true,
            Mode::Auto => prefer_gpu && body.len() as u64 >= GPU_MIN,
        };
        let gpu = if want_gpu {
            let t = Instant::now();
            match inflate::get() {
                Ok(g) => {
                    let info = d.gpu.get_or_insert_with(GpuInfo::default);
                    if info.device.is_none() {
                        info.device = Some(g.device.clone());
                        info.startup_seconds = Some(t.elapsed().as_secs_f64());
                    }
                    Some(g)
                }
                Err(e) => {
                    prefer_gpu = false;
                    let msg = format!("GPU unavailable ({e}); used the CPU");
                    if mode == Mode::Gpu { d.notes.push(msg) } else { d.info.push(msg) }
                    None
                }
            }
        } else {
            None
        };
        // the last 4 bytes are the size of a single member's output (mod 4 GiB): a hint
        let hint = if pos == 0 && data.len() >= 18 {
            u32::from_le_bytes(data[data.len() - 4..].try_into().unwrap()) as u64
        } else {
            0
        };
        let (mut crc, mut size) = (crc32fast::Hasher::new(), 0u64);
        let used = {
            let mut counted = |c: &[u8]| {
                crc.update(c);
                size += c.len() as u64;
                sink(c)
            };
            match gpu {
                Some(g) => match g.inflate(body, hint, |c| counted(c)) {
                    Ok(s) => {
                        let info = d.gpu.as_mut().unwrap();
                        info.batches += 1;
                        info.busy_seconds += s.seconds;
                        d.stats.out_size += s.out_size;
                        d.stats.segments += s.segments;
                        d.stats.rounds += s.rounds;
                        d.stats.jobs += s.jobs;
                        if s.in_used < GPU_MIN {
                            prefer_gpu = false; // small members: the next ones go to the CPU
                        }
                        s.in_used
                    }
                    Err(inflate::Error::Data(m)) | Err(inflate::Error::Truncated(m)) => return Err(Fail::Corrupt(m)),
                    Err(inflate::Error::Aborted) => return Err(Fail::Stopped),
                    Err(inflate::Error::Gpu(m)) => return Err(Fail::Retry(format!("GPU error: {m}"))),
                },
                None => cpu_member(body, &mut counted)?,
            }
        };
        let t = h.len + used as usize;
        let trailer = rest.get(t..t + 8).ok_or_else(|| Fail::Corrupt("the file ends before a member's trailer".into()))?;
        let want_crc = u32::from_le_bytes(trailer[..4].try_into().unwrap());
        let want_size = u32::from_le_bytes(trailer[4..].try_into().unwrap());
        if crc.finalize() != want_crc || size as u32 != want_size {
            let what = if size as u32 != want_size { "size" } else { "CRC" };
            return Err(if gpu.is_some() {
                Fail::Retry(format!("GPU output failed the {what} check"))
            } else {
                Fail::Corrupt(format!("{what} mismatch in member {}", d.members + 1))
            });
        }
        d.members += 1;
        d.out[if gpu.is_some() { 0 } else { 1 }] += size;
        pos += t + 8;
    }
    if d.members == 0 {
        return Err(Fail::Corrupt("no gzip member".into()));
    }
    d.packed = data.len() as u64;
    Ok(())
}

/// The decompressed bytes, as a reader, for the tar extractor
struct Pipe {
    rx: Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.cur.len() {
            match self.rx.recv() {
                Ok(v) => {
                    self.cur = v;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.cur.len() - self.pos);
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Default)]
struct Untarred {
    files: u64,
    bytes: u64,
    failed: Vec<(String, String)>,
    renamed: Vec<String>, // notes on the names written under another
    error: Option<String>,
}

/// Extract (root given) or read through (none) a tar stream; with `list`, print each entry
fn untar(reader: impl Read, root: Option<&Path>, list: bool) -> Untarred {
    let mut u = Untarred::default();
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_mtime(true);
    ar.set_overwrite(true);
    let entries = match ar.entries() {
        Ok(e) => e,
        Err(x) => {
            u.error = Some(format!("not a tar archive: {x}"));
            return u;
        }
    };
    for entry in entries {
        let mut e = match entry {
            Ok(e) => e,
            Err(x) => {
                u.error = Some(format!("tar archive is damaged: {x}"));
                return u;
            }
        };
        let name = e.path().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| "?".into());
        let file = e.header().entry_type().is_file();
        let size = e.size();
        if list {
            crate::ui::say!("{:>12}  {name}", if file { crate::commas(size) } else { String::new() });
        }
        let kind = e.header().entry_type();
        match root {
            // a name of a Windows device goes where safe_relpath puts it (CON to _CON): under its
            // own name a file's data would go to the device
            Some(r) if zip::device_renamed(&name) && (kind.is_file() || kind.is_dir()) => match zip::safe_relpath(&name) {
                Some(rel) => {
                    let dst = r.join(&rel);
                    let made = dst.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| e.unpack(&dst).map(|_| ()));
                    match made {
                        Ok(()) => u.renamed.push(format!(
                            "{name} uses a name this Windows takes for a device (what is written to one is lost): written as {}",
                            rel.display()
                        )),
                        Err(x) => {
                            u.failed.push((name, x.to_string()));
                            continue;
                        }
                    }
                }
                None => {
                    u.failed.push((name, "path leaves the output folder; skipped".into()));
                    continue;
                }
            },
            Some(r) => match e.unpack_in(r) {
                Ok(true) => {}
                Ok(false) => {
                    u.failed.push((name, "path leaves the output folder; skipped".into()));
                    continue;
                }
                Err(x) => {
                    u.failed.push((name, x.to_string()));
                    continue;
                }
            },
            None => {
                if let Err(x) = io::copy(&mut e, &mut io::sink()) {
                    u.error = Some(format!("tar archive is damaged: {x}"));
                    return u;
                }
            }
        }
        if file {
            u.files += 1;
            u.bytes += size;
        }
    }
    // the padding after the end-of-archive blocks, so the decoder's checks run to the end
    let _ = io::copy(&mut ar.into_inner(), &mut io::sink());
    u
}

/// Extract (out_root given), test or list (`list`) a gzip file
pub fn run(cmd: char, archive: &str, data: &[u8], out_root: Option<PathBuf>, opts: &Opts) -> Outcome {
    let t0 = Instant::now();
    let tar = is_tar_name(archive);
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut notes = Vec::new();
    let mut renamed: Vec<String>;
    let mut mode = opts.mode;
    let mut files = 0u64;
    let mut retried = 0;
    let mut d;
    loop {
        d = Decoded::default();
        let result = if tar {
            let (tx, rx) = bounded::<Vec<u8>>(8);
            let root = out_root.clone();
            let list = cmd == 'l';
            let extractor = std::thread::spawn(move || {
                if let Some(r) = &root {
                    let _ = std::fs::create_dir_all(r);
                }
                untar(Pipe { rx, cur: Vec::new(), pos: 0 }, root.as_deref(), list)
            });
            let mut sink = |c: &[u8]| c.chunks(PIPE_CHUNK).all(|p| tx.send(p.to_vec()).is_ok());
            let r = decode(data, mode, &mut sink, &mut d);
            drop(tx); // the extractor sees the end
            let u = extractor.join().unwrap_or_default();
            files = u.files;
            failed = u.failed;
            renamed = u.renamed;
            match (r, u.error) {
                (Ok(()), Some(e)) => Err(Fail::Corrupt(e)),
                (Err(Fail::Stopped), Some(e)) => Err(Fail::Corrupt(e)),
                (r, _) => r,
            }
        } else {
            let (out_name, note) = output_file(archive);
            renamed = out_root.is_some().then_some(note).flatten().into_iter().collect();
            let target = out_root.as_ref().map(|r| r.join(out_name));
            let mut file = match &target {
                Some(p) => {
                    let _ = std::fs::create_dir_all(p.parent().unwrap());
                    match File::create(p) {
                        Ok(f) => Some(f),
                        Err(x) => {
                            failed.push((p.display().to_string(), x.to_string()));
                            break;
                        }
                    }
                }
                None => None,
            };
            let mut write_err = None;
            let mut sink = |c: &[u8]| match file.as_mut().map(|f| f.write_all(c)) {
                Some(Err(x)) => {
                    write_err = Some(x.to_string());
                    false
                }
                _ => true,
            };
            let r = decode(data, mode, &mut sink, &mut d);
            if let (Some(f), Ok(m)) = (&file, std::fs::metadata(archive).and_then(|m| m.modified())) {
                let _ = f.set_modified(m);
            }
            files = 1;
            match (r, write_err) {
                (Err(Fail::Stopped), Some(e)) => Err(Fail::Corrupt(format!("could not write the output: {e}"))),
                (r, _) => r,
            }
        };
        match result {
            Ok(()) => break,
            Err(Fail::Retry(why)) if mode != Mode::Cpu => {
                notes.push(format!("{why}; decoded again on the CPU"));
                retried = 1;
                mode = Mode::Cpu;
            }
            Err(Fail::Retry(why)) | Err(Fail::Corrupt(why)) => {
                failed.push((archive.to_string(), why));
                break;
            }
            Err(Fail::Stopped) => {
                failed.push((archive.to_string(), "stopped".into()));
                break;
            }
        }
    }
    notes.extend(d.notes.drain(..));
    notes.extend(renamed);
    let mut info = std::mem::take(&mut d.info);
    if d.members > 1 {
        info.push(format!("{} gzip members", d.members));
    }
    if d.stats.segments > 0 {
        info.push(format!(
            "GPU inflate: {} segments, {} decode rounds, {} decoders",
            d.stats.segments,
            d.stats.rounds,
            crate::commas(d.stats.jobs)
        ));
    }
    let mut counts = [Count::default(); 3];
    for (via, bytes) in [(Via::Gpu, d.out[0]), (Via::Cpu, d.out[1])] {
        counts[via as usize].bytes = bytes;
    }
    // the files go to whichever decoded most of the bytes
    counts[if d.out[0] >= d.out[1] && d.out[0] > 0 { 0 } else { 1 }].files = if failed.is_empty() || tar { files } else { 0 };
    Outcome {
        seconds: t0.elapsed().as_secs_f64(),
        counts,
        failed,
        notes,
        info,
        gpu: d.gpu,
        retried,
        stream: Some((d.packed, d.out[0] + d.out[1])),
    }
}
