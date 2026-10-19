//! Recovering damaged gzip files: gzrescue (gzrescue.dll, libgzrescue.so, libgzrescue.dylib;
//! built from gpuinflate/rescue.cpp, CPU only) decodes a damaged .gz block by block and says, byte by byte, what is exact, what lies in a
//! damaged block and what cannot be known; this module loads it, and extracts what a damaged
//! .tar.gz still holds, skipping to the next valid tar header after a destroyed region.

use crate::{dylib, zip};
use libloading::Library;
use std::ffi::{CStr, c_char, c_void};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ABI_VERSION: u32 = 4;
pub const UNKNOWN: u8 = 0;
pub const EXACT: u8 = 1;
pub const SUSPECT: u8 = 2;
pub const INFERRED: u8 = 3;

#[repr(C)]
struct Options {
    size: u32,
    fill: u8,
    no_infer: u8,
    cr_lost: u8,
    crs_expected: u8,
    max_output: u64,
    cr_budget: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PieceRaw {
    out_offset: u64,
    length: u64,
    decoded: u64,
    known: u64,
    in_byte_start: u64,
    in_byte_end: u64,
    suspect: i32,
    status: i32,
    blocks: u32,
    symbols: u32,
    why: *const c_char,
}

/// gr_stats
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub input_size: u64,
    pub output_size: u64,
    pub exact_bytes: u64,
    pub suspect_bytes: u64,
    pub unknown_bytes: u64,
    pub headers: u32,
    pub pieces: u32,
    pub damaged: u32,
    pub members: u32,
    /// 1 ok, 0 mismatch, -1 not checkable
    pub crc_check: i32,
    /// output length minus the trailer's, when every block stayed in step
    pub length_off: i64,
    pub truncated: i32,
    pub header_damaged: i32,
    /// byte after the last decoded block (a ZIP's data descriptor follows it)
    pub input_end: u64,
    /// bytes that copied from data lost with a damaged block, their values inferred
    pub inferred_bytes: u64,
    pub ascii_undone: i32,
    /// CRs put back where a CR LF to LF transfer had removed them (judged by the CRC)
    pub cr_restored: u32,
    /// input bytes the search for them decoded
    pub cr_decoded: u64,
    /// some damage kept the decoder in step (a flipped bit, a lost CR) and no checksum vouches for
    /// the rest: damage like it may lie where nothing shows
    pub hidden_damage: i32,
}

#[derive(Clone, Debug)]
pub struct Piece {
    pub out_offset: u64,
    pub length: u64,
    pub decoded: u64,
    pub known: u64,
    pub in_byte_start: u64,
    pub in_byte_end: u64,
    pub suspect: bool,
    pub why: String,
    pub blocks: u32,
    pub symbols: u32,
}

pub struct Report {
    pub stats: Stats,
    pub pieces: Vec<Piece>,
    pub notes: Vec<String>,
}

impl Report {
    /// Nothing was damaged, lost or in doubt
    pub fn clean(&self) -> bool {
        self.stats.damaged == 0 && self.stats.unknown_bytes == 0 && self.stats.crc_check != 0 && self.stats.length_off == 0
            && self.stats.truncated == 0
    }
}

type Sink = unsafe extern "C" fn(*mut c_void, *const u8, *const u8, usize) -> i32;
type SinkFn<'a> = &'a mut dyn FnMut(&[u8], &[u8]) -> bool;

pub struct Rescue {
    _lib: Library,
    pub path: String,
    rescue: unsafe extern "C" fn(*const u8, u64, *const Options, Option<Sink>, *mut c_void, *mut *mut c_void) -> i32,
    rescue_raw: unsafe extern "C" fn(*const u8, u64, u64, i32, u32, *const Options, Option<Sink>, *mut c_void, *mut *mut c_void) -> i32,
    stats: unsafe extern "C" fn(*const c_void) -> *const Stats,
    pieces: unsafe extern "C" fn(*const c_void, *mut *const PieceRaw) -> u32,
    notes: unsafe extern "C" fn(*const c_void, *mut *const *const c_char) -> u32,
    free: unsafe extern "C" fn(*mut c_void),
}

unsafe impl Send for Rescue {}
unsafe impl Sync for Rescue {}

