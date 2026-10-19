//! Reading a ZIP file's central directory: entry names, sizes, methods and where each
//! entry's compressed bytes start, plus the route each entry takes through the extractor.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::SystemTime;

pub const STORED: u16 = 0;
pub const DEFLATED: u16 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// Deflate: decoded on the GPU or the CPU, decided while running
    Deflate,
    /// stored, or an empty Deflate entry
    Cpu,
    /// encrypted, or a method only 7-Zip reads here (bzip2, LZMA, Zstandard, Deflate64...)
    SevenZip,
    Dir,
    /// the path would leave the output folder
    Unsafe,
    /// a later entry has the same path
    Dup,
    /// its bytes overlap another entry's: data shared, as in a zip bomb
    Overlap,
}

pub struct Entry {
    /// the name as stored, with backslashes turned into slashes
    pub name: String,
    /// the name's bytes as stored, before any decoding
    pub raw_name: Vec<u8>,
    /// where the entry goes below the output folder
    pub path: Option<PathBuf>,
    pub method: u16,
    pub flags: u16,
    pub crc: u32,
    pub csize: u64,
    pub size: u64,
    pub data_offset: u64,
    pub dos_time: u16,
    pub dos_date: u16,
    /// a regular file's permission bits, from a ZIP made on Unix or macOS (0: none)
    #[cfg_attr(not(unix), allow(dead_code))]
    pub unix_mode: u32,
    pub route: Route,
}

/// The permission bits a ZIP keeps for a regular file: the high 16 bits of its external
/// attributes hold a Unix st_mode when it was made on Unix (3) or macOS (19). Not those of a
/// link or a folder, and never setuid, setgid or sticky (Info-ZIP's unzip drops them too).
fn unix_mode(made_by: u8, external: u32) -> u32 {
    let mode = external >> 16;
    let regular = matches!(mode & 0o170000, 0 | 0o100000);
    if matches!(made_by, 3 | 19) && regular { mode & 0o777 } else { 0 }
}

pub fn method_name(m: u16) -> String {
    match m {
        0 => "stored",
        8 => "deflate",
        9 => "deflate64",
        12 => "bzip2",
        14 => "lzma",
        93 => "zstd",
        95 => "xz",
        98 => "ppmd",
        99 => "aes",
        _ => return m.to_string(),
    }
    .to_string()
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Position of the end-of-central-directory record, searched from the end like Python's zipfile.
pub fn find_eocd(data: &[u8]) -> Option<usize> {
    if data.len() < 22 {
        return None;
    }
    let lowest = data.len().saturating_sub(22 + 65535);
    (lowest..=data.len() - 22).rev().find(|&p| u32le(data, p) == 0x0605_4b50)
}

fn cp437() -> &'static [char] {
    static TABLE: OnceLock<Vec<char>> = OnceLock::new();
    TABLE.get_or_init(|| {
        "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}"
            .chars()
            .collect()
    })
}

/// An entry's name. Without the UTF-8 flag the bytes are in the creating system's code page:
/// CP437 from DOS and Windows tools, but Unix and macOS tools (Info-ZIP's zip among them) store
/// a UTF-8 system's names as they are, without the flag, so a valid UTF-8 name from those is
/// read as UTF-8. Info-ZIP's Unicode Path field, when its CRC matches the name, wins over both.
pub fn decode_name(raw: &[u8], utf8: bool, made_on_unix: bool, unicode_path: Option<String>) -> String {
    let raw = raw.split(|&b| b == 0).next().unwrap_or(raw);
    let name = if let Some(name) = unicode_path {
        name
    } else if utf8 || (made_on_unix && std::str::from_utf8(raw).is_ok()) {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        let table = cp437();
        raw.iter().map(|&b| if b < 0x80 { b as char } else { table[(b - 0x80) as usize] }).collect()
    };
    name.replace('\\', "/")
}

/// Info-ZIP's Unicode Path extra field (0x7075), when it is there and still matches the name: version
/// 1, the CRC-32 of the name it replaces, then the name in UTF-8. Directory and local headers both
/// carry it.
pub fn unicode_path(extra: &[u8], raw_name: &[u8]) -> Option<String> {
    let mut x = 0;
    while x + 4 <= extra.len() {
        let (id, len) = (u16le(extra, x), u16le(extra, x + 2) as usize);
        if id == 0x7075 && len >= 5 && x + 4 + len <= extra.len() && extra[x + 4] == 1 && u32le(extra, x + 5) == crc32fast::hash(raw_name) {
            return std::str::from_utf8(&extra[x + 9..x + 4 + len]).ok().map(str::to_string);
        }
        x += 4 + len;
    }
    None
}

