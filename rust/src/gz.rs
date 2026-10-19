//! gzip files (.gz, .tgz, .tar.gz). Each member's Deflate stream is decoded on the GPU by
//! gpuinflate (large members) or on one CPU thread (zlib-rs), its CRC32 and size checked, and
//! the output streamed as it comes: to one file, or for tar, to an extractor running alongside.

use crate::extract::{Count, GpuInfo, Outcome, Via};
use crate::inflate;
use crate::outfile::OutFile;
use crate::zip;
use crate::{Mode, Opts};
use crossbeam_channel::{Receiver, bounded};
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
    name_notes: Vec<String>, // on names: written under another, or two names one file here
    error: Option<String>,
}

/// Where a tar entry goes below the output folder: where unpack_in would put it, or with a `_`
/// before a part that names a Windows device (safe_relpath). None: it would leave the folder.
/// An empty path (only slashes or dots) is Some(""), which unpack_in writes nothing for either.
fn tar_path<R: Read>(e: &mut tar::Entry<'_, R>, name: &str) -> io::Result<Option<PathBuf>> {
    if zip::device_renamed(name) {
        return Ok(zip::safe_relpath(name));
    }
    let mut rel = PathBuf::new();
    for c in e.path()?.components() {
        match c {
            std::path::Component::Prefix(..) | std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => return Ok(None),
            std::path::Component::Normal(p) => rel.push(p),
        }
    }
    Ok(Some(rel))
}

/// Create `dir` and the folders above it that are missing, each only once the folder it goes in
/// is inside the output folder with links resolved (a link an earlier entry made must not carry
/// a file out of it), as the tar crate's unpack_in does
fn make_dirs_inside(canon_root: &Path, dir: &Path) -> io::Result<()> {
    let inside = |p: &Path| -> io::Result<()> {
        if p.canonicalize()?.starts_with(canon_root) {
            Ok(())
        } else {
            Err(io::Error::other(format!("{} lies outside the output folder", p.display())))
        }
    };
    let mut missing = Vec::new();
    let mut a = dir;
    while a.symlink_metadata().is_err() {
        missing.push(a);
        match a.parent() {
            Some(p) => a = p,
            None => break,
        }
    }
    for m in missing.into_iter().rev() {
        if let Some(p) = m.parent() {
            inside(p)?;
        }
        std::fs::create_dir_all(m)?;
    }
    inside(dir)
}

/// A tar entry's permissions and time, as the tar crate sets them (a time of 0 as 1)
fn tar_mode_mtime<R: Read>(e: &tar::Entry<'_, R>) -> (u32, Option<std::time::SystemTime>) {
    let mtime = e.header().mtime().ok().map(|t| std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(1)));
    (e.header().mode().unwrap_or(0o644), mtime)
}

/// A tar's regular file, written whole or not at all, with its time and permissions (on
/// Windows, read-only when the owner may not write)
fn write_tar_file(dst: &Path, src: &mut impl Read, mode: u32, mtime: Option<std::time::SystemTime>) -> io::Result<()> {
    let mut f = OutFile::create(dst)?;
    io::copy(src, &mut f)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.file().set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
    }
    f.finish(mtime)?;
    #[cfg(windows)]
    if mode & 0o200 == 0 {
        let mut p = std::fs::metadata(dst)?.permissions();
        p.set_readonly(true);
        std::fs::set_permissions(dst, p)?;
    }
    Ok(())
}

/// The note on a tar's files whose names differ only in case from one before them, where file
/// names ignore case (Windows, macOS): pairs of (the one before, the one written over it)
pub fn case_note(pairs: &[(String, String)]) -> Option<String> {
    if pairs.is_empty() {
        return None;
    }
    let shown: Vec<String> = pairs.iter().take(5).map(|(a, b)| format!("{b} over {a}")).collect();
    Some(format!(
        "{} only in case from one before, and file names here ignore case: each was written over it, the last copy kept ({}{})",
        if pairs.len() == 1 { "1 file differs".to_string() } else { format!("{} files differ", pairs.len()) },
        shown.join(", "),
        if pairs.len() > 5 { ", ..." } else { "" }
    ))
}