/// Places gzrescue is looked for, in order.
pub fn candidates() -> Vec<PathBuf> {
    dylib::candidates("GPUZIP_RESCUE_DIR", &[dylib::file_name("gzrescue")])
}

/// The library, loaded once per process on first use.
pub fn get() -> Result<&'static Rescue, String> {
    static LIB: OnceLock<Result<Rescue, String>> = OnceLock::new();
    LIB.get_or_init(Rescue::load).as_ref().map_err(|e| e.clone())
}

unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
    unsafe {
        lib.get::<T>(name)
            .map(|s| *s)
            .map_err(|e| format!("{} missing: {e}", String::from_utf8_lossy(&name[..name.len() - 1])))
    }
}

unsafe extern "C" fn trampoline(user: *mut c_void, data: *const u8, mask: *const u8, len: usize) -> i32 {
    let f = unsafe { &mut *(user as *mut SinkFn) };
    let (d, m) = unsafe { (std::slice::from_raw_parts(data, len), std::slice::from_raw_parts(mask, len)) };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(d, m))) {
        Ok(true) => 0,
        _ => 1,
    }
}

impl Rescue {
    fn load() -> Result<Rescue, String> {
        unsafe {
            let name = dylib::file_name("gzrescue");
            let (lib, path) = dylib::load_first(candidates()).ok_or_else(|| {
                format!("{name} not found: put it next to {} or set GPUZIP_RESCUE_DIR", dylib::exe_name())
            })?;
            let abi: unsafe extern "C" fn() -> u32 = sym(&lib, b"gr_abi_version\0")?;
            if abi() != ABI_VERSION {
                return Err(format!("{name} speaks version {} of the interface, this gpuzip {ABI_VERSION}", abi()));
            }
            Ok(Rescue {
                rescue: sym(&lib, b"gr_rescue\0")?,
                rescue_raw: sym(&lib, b"gr_rescue_raw\0")?,
                stats: sym(&lib, b"gr_result_stats\0")?,
                pieces: sym(&lib, b"gr_result_pieces\0")?,
                notes: sym(&lib, b"gr_result_notes\0")?,
                free: sym(&lib, b"gr_free\0")?,
                _lib: lib,
                path,
            })
        }
    }

    /// Recover the gzip data in `input`, passing the output in order to `sink` with its mask
    /// (EXACT, SUSPECT or UNKNOWN per byte); `sink` returns false to stop.
    pub fn rescue(&self, input: &[u8], fill: u8, sink: impl FnMut(&[u8], &[u8]) -> bool) -> Result<Report, String> {
        self.run(input, None, fill, 0, 0, sink)
    }

    /// The same for a raw Deflate stream (a ZIP entry) that starts at `input[0]`; `expected`:
    /// the original's size and, if known, its CRC-32, standing in for the gzip trailer. `crs`:
    /// when the file lost its CRs to an ASCII transfer, how many this entry lost if known (its
    /// stored size against the room it has): 1-8, 255 none, 0 unknown. `cr_budget`: the input
    /// bytes the search for them may decode (0: the library's own limit, 1 GiB); the report says
    /// how many it did (stats.cr_decoded).
    pub fn rescue_raw(&self, input: &[u8], expected: (Option<u64>, Option<u32>), fill: u8, crs: u8, cr_budget: u64, sink: impl FnMut(&[u8], &[u8]) -> bool) -> Result<Report, String> {
        self.run(input, Some(expected), fill, crs, cr_budget, sink)
    }