/// The entry's path below the output folder, or None if it would escape it. On Windows a part
/// that names a device gets a `_` before it, as 7-Zip does.
pub fn safe_relpath(name: &str) -> Option<PathBuf> {
    let mut parts = Vec::new();
    for p in name.split(['/', '\\']) {
        if p.is_empty() || p == "." {
            continue;
        }
        if p == ".." {
            return None;
        }
        let clean: String = p.chars().map(|c| if ":<>|\"?*".contains(c) { '_' } else { c }).collect();
        let clean = clean.trim_end_matches(['.', ' ']);
        parts.push(if clean.is_empty() {
            "_".to_string()
        } else if cfg!(windows) && is_windows_device(clean) {
            format!("_{clean}")
        } else {
            clean.to_string()
        });
    }
    if parts.is_empty() { None } else { Some(parts.iter().collect()) }
}

/// Does Windows take this file name for a device? A file written under one goes to the device:
/// NUL's without an error, and lost. Microsoft lists CON, PRN, AUX, NUL, COM0-9, LPT0-9 (with the
/// digits ¹ ² ³ too), CONIN$ and CONOUT$, in any case, bare or with any extension, and Windows 10
/// takes them all so; Windows 11 only NUL inside a folder (build 26300, measured: con, CON.txt,
/// aux.c and com1 are files there). So a name on the list is put to this Windows itself.
pub fn is_windows_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let listed = match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let c: Vec<char> = stem.chars().collect();
            c.len() == 4 && matches!(&c[..3], ['C', 'O', 'M'] | ['L', 'P', 'T']) && matches!(c[3], '0'..='9' | '¹' | '²' | '³')
        }
    };
    listed && windows_takes_for_device(part)
}