/// Files of a tar up to this size are read whole and written by TarWriters; larger ones by the
/// reader as they come
const TAR_SMALL: u64 = 256 << 10;

enum TarJob {
    File { name: String, dst: PathBuf, data: Vec<u8>, mode: u32, mtime: Option<std::time::SystemTime> },
    Wait(crossbeam_channel::Sender<()>), // answered once every file sent before it is written
}

/// The small files of a tar, written on a pool of threads: where there are many, creating them
/// is most of the time (30,000 files of a tar.gz on Windows: 0.37 s to decode, 13.4 s to write
/// one after another; the same files from a ZIP, written on 16 threads, 4.2 s). The same path
/// (in any case where names ignore it) always goes to the same thread, in order, so its last
/// copy wins, as in a tar; a link waits for every file sent before it, which it may name.
struct TarWriters {
    txs: Vec<crossbeam_channel::Sender<TarJob>>,
    handles: Vec<std::thread::JoinHandle<Vec<(String, String, u64)>>>,
}

impl TarWriters {
    fn new(n: usize) -> TarWriters {
        let (mut txs, mut handles) = (Vec::new(), Vec::new());
        for _ in 0..n {
            let (tx, rx) = bounded::<TarJob>(16); // at most 16 x 256 KB waiting on a thread
            txs.push(tx);
            handles.push(std::thread::spawn(move || {
                let mut failed = Vec::new();
                for job in rx {
                    match job {
                        TarJob::File { name, dst, data, mode, mtime } => {
                            if let Err(x) = write_tar_file(&dst, &mut data.as_slice(), mode, mtime) {
                                failed.push((name, x.to_string(), data.len() as u64));
                            }
                        }
                        TarJob::Wait(done) => {
                            let _ = done.send(());
                        }
                    }
                }
                failed
            }));
        }
        TarWriters { txs, handles }
    }

    /// The thread a path goes to
    fn thread_of(&self, dst: &Path) -> usize {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let s = dst.to_string_lossy();
        if cfg!(any(windows, target_os = "macos")) { s.to_lowercase().hash(&mut h) } else { s.hash(&mut h) }
        (h.finish() % self.txs.len() as u64) as usize
    }

    /// Until every file sent so far to thread `which` (None: to all) is written
    fn wait(&self, which: Option<usize>) {
        let (done, back) = crossbeam_channel::unbounded();
        let mut asked = 0;
        for (i, tx) in self.txs.iter().enumerate() {
            if which.is_none_or(|w| w == i) && tx.send(TarJob::Wait(done.clone())).is_ok() {
                asked += 1;
            }
        }
        for _ in 0..asked {
            let _ = back.recv();
        }
    }

    /// Every file written; those that failed, with their sizes
    fn finish(self) -> Vec<(String, String, u64)> {
        drop(self.txs);
        self.handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    }
}