    fn run(&self, input: &[u8], raw: Option<(Option<u64>, Option<u32>)>, fill: u8, crs_expected: u8, cr_budget: u64, mut sink: impl FnMut(&[u8], &[u8]) -> bool) -> Result<Report, String> {
        let cr_lost = if CR_LOST.load(std::sync::atomic::Ordering::Relaxed) { 1 } else { 0 };
        let max_output = max_output(input.len() as u64);
        let opt = Options { size: std::mem::size_of::<Options>() as u32, fill, no_infer: 0, cr_lost, crs_expected, max_output, cr_budget };
        let mut f: SinkFn = &mut sink;
        let user = &mut f as *mut SinkFn as *mut c_void;
        let mut res: *mut c_void = std::ptr::null_mut();
        let rc = unsafe {
            match raw {
                None => (self.rescue)(input.as_ptr(), input.len() as u64, &opt, Some(trampoline), user, &mut res),
                Some((size, crc)) => (self.rescue_raw)(
                    input.as_ptr(),
                    input.len() as u64,
                    size.unwrap_or(u64::MAX),
                    crc.is_some() as i32,
                    crc.unwrap_or(0),
                    &opt,
                    Some(trampoline),
                    user,
                    &mut res,
                ),
            }
        };
        if res.is_null() {
            return Err(match rc {
                1 => "no Deflate data could be recognised".to_string(),
                4 => "out of memory".to_string(),
                5 => format!(
                    "the output would be larger than {} ({:.0} times the input: a decompression bomb, or data that compresses \
                     extremely well); --max-output SIZE raises the limit, --max-output 0 removes it",
                    crate::human(max_output),
                    max_output as f64 / input.len().max(1) as f64
                ),
                6 => "gzrescue failed: a block decoded differently the second time (please report this file)".to_string(),
                _ => format!("gzrescue failed ({rc})"),
            });
        }
        let report = unsafe {
            let stats = *(self.stats)(res);
            let mut raw: *const PieceRaw = std::ptr::null();
            let np = (self.pieces)(res, &mut raw) as usize;
            let pieces = (0..np)
                .map(|k| {
                    let p = *raw.add(k);
                    Piece {
                        out_offset: p.out_offset,
                        length: p.length,
                        decoded: p.decoded,
                        known: p.known,
                        in_byte_start: p.in_byte_start,
                        in_byte_end: p.in_byte_end,
                        suspect: p.suspect != 0,
                        why: if p.why.is_null() { String::new() } else { CStr::from_ptr(p.why).to_string_lossy().into_owned() },
                        blocks: p.blocks,
                        symbols: p.symbols,
                    }
                })
                .collect();
            let mut nraw: *const *const c_char = std::ptr::null();
            let nn = (self.notes)(res, &mut nraw) as usize;
            let notes = (0..nn).map(|k| CStr::from_ptr(*nraw.add(k)).to_string_lossy().into_owned()).collect();
            (self.free)(res);
            Report { stats, pieces, notes }
        };
        if rc == 2 {
            return Err("stopped while writing the output".into());
        }
        Ok(report)
    }
}

/// The most output one rescue may write, set by --max-output (bytes; 0 no limit); unset, the
/// default: 100 times the input, at least 4 GiB. Deflate expands at most 1032 times; real data
/// rarely reaches 100 (text 3-10, logs up to 50), a decompression bomb does.
static MAX_OUTPUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

pub fn set_max_output(v: Option<u64>) {
    MAX_OUTPUT.store(v.unwrap_or(u64::MAX), std::sync::atomic::Ordering::Relaxed);
}

fn max_output(input: u64) -> u64 {
    match MAX_OUTPUT.load(std::sync::atomic::Ordering::Relaxed) {
        u64::MAX => input.saturating_mul(100).max(4 << 30),
        v => v,
    }
}

// ---------------------------------------------------------------- ASCII-mode transfers

/// LF bytes, and how many have a CR right before them
pub fn crlf_counts(data: &[u8]) -> (usize, usize) {
    let mut lf = 0;
    let mut crlf = 0;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            lf += 1;
            if i > 0 && data[i - 1] == b'\r' {
                crlf += 1;
            }
        }
    }
    (lf, crlf)
}

/// When nearly every LF has a CR before it (an ASCII-mode transfer from Unix to Windows; in
/// compressed data one LF in 256 does by chance), the data with one CR removed before each LF.
pub fn undo_lf_to_crlf(data: &[u8]) -> Option<Vec<u8>> {
    let (lf, crlf) = crlf_counts(data);
    if lf < 16 || (crlf as f64) < 0.98 * lf as f64 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() - crlf);
    for (i, &b) in data.iter().enumerate() {
        if b == b'\r' && data.get(i + 1) == Some(&b'\n') {
            continue;
        }
        out.push(b);
    }
    Some(out)
}

/// The other way, Windows to Unix, every CR LF became LF. Compressed data is close to random,
/// so CR LF occurs about once in 65,536 positions, and a file with none may have gone through
/// such a transfer. Weak evidence for a small file, but it only starts a search for the CRs a
/// damaged ZIP entry wants back, and the entry's CRC judges every one: intact entries never
/// enter it.
pub fn looks_cr_stripped(data: &[u8]) -> bool {
    let (lf, pairs) = crlf_counts(data);
    pairs == 0 && lf >= 16
}

