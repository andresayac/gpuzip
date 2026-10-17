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
}

pub struct Entry {
    /// the name as stored, with backslashes turned into slashes
    pub name: String,
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
    pub route: Route,
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

fn decode_name(raw: &[u8], utf8: bool) -> String {
    let raw = raw.split(|&b| b == 0).next().unwrap_or(raw);
    let name = if utf8 {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        let table = cp437();
        raw.iter().map(|&b| if b < 0x80 { b as char } else { table[(b - 0x80) as usize] }).collect()
    };
    name.replace('\\', "/")
}

/// The entry's path below the output folder, or None if it would escape it.
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
        parts.push(if clean.is_empty() { "_".to_string() } else { clean.to_string() });
    }
    if parts.is_empty() { None } else { Some(parts.iter().collect()) }
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
            name: decode_name(raw_name, flags & 0x800 != 0),
            path: None,
            method,
            flags,
            crc: u32le(data, p + 16),
            csize,
            size,
            data_offset: (header as i64 + concat).max(0) as u64, // the local header for now; plan() moves it to the data
            dos_time: u16le(data, p + 12),
            dos_date: u16le(data, p + 14),
            route: Route::Cpu,
        });
        p += 46 + nlen + xlen + clen;
    }
    Ok(entries)
}

/// Give every entry its route and point it at its compressed bytes.
fn plan(data: &[u8], entries: &mut [Entry]) -> Result<(), String> {
    let mut by_path: HashMap<String, usize> = HashMap::new();
    for i in 0..entries.len() {
        let e = &mut entries[i];
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
            let key = if cfg!(windows) { path.to_lowercase() } else { path.into_owned() };
            if let Some(prev) = by_path.insert(key, i) {
                entries[prev].route = Route::Dup;
            }
        }
    }
    Ok(())
}