/// Windows: a tar's symbolic links, made once what they name is written. Windows makes real
/// ones only for an administrator or in Developer Mode (else error 1314: 63 links of
/// linux-6.6.tar.gz failed so). Then a link to a file is written as a hard link to it, which
/// needs no privilege, or a copy; a link to a folder as a copy of it, all copies within
/// `budget` bytes (the archive's own files, 64 MB at least: links cannot make an extraction
/// much more than twice as large). A link that leads out of the output folder, names nothing, or whose name a file
/// or folder of the archive already took, is not made. Returns the notes that say so.
#[cfg(windows)]
fn make_links(root: &Path, canon_root: &Path, links: Vec<(String, PathBuf, PathBuf)>, mut budget: u64) -> Vec<String> {
    let (mut real, mut hard, mut copies) = (0usize, 0usize, 0usize);
    let (mut outside, mut taken, mut too_big) = (Vec::new(), Vec::new(), Vec::new());
    let mut pending = links;
    // a link may name another link: what it names is made on a later round
    loop {
        let before = pending.len();
        let mut left = Vec::new();
        for (name, rel, target) in pending {
            let dst = root.join(&rel);
            // out of the output folder by its very path: absolute, or more .. than folders above it
            let mut depth = rel.components().count() as i64 - 1;
            let leaves = target.has_root()
                || target.is_absolute()
                || target.components().any(|c| {
                    depth += match c {
                        std::path::Component::ParentDir => -1,
                        std::path::Component::Normal(_) => 1,
                        _ => 0,
                    };
                    depth < 0
                });
            if leaves {
                outside.push(name);
                continue;
            }
            let Ok(canon) = dst.parent().unwrap_or(root).join(&target).canonicalize() else {
                left.push((name, rel, target));
                continue;
            };
            if !canon.starts_with(canon_root) || make_dirs_inside(canon_root, dst.parent().unwrap_or(root)).is_err() {
                outside.push(name);
            } else if dst.symlink_metadata().is_ok() {
                taken.push(name);
            } else if canon.is_dir() {
                if std::os::windows::fs::symlink_dir(&target, &dst).is_ok() {
                    real += 1;
                } else if copy_tree(&canon, &dst, &mut budget) {
                    copies += 1;
                } else {
                    too_big.push(name);
                }
            } else if std::os::windows::fs::symlink_file(&target, &dst).is_ok() {
                real += 1;
            } else if std::fs::hard_link(&canon, &dst).is_ok() {
                hard += 1;
            } else if copy_tree(&canon, &dst, &mut budget) {
                copies += 1;
            } else {
                too_big.push(name);
            }
        }
        if left.is_empty() || left.len() == before {
            pending = left;
            break;
        }
        pending = left;
    }
    let list = |v: &[String]| v.iter().take(5).cloned().collect::<Vec<_>>().join(", ") + if v.len() > 5 { ", ..." } else { "" };
    let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut notes = Vec::new();
    if hard + copies > 0 {
        notes.push(format!(
            "{} not made as such (Windows lets only an administrator, or Developer Mode, make them): written as {} to \
             the files they name and {}{}",
            count(hard + copies, "symbolic link", "symbolic links"),
            count(hard, "hard link", "hard links"),
            count(copies, "copy", "copies"),
            if real > 0 { format!("; {real} made") } else { String::new() }
        ));
    }
    let nothing: Vec<String> = pending.into_iter().map(|(n, _, _)| n).collect();
    for (v, what) in [
        (&outside, "leading out of the output folder"),
        (&nothing, "naming nothing the archive holds"),
        (&taken, "whose name a file or folder of the archive took"),
        (&too_big, "whose copies would pass the size of the archive's own files (64 MB at least)"),
    ] {
        if !v.is_empty() {
            notes.push(format!("{} {what}, not made: {}", count(v.len(), "link", "links"), list(v)));
        }
    }
    notes
}

/// Copy a file, or a folder and all in it, from `src` to `dst` if it fits in `budget` bytes
/// (which it then takes from). What is copied is listed first: a link to a folder above itself
/// is copied into itself, and must not copy its own copy on. Links inside are not followed.
#[cfg(windows)]
fn copy_tree(src: &Path, dst: &Path, budget: &mut u64) -> bool {
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![PathBuf::new()];
    if src.is_file() {
        files.push((PathBuf::new(), src.metadata().map_or(0, |m| m.len())));
        stack.clear();
    }
    while let Some(rel) = stack.pop() {
        dirs.push(rel.clone());
        let Ok(read) = std::fs::read_dir(src.join(&rel)) else { return false };
        for d in read.flatten() {
            let Ok(t) = d.file_type() else { continue };
            if t.is_symlink() {
                continue;
            }
            let r = rel.join(d.file_name());
            if t.is_dir() {
                stack.push(r);
            } else {
                files.push((r, d.metadata().map_or(0, |m| m.len())));
            }
        }
    }
    let total: u64 = files.iter().map(|(_, n)| n).sum();
    if total > *budget {
        return false;
    }
    *budget -= total;
    for d in &dirs {
        let _ = std::fs::create_dir_all(dst.join(d));
    }
    files.iter().all(|(r, _)| {
        let (from, to) = if r.as_os_str().is_empty() { (src.to_path_buf(), dst.to_path_buf()) } else { (src.join(r), dst.join(r)) };
        std::fs::copy(from, to).is_ok()
    })
}