/// Whether the file being rescued lost its CRs that way: its ZIP entries, too short to tell on
/// their own, are then searched for the CRs their CRC wants back
static CR_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_cr_lost(v: bool) {
    CR_LOST.store(v, std::sync::atomic::Ordering::Relaxed);
}

static ASCII_UNDONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the file being rescued had its ASCII-mode conversion undone (for the reports)
pub fn set_ascii_note(v: bool) {
    ASCII_UNDONE.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn ascii_undone() -> bool {
    ASCII_UNDONE.load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------- tar, with damage

#[derive(Default)]
pub struct TarOutcome {
    pub intact: u64,
    /// files whose bytes all lie in exact or damaged-but-flagged blocks (right, or wrong from
    /// some point inside the block), or that the next header does not follow where it should
    pub suspect: Vec<String>,
    /// files complete, but with bytes inferred (copies of data lost with a damaged block): likely
    /// right, not certain
    pub inferred: Vec<String>,
    /// files with unknown bytes (written as the fill byte) or cut short
    pub partial: Vec<String>,
    /// regions with no readable tar header: (offset, bytes)
    pub skipped: Vec<(u64, u64)>,
    /// headers with unknown bytes made whole from the header before them, confirmed by their checksum
    pub repaired: u64,
    /// files that would be intact, but the stream's checksum fails and the damage could not be
    /// located (inside raw stored data, or a flipped bit that kept the decoder in step): any of
    /// them may be the one
    pub unverified: u64,
    pub failed: Vec<(String, String)>,
    pub bytes: u64,
    /// files and folders that name a Windows device, written with a `_` before (safe_relpath)
    pub renamed: Vec<String>,
    /// files whose names differ only in case from one before, where names ignore case: (the
    /// one before, the one written over it)
    pub case_pairs: Vec<(String, String)>,
    /// what became of the links: those not made as such, or not made
    pub link_notes: Vec<String>,
}

fn octal(field: &[u8]) -> Option<u64> {
    if field.first().map_or(false, |&b| b & 0x80 != 0) {
        // GNU base-256 for sizes over 8 GB
        return Some(field.iter().skip(1).fold(0u64, |a, &b| a << 8 | b as u64));
    }
    let s: String = field.iter().take_while(|&&b| b != 0 && b != b' ').map(|&b| b as char).collect();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s.trim(), 8).ok()
}

fn header_valid(h: &[u8]) -> bool {
    let Some(want) = octal(&h[148..156]) else { return false };
    let (mut unsigned, mut signed) = (0u64, 0i64);
    for (i, &b) in h.iter().enumerate() {
        let v = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += v as u64;
        signed += v as i8 as i64;
    }
    want == unsigned || want as i64 == signed
}

fn field_str(f: &[u8]) -> String {
    let end = f.iter().position(|&b| b == 0).unwrap_or(f.len());
    String::from_utf8_lossy(&f[..end]).into_owned()
}

/// Extract (or, with no root, only read through) a tar that may be damaged: `data` with `mask`
/// per byte as gzrescue gives them, all at once (TarReader does it a chunk at a time)
#[cfg(test)]
pub fn untar(data: &[u8], mask: &[u8], root: Option<&Path>, mtime_from_header: bool) -> TarOutcome {
    let mut r = TarReader::new(root, mtime_from_header);
    r.feed(data, mask);
    r.finish()
}

/// The longest GNU long name or pax header kept (a larger one is read only that far)
const MAX_META: usize = 64 << 20;

/// What the bytes after a header are
enum Body {
    /// a regular file, written as its bytes arrive
    File { name: String, file: Option<File>, mtime: Option<u64>, err: Option<String>, unknown: bool, suspect: bool, inferred: bool, repaired: bool },
    /// a GNU long name ('L') or pax records ('x') for the next header
    Meta { typeflag: u8, buf: Vec<u8> },
    Skip,
}

enum State {
    /// a header at `pos`
    Header,
    /// an entry's data, from `at` up to `end` (where its size says it ends); the next header is at `next`
    Data { body: Body, at: u64, end: u64, next: u64 },
    /// no header at `start`: the next one that verifies, with a ustar magic to be sure, is looked
    /// for byte by byte from `q` (not in steps of 512: when a damaged block's length could only be
    /// estimated, everything after it is shifted by an arbitrary amount)
    Search { start: u64, q: u64 },
    Done,
}

/// Extracts a tar that may be damaged as gzrescue writes it out, a chunk at a time with its mask:
/// it holds only the header being read or looked for and long names and pax records, and writes
/// each file as its bytes arrive. A header that does not verify starts a search for the next one
/// that does; what lies between is reported as skipped.
pub struct TarReader<'a> {
    root: Option<&'a Path>,
    mtime_from_header: bool,
    o: TarOutcome,
    data: Vec<u8>,
    mask: Vec<u8>,
    base: u64, // the stream offset of data[0]
    pos: u64,  // where the next header is
    state: State,
    root_made: bool,
    long_name: Option<String>,
    pax_path: Option<String>,
    pax_size: Option<u64>,
    long_link: Option<String>, // GNU's long link name ('K') for the next header
    pax_link: Option<String>,  // pax linkpath
    // the links, made once every file is written: (name, what it names, a hard link)
    links: Vec<(String, String, bool)>,
    last_valid: Option<[u8; 512]>,
    // the last regular file: its name, status and where the header after it must be
    prev_file: Option<(String, u8, u64)>,
    by_case: std::collections::HashMap<String, String>, // a file's path in lower case: as written
}

