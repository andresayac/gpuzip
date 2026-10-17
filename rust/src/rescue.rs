//! Recovering damaged gzip files: gzrescue.dll (built from gpuinflate/rescue.cpp, CPU only)
//! decodes a damaged .gz block by block and says, byte by byte, what is exact, what lies in a
//! damaged block and what cannot be known; this module loads it, and extracts what a damaged
//! .tar.gz still holds, skipping to the next valid tar header after a destroyed region.

use crate::zip;
use libloading::Library;
use std::ffi::{CStr, c_char, c_void};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ABI_VERSION: u32 = 3;
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

/// Places gzrescue.dll is looked for, in order.
pub fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os("GPUZIP_RESCUE_DIR") {
        out.push(PathBuf::from(dir).join("gzrescue.dll"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("gzrescue.dll"));
        }
    }
    out.push(PathBuf::from("gzrescue.dll")); // the system search path (PATH)
    out
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
            let (lib, path) = candidates()
                .into_iter()
                .find_map(|p| Library::new(&p).ok().map(|l| (l, p.display().to_string())))
                .ok_or("gzrescue.dll not found: put it next to gpuzip.exe or set GPUZIP_RESCUE_DIR")?;
            let abi: unsafe extern "C" fn() -> u32 = sym(&lib, b"gr_abi_version\0")?;
            if abi() != ABI_VERSION {
                return Err(format!("gzrescue.dll speaks version {} of the interface, this gpuzip {ABI_VERSION}", abi()));
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
        self.run(input, None, fill, 0, sink)
    }

    /// The same for a raw Deflate stream (a ZIP entry) that starts at `input[0]`; `expected`:
    /// the original's size and, if known, its CRC-32, standing in for the gzip trailer. `crs`:
    /// when the file lost its CRs to an ASCII transfer, how many this entry lost if known (its
    /// stored size against the room it has): 1-8, 255 none, 0 unknown.
    pub fn rescue_raw(&self, input: &[u8], expected: (Option<u64>, Option<u32>), fill: u8, crs: u8, sink: impl FnMut(&[u8], &[u8]) -> bool) -> Result<Report, String> {
        self.run(input, Some(expected), fill, crs, sink)
    }

    fn run(&self, input: &[u8], raw: Option<(Option<u64>, Option<u32>)>, fill: u8, crs_expected: u8, mut sink: impl FnMut(&[u8], &[u8]) -> bool) -> Result<Report, String> {
        let cr_lost = if CR_LOST.load(std::sync::atomic::Ordering::Relaxed) { 1 } else { 0 };
        let opt = Options { size: std::mem::size_of::<Options>() as u32, fill, no_infer: 0, cr_lost, crs_expected };
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
/// per byte as gzrescue gives them. A header that does not verify starts a search for the next
/// one that does, 512 bytes at a time; what lies between is reported as skipped.
pub fn untar(data: &[u8], mask: &[u8], root: Option<&Path>, mtime_from_header: bool) -> TarOutcome {
    let mut o = TarOutcome::default();
    let n = data.len();
    let mut pos = 0usize;
    let mut long_name: Option<String> = None;
    let mut pax_path: Option<String> = None;
    let mut pax_size: Option<u64> = None;
    let mut last_valid: Option<[u8; 512]> = None;
    // the last regular file: its name, status and where the header after it must be
    let mut prev_file: Option<(String, u8, usize)> = None;
    while pos + 512 <= n {
        let raw = &data[pos..pos + 512];
        let hmask = &mask[pos..pos + 512];
        // A header whose unknown bytes copied from garbage (the fields that repeat from header
        // to header: magic, mode, owner) is made whole with the previous header's bytes at those
        // places, the very bytes the encoder had copied; the checksum confirms the repair.
        let mut hbuf = [0u8; 512];
        hbuf.copy_from_slice(raw);
        let mut repaired = false;
        if !header_valid(&hbuf) && hmask.iter().any(|&m| m == UNKNOWN) {
            if let Some(prev) = &last_valid {
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
                break; // the end-of-archive blocks
            }
            // zeros standing in for unknown data: not a header
        } else if header_valid(h) {
            last_valid = Some(hbuf);
            if repaired {
                o.repaired += 1;
            }
            let typeflag = h[156];
            prev_file = None;
            let size = pax_size.take().or_else(|| octal(&h[124..136])).unwrap_or(0);
            let data_start = pos + 512;
            let data_end = data_start.saturating_add(size as usize).min(n);
            let next = data_start.saturating_add(((size + 511) / 512 * 512) as usize);
            let mut name = pax_path.take().or_else(|| long_name.take()).unwrap_or_else(|| {
                let base = field_str(&h[0..100]);
                let prefix = if &h[257..262] == b"ustar" { field_str(&h[345..500]) } else { String::new() };
                if prefix.is_empty() { base } else { format!("{prefix}/{base}") }
            });
            name = name.replace('\\', "/");
            match typeflag {
                b'L' => long_name = Some(field_str(&data[data_start..data_end])), // GNU long name
                b'x' => {
                    // pax: "<len> key=value\n" records for the next header
                    let mut p = data_start;
                    while p < data_end {
                        let rec = &data[p..data_end];
                        let Some(sp) = rec.iter().position(|&b| b == b' ') else { break };
                        let Ok(len) = String::from_utf8_lossy(&rec[..sp]).parse::<usize>() else { break };
                        if len == 0 || p + len > data_end {
                            break;
                        }
                        let kv = &data[p + sp + 1..p + len - 1];
                        if let Some(eq) = kv.iter().position(|&b| b == b'=') {
                            let (k, v) = (&kv[..eq], String::from_utf8_lossy(&kv[eq + 1..]).into_owned());
                            match k {
                                b"path" => pax_path = Some(v),
                                b"size" => pax_size = v.parse().ok(),
                                _ => {}
                            }
                        }
                        p += len;
                    }
                }
                b'5' => {
                    if let (Some(r), Some(rel)) = (root, zip::safe_relpath(&name)) {
                        let _ = std::fs::create_dir_all(r.join(rel));
                    }
                }
                b'0' | 0 | b'7' => {
                    let body = &data[data_start..data_end];
                    let bmask = &mask[data_start..data_end];
                    let cut = data_end < data_start + size as usize;
                    let status = if cut || bmask.iter().any(|&m| m == UNKNOWN) {
                        2
                    } else if repaired || bmask.iter().any(|&m| m == SUSPECT) {
                        1
                    } else if bmask.iter().any(|&m| m == INFERRED) {
                        3
                    } else {
                        0
                    };
                    if let Some(r) = root {
                        match zip::safe_relpath(&name) {
                            Some(rel) => {
                                let path = r.join(rel);
                                if let Some(parent) = path.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                let written = File::create(&path).and_then(|mut f| {
                                    f.write_all(body)?;
                                    if mtime_from_header {
                                        if let Some(t) = octal(&h[136..148]) {
                                            let _ = f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t));
                                        }
                                    }
                                    Ok(())
                                });
                                if let Err(e) = written {
                                    o.failed.push((name.clone(), e.to_string()));
                                }
                            }
                            None => o.failed.push((name.clone(), "path leaves the output folder; skipped".into())),
                        }
                    }
                    o.bytes += body.len() as u64;
                    prev_file = Some((name.clone(), status, next));
                    match status {
                        0 => o.intact += 1,
                        1 => o.suspect.push(name),
                        3 => o.inferred.push(name),
                        _ => o.partial.push(name),
                    }
                }
                _ => {} // links, devices, global pax headers: nothing to write here
            }
            pos = next;
            continue;
        }
        // The header after a file is not where its size says, yet the bytes there are exact: the
        // decoder went through damage it could not see (a lost byte, after which the Huffman codes
        // fell back in step), and that file's data most likely has bytes lost or added.
        if let Some((name, status, at)) = prev_file.take() {
            if at == pos && hmask.iter().all(|&m| m == EXACT || m == INFERRED) {
                match status {
                    0 => {
                        o.intact -= 1;
                        o.suspect.push(name);
                    }
                    3 => {
                        o.inferred.retain(|x| *x != name);
                        o.suspect.push(name);
                    }
                    _ => {}
                }
            }
        }
        // No header here: find the next one that verifies, with a ustar magic to be sure. Byte
        // by byte, not in steps of 512: when a damaged block's length could only be estimated,
        // everything after it is shifted by an arbitrary amount.
        let start = pos;
        let mut q = pos + 1;
        while q + 512 <= n && !(&data[q + 257..q + 262] == b"ustar" && header_valid(&data[q..q + 512])) {
            q += 1;
        }
        if q + 512 > n {
            o.skipped.push((start as u64, (n - start) as u64));
            break;
        }
        // the members between the damage and q form a chain that ends at q: walk it back, so
        // only the destroyed stretch itself is skipped
        let mut back = q;
        while back >= start + 512 {
            let c = back - 512;
            let h = &data[c..c + 512];
            if &h[257..262] != b"ustar" || !header_valid(h) {
                break;
            }
            let Some(size) = octal(&h[124..136]) else { break };
            if c + 512 + ((size + 511) / 512 * 512) as usize != back {
                break;
            }
            back = c;
        }
        o.skipped.push((start as u64, (back - start) as u64));
        pos = back;
    }
    o
}