/// Extract (root given) or read through (none) a tar stream; with `list`, print each entry.
/// `writers`: threads that write its small files.
fn untar(reader: impl Read, root: Option<&Path>, list: bool, writers: usize) -> Untarred {
    let mut u = Untarred::default();
    let canon_root = root.map_or(Ok(PathBuf::new()), |r| r.canonicalize());
    let pool = root.map(|_| TarWriters::new(writers.max(1)));
    // folders already made or found inside the output folder; forgotten at a link, which could
    // make a path lead elsewhere
    let mut inside: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut by_case: std::collections::HashMap<String, String> = std::collections::HashMap::new(); // a file's path in lower case: as written
    let mut case_pairs: Vec<(String, String)> = Vec::new(); // (the one before, the one written over it)
    let mut folders: Vec<(PathBuf, Option<std::time::SystemTime>, u32)> = Vec::new(); // their times and modes, given last
    #[allow(unused_mut)] // filled on Windows only
    let mut links: Vec<(String, PathBuf, PathBuf)> = Vec::new(); // symbolic links to make last: name, path, target
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
                break;
            }
        };
        let name = e.path().map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| "?".into());
        let file = e.header().entry_type().is_file();
        let size = e.size();
        if list {
            crate::ui::say!("{:>12}  {name}", if file { crate::commas(size) } else { String::new() });
        }
        let kind = e.header().entry_type();
        let device = zip::device_renamed(&name);
        match root {
            // regular files are written here, to take their names only once whole (OutFile), and
            // folders made, their modes and times given last (set_folders)
            Some(r) if kind.is_file() || kind.is_dir() => match tar_path(&mut e, &name) {
                Ok(Some(rel)) if rel.as_os_str().is_empty() => {}
                Ok(Some(rel)) => {
                    let dst = r.join(&rel);
                    // names that differ only in case are one file where names ignore it: the
                    // last copy is written over the other (13 pairs in linux-6.6.tar.gz)
                    if kind.is_file() && cfg!(any(windows, target_os = "macos")) {
                        let exact = rel.to_string_lossy().replace('\\', "/");
                        if let Some(prev) = by_case.insert(exact.to_lowercase(), exact.clone()) {
                            if prev != exact {
                                case_pairs.push((prev, exact));
                            }
                        }
                    }
                    let dir = if kind.is_dir() { dst.clone() } else { dst.parent().unwrap_or(r).to_path_buf() };
                    let made = match &canon_root {
                        Ok(_) if inside.contains(&dir) => Ok(()),
                        Ok(c) => make_dirs_inside(c, &dir).map(|_| {
                            inside.insert(dir);
                        }),
                        Err(x) => Err(io::Error::new(x.kind(), x.to_string())),
                    };
                    let made = made.and_then(|_| {
                        if kind.is_dir() {
                            return Ok(());
                        }
                        let (mode, mtime) = tar_mode_mtime(&e);
                        let pool = pool.as_ref().unwrap();
                        let thread = pool.thread_of(&dst);
                        if size <= TAR_SMALL {
                            let mut data = Vec::with_capacity(size as usize);
                            e.read_to_end(&mut data)?;
                            let job = TarJob::File { name: name.clone(), dst: dst.clone(), data, mode, mtime };
                            return pool.txs[thread].send(job).map_err(|_| io::Error::other("a writer thread stopped"));
                        }
                        // a large one as it comes, once an earlier copy of the same path is written
                        pool.wait(Some(thread));
                        write_tar_file(&dst, &mut e, mode, mtime)
                    });
                    if made.is_ok() && kind.is_dir() {
                        let (mode, mtime) = tar_mode_mtime(&e);
                        folders.push((dst.clone(), mtime, mode & 0o777));
                    }
                    match made {
                        Ok(()) if device => u.name_notes.push(format!(
                            "{name} uses a name this Windows takes for a device (what is written to one is lost): written as {}",
                            rel.display()
                        )),
                        Ok(()) => {}
                        Err(x) => {
                            u.failed.push((name, x.to_string()));
                            continue;
                        }
                    }
                }
                Ok(None) => {
                    u.failed.push((name, "path leaves the output folder; skipped".into()));
                    continue;
                }
                Err(x) => {
                    u.failed.push((name, x.to_string()));
                    continue;
                }
            },
            Some(r) => {
                if cfg!(windows) && kind == tar::EntryType::Symlink {
                    // made at the end, once what it names is written (make_links)
                    match (tar_path(&mut e, &name), e.link_name()) {
                        (Ok(Some(rel)), Ok(Some(target))) if !rel.as_os_str().is_empty() => links.push((name, rel, target.into_owned())),
                        _ => u.failed.push((name, "a link with no path or no target; skipped".into())),
                    }
                    continue;
                }
                if !kind.is_dir() {
                    // a link, or anything else unpack_in makes: once every file before it is written
                    if let Some(p) = &pool {
                        p.wait(None);
                    }
                    inside.clear();
                }
                match e.unpack_in(r) {
                    Ok(true) => {}
                    Ok(false) => {
                        u.failed.push((name, "path leaves the output folder; skipped".into()));
                        continue;
                    }
                    Err(x) => {
                        u.failed.push((name, x.to_string()));
                        continue;
                    }
                }
            }
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
    u.name_notes.extend(case_note(&case_pairs));
    // the files still being written, and those that could not be
    for (name, x, size) in pool.map(TarWriters::finish).unwrap_or_default() {
        u.failed.push((name, x));
        u.files = u.files.saturating_sub(1);
        u.bytes = u.bytes.saturating_sub(size);
    }
    #[cfg(windows)]
    if let (Some(r), Ok(c)) = (root, &canon_root) {
        u.name_notes.extend(make_links(r, c, std::mem::take(&mut links), u.bytes.max(64 << 20)));
    }
    // the folders' times, now that what goes in them is written
    crate::outfile::set_folders(folders);
    // the padding after the end-of-archive blocks, so the decoder's checks run to the end
    if u.error.is_none() {
        let _ = io::copy(&mut ar.into_inner(), &mut io::sink());
    }
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
            let writers = opts.io_threads.unwrap_or_else(|| crate::cpu_threads().min(32));
            let extractor = std::thread::spawn(move || {
                if let Some(r) = &root {
                    let _ = std::fs::create_dir_all(r);
                }
                untar(Pipe { rx, cur: Vec::new(), pos: 0 }, root.as_deref(), list, writers)
            });
            let mut sink = |c: &[u8]| c.chunks(PIPE_CHUNK).all(|p| tx.send(p.to_vec()).is_ok());
            let r = decode(data, mode, &mut sink, &mut d);
            drop(tx); // the extractor sees the end
            let u = extractor.join().unwrap_or_default();
            files = u.files;
            failed = u.failed;
            renamed = u.name_notes;
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
                    match OutFile::create(p) {
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
            files = 1;
            let r = match (r, write_err) {
                (Err(Fail::Stopped), Some(e)) => Err(Fail::Corrupt(format!("could not write the output: {e}"))),
                (r, _) => r,
            };
            // whole and checked: the output takes its name; else it goes (a damaged .gz: rescue)
            match (r, file) {
                (Ok(()), Some(f)) => f
                    .finish(std::fs::metadata(archive).and_then(|m| m.modified()).ok())
                    .map_err(|x| Fail::Corrupt(format!("could not write the output: {x}"))),
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