impl<'a> TarReader<'a> {
    pub fn new(root: Option<&'a Path>, mtime_from_header: bool) -> Self {
        TarReader {
            root,
            mtime_from_header,
            o: TarOutcome::default(),
            data: Vec::new(),
            mask: Vec::new(),
            base: 0,
            pos: 0,
            state: State::Header,
            root_made: false,
            long_name: None,
            pax_path: None,
            pax_size: None,
            long_link: None,
            pax_link: None,
            links: Vec::new(),
            last_valid: None,
            prev_file: None,
            by_case: std::collections::HashMap::new(),
        }
    }

    /// The next bytes of the tar
    pub fn feed(&mut self, data: &[u8], mask: &[u8]) {
        self.data.extend_from_slice(data);
        self.mask.extend_from_slice(mask);
        self.run(false);
        // what is behind every position still needed goes
        let keep = match &self.state {
            State::Header => self.pos,
            State::Data { at, .. } => *at,
            State::Search { q, .. } => *q,
            State::Done => self.base + self.data.len() as u64,
        };
        let cut = keep.saturating_sub(self.base).min(self.data.len() as u64) as usize;
        if cut > (1 << 20) || cut == self.data.len() {
            self.data.drain(..cut);
            self.mask.drain(..cut);
            self.base += cut as u64;
        }
    }

    /// The end of the tar
    pub fn finish(mut self) -> TarOutcome {
        self.run(true);
        if let (Some(r), false) = (self.root, self.links.is_empty()) {
            self.make_root();
            self.make_links(r);
        }
        self.o
    }

    /// The tar's links, now that what they name is written: hard links first (a member of the
    /// archive, by its path from the archive's root), then symbolic ones as `x` makes them
    /// (gz::make_links). They were not written at all, and nothing said so: linux-6.6.tar.gz,
    /// rescued sound, came back without its 63 links.
    fn make_links(&mut self, root: &Path) {
        let Ok(canon_root) = root.canonicalize() else { return };
        let (mut symbolic, mut lost) = (Vec::new(), Vec::new());
        for (name, target, hard) in std::mem::take(&mut self.links) {
            let Some(rel) = zip::safe_relpath(&name) else {
                self.o.failed.push((name, "path leaves the output folder; skipped".into()));
                continue;
            };
            if !hard {
                symbolic.push((name, rel, PathBuf::from(target)));
                continue;
            }
            let dst = root.join(&rel);
            let made = zip::safe_relpath(&target).map(|t| root.join(t)).is_some_and(|src| {
                src.is_file()
                    && dst.symlink_metadata().is_err()
                    && dst.parent().is_none_or(|d| std::fs::create_dir_all(d).is_ok())
                    && (std::fs::hard_link(&src, &dst).is_ok() || std::fs::copy(&src, &dst).is_ok())
            });
            if !made {
                lost.push(name);
            }
        }
        if !lost.is_empty() {
            let shown = lost.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            self.o.link_notes.push(format!(
                "{} hard link{} naming no file written here (or whose name a file took), not made: {shown}{}",
                lost.len(),
                if lost.len() == 1 { "" } else { "s" },
                if lost.len() > 5 { ", ..." } else { "" }
            ));
        }
        let budget = self.o.bytes.max(64 << 20);
        self.o.link_notes.extend(crate::gz::make_links(root, &canon_root, symbolic, budget));
    }