/// GetFullPathNameW makes `\\.\NAME` of a name Windows takes for a device, and opens nothing (to
/// write to PRN or COM1 to find out could reach a real printer or port); if it fails, the name
/// is taken for one
#[cfg(windows)]
fn windows_takes_for_device(part: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFullPathNameW(name: *const u16, len: u32, buf: *mut u16, file_part: *mut *mut u16) -> u32;
    }
    let path: Vec<u16> = std::ffi::OsStr::new(&format!("C:\\gpuzip\\{part}")).encode_wide().chain([0]).collect();
    let mut buf = [0u16; 512];
    // SAFETY: path ends in a 0; buf's length is passed, and nothing else is written
    let n = unsafe { GetFullPathNameW(path.as_ptr(), buf.len() as u32, buf.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    n == 0 || (n < buf.len() && buf.starts_with(&[b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16]))
}

#[cfg(not(windows))]
fn windows_takes_for_device(_: &str) -> bool {
    false
}

/// A rescue's note on the files it wrote under another name because they name a Windows device
pub fn device_note(names: &[String]) -> Option<String> {
    let (first, more) = names.split_first()?;
    let shown: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
    Some(if more.is_empty() {
        format!("{first} uses a name this Windows takes for a device (what is written to one is lost): written with a _ before it")
    } else {
        format!(
            "{} names use names this Windows takes for devices (what is written to one is lost), written with a _ before them: {}{}",
            names.len(),
            shown.join(", "),
            if names.len() > 5 { ", ..." } else { "" }
        )
    })
}

/// Did safe_relpath rename a part of this name because it names a Windows device?
pub fn device_renamed(name: &str) -> bool {
    cfg!(windows)
        && name
            .split(['/', '\\'])
            .map(|p| p.chars().map(|c| if ":<>|\"?*".contains(c) { '_' } else { c }).collect::<String>())
            .any(|p| is_windows_device(p.trim_end_matches(['.', ' '])))
}

/// The entry's DOS timestamp as a local time, like Python's time.mktime.
pub fn dos_time(date: u16, time: u16) -> Option<SystemTime> {
    use chrono::{Local, NaiveDate, TimeZone};
    let day = NaiveDate::from_ymd_opt(1980 + (date >> 9) as i32, ((date >> 5) & 0xf) as u32, (date & 0x1f) as u32)?;
    let at = day.and_hms_opt((time >> 11) as u32, ((time >> 5) & 0x3f) as u32, (((time & 0x1f) * 2) as u32).min(59))?;
    Some(SystemTime::from(Local.from_local_datetime(&at).earliest()?))
}

/// Every entry of the archive, each with its route.
pub fn read_entries(data: &[u8]) -> Result<Vec<Entry>, String> {
    let mut entries = read_central_directory(data, false)?;
    plan(data, &mut entries)?;
    Ok(entries)
}

/// The central directory alone: names, sizes, CRCs and where each local header is
/// (`data_offset`), without looking at the local headers. For the rescue of damaged files.
/// `lost_ok`: the directory may say it lies further on than it does, when bytes before it were
/// lost (a download resumed past a gap); it is read where it ends, at the end record, and the
/// offsets it gives are moved back by as much (right for the entries after the gap only).
pub fn read_central_directory(data: &[u8], lost_ok: bool) -> Result<Vec<Entry>, String> {
    let eocd = find_eocd(data).ok_or("not a ZIP file")?;
    let mut size_cd = u32le(data, eocd + 12) as u64;
    let mut offset_cd = u32le(data, eocd + 16) as u64;
    let mut end_of_cd = eocd as u64;
    if eocd >= 76 && u32le(data, eocd - 20) == 0x0706_4b50 {
        let rec = eocd - 20 - 56; // the Zip64 end record sits right before its locator
        if u32le(data, rec) == 0x0606_4b50 {
            size_cd = u64le(data, rec + 40);
            offset_cd = u64le(data, rec + 48);
            end_of_cd = rec as u64;
        }
    }
    // data prepended to the archive (self-extractors) shifts every offset
    let concat = end_of_cd as i64 - (size_cd + offset_cd) as i64;
    if concat < 0 && (!lost_ok || (size_cd as i64) > end_of_cd as i64) {
        return Err("central directory points past the end of the file".into());
    }
    let start = (offset_cd as i64 + concat) as usize;
    let end = start + size_cd as usize;
    if end > data.len() {
        return Err("central directory is truncated".into());
    }

    let mut entries = Vec::new();
    let mut p = start;
    while p + 46 <= end {
        if u32le(data, p) != 0x0201_4b50 {
            return Err(format!("bad central directory entry at {p}"));
        }
        let flags = u16le(data, p + 8);
        let method = u16le(data, p + 10);
        let (nlen, xlen, clen) = (u16le(data, p + 28) as usize, u16le(data, p + 30) as usize, u16le(data, p + 32) as usize);
        let mut csize = u32le(data, p + 20) as u64;
        let mut size = u32le(data, p + 24) as u64;
        let mut header = u32le(data, p + 42) as u64;
        if p + 46 + nlen + xlen > data.len() {
            return Err("central directory entry is truncated".into());
        }
        let raw_name = &data[p + 46..p + 46 + nlen];
        let extra = &data[p + 46 + nlen..p + 46 + nlen + xlen];
        let unicode_path = unicode_path(extra, raw_name);
        let mut x = 0;
        while x + 4 <= extra.len() {
            let (id, len) = (u16le(extra, x), u16le(extra, x + 2) as usize);
            if id == 0x0001 && x + 4 + len <= extra.len() {
                // Zip64: the 64-bit values of the fields that overflowed, in this order
                let mut q = x + 4;
                for field in [&mut size, &mut csize, &mut header] {
                    if *field == 0xffff_ffff && q + 8 <= x + 4 + len {
                        *field = u64le(extra, q);
                        q += 8;
                    }
                }
            }
            x += 4 + len;
        }
        entries.push(Entry {
            // the high byte of "version made by": 3 Unix, 19 macOS
            name: decode_name(raw_name, flags & 0x800 != 0, matches!(data[p + 5], 3 | 19), unicode_path),
            raw_name: raw_name.to_vec(),
            path: None,
            method,
            flags,
            crc: u32le(data, p + 16),
            csize,
            size,
            data_offset: (header as i64 + concat).max(0) as u64, // the local header for now; plan() moves it to the data
            dos_time: u16le(data, p + 12),
            dos_date: u16le(data, p + 14),
            unix_mode: unix_mode(data[p + 5], u32le(data, p + 38)),
            route: Route::Cpu,
        });
        p += 46 + nlen + xlen + clen;
    }
    Ok(entries)
}

/// Give every entry its route and point it at its compressed bytes.
fn plan(data: &[u8], entries: &mut [Entry]) -> Result<(), String> {
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut spans: Vec<(u64, u64, usize)> = Vec::new(); // each entry's bytes: local header to the end of its data
    for i in 0..entries.len() {
        let e = &mut entries[i];
        let h = e.data_offset as usize;
        if h + 30 <= data.len() && u32le(data, h) == 0x0403_4b50 {
            let start = (h + 30 + u16le(data, h + 26) as usize + u16le(data, h + 28) as usize) as u64;
            let need = if e.method == STORED && e.flags & 0x1 == 0 { e.size.max(e.csize) } else { e.csize };
            spans.push((h as u64, start + need, i));
        }
        e.path = safe_relpath(&e.name);
        e.route = if e.path.is_none() {
            Route::Unsafe
        } else if e.name.ends_with('/') {
            Route::Dir
        } else if e.flags & 0x1 != 0 || (e.method != STORED && e.method != DEFLATED) {
            Route::SevenZip
        } else if e.method == DEFLATED && e.size > 0 {
            Route::Deflate
        } else {
            Route::Cpu
        };
        if matches!(e.route, Route::Deflate | Route::Cpu) {
            let h = e.data_offset as usize;
            if h + 30 > data.len() || u32le(data, h) != 0x0403_4b50 {
                return Err(format!("bad local header for {:?}", e.name));
            }
            e.data_offset = (h + 30 + u16le(data, h + 26) as usize + u16le(data, h + 28) as usize) as u64;
            let need = if e.method == STORED { e.size } else { e.csize };
            if e.data_offset + need > data.len() as u64 {
                return Err(format!("{:?} runs past the end of the file", e.name));
            }
        }
        if matches!(e.route, Route::Deflate | Route::Cpu | Route::SevenZip) {
            let path = e.path.as_ref().unwrap().to_string_lossy();
            // Windows and macOS file names ignore case: two entries that differ only in case are one file
            let key = if cfg!(any(windows, target_os = "macos")) { path.to_lowercase() } else { path.into_owned() };
            if let Some(prev) = by_path.insert(key, i) {
                entries[prev].route = Route::Dup;
            }
        }
    }
    // Entries whose bytes overlap, a local header inside another entry's header or data, share
    // data, which is how a zip bomb multiplies it: one 100 MB stream named by 20 entries wrote
    // 2 GB from 103 KB; each entry quoting the next one's header in a stored block (Fifield's
    // "better zip bomb"), 200 MB from 12 KB. Info-ZIP's unzip and Python's zipfile refuse them;
    // here each entry that overlaps another is skipped, the others are extracted.
    spans.sort_unstable();
    let (mut reach, mut holder) = (0u64, usize::MAX); // the farthest end so far, and whose
    let mut overlaps = vec![false; entries.len()];
    for &(start, end, i) in &spans {
        if holder != usize::MAX && start < reach {
            overlaps[i] = true;
            overlaps[holder] = true;
        }
        if end > reach {
            (reach, holder) = (end, i);
        }
    }
    for (e, over) in entries.iter_mut().zip(overlaps) {
        if over && matches!(e.route, Route::Deflate | Route::Cpu | Route::SevenZip | Route::Dup) {
            e.route = Route::Overlap;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{decode_name, device_renamed, is_windows_device, safe_relpath};
    use std::path::PathBuf;

    #[test]
    fn names_without_the_utf8_flag() {
        let raw = "año 2026/naïve.txt".as_bytes();
        // Info-ZIP on Linux or macOS: UTF-8 bytes, no flag
        assert_eq!(decode_name(raw, false, true, None), "año 2026/naïve.txt");
        // the same bytes from a DOS or Windows tool are CP437
        assert_eq!(decode_name(raw, false, false, None), "a├▒o 2026/na├»ve.txt");
        // not UTF-8, even from Unix: CP437
        assert_eq!(decode_name(b"caf\x82.txt", false, true, None), "café.txt");
        assert_eq!(decode_name(b"x\\y.txt", true, false, None), "x/y.txt");
        assert_eq!(decode_name(b"a.txt", false, false, Some("ä.txt".into())), "ä.txt");
    }

    #[test]
    fn unix_modes() {
        use super::unix_mode;
        assert_eq!(unix_mode(3, 0o100755 << 16), 0o755); // a script made on Unix
        assert_eq!(unix_mode(19, 0o644 << 16), 0o644); // macOS, no file type given
        assert_eq!(unix_mode(3, 0o104755 << 16), 0o755); // setuid dropped
        assert_eq!(unix_mode(3, 0o120777 << 16), 0); // a link
        assert_eq!(unix_mode(3, 0o040755 << 16), 0); // a folder
        assert_eq!(unix_mode(0, 0o100755 << 16), 0); // made on DOS: those bits mean nothing
    }

    #[test]
    fn device_names() {
        // NUL is a device on every Windows (the others on Windows 10, not 11), and nowhere else
        for n in ["nul", "NUL", "Nul", "nul "] {
            assert_eq!(is_windows_device(n), cfg!(windows), "{n}");
        }
        for n in ["nulx", ".nul", "connect", "com10", "lpt", "auxiliary", "a.nul"] {
            assert!(!is_windows_device(n), "{n}");
        }
        let renamed = if cfg!(windows) { "a/_nul/x.txt" } else { "a/nul/x.txt" };
        assert_eq!(safe_relpath("a/nul/x.txt"), Some(PathBuf::from(renamed)));
        assert_eq!(device_renamed("a/NUL/x.txt"), cfg!(windows));
        assert!(!device_renamed("a/nulx/x.txt"));
    }
}