    fn end(&self) -> u64 {
        self.base + self.data.len() as u64
    }

    fn slice(&self, from: u64, to: u64) -> (&[u8], &[u8]) {
        let (a, b) = ((from - self.base) as usize, (to - self.base) as usize);
        (&self.data[a..b], &self.mask[a..b])
    }

    fn make_root(&mut self) {
        if let (Some(r), false) = (self.root, self.root_made) {
            let _ = std::fs::create_dir_all(r);
            self.root_made = true;
        }
    }

    fn run(&mut self, eof: bool) {
        loop {
            match std::mem::replace(&mut self.state, State::Done) {
                State::Done => return,
                State::Header => {
                    if self.pos + 512 > self.end() {
                        self.state = if eof { State::Done } else { State::Header };
                        return;
                    }
                    self.header();
                }
                State::Data { mut body, mut at, end, next } => {
                    let upto = end.min(self.end());
                    if upto > at {
                        let (a, b) = ((at - self.base) as usize, (upto - self.base) as usize);
                        let (d, m) = (&self.data[a..b], &self.mask[a..b]);
                        match &mut body {
                            Body::File { file, err, unknown, suspect, inferred, .. } => {
                                *unknown |= m.iter().any(|&x| x == UNKNOWN);
                                *suspect |= m.iter().any(|&x| x == SUSPECT);
                                *inferred |= m.iter().any(|&x| x == INFERRED);
                                if let Some(f) = file {
                                    if err.is_none() {
                                        if let Err(e) = f.write_all(d) {
                                            *err = Some(e.to_string());
                                        }
                                    }
                                }
                            }
                            Body::Meta { buf, .. } => {
                                let room = MAX_META.saturating_sub(buf.len()).min(d.len());
                                buf.extend_from_slice(&d[..room]);
                            }
                            Body::Skip => {}
                        }
                        self.o.bytes += if matches!(body, Body::File { .. }) { upto - at } else { 0 };
                        at = upto;
                    }
                    if at < end && !eof {
                        self.state = State::Data { body, at, end, next };
                        return;
                    }
                    self.entry_done(body, at < end, next);
                    self.pos = next;
                    self.state = State::Header;
                }
                State::Search { start, mut q } => {
                    while q + 512 <= self.end() {
                        let (h, _) = self.slice(q, q + 512);
                        if &h[257..262] == b"ustar" && header_valid(h) {
                            self.o.skipped.push((start, q - start));
                            self.pos = q;
                            break;
                        }
                        q += 1;
                    }
                    if q + 512 <= self.end() {
                        self.state = State::Header;
                        continue;
                    }
                    if eof {
                        self.o.skipped.push((start, self.end() - start));
                        self.state = State::Done;
                        return;
                    }
                    self.state = State::Search { start, q };
                    return;
                }
            }
        }
    }

    /// The header at pos, with 512 bytes there
    fn header(&mut self) {
        let pos = self.pos;
        let (raw, hmask) = self.slice(pos, pos + 512);
        let (raw, hmask): ([u8; 512], [u8; 512]) = (raw.try_into().unwrap(), hmask.try_into().unwrap());
        // A header whose unknown bytes copied from garbage (the fields that repeat from header
        // to header: magic, mode, owner) is made whole with the previous header's bytes at those
        // places, the very bytes the encoder had copied; the checksum confirms the repair.
        let mut hbuf = raw;
        let mut repaired = false;
        if !header_valid(&hbuf) && hmask.iter().any(|&m| m == UNKNOWN) {
            if let Some(prev) = &self.last_valid {
                for i in 0..512 {
                    if hmask[i] == UNKNOWN {
                        hbuf[i] = prev[i];
                    }
                }
                repaired = header_valid(&hbuf);
            }
        }
        let h: &[u8] = &hbuf;
        if raw.iter().all(|&b| b == 0) {
            if hmask.iter().all(|&m| m == EXACT) {
                self.state = State::Done; // the end-of-archive blocks
                return;
            }
            // zeros standing in for unknown data: not a header
        } else if header_valid(h) {
            self.last_valid = Some(hbuf);
            if repaired {
                self.o.repaired += 1;
            }
            let typeflag = h[156];
            self.prev_file = None;
            let size = self.pax_size.take().or_else(|| octal(&h[124..136])).unwrap_or(0);
            let data_start = pos + 512;
            let end = data_start.saturating_add(size);
            let next = data_start.saturating_add((size + 511) / 512 * 512);
            let mut name = self.pax_path.take().or_else(|| self.long_name.take()).unwrap_or_else(|| {
                let base = field_str(&h[0..100]);
                let prefix = if &h[257..262] == b"ustar" { field_str(&h[345..500]) } else { String::new() };
                if prefix.is_empty() { base } else { format!("{prefix}/{base}") }
            });
            name = name.replace('\\', "/");
            let link = self.pax_link.take().or_else(|| self.long_link.take());
            let body = match typeflag {
                b'L' | b'K' | b'x' => Body::Meta { typeflag, buf: Vec::new() },
                b'1' | b'2' => {
                    if self.root.is_some() {
                        self.links.push((name.clone(), link.unwrap_or_else(|| field_str(&h[157..257])), typeflag == b'1'));
                    }
                    Body::Skip
                }
                b'5' => {
                    if let (Some(r), Some(rel)) = (self.root, zip::safe_relpath(&name)) {
                        self.make_root();
                        let _ = std::fs::create_dir_all(r.join(rel));
                        if zip::device_renamed(&name) {
                            self.o.renamed.push(name.clone());
                        }
                    }
                    Body::Skip
                }
                b'0' | 0 | b'7' => {
                    let mut err = None;
                    let mut file = None;
                    if let Some(r) = self.root {
                        match zip::safe_relpath(&name) {
                            Some(rel) => {
                                self.make_root();
                                // one file for two names where names ignore case: the last is kept
                                if cfg!(any(windows, target_os = "macos")) {
                                    let exact = rel.to_string_lossy().replace('\\', "/");
                                    if let Some(prev) = self.by_case.insert(exact.to_lowercase(), exact.clone()) {
                                        if prev != exact {
                                            self.o.case_pairs.push((prev, exact));
                                        }
                                    }
                                }
                                let path = r.join(rel);
                                if let Some(parent) = path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                match File::create(&path) {
                                    Ok(f) => {
                                        file = Some(f);
                                        if zip::device_renamed(&name) {
                                            self.o.renamed.push(name.clone());
                                        }
                                    }
                                    Err(e) => err = Some(e.to_string()),
                                }
                            }
                            None => err = Some("path leaves the output folder; skipped".into()),
                        }
                    }
                    let mtime = if self.mtime_from_header { octal(&h[136..148]) } else { None };
                    Body::File { name, file, mtime, err, unknown: false, suspect: false, inferred: false, repaired }
                }
                _ => Body::Skip, // links, devices, global pax headers: nothing to write here
            };
            self.state = State::Data { body, at: data_start, end, next };
            return;
        }
        // The header after a file is not where its size says, yet the bytes there are exact: the
        // decoder went through damage it could not see (a lost byte, after which the Huffman codes
        // fell back in step), and that file's data most likely has bytes lost or added.
        if let Some((name, status, at)) = self.prev_file.take() {
            if at == pos && hmask.iter().all(|&m| m == EXACT || m == INFERRED) {
                match status {
                    0 => {
                        self.o.intact -= 1;
                        self.o.suspect.push(name);
                    }
                    3 => {
                        self.o.inferred.retain(|x| *x != name);
                        self.o.suspect.push(name);
                    }
                    _ => {}
                }
            }
        }
        // (The members between the damage and the header found would form a chain that ends
        // there; none can: the search passed over every position where one could begin.)
        self.state = State::Search { start: pos, q: pos + 1 };
    }

    /// An entry's data is all read (or the tar ends inside it: `cut`)
    fn entry_done(&mut self, body: Body, cut: bool, next: u64) {
        match body {
            Body::File { name, file, mtime, err, unknown, suspect, inferred, repaired } => {
                let mut err = err;
                if let (Some(f), Some(t)) = (file, mtime) {
                    if err.is_none() {
                        let _ = f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t));
                    }
                }
                if let Some(e) = err.take() {
                    self.o.failed.push((name.clone(), e));
                }
                let status = if cut || unknown {
                    2
                } else if repaired || suspect {
                    1
                } else if inferred {
                    3
                } else {
                    0
                };
                self.prev_file = Some((name.clone(), status, next));
                match status {
                    0 => self.o.intact += 1,
                    1 => self.o.suspect.push(name),
                    3 => self.o.inferred.push(name),
                    _ => self.o.partial.push(name),
                }
            }
            Body::Meta { typeflag: b'L', buf } => self.long_name = Some(field_str(&buf)), // GNU long name
            Body::Meta { typeflag: b'K', buf } => self.long_link = Some(field_str(&buf)), // GNU long link name
            Body::Meta { buf, .. } => {
                // pax: "<len> key=value\n" records for the next header
                let mut p = 0;
                while p < buf.len() {
                    let rec = &buf[p..];
                    let Some(sp) = rec.iter().position(|&b| b == b' ') else { break };
                    let Ok(len) = String::from_utf8_lossy(&rec[..sp]).parse::<usize>() else { break };
                    if len == 0 || p + len > buf.len() {
                        break;
                    }
                    let kv = &buf[p + sp + 1..p + len - 1];
                    if let Some(eq) = kv.iter().position(|&b| b == b'=') {
                        let (k, v) = (&kv[..eq], String::from_utf8_lossy(&kv[eq + 1..]).into_owned());
                        match k {
                            b"path" => self.pax_path = Some(v),
                            b"linkpath" => self.pax_link = Some(v),
                            b"size" => self.pax_size = v.parse().ok(),
                            _ => {}
                        }
                    }
                    p += len;
                }
            }
            Body::Skip => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_of(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(1_700_000_000);
            h.set_cksum();
            b.append_data(&mut h, name, &data[..]).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn summary(o: &TarOutcome) -> String {
        format!("{} {:?} {:?} {:?} {:?} {} {} {:?}", o.intact, o.suspect, o.inferred, o.partial, o.skipped, o.repaired, o.bytes, o.failed)
    }

    #[test]
    fn a_tar_read_in_chunks_reads_as_one_read_whole() {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..40u32 {
            let len = (i as usize * 997) % 5000 + i as usize;
            files.push((format!("dir/f{i}.txt"), (0..len).map(|k| (k as u32 * 31 + i) as u8).collect()));
        }
        files.push((format!("{}/long.txt", "n".repeat(150)), b"long name".to_vec())); // a pax or GNU long name
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
        let tar = tar_of(&refs);
        // damage: a stretch of unknown bytes over a header and file data, suspect bytes in a file,
        // inferred ones in another, garbage that shifts nothing, and a cut at the end
        let mut data = tar.clone();
        let mut mask = vec![EXACT; data.len()];
        for i in 9000..15000 {
            mask[i] = UNKNOWN;
            data[i] = 0;
        }
        for i in 30000..30100 {
            mask[i] = SUSPECT;
        }
        for i in 50000..50010 {
            mask[i] = INFERRED;
        }
        for i in 70001..70600 {
            data[i] ^= 0x5a;
        }
        let cut = data.len() - 3000;
        let (data, mask) = (&data[..cut], &mask[..cut]);
        let o = untar(data, mask, None, false);
        assert!(!o.partial.is_empty() && !o.suspect.is_empty() && !o.inferred.is_empty() && !o.skipped.is_empty(), "{}", summary(&o));
        let whole = summary(&o);
        for chunk in [1usize, 7, 511, 512, 513, 4096, 65536] {
            let mut r = TarReader::new(None, false);
            for (d, m) in data.chunks(chunk).zip(mask.chunks(chunk)) {
                r.feed(d, m);
            }
            assert_eq!(summary(&r.finish()), whole, "chunks of {chunk}");
        }
        // undamaged: every file intact
        let mut r = TarReader::new(None, false);
        r.feed(&tar, &vec![EXACT; tar.len()]);
        let o = r.finish();
        assert_eq!(o.intact as usize, files.len());
        assert!(o.skipped.is_empty() && o.partial.is_empty() && o.suspect.is_empty());
    }
}
