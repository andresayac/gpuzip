//! Recovering damaged ZIP files, and the Office documents that are ZIPs (docx, xlsx, pptx).
//!
//! A ZIP has every entry's name, sizes and CRC twice: in the local header before its data and in
//! the central directory at the end. An interrupted download loses the directory; a bad sector
//! destroys a stretch of data or a header. This module reads the directory if it can, scans the
//! whole file for local headers to find the entries the directory does not vouch for, and
//! recovers every entry: a stored one by copying and checking its CRC, a Deflate one through
//! gzrescue, which knows its blocks and marks what was damaged. Each entry's own CRC says
//! whether it is intact. For an Office document the damaged XML parts are made well-formed
//! again, the text is extracted, and a document is rebuilt from the recovered parts.

use crate::rescue::{INFERRED, Rescue, UNKNOWN};
use crate::zip::{self, DEFLATED, STORED};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// CRC verified
    Intact,
    /// damaged inside: flagged blocks, unknown bytes, or cut short
    Partial,
    /// the data decodes without visible damage, but the CRC does not match (damage inside raw
    /// stored data or a flipped bit that kept the decoder in step), or no CRC is known
    Unverified,
    /// nothing usable could be found for it
    Lost,
    /// encrypted or a method not read here
    Unsupported,
}

pub struct Recovered {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub size: u64,
    pub from_scan: bool, // found by scanning, not in the directory
    pub office_part: Option<Vec<u8>>, // the bytes kept for rebuilding an Office document
}

#[derive(Default)]
pub struct Outcome {
    pub entries: Vec<Recovered>,
    pub cd_ok: bool,
    pub cd_error: String,
    pub scanned_headers: usize,
    pub notes: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub office: Option<OfficeOutcome>,
    /// directory entries whose local header was found elsewhere than the directory says
    pub moved: usize,
    /// CRs put back that an ASCII-mode transfer had removed, and in how many entries
    pub crs_restored: u64,
    pub cr_entries: usize,
    /// directory entries not written because their data would be an earlier intact entry's
    pub shared: usize,
    /// local headers found only by scanning, inside an intact entry's data: not entries
    pub inside: usize,
    /// entries not written over a file of the same name that an earlier entry gave back intact
    pub kept_intact: usize,
}

#[derive(Default)]
pub struct OfficeOutcome {
    pub kind: String, // docx, xlsx, pptx, or "office"
    pub repaired_parts: Vec<String>,
    pub text_file: Option<PathBuf>,
    pub text_chars: usize,
    pub rebuilt: Option<PathBuf>,
    /// what was changed so the rebuilt package opens
    pub package_fixes: Vec<String>,
}

/// A local file header found by scanning
struct Local {
    offset: usize,
    data_offset: usize,
    flags: u16,
    method: u16,
    crc: u32,
    csize: u64,
    size: u64,
    name: String,
    raw_name: Vec<u8>, // as stored: what the directory's entry is matched on
    dos_time: u16,
    dos_date: u16,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn parse_local(data: &[u8], h: usize) -> Option<Local> {
    if h + 30 > data.len() || u32le(data, h) != 0x0403_4b50 {
        return None;
    }
    let method = u16le(data, h + 8);
    let nlen = u16le(data, h + 26) as usize;
    let xlen = u16le(data, h + 28) as usize;
    if nlen == 0 || nlen > 1024 || h + 30 + nlen + xlen > data.len() {
        return None;
    }
    let raw = &data[h + 30..h + 30 + nlen];
    if raw.iter().any(|&b| b < 0x20 && b != b'\t') {
        return None; // names do not hold control characters: a signature inside data
    }
    let flags = u16le(data, h + 6);
    let mut csize = u32le(data, h + 18) as u64;
    let mut size = u32le(data, h + 22) as u64;
    let extra = &data[h + 30 + nlen..h + 30 + nlen + xlen];
    // decoded as the directory decodes it: UTF-8 when flagged, else Info-ZIP's Unicode Path, else
    // CP437; a local header does not say what system made it, so a name that is valid UTF-8 is
    // taken as UTF-8 (what zip tools on Unix and macOS write without the flag)
    let name = zip::decode_name(raw, flags & 0x800 != 0, std::str::from_utf8(raw).is_ok(), zip::unicode_path(extra, raw));
    // Zip64 in the local header's extra field
    let mut x = 0;
    while x + 4 <= extra.len() {
        let (id, len) = (u16le(extra, x), u16le(extra, x + 2) as usize);
        if id == 1 && x + 4 + len <= extra.len() {
            let mut q = x + 4;
            for field in [&mut size, &mut csize] {
                if *field == 0xffff_ffff && q + 8 <= x + 4 + len {
                    *field = u64::from_le_bytes(extra[q..q + 8].try_into().unwrap());
                    q += 8;
                }
            }
        }
        x += 4 + len;
    }
    Some(Local {
        offset: h,
        data_offset: h + 30 + nlen + xlen,
        flags,
        method,
        crc: u32le(data, h + 14),
        csize,
        size,
        name,
        raw_name: raw.to_vec(),
        dos_time: u16le(data, h + 10),
        dos_date: u16le(data, h + 12),
    })
}

/// Every plausible local file header in the file. A ZIP stored inside an entry (a wheel, a
/// jar) shows its own headers in the data; one that lies inside a stored entry whose size is
/// known is skipped, as are the entries nested in it. A compressed entry's data shows no such
/// headers, and its size must not hide the real ones: after bytes were lost, an entry's size
/// reaches over the entries that follow.
fn scan_locals(data: &[u8]) -> Vec<Local> {
    let mut out: Vec<Local> = Vec::new();
    let mut i = 0;
    let mut cover_end = 0usize; // the data of the entries accepted so far reaches here
    while i + 4 <= data.len() {
        if data[i] == b'P' && data[i + 1] == b'K' && data[i + 2] == 3 && data[i + 3] == 4 {
            if let Some(l) = parse_local(data, i) {
                if l.offset < cover_end {
                    i += 4; // inside another entry's data: embedded
                    continue;
                }
                let known_size = l.flags & 8 == 0 || l.csize > 0;
                if known_size && l.method == STORED {
                    cover_end = cover_end.max(l.data_offset + l.csize as usize);
                }
                i = l.data_offset.max(i + 1);
                out.push(l);
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Data descriptor after a bit-3 entry's data: (crc, csize, size), with or without signature
fn data_descriptor(data: &[u8], at: usize) -> Option<(u32, u64, u64)> {
    let p = if at + 4 <= data.len() && u32le(data, at) == 0x0807_4b50 { at + 4 } else { at };
    if p + 12 > data.len() {
        return None;
    }
    Some((u32le(data, p), u32le(data, p + 4) as u64, u32le(data, p + 8) as u64))
}

/// `mode`: the permission bits the directory keeps for it, made on Unix or macOS (0: none)
fn write_file(root: Option<&Path>, name: &str, bytes: &[u8], dos_time: u16, dos_date: u16, mode: u32) -> Result<(), String> {
    let Some(root) = root else { return Ok(()) };
    let rel = zip::safe_relpath(name).ok_or("path leaves the output folder; skipped")?;
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = File::create(&path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
    if let Some(t) = zip::dos_time(dos_date, dos_time) {
        let _ = f.set_modified(t);
    }
    Ok(())
}

struct Task {
    name: String,
    raw_name: Vec<u8>,
    data_offset: Option<usize>, // None: the local header is destroyed
    header_offset: usize,
    method: u16,
    flags: u16,
    crc: Option<u32>,
    csize: Option<u64>,
    size: Option<u64>,
    dos_time: u16,
    dos_date: u16,
    from_scan: bool,
    cd_extra_len: usize,
    unix_mode: u32, // from the directory (a local header has none)
}

/// Recover every entry of `data` into `root` (none: only read). `fill` is written where bytes are unknown.
pub fn rescue_zip(data: &[u8], lib: &Rescue, root: Option<&Path>, fill: u8, keep_parts: bool) -> Outcome {
    let mut o = Outcome::default();
    let locals = scan_locals(data);
    o.scanned_headers = locals.len();
    let by_offset: BTreeMap<usize, &Local> = locals.iter().map(|l| (l.offset, l)).collect();
    let next_header_after = |pos: usize| -> usize { by_offset.range(pos + 1..).next().map(|(&k, _)| k).unwrap_or(data.len()) };

    // what the directory says, if it can be read
    let mut tasks: Vec<Task> = Vec::new();
    let mut covered: Vec<usize> = Vec::new(); // local header offsets the directory's entries took
    // the scanned headers by name: when bytes were inserted or lost before an entry, the offset
    // the directory gives is off, but the entry's own local header is still there, elsewhere
    // A header and its directory entry are matched on the name's bytes, or on the name they
    // decode to: decoding can tell a name apart from its own header (one flagged UTF-8, the other
    // not), and the bytes can differ for one name (a header in CP437 with Info-ZIP's Unicode Path
    // field, the directory in UTF-8: every such entry of a sound archive was rescued as if its
    // header were destroyed, partial, and its header, left over, as another entry of that name)
    let mut by_name: std::collections::HashMap<&[u8], Vec<&Local>> = std::collections::HashMap::new();
    let mut by_decoded: std::collections::HashMap<&str, Vec<&Local>> = std::collections::HashMap::new();
    for l in &locals {
        by_name.entry(l.raw_name.as_slice()).or_default().push(l);
        if l.name.as_bytes() != l.raw_name.as_slice() {
            by_decoded.entry(l.name.as_str()).or_default().push(l);
        }
    }
    match zip::read_central_directory(data, true) {
        Ok(cd) => {
            o.cd_ok = true;
            for e in &cd {
                let at = e.data_offset as usize; // the local header, as the directory says
                let local = by_offset.get(&at).copied().filter(|l| l.raw_name == e.raw_name || l.name == e.name).or_else(|| {
                    // the same name, the same CRC when the header has one, nearest the expected place
                    let named = by_name.get(e.raw_name.as_slice()).into_iter().flatten();
                    let decoded = by_decoded.get(e.name.as_str()).into_iter().flatten();
                    named
                        .chain(decoded)
                        .filter(|l| !covered.contains(&l.offset) && (l.crc == e.crc || (l.flags & 8 != 0 && l.crc == 0)))
                        .min_by_key(|l| (l.offset as i64 - at as i64).unsigned_abs())
                        .copied()
                });
                if let Some(l) = local {
                    if l.offset != at {
                        o.moved += 1;
                    }
                }
                let h = local.map(|l| l.offset).unwrap_or(at);
                covered.push(h);
                tasks.push(Task {
                    name: e.name.clone(),
                    raw_name: e.raw_name.clone(),
                    data_offset: local.map(|l| l.data_offset),
                    header_offset: h,
                    method: e.method,
                    flags: e.flags,
                    crc: Some(e.crc),
                    csize: Some(e.csize),
                    size: Some(e.size),
                    dos_time: e.dos_time,
                    dos_date: e.dos_date,
                    from_scan: false,
                    cd_extra_len: 0,
                    unix_mode: e.unix_mode,
                });
            }
            if o.moved > 0 {
                o.notes.push(format!(
                    "{} entries were not where the directory says: bytes were inserted or lost before them; found by name and CRC",
                    o.moved
                ));
            }
        }
        Err(e) => {
            o.cd_error = e;
            o.notes.push(format!("the central directory cannot be read ({}): the entries come from the local headers found by scanning", o.cd_error));
        }
    }
    let cd_names: std::collections::HashSet<Vec<u8>> = tasks.iter().map(|t| t.raw_name.clone()).collect();
    let cd_decoded: std::collections::HashSet<String> = tasks.iter().map(|t| t.name.clone()).collect();
    // entries only the scan knows
    for l in &locals {
        // a header the directory took, or a second copy of a name the directory has (bytes
        // written twice by a bad resume): skip
        if covered.contains(&l.offset) || cd_names.contains(&l.raw_name) || cd_decoded.contains(&l.name) {
            continue;
        }
        let bit3 = l.flags & 8 != 0;
        tasks.push(Task {
            name: l.name.clone(),
            raw_name: l.raw_name.clone(),
            data_offset: Some(l.data_offset),
            header_offset: l.offset,
            method: l.method,
            flags: l.flags,
            crc: if bit3 && l.crc == 0 { None } else { Some(l.crc) },
            csize: if bit3 && l.csize == 0 { None } else { Some(l.csize) },
            size: if bit3 && l.size == 0 { None } else { Some(l.size) },
            dos_time: l.dos_time,
            dos_date: l.dos_date,
            from_scan: true,
            cd_extra_len: 0,
            unix_mode: 0,
        });
    }
    tasks.sort_by_key(|t| t.header_offset);
    // an Office document; when cut short it has lost [Content_Types].xml, written last
    let office = tasks.iter().any(|t| t.name == "[Content_Types].xml")
        || (keep_parts && tasks.iter().any(|t| ["word/", "xl/", "ppt/"].iter().any(|d| t.name.starts_with(d))));
    let mut cr_left = CR_SEARCH_PER_FILE;
    // How far the bytes of the directory's entries that came out intact reach, and whose: the
    // input their data took, which their CRC vouches for. No two entries of a ZIP share bytes.
    // A directory entry whose data would start inside them is not written: a zip bomb multiplies
    // its data so (one 100 MB stream named by 20 entries: 2 GB written from 103 KB; Fifield's,
    // each entry quoting the next one's local header: 200 MB from 12 KB), and a directory whose
    // offsets are off (bytes lost before them) points into others (lib.zip with a 512 KB gap: 114
    // entries written from other entries' data, their longest run of right bytes 17). A header
    // only the scan found there is part of that entry's data (an archive stored in it: 7 in
    // lib.zip, written as 754 KB of nothing right), not an entry. Entries only the scan found set
    // no reach: data from another file written over a cluster can hold an intact entry of its own
    // (crosslinked: one of lib.zip's in the pptx, over the places of three of its entries). The
    // tasks go in the order of their headers, so the farthest reach is enough.
    let mut reach: (usize, String) = (0, String::new());
    let mut intact_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // The data of the directory's entries whose own local header was found, up to the next one's
    // header at most (bytes lost before it make the size reach past): a header only the scan
    // found in there is part of that entry (an archive stored in it, or a .jar in a tar that
    // Deflate kept in stored blocks: silesia's mozilla, damaged, gave 3 of them as entries), even
    // when the entry does not come out intact
    let mut cd_heads: Vec<usize> = tasks.iter().filter(|t| !t.from_scan).map(|t| t.header_offset).collect();
    cd_heads.sort_unstable();
    let spans: Vec<(usize, usize)> = tasks
        .iter()
        .filter(|t| !t.from_scan)
        .filter_map(|t| {
            let (d, c) = (t.data_offset?, t.csize? as usize);
            let next = cd_heads.iter().copied().find(|&h| h > t.header_offset).unwrap_or(data.len());
            (c > 0).then_some((d, (d + c).min(next)))
        })
        .collect();

    for t in tasks {
        if t.from_scan && (t.header_offset < reach.0 || spans.iter().any(|&(a, b)| a < t.header_offset && t.header_offset < b)) {
            o.inside += 1;
            continue;
        }
        if t.name.ends_with('/') {
            if let (Some(r), Some(rel)) = (root, zip::safe_relpath(&t.name)) {
                let _ = std::fs::create_dir_all(r.join(rel));
            }
            continue;
        }
        let mut rec = Recovered { name: t.name.clone(), status: Status::Lost, detail: String::new(), size: 0, from_scan: t.from_scan, office_part: None };
        if t.flags & 1 != 0 {
            rec.status = Status::Unsupported;
            rec.detail = "encrypted".into();
            o.entries.push(rec);
            continue;
        }
        if t.method != STORED && t.method != DEFLATED && !OTHER_METHODS.contains(&t.method) {
            rec.status = Status::Unsupported;
            rec.detail = format!("method {} is not read here", zip::method_name(t.method));
            o.entries.push(rec);
            continue;
        }
        // where the data starts: from the local header, or guessed when that header is destroyed
        let mut starts: Vec<usize> = Vec::new();
        // a rescue of the entry from each start, until one comes out this good: a whole rescue each
        let mut enough = Status::Intact;
        match t.data_offset {
            Some(d) => starts.push(d),
            None => {
                let base = t.header_offset + 30 + t.name.len();
                for x in [0usize, t.cd_extra_len, 9, 24, 28, 20] {
                    if base + x <= data.len() && !starts.contains(&(base + x)) {
                        starts.push(base + x);
                    }
                }
                rec.detail = "local header destroyed; data found by its CRC".into();
                if t.method == DEFLATED {
                    enough = order_starts(data, &t, &mut starts, |s| lost_bytes(data, &t, s, next_header_after(s)) == 255);
                }
            }
        }
        let mut best: Option<(Status, Vec<u8>, String, usize)> = None; // and where its data ends
        let mut shared = false; // a start lay in an earlier intact entry's bytes
        for start in starts {
            // (an empty entry has no data to share: lib.zip's empty __init__.py files came out
            // right from such starts)
            if start < reach.0 && t.size != Some(0) {
                shared = true;
                continue;
            }
            let limit = next_header_after(start);
            let (status, bytes, detail, crs, used) = recover_entry(data, lib, &t, start, limit, fill, &mut cr_left);
            if status == Status::Intact && crs > 0 {
                o.crs_restored += crs as u64;
                o.cr_entries += 1;
            }
            let better = match &best {
                None => true,
                Some((s, ..)) => (status as u8) < (*s as u8),
            };
            if better {
                best = Some((status, bytes, detail, start + used));
            }
            if (status as u8) <= (enough as u8) {
                break;
            }
        }
        if best.is_none() && shared {
            rec.detail = format!(
                "its data would be bytes of {}, which came out intact: no two entries share bytes (a zip bomb's do; or the directory is off here); not written",
                reach.1
            );
            o.shared += 1;
        }
        if let Some((status, bytes, detail, end)) = best {
            if status == Status::Intact && !t.from_scan && end > reach.0 {
                reach = (end, t.name.clone());
            }
            rec.status = status;
            rec.size = bytes.len() as u64;
            if !detail.is_empty() {
                rec.detail = if rec.detail.is_empty() { detail } else { format!("{}; {detail}", rec.detail) };
            }
            if status != Status::Lost && status != Status::Intact && intact_names.contains(&t.name) {
                // two entries of one name (bytes written twice, a header matched to no entry):
                // what one gave back intact is not written over
                let why = "not written: an entry of the same name came out intact";
                rec.detail = if rec.detail.is_empty() { why.into() } else { format!("{}; {why}", rec.detail) };
                o.kept_intact += 1;
            } else if status != Status::Lost {
                if let Err(e) = write_file(root, &t.name, &bytes, t.dos_time, t.dos_date, t.unix_mode) {
                    o.failed.push((t.name.clone(), e));
                }
                if status == Status::Intact {
                    intact_names.insert(t.name.clone());
                }
                if office && keep_parts {
                    rec.office_part = Some(bytes);
                }
            }
        }
        o.entries.push(rec);
    }
    if o.shared > 0 {
        o.notes.push(format!(
            "{} entr{} of the directory would have read bytes another entry had already given back intact: not written \
             (no two entries share data; a zip bomb's do, and a directory whose offsets are off points into others)",
            o.shared,
            if o.shared == 1 { "y" } else { "ies" }
        ));
    }
    if o.kept_intact > 0 {
        o.notes.push(format!(
            "{} entr{} not written over a file of the same name that another entry gave back intact",
            o.kept_intact,
            if o.kept_intact == 1 { "y" } else { "ies" }
        ));
    }
    if o.inside > 0 {
        o.notes.push(format!(
            "{} local header{} found inside other entries' data (an archive stored in one, or another file's bytes written over it): not entries of this one",
            o.inside,
            if o.inside == 1 { "" } else { "s" }
        ));
    }
    if root.is_some() {
        let renamed: Vec<String> = o
            .entries
            .iter()
            .filter(|e| !matches!(e.status, Status::Lost | Status::Unsupported) && zip::device_renamed(&e.name))
            .map(|e| e.name.clone())
            .collect();
        o.notes.extend(zip::device_note(&renamed));
    }
    if o.crs_restored > 0 {
        o.notes.push(format!(
            "the file went through an ASCII-mode transfer that removed the CR of every CR LF: {} CRs put back in {} entries, each entry's CRC confirming them",
            o.crs_restored, o.cr_entries
        ));
    }
    if office {
        o.office = Some(office_repair(&mut o.entries, root, fill));
    }
    o
}

/// How many bytes an entry's data lost, when that can be told: its stored compressed size against
/// the room it has up to the next header (a local one, or the directory's): 1-8, 254 for more,
/// 255 when it fits exactly, 0 when it cannot be told. For an ASCII transfer that removed CRs: how many.
fn lost_bytes(data: &[u8], t: &Task, start: usize, limit: usize) -> u8 {
    let Some(csize) = t.csize else { return 0 };
    let dir = data[start..limit.min(data.len())].windows(4).position(|w| w == b"PK\x01\x02").map(|k| start + k);
    let room = dir.unwrap_or(limit).min(limit).saturating_sub(start) as u64;
    // a bit-3 entry has a data descriptor after its data, 16 bytes with its signature or 12 without
    let rooms: &[u64] = if t.flags & 8 != 0 { &[16, 12] } else { &[0] };
    let fits: Vec<u64> = rooms.iter().filter_map(|&d| room.checked_sub(d)).filter(|&r| r <= csize && csize - r <= 8).map(|r| csize - r).collect();
    match fits.as_slice() {
        [0] => 255,
        [k] => *k as u8,
        // more lost than any search here puts back: room short by more than 8 bytes
        [] if rooms.iter().all(|&d| room.saturating_sub(d) + 8 < csize) => 254,
        _ => 0,
    }
}

/// Output a guessed start must decode to, cleanly and as the beginning of a stream, to be taken for
/// where an entry's data begins: what gzrescue decodes to confirm a block header it finds.
const PROBE_OUT: u64 = 256 << 10;

/// Whether the entry's Deflate stream begins at `start`: from there its first PROBE_OUT bytes (all
/// of it, when the entry is smaller) decode cleanly, nothing referring back before the start.
fn stream_begins(data: &[u8], t: &Task, start: usize) -> bool {
    let want = t.size.map_or(PROBE_OUT, |s| s.min(PROBE_OUT));
    let mut out = vec![0u8; want as usize];
    match libdeflater::Decompressor::new().deflate_decompress(&data[start..], &mut out) {
        // the whole stream: as long as the entry
        Ok(n) => t.size.map_or(true, |s| n as u64 == s),
        // the probe's worth, and the entry is longer
        Err(libdeflater::DecompressionError::InsufficientSpace) => t.size.map_or(true, |s| s > want),
        Err(libdeflater::DecompressionError::BadData) => false,
    }
}

/// The order in which to rescue a Deflate entry whose local header is destroyed from the places its
/// data may start at, and how good a result ends the search (each try is a whole rescue of the
/// entry). A start where the stream begins, when only one does, comes first, and anything it gives
/// back ends the search. Otherwise the start whose data ends where the next header begins (`fits`),
/// when only one does, comes first, so that its result is kept when the others' are no better (from
/// a wrong start the stream is cut short or runs into the next entry), and only an intact entry ends
/// the search; but when every start lies in one run of zeros or 0xFF bytes (a hole, erased flash),
/// no stream can begin at any of them: its beginning is lost whichever start is right, and none
/// can give back more than a partial entry.
fn order_starts(data: &[u8], t: &Task, starts: &mut Vec<usize>, fits: impl Fn(usize) -> bool) -> Status {
    let first = |starts: &mut Vec<usize>, s: usize| {
        starts.retain(|&x| x != s);
        starts.insert(0, s);
    };
    let begins: Vec<usize> = starts.iter().copied().filter(|&s| stream_begins(data, t, s)).collect();
    if let [s] = begins[..] {
        first(starts, s);
        return Status::Unverified;
    }
    if let [s] = starts.iter().copied().filter(|&s| fits(s)).collect::<Vec<_>>()[..] {
        first(starts, s);
    }
    let (Some(&lo), Some(&hi)) = (starts.iter().min(), starts.iter().max()) else { return Status::Intact };
    // from each start the stream's first block would be a stored one whose length and its
    // complement read zero, or of the invalid type 3
    let run = &data[lo..(hi + 5).min(data.len())];
    if begins.is_empty() && (run.iter().all(|&b| b == 0) || run.iter().all(|&b| b == 0xff)) {
        return Status::Partial;
    }
    Status::Intact
}

/// What the searches for CRs an ASCII-mode transfer removed may decode in one file, all entries
/// together (each at most 1 GiB): a ZIP of thousands of entries that each lost a few would
/// otherwise take hours. The CRC judges every repair, so the limit only gives up on some.
const CR_SEARCH_PER_FILE: u64 = 16 << 30;

/// One entry's data from `start`, bounded by `limit`: (status, bytes, detail, CRs put back).
/// `cr_left`: what the search for lost CRs may still decode in this file.
/// A raw Deflate stream that inflates to exactly `size` bytes, and how many input bytes it took
fn inflate_exact(slice: &[u8], size: usize) -> Option<(Vec<u8>, usize)> {
    use libdeflate_sys::*;
    let mut out = vec![0u8; size];
    let (mut used, mut made) = (0usize, 0usize);
    // SAFETY: the decompressor is checked, used once and freed; both buffers' lengths are passed
    unsafe {
        let d = libdeflate_alloc_decompressor();
        if d.is_null() {
            return None;
        }
        let r = libdeflate_deflate_decompress_ex(d, slice.as_ptr().cast(), slice.len(), out.as_mut_ptr().cast(), out.len(), &mut used, &mut made);
        libdeflate_free_decompressor(d);
        (r == libdeflate_result_LIBDEFLATE_SUCCESS && made == size).then_some((out, used))
    }
}

/// Methods read here besides stored and Deflate: Deflate64 (9), bzip2 (12), LZMA (14),
/// Zstandard (93, and 20, its number before 2020). Before, every such entry was "not readable
/// here", undamaged ones too (7-Zip's LZMA ZIP: 26 of 428 files back from a sound archive)
const OTHER_METHODS: [u16; 5] = [9, 12, 14, 20, 93];

/// An entry of one of OTHER_METHODS from its data at `start`: whole when it decodes to its size
/// and CRC. Where it breaks, what came before is given as partial, unchecked: these decoders
/// do not go on past damage (Deflate's block-by-block rescue does), so the rest is lost.
fn recover_other(data: &[u8], t: &Task, start: usize, limit: usize) -> (Status, Vec<u8>, String, u32, usize) {
    let end = match t.csize {
        Some(c) if c > 0 => (start as u64 + c).min(limit as u64).min(data.len() as u64) as usize,
        _ => limit.min(data.len()),
    };
    let src = &data[start..end];
    // at most one byte more than the directory says: an entry that lies about its size, or a
    // stream made to explode, stops there
    let cap = t.size.map_or(1 << 30, |s| s.saturating_add(1)).min(1 << 31) as usize;
    let (bytes, err) = decode_other(t.method, t.flags, src, t.size, cap);
    let name = zip::method_name(t.method);
    let cut = t.csize.is_some_and(|c| ((end - start) as u64) < c);
    // bzip2's blocks stand alone: one that breaks costs only itself
    let whole = err.is_none() && t.crc.is_some_and(|c| crc32fast::hash(&bytes) == c);
    if t.method == 12 && !whole {
        if let Some((out, lost, detail)) = bzip2_blocks(src, t.size, cap) {
            if lost == 0 && t.crc.is_some_and(|c| crc32fast::hash(&out) == c) && t.size.is_none_or(|s| s == out.len() as u64) {
                return (Status::Intact, out, String::new(), 0, end - start);
            }
            if lost > 0 || out.len() > bytes.len() {
                return (Status::Partial, out, detail, 0, 0);
            }
        }
    }
    match (err, t.crc) {
        (None, Some(c)) if crc32fast::hash(&bytes) == c && t.size.is_none_or(|s| s == bytes.len() as u64) => {
            (Status::Intact, bytes, String::new(), 0, end - start)
        }
        (None, Some(_)) => (Status::Unverified, bytes, format!("{name}: decodes, but the CRC does not match: damage the decoder could not see"), 0, 0),
        (None, None) => (Status::Unverified, bytes, "no CRC known".into(), 0, 0),
        (Some(e), _) => {
            let n = bytes.len();
            let why = if cut { "cut short".to_string() } else { e };
            let detail = format!("{name} stream broken ({why}): the {n} bytes before the break are given, unchecked; past a break {name} is not read here");
            (if n > 0 { Status::Partial } else { Status::Lost }, bytes, detail, 0, 0)
        }
    }
}

/// bzip2's CRC-32: MSB first, polynomial 0x04C11DB7, as each block stores its own
fn bzip2_crc(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = (i as u32) << 24;
            for _ in 0..8 {
                c = if c & 0x8000_0000 != 0 { c << 1 ^ 0x04C1_1DB7 } else { c << 1 };
            }
            *v = c;
        }
        t
    });
    !data.iter().fold(!0u32, |c, &b| c << 8 ^ t[((c >> 24) as u8 ^ b) as usize])
}

/// A bzip2 stream rescued block by block, as bzip2recover does. Every block begins with a 48-bit
/// mark at any bit (0x314159265359) and its own CRC, and is decoded on its own: each is wrapped
/// in a stream of one block and kept when its output has its CRC. A broken block costs only its
/// own bytes, written as zeros: where it lay comes from the entry's size when one block is lost
/// (exact), else from the sizes of the blocks around (close). A block whose mark is destroyed
/// lies in the bits of the one before, which then spans about twice as many; it is lost after
/// that one. Returns the output, how many blocks were lost, and what to say; None when no block
/// mark is found.
fn bzip2_blocks(src: &[u8], size: Option<u64>, cap: usize) -> Option<(Vec<u8>, usize, String)> {
    const BLOCK: u64 = 0x3141_5926_5359;
    const END: u64 = 0x1772_4538_5090;
    let bits = src.len() as u64 * 8;
    // the 48 bits from bit b, MSB first
    let take = |b: u64, n: u32| -> u64 {
        let mut v = 0u64;
        for i in 0..n as u64 {
            let at = b + i;
            let bit = if at < bits { (src[(at / 8) as usize] >> (7 - at % 8)) & 1 } else { 0 };
            v = v << 1 | bit as u64;
        }
        v
    };
    // every mark, bit by bit: a 64-bit window slid along the data
    let mut marks: Vec<(u64, bool)> = Vec::new(); // (bit, is the end mark)
    let mut window = 0u64;
    for (i, &byte) in src.iter().enumerate() {
        for k in 0..8 {
            window = window << 1 | ((byte >> (7 - k)) & 1) as u64;
            let n = i as u64 * 8 + k as u64 + 1;
            if n >= 48 {
                let w = window & 0xFFFF_FFFF_FFFF;
                if w == BLOCK || w == END {
                    marks.push((n - 48, w == END));
                }
            }
        }
    }
    let blocks: Vec<(u64, u64)> = marks
        .iter()
        .enumerate()
        .filter(|(_, (_, end))| !end)
        .map(|(k, &(b, _))| (b, marks.get(k + 1).map_or(bits, |&(n, _)| n)))
        .collect();
    if blocks.is_empty() {
        return None;
    }
    let level = if src.len() > 3 && &src[..3] == b"BZh" && (b'1'..=b'9').contains(&src[3]) { src[3] } else { b'9' };
    // each block, wrapped as a stream of its own: header, the block's bits, the end mark and a
    // stream CRC equal to the block's, padded to a byte
    let decode = |(from, to): (u64, u64)| -> Option<Vec<u8>> {
        let crc = take(from + 48, 32) as u32;
        let mut out = vec![b'B', b'Z', b'h', level];
        let (mut acc, mut n) = (0u64, 0u32);
        let mut put = |v: u64, w: u32, out: &mut Vec<u8>| {
            for i in (0..w).rev() {
                acc = acc << 1 | (v >> i) & 1;
                n += 1;
                if n == 8 {
                    out.push(acc as u8);
                    (acc, n) = (0, 0);
                }
            }
        };
        let mut b = from;
        while b < to {
            let w = (to - b).min(32) as u32;
            put(take(b, w), w, &mut out);
            b += w as u64;
        }
        put(END, 48, &mut out);
        put(crc as u64, 32, &mut out);
        if n > 0 {
            out.push((acc << (8 - n)) as u8);
        }
        let (got, _) = decode_other(12, 0, &out, None, cap.min(64 << 20));
        (!got.is_empty() && bzip2_crc(&got) == crc).then_some(got)
    };
    let decoded: Vec<Option<Vec<u8>>> = blocks.iter().map(|&r| decode(r)).collect();
    // a good block spanning about twice the bits of the others holds a lost block after its own
    let mut spans: Vec<u64> = blocks.iter().zip(&decoded).filter(|(_, d)| d.is_some()).map(|(&(a, b), _)| b - a).collect();
    spans.sort_unstable();
    let median_span = spans.get(spans.len() / 2).copied().unwrap_or(0);
    let mut pieces: Vec<Option<Vec<u8>>> = Vec::new(); // in order; None: a lost block
    for (k, d) in decoded.into_iter().enumerate() {
        let (a, b) = blocks[k];
        let swallowed = d.is_some() && median_span > 0 && k + 1 < blocks.len() && (b - a) * 2 > median_span * 3;
        pieces.push(d);
        if swallowed {
            pieces.push(None);
        }
    }
    let good: u64 = pieces.iter().flatten().map(|p| p.len() as u64).sum();
    let lost = pieces.iter().filter(|p| p.is_none()).count();
    // a lost block's size: a full block's, as every block but the stream's last is (the median of
    // the good ones but the last); the last lost one takes what the entry's size leaves
    let mut lens: Vec<u64> = pieces[..pieces.len() - 1].iter().flatten().map(|p| p.len() as u64).collect();
    lens.sort_unstable();
    let typical = lens.get(lens.len() / 2).copied().unwrap_or_else(|| pieces.iter().flatten().map(|p| p.len() as u64).max().unwrap_or(0));
    let missing = size.map(|s| s.saturating_sub(good));
    let mut out = Vec::new();
    let mut gaps = Vec::new();
    let mut left = lost;
    for p in &pieces {
        match p {
            Some(b) => out.extend_from_slice(b),
            None => {
                left -= 1;
                let given: u64 = gaps.iter().map(|&g| g as u64).sum();
                let n = match missing {
                    Some(m) if left == 0 => m.saturating_sub(given), // exact when one block is lost
                    Some(m) => typical.min(m.saturating_sub(given)),
                    None => typical,
                } as usize;
                gaps.push(n);
                out.resize(out.len() + n, 0);
            }
        }
    }
    if let (Some(s), Some(m)) = (size, missing) {
        if lost > 0 {
            out.resize((good + m).min(s) as usize, 0);
        }
    }
    let total = pieces.len();
    let place = if lost <= 1 && size.is_some() { "in place" } else { "where full blocks would put them: not exact, as a block's size varies" };
    let detail = format!(
        "{} of {total} bzip2 blocks rescued, each checked by its own CRC; {lost} lost ({} bytes written as zeros, {place})",
        total - lost,
        gaps.iter().sum::<usize>()
    );
    Some((out, lost, detail))
}

/// Decode `src`, an entry's data of method `method`, into at most `cap` bytes: what came out, and
/// the error that stopped it, if one did
fn decode_other(method: u16, flags: u16, src: &[u8], size: Option<u64>, cap: usize) -> (Vec<u8>, Option<String>) {
    use std::io::Read;
    // whatever a Read gives, up to cap
    fn read_all(r: &mut impl Read, cap: usize) -> (Vec<u8>, Option<String>) {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            match r.read(&mut buf) {
                Ok(0) => return (out, None),
                Ok(n) if out.len() + n > cap => return (out, Some("more data than the directory says".into())),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => return (out, Some(e.to_string())),
            }
        }
    }
    match method {
        12 => read_all(&mut bzip2::read::BzDecoder::new(src), cap),
        20 | 93 => match ruzstd::decoding::StreamingDecoder::new(src) {
            Ok(mut d) => read_all(&mut d, cap),
            Err(e) => (Vec::new(), Some(e.to_string())),
        },
        14 => {
            // ZIP's LZMA: 2 bytes of version, the size of the properties (5), the properties, then
            // the stream, which ends with a marker when bit 1 of the flags is set
            if src.len() < 9 || u16::from_le_bytes([src[2], src[3]]) != 5 {
                return (Vec::new(), Some("not an LZMA header".into()));
            }
            struct Capped(Vec<u8>, usize);
            impl std::io::Write for Capped {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    if self.0.len() + b.len() > self.1 {
                        return Err(std::io::Error::other("more data than the directory says"));
                    }
                    self.0.extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let known = if flags & 2 != 0 { None } else { size };
            let opts = lzma_rs::decompress::Options { unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(known), ..Default::default() };
            let mut out = Capped(Vec::new(), cap);
            let r = lzma_rs::lzma_decompress_with_options(&mut &src[4..], &mut out, &opts);
            (out.0, r.err().map(|e| e.to_string()))
        }
        9 => {
            let mut inf = deflate64::InflaterManaged::new();
            let (mut out, mut at) = (Vec::new(), 0usize);
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let r = inf.inflate(&src[at..], &mut buf);
                at += r.bytes_consumed;
                if out.len() + r.bytes_written > cap {
                    return (out, Some("more data than the directory says".into()));
                }
                out.extend_from_slice(&buf[..r.bytes_written]);
                if r.data_error || inf.errored() {
                    return (out, Some("invalid Deflate64 data".into()));
                }
                if inf.finished() {
                    return (out, None);
                }
                if r.bytes_consumed == 0 && r.bytes_written == 0 {
                    return (out, Some("the stream ends before its last block".into()));
                }
            }
        }
        _ => (Vec::new(), Some(format!("method {method} is not read here"))),
    }
}

/// One entry from its data at `start`: its status, bytes, what to say, CRs put back, and for an
/// intact one how many input bytes its data took
fn recover_entry(data: &[u8], lib: &Rescue, t: &Task, start: usize, limit: usize, fill: u8, cr_left: &mut u64) -> (Status, Vec<u8>, String, u32, usize) {
    if start >= data.len() {
        return (Status::Lost, Vec::new(), "data lies beyond the end of the file".into(), 0, 0);
    }
    if OTHER_METHODS.contains(&t.method) {
        return recover_other(data, t, start, limit);
    }
    if t.method == STORED {
        let size = t.size.or(t.csize);
        let end = match size {
            Some(s) => (start as u64 + s).min(data.len() as u64) as usize,
            None => limit, // a bit-3 stored entry: up to the next header, descriptor included
        };
        let mut bytes = data[start..end].to_vec();
        let cut = size.map_or(false, |s| ((end - start) as u64) < s);
        let mut crc = t.crc;
        if size.is_none() {
            // the descriptor sits at the end: (crc, csize, size), signature optional
            if let Some((c, _, s)) = data_descriptor(data, end.saturating_sub(16)).or_else(|| data_descriptor(data, end.saturating_sub(12))) {
                if (s as usize) <= bytes.len() {
                    bytes.truncate(s as usize);
                    crc = Some(c);
                }
            }
        }
        if cut {
            let n = bytes.len();
            return (Status::Partial, bytes, format!("cut short: {n} of {} bytes", size.unwrap_or(0)), 0, 0);
        }
        match crc {
            Some(c) if crc32fast::hash(&bytes) == c => (Status::Intact, bytes, String::new(), 0, end - start),
            Some(_) => (Status::Unverified, bytes, "stored data, CRC mismatch: damaged somewhere inside".into(), 0, 0),
            None => (Status::Unverified, bytes, "no CRC known".into(), 0, 0),
        }
    } else {
        let end = match t.csize {
            Some(c) if c > 0 => (start as u64 + c).min(data.len() as u64) as usize,
            _ => limit,
        };
        let slice = &data[start..end];
        // the common case first: an entry that inflates to its size and CRC is intact, with no
        // need for the block-by-block rescue, which mostly searches for block headers (a 17 MB
        // ZIP of 3,440 entries: 25 s through the rescue, 2 s this way)
        if let (Some(size), Some(crc)) = (t.size, t.crc) {
            if size <= 1 << 31 {
                if let Some((out, used)) = inflate_exact(slice, size as usize) {
                    if crc32fast::hash(&out) == crc {
                        return (Status::Intact, out, String::new(), 0, used);
                    }
                }
            }
        }
        let mut out = Vec::with_capacity(t.size.unwrap_or(slice.len() as u64 * 3) as usize);
        let (mut unknown, mut inferred) = (0u64, 0u64);
        // with the file's budget spent, no CRs are looked for (255: none lost)
        let crs = if *cr_left == 0 { 255 } else { lost_bytes(data, t, start, limit) };
        let report = lib.rescue_raw(slice, (t.size, t.crc), fill, crs, (*cr_left).min(1 << 30), |d, m| {
            out.extend_from_slice(d);
            unknown += m.iter().filter(|&&x| x == UNKNOWN).count() as u64;
            inferred += m.iter().filter(|&&x| x == INFERRED).count() as u64;
            true
        });
        let report = match report {
            Ok(r) => r,
            Err(e) => return (Status::Lost, Vec::new(), e, 0, 0),
        };
        let s = &report.stats;
        *cr_left = cr_left.saturating_sub(s.cr_decoded);
        let crs = s.cr_restored;
        let cut = t.csize.map_or(false, |c| ((end - start) as u64) < c) || s.truncated != 0;
        if s.damaged > 0 || unknown > 0 || inferred > 0 || cut {
            let mut d = Vec::new();
            if s.damaged > 0 {
                d.push(format!("{} damaged block{}", s.damaged, if s.damaged == 1 { "" } else { "s" }));
            }
            if unknown > 0 {
                d.push(format!("{} bytes unknown", unknown));
            }
            if inferred > 0 {
                d.push(format!("{} bytes inferred", inferred));
            }
            if cut {
                d.push("cut short".into());
            }
            return (Status::Partial, out, d.join(", "), 0, 0);
        }
        // in step throughout: the CRC decides
        let crc = t.crc.or_else(|| {
            // bit 3: the descriptor follows the stream
            data_descriptor(data, start + s.input_end as usize).map(|(c, _, _)| c)
        });
        match crc {
            Some(c) if crc32fast::hash(&out) == c && t.size.map_or(true, |z| z == out.len() as u64) => {
                let d = if crs > 0 { format!("{crs} CR{} put back that an ASCII transfer had removed; the CRC confirms", if crs == 1 { "" } else { "s" }) } else { String::new() };
                (Status::Intact, out, d, crs, s.input_end as usize)
            }
            Some(_) => (Status::Unverified, out, "decodes in step but the CRC does not match: damage the decoder could not see".into(), 0, 0),
            None => (Status::Unverified, out, "no CRC known".into(), 0, 0),
        }
    }
}

// ---------------------------------------------------------------- Office documents

/// Make a damaged XML part well-formed again: garbage and unknown bytes are dropped, broken
/// tags removed, open elements closed in order. The result is not the original, but it opens.
pub fn repair_xml(src: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(src);
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut stack: Vec<String> = Vec::new();
    let mut declared: std::collections::HashSet<String> = ["xml".to_string()].into_iter().collect();
    let mut root: Option<String> = None; // the document element, once it began
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        if bytes[i] != b'<' {
            // text: keep what XML allows, up to the next tag; an '&' must begin a whole entity
            let j = text[i..].find('<').map(|k| i + k).unwrap_or(n);
            let seg = &text[i..j];
            if stack.is_empty() {
                // outside the document element only white space may stand (and a byte order mark)
                if seg.chars().all(|c| c.is_whitespace() || c == '\u{feff}') {
                    out.push_str(seg);
                }
                i = j;
                continue;
            }
            let mut k = 0;
            while k < seg.len() {
                let ch = seg[k..].chars().next().unwrap();
                let w = ch.len_utf8();
                let ok = ch == '\t' || ch == '\n' || ch == '\r' || (ch >= ' ' && ch != '\u{fffd}' && ch != '\u{fffe}' && ch != '\u{ffff}');
                if !ok {
                    k += w;
                    continue;
                }
                if ch == '&' {
                    let entity_len = seg[k..].find(';').filter(|&e| {
                        let body = &seg[k + 1..k + e];
                        !body.is_empty()
                            && body.len() <= 10
                            && (body.chars().all(|c| c.is_ascii_alphanumeric())
                                || body.strip_prefix('#').map_or(false, |d| d.chars().all(|c| c.is_ascii_digit()) && !d.is_empty())
                                || body.strip_prefix("#x").map_or(false, |d| d.chars().all(|c| c.is_ascii_hexdigit()) && !d.is_empty()))
                    });
                    match entity_len {
                        Some(e) => {
                            out.push_str(&seg[k..k + e + 1]);
                            k += e + 1;
                        }
                        None => {
                            out.push_str("&amp;");
                            k += 1;
                        }
                    }
                    continue;
                }
                if ch == ']' && seg[k..].starts_with("]]>") {
                    out.push_str("]]&gt;");
                    k += 3;
                    continue;
                }
                out.push(ch);
                k += w;
            }
            i = j;
            continue;
        }
        // a tag: find its end, honouring quotes; comments, declarations and CDATA as a unit
        let rest = &text[i..];
        if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(k) => {
                    out.push_str(&rest[..k + 3]);
                    i += k + 3;
                }
                None => i = n,
            }
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            match rest.find("]]>") {
                Some(k) => {
                    out.push_str(&rest[..k + 3]);
                    i += k + 3;
                }
                None => i = n,
            }
            continue;
        }
        let mut j = i + 1;
        let mut quote: Option<u8> = None;
        let mut closed = false;
        while j < n {
            let c = bytes[j];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => {
                    if c == b'"' || c == b'\'' {
                        quote = Some(c);
                    } else if c == b'>' {
                        closed = true;
                        break;
                    } else if c == b'<' {
                        break; // a new tag began: this one is broken
                    }
                }
            }
            j += 1;
        }
        if !closed {
            i = j; // drop the broken tag
            continue;
        }
        let tag = &text[i..j + 1];
        i = j + 1;
        if tag.starts_with("<?") || tag.starts_with("<!") {
            // the XML declaration only at the very start, a doctype only before the document
            // element: copies of them from garbage further in are dropped
            let declaration = tag.len() > 5 && tag[2..5].eq_ignore_ascii_case("xml") && !is_name_char(tag[5..].chars().next().unwrap_or(' '));
            let keep = if declaration {
                out.is_empty() || out == "\u{feff}"
            } else if tag.starts_with("<!DOCTYPE") {
                root.is_none()
            } else {
                tag.ends_with("?>")
            };
            if keep {
                out.push_str(tag);
            }
            continue;
        }
        let inner = &tag[1..tag.len() - 1];
        if let Some(end_name) = inner.strip_prefix('/') {
            let name = end_name.trim();
            if !valid_qname(name, &declared) {
                continue;
            }
            // close down to the matching element; an unmatched end tag is dropped, and so is one
            // closing the document element with more than white space after it: garbage can
            // hold such a tag, and what follows is still the document
            if let Some(pos) = stack.iter().rposition(|s| s == name) {
                if pos == 0 && !text[i..].trim().is_empty() {
                    continue;
                }
                while stack.len() > pos {
                    let open = stack.pop().unwrap();
                    out.push_str(&format!("</{open}>"));
                }
            }
            continue;
        }
        let self_closing = inner.ends_with('/');
        let body = if self_closing { &inner[..inner.len() - 1] } else { inner };
        let Some(name) = tag_name_and_attributes(body, &mut declared) else { continue }; // garbage that looks like a tag
        if stack.is_empty() && root.is_some() {
            continue; // a second document element: XML allows one
        }
        if root.is_none() {
            root = Some(name.clone());
        }
        out.push_str(tag);
        if !self_closing {
            stack.push(name);
        }
    }
    while let Some(open) = stack.pop() {
        out.push_str(&format!("</{open}>"));
    }
    out.into_bytes()
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == ':' || c == '-' || c == '.'
}

/// A qualified name: `local` or `prefix:local`, the prefix one the document declared
fn valid_qname(name: &str, declared: &std::collections::HashSet<String>) -> bool {
    let mut parts = name.splitn(2, ':');
    let first = parts.next().unwrap_or("");
    let good_part = |p: &str| !p.is_empty() && p.chars().all(is_name_char) && !p.contains(':') && (p.chars().next().unwrap().is_alphabetic() || p.starts_with('_'));
    match parts.next() {
        None => good_part(first),
        Some(local) => good_part(first) && good_part(local) && declared.contains(first),
    }
}

/// A start tag's body must be a qualified name followed by `name="value"` pairs. Returns the
/// element's name, after adding the namespace prefixes the tag declares to `declared`; None for
/// text that only looks like a tag.
fn tag_name_and_attributes(body: &str, declared: &mut std::collections::HashSet<String>) -> Option<String> {
    let name: String = body.chars().take_while(|&c| is_name_char(c)).collect();
    if name.is_empty() {
        return None;
    }
    let mut r = &body[name.len()..];
    let mut new_prefixes = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    loop {
        r = r.trim_start();
        if r.is_empty() {
            break;
        }
        let an: String = r.chars().take_while(|&c| is_name_char(c)).collect();
        if an.is_empty() || seen.contains(&an) {
            return None; // not an attribute, or one given twice: XML allows neither
        }
        seen.push(an.clone());
        r = r[an.len()..].trim_start();
        if !r.starts_with('=') {
            return None;
        }
        r = r[1..].trim_start();
        let q = r.chars().next()?;
        if q != '"' && q != '\'' {
            return None;
        }
        let end = r[1..].find(q)?;
        if r[1..1 + end].contains('<') {
            return None;
        }
        if let Some(p) = an.strip_prefix("xmlns:") {
            new_prefixes.push(p.to_string());
        } else if an != "xmlns" && !valid_qname(&an, declared) && !new_prefixes.iter().any(|p| an.starts_with(&format!("{p}:"))) {
            return None;
        }
        r = &r[1 + end + 1..];
    }
    for p in &new_prefixes {
        declared.insert(p.clone());
    }
    if !valid_qname(&name, declared) {
        for p in &new_prefixes {
            declared.remove(p);
        }
        return None;
    }
    Some(name)
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The text of an Office XML part: the contents of w:t (Word), a:t (PowerPoint) and t
/// (spreadsheet shared strings) elements, paragraphs on their own lines
pub fn extract_text(xml: &[u8]) -> String {
    let s = String::from_utf8_lossy(xml);
    let mut out = String::new();
    let mut i = 0;
    while let Some(k) = s[i..].find('<') {
        let start = i + k;
        let Some(e) = s[start..].find('>') else { break };
        let tag = &s[start + 1..start + e];
        i = start + e + 1;
        let name: &str = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if name == "w:t" || name == "a:t" || name == "t" {
            if tag.ends_with('/') {
                continue;
            }
            let close = format!("</{name}>");
            if let Some(c) = s[i..].find(&close) {
                out.push_str(&xml_unescape(&s[i..i + c]));
                i += c + close.len();
            }
        } else if name == "/w:p" || name == "/a:p" || name == "w:br" || name == "/si" || name == "/w:tr" {
            out.push('\n');
        } else if name == "w:tab" {
            out.push('\t');
        }
    }
    out
}

fn is_xml_part(name: &str) -> bool {
    let l = name.to_lowercase();
    l.ends_with(".xml") || l.ends_with(".rels")
}

/// Repair the XML parts that were damaged, extract the text, rebuild a document
fn office_repair(entries: &mut [Recovered], root: Option<&Path>, _fill: u8) -> OfficeOutcome {
    let mut of = OfficeOutcome::default();
    of.kind = if entries.iter().any(|e| e.name.starts_with("word/")) {
        "docx".into()
    } else if entries.iter().any(|e| e.name.starts_with("xl/")) {
        "xlsx".into()
    } else if entries.iter().any(|e| e.name.starts_with("ppt/")) {
        "pptx".into()
    } else {
        "office".into()
    };
    let mut text = String::new();
    for e in entries.iter_mut() {
        let Some(part) = e.office_part.as_mut() else { continue };
        if is_xml_part(&e.name) && e.status != Status::Intact {
            let fixed = repair_xml(part);
            if fixed != *part {
                *part = fixed;
                of.repaired_parts.push(e.name.clone());
                if let Err(err) = write_file(root, &e.name, part, 0, 0, 0) {
                    e.detail = format!("{}; could not write the repaired part: {err}", e.detail);
                }
            }
        }
        let main_text = (of.kind == "docx" && e.name == "word/document.xml")
            || (of.kind == "xlsx" && e.name == "xl/sharedStrings.xml")
            || (of.kind == "pptx" && e.name.starts_with("ppt/slides/slide") && e.name.ends_with(".xml"));
        if main_text {
            let t = extract_text(part);
            if of.kind == "pptx" && !t.trim().is_empty() {
                text.push_str(&format!("--- {}\n", e.name));
            }
            text.push_str(&t);
        }
    }
    if let Some(r) = root {
        if !text.trim().is_empty() {
            let p = r.join(format!("recovered-text.txt"));
            if std::fs::write(&p, text.as_bytes()).is_ok() {
                of.text_chars = text.chars().count();
                of.text_file = Some(p);
            }
        }
        // a document from the recovered parts, stored, so an Office application can open it
        let mut parts: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for e in entries.iter() {
            let Some(part) = &e.office_part else { continue };
            if e.status == Status::Lost || e.status == Status::Unsupported {
                continue;
            }
            // a damaged calculation chain is only a cache: Office rebuilds it
            if e.name == "xl/calcChain.xml" && e.status != Status::Intact {
                continue;
            }
            parts.insert(e.name.clone(), part.clone());
        }
        // a damaged part with nothing left of its XML is as good as lost
        let damaged: std::collections::HashSet<&str> = entries.iter().filter(|e| e.status != Status::Intact).map(|e| e.name.as_str()).collect();
        let mut fixes = Vec::new();
        let gone: Vec<String> = parts.iter().filter(|(n, b)| damaged.contains(n.as_str()) && is_xml_part(n) && !has_element(b)).map(|(n, _)| n.clone()).collect();
        for n in gone {
            parts.remove(&n);
            fixes.push(format!("{n}: nothing of it survived, left out"));
        }
        fixes.extend(crate::officefix::canonical_case(&mut parts));
        fixes.extend(crate::officefix::replace_damaged_themes(&mut parts, &damaged));
        match of.kind.as_str() {
            "xlsx" => fixes.extend(fix_spreadsheet(&mut parts, &damaged)),
            "docx" => fixes.extend(crate::officefix::fix_word(&mut parts, &damaged)),
            "pptx" => fixes.extend(crate::officefix::fix_presentation(&mut parts, &damaged)),
            _ => {}
        }
        fixes.extend(make_openable(&mut parts, &of.kind));
        // without its main part (and nothing found to make one from) a package is no document:
        // Word and PowerPoint refuse it
        let main = main_part(&of.kind);
        let whole = main.is_empty() || parts.contains_key(main);
        if !whole {
            fixes.push(format!("{main} was lost and nothing found could stand in for it: no document rebuilt"));
        }
        of.package_fixes = fixes;
        if whole && !parts.is_empty() {
            let zipped = stored_zip(&parts);
            let p = r.join(format!("recovered.{}", of.kind));
            if std::fs::write(&p, &zipped).is_ok() {
                of.rebuilt = Some(p);
            }
        }
    }
    of
}

/// It holds at least one element
fn has_element(xml: &[u8]) -> bool {
    let s = String::from_utf8_lossy(xml);
    s.match_indices('<').any(|(i, _)| s[i + 1..].chars().next().map_or(false, |c| c.is_alphabetic() || c == '_'))
}

/// A workbook for sheets whose own was lost, and every sheet that cannot be opened as it is
/// rebuilt from its valid cells: a damaged one, or one referring to strings or formats that
/// are gone. An intact sheet that refers to nothing missing is kept as it is.
fn fix_spreadsheet(parts: &mut BTreeMap<String, Vec<u8>>, damaged: &std::collections::HashSet<&str>) -> Vec<String> {
    let mut fixes = Vec::new();
    if !parts.contains_key("xl/workbook.xml") {
        let n = crate::sheetfix::synthesize_workbook(parts);
        if n > 0 {
            fixes.push(format!("xl/workbook.xml was lost: one made for the {n} sheets found, named by their numbers"));
        }
    }
    let lim = crate::sheetfix::limits(parts);
    let sheets: Vec<String> = parts.keys().filter(|n| n.starts_with("xl/worksheets/") && n.ends_with(".xml")).cloned().collect();
    for name in sheets {
        let Some(r) = crate::sheetfix::sanitize_sheet(&parts[&name], &lim) else {
            parts.remove(&name);
            fixes.push(format!("{name}: no worksheet left in it, left out"));
            continue;
        };
        if !damaged.contains(name.as_str()) && r.dropped == 0 && r.restyled == 0 {
            continue;
        }
        let mut what = vec!["rebuilt from its valid cells".to_string()];
        if r.dropped > 0 {
            what.push(format!("{} cells that could not stand dropped", r.dropped));
        }
        if r.restyled > 0 {
            what.push(format!("{} cells given the default format, theirs being lost", r.restyled));
        }
        parts.insert(name.clone(), r.xml);
        fixes.push(format!("{name}: {}", what.join(", ")));
    }
    fixes
}

/// A ZIP of the parts, stored (no compression), in the order Office expects: content types first
fn stored_zip(parts: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut names: Vec<&String> = parts.keys().collect();
    names.sort_by_key(|n| (n.as_str() != "[Content_Types].xml", n.as_str() != "_rels/.rels", n.to_string()));
    let (mut zipped, mut central, mut count) = (Vec::new(), Vec::new(), 0u16);
    for name in names {
        let part = &parts[name];
        let nb = name.as_bytes();
        let crc = crc32fast::hash(part);
        let offset = zipped.len() as u32;
        zipped.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        zipped.extend_from_slice(&[20, 0, 0, 8, 0, 0, 0, 0, 0x21, 0]); // version 2.0, UTF-8 names, stored, 1980-01-01
        zipped.extend_from_slice(&crc.to_le_bytes());
        zipped.extend_from_slice(&(part.len() as u32).to_le_bytes());
        zipped.extend_from_slice(&(part.len() as u32).to_le_bytes());
        zipped.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        zipped.extend_from_slice(&0u16.to_le_bytes());
        zipped.extend_from_slice(nb);
        zipped.extend_from_slice(part);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 8, 0, 0, 0, 0, 0x21, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(part.len() as u32).to_le_bytes());
        central.extend_from_slice(&(part.len() as u32).to_le_bytes());
        central.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 12]); // extra, comment, disk, attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(nb);
        count += 1;
    }
    let cd_offset = zipped.len() as u32;
    zipped.extend_from_slice(&central);
    zipped.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    zipped.extend_from_slice(&[0, 0, 0, 0]);
    zipped.extend_from_slice(&count.to_le_bytes());
    zipped.extend_from_slice(&count.to_le_bytes());
    zipped.extend_from_slice(&(central.len() as u32).to_le_bytes());
    zipped.extend_from_slice(&cd_offset.to_le_bytes());
    zipped.extend_from_slice(&0u16.to_le_bytes());
    zipped
}

/// The value of attribute `name` in an element's text
pub(crate) fn attr(elem: &str, name: &str) -> Option<String> {
    let mut rest = elem;
    loop {
        let k = rest.find(name)?;
        let before = rest[..k].chars().last();
        let after = &rest[k + name.len()..];
        let after_t = after.trim_start();
        if before.map_or(false, |c| c.is_whitespace()) && after_t.starts_with('=') {
            let v = after_t[1..].trim_start();
            let q = v.chars().next()?;
            if q == '"' || q == '\'' {
                let end = v[1..].find(q)?;
                return Some(v[1..1 + end].to_string());
            }
            return None;
        }
        rest = &rest[k + name.len()..];
    }
}

/// The XML with the elements named `tag` for which `keep` says false removed (self-closing or
/// with content); returns it and how many went
pub(crate) fn filter_elements(xml: &str, tag: &str, mut keep: impl FnMut(&str) -> bool) -> (String, usize) {
    let mut out = String::with_capacity(xml.len());
    let mut removed = 0;
    let open = format!("<{tag}");
    let mut i = 0;
    while let Some(k) = xml[i..].find(&open) {
        let start = i + k;
        let next = xml[start + open.len()..].chars().next();
        if !matches!(next, Some(c) if c.is_whitespace() || c == '/' || c == '>') {
            out.push_str(&xml[i..start + open.len()]);
            i = start + open.len();
            continue;
        }
        let Some(gt) = xml[start..].find('>') else { break };
        let head_end = start + gt + 1;
        let self_closing = xml[start..head_end].ends_with("/>");
        let end = if self_closing {
            head_end
        } else {
            let close = format!("</{tag}>");
            xml[head_end..].find(&close).map(|c| head_end + c + close.len()).unwrap_or(head_end)
        };
        out.push_str(&xml[i..start]);
        if keep(&xml[start..head_end]) {
            out.push_str(&xml[start..end]);
        } else {
            removed += 1;
        }
        i = end;
    }
    out.push_str(&xml[i..]);
    (out, removed)
}

/// The part a relationship's target names, from the folder of the part that owns the .rels
pub(crate) fn resolve_target(rels_name: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    // xl/_rels/workbook.xml.rels -> xl/ ; _rels/.rels -> (root)
    let base = rels_name.rsplit_once("_rels/").map(|(dir, _)| dir).unwrap_or("");
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn main_part(kind: &str) -> &'static str {
    match kind {
        "docx" => "word/document.xml",
        "xlsx" => "xl/workbook.xml",
        "pptx" => "ppt/presentation.xml",
        _ => "",
    }
}

fn content_type_for(name: &str) -> Option<&'static str> {
    let ct = |s: &'static str| Some(s);
    if name == "word/document.xml" {
        return ct("application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml");
    }
    if name == "word/styles.xml" {
        return ct("application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml");
    }
    if name == "word/settings.xml" {
        return ct("application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml");
    }
    if name == "xl/workbook.xml" {
        return ct("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml");
    }
    if name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml") {
        return ct("application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml");
    }
    if name == "xl/sharedStrings.xml" {
        return ct("application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml");
    }
    if name == "xl/styles.xml" {
        return ct("application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml");
    }
    if name == "ppt/presentation.xml" {
        return ct("application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml");
    }
    if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
        return ct("application/vnd.openxmlformats-officedocument.presentationml.slide+xml");
    }
    if name.starts_with("xl/theme/theme") || name.starts_with("ppt/theme/theme") || name.starts_with("word/theme/theme") {
        return ct("application/vnd.openxmlformats-officedocument.theme+xml");
    }
    if name == "docProps/core.xml" {
        return ct("application/vnd.openxmlformats-package.core-properties+xml");
    }
    if name == "docProps/app.xml" {
        return ct("application/vnd.openxmlformats-officedocument.extended-properties+xml");
    }
    // the other parts Office writes often, by folder and name
    let numbered = |prefix: &str| name.strip_prefix(prefix).and_then(|r| r.strip_suffix(".xml")).map_or(false, |k| !k.is_empty() && k.chars().all(|c| c.is_ascii_digit()));
    match () {
        _ if numbered("word/header") => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"),
        _ if numbered("word/footer") => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"),
        _ if name == "word/numbering.xml" => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"),
        _ if name == "word/fontTable.xml" => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"),
        _ if name == "word/webSettings.xml" => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.webSettings+xml"),
        _ if name == "word/footnotes.xml" => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"),
        _ if name == "word/endnotes.xml" => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.endnotes+xml"),
        _ if name == "word/comments.xml" => ct("application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml"),
        _ if numbered("ppt/slideMasters/slideMaster") => ct("application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"),
        _ if numbered("ppt/slideLayouts/slideLayout") => ct("application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"),
        _ if numbered("ppt/notesSlides/notesSlide") => ct("application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"),
        _ if numbered("ppt/notesMasters/notesMaster") => ct("application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"),
        _ if numbered("ppt/handoutMasters/handoutMaster") => ct("application/vnd.openxmlformats-officedocument.presentationml.handoutMaster+xml"),
        _ if name == "ppt/presProps.xml" => ct("application/vnd.openxmlformats-officedocument.presentationml.presProps+xml"),
        _ if name == "ppt/viewProps.xml" => ct("application/vnd.openxmlformats-officedocument.presentationml.viewProps+xml"),
        _ if name == "ppt/tableStyles.xml" => ct("application/vnd.openxmlformats-officedocument.presentationml.tableStyles+xml"),
        _ if name == "xl/calcChain.xml" => ct("application/vnd.openxmlformats-officedocument.spreadsheetml.calcChain+xml"),
        _ if numbered("xl/tables/table") => ct("application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"),
        _ if numbered("xl/drawings/drawing") => ct("application/vnd.openxmlformats-officedocument.drawing+xml"),
        _ if numbered("xl/charts/chart") || numbered("word/charts/chart") || numbered("ppt/charts/chart") => ct("application/vnd.openxmlformats-officedocument.drawingml.chart+xml"),
        _ if name == "docProps/custom.xml" => ct("application/vnd.openxmlformats-officedocument.custom-properties+xml"),
        _ => None,
    }
}

/// Make the recovered parts a package an Office application opens: no references to parts that
/// were lost, and the package's own files (content types, root relationships) present. Returns
/// what was changed.
fn make_openable(parts: &mut BTreeMap<String, Vec<u8>>, kind: &str) -> Vec<String> {
    let mut fixes = Vec::new();
    let well_formed = |b: &[u8]| -> bool { repair_xml(b) == b };
    // [Content_Types].xml
    let ct_ok = parts.get("[Content_Types].xml").map_or(false, |b| well_formed(b) && String::from_utf8_lossy(b).contains("<Types"));
    if !ct_ok {
        let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">");
        for (ext, t) in [("rels", "application/vnd.openxmlformats-package.relationships+xml"), ("xml", "application/xml"), ("png", "image/png"),
                         ("jpeg", "image/jpeg"), ("jpg", "image/jpeg"), ("gif", "image/gif"), ("emf", "image/x-emf"), ("wmf", "image/x-wmf"),
                         ("bin", "application/vnd.openxmlformats-officedocument.oleObject"), ("bmp", "image/bmp"), ("tif", "image/tiff"), ("tiff", "image/tiff"),
                         ("svg", "image/svg+xml"), ("wdp", "image/vnd.ms-photo"), ("vml", "application/vnd.openxmlformats-officedocument.vmlDrawing"),
                         ("odttf", "application/vnd.openxmlformats-officedocument.obfuscatedFont"), ("fntdata", "application/x-fontdata"),
                         ("mp4", "video/mp4"), ("wav", "audio/wav"), ("mp3", "audio/mpeg"), ("m4a", "audio/mp4"),
                         ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
                         ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
                         ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation")] {
            x.push_str(&format!("<Default Extension=\"{ext}\" ContentType=\"{t}\"/>"));
        }
        for name in parts.keys() {
            if let Some(t) = content_type_for(name) {
                x.push_str(&format!("<Override PartName=\"/{name}\" ContentType=\"{t}\"/>"));
            }
        }
        x.push_str("</Types>");
        parts.insert("[Content_Types].xml".into(), x.into_bytes());
        fixes.push("[Content_Types].xml rebuilt from the parts present".to_string());
    } else {
        let present: std::collections::HashSet<String> = parts.keys().cloned().collect();
        let ct = String::from_utf8_lossy(&parts["[Content_Types].xml"]).into_owned();
        let (x, n) = filter_elements(&ct, "Override", |e| attr(e, "PartName").map_or(true, |p| present.contains(p.trim_start_matches('/'))));
        if n > 0 {
            parts.insert("[Content_Types].xml".into(), x.into_bytes());
            fixes.push(format!("{n} content types of lost parts removed"));
        }
        // a part made here, or one whose override was lost, would pass for plain XML: Office
        // then does not know it for what it is
        let ct = String::from_utf8_lossy(&parts["[Content_Types].xml"]).into_owned();
        let missing: Vec<String> = parts
            .keys()
            .filter_map(|name| content_type_for(name).filter(|_| !ct.contains(&format!("PartName=\"/{name}\""))).map(|t| format!("<Override PartName=\"/{name}\" ContentType=\"{t}\"/>")))
            .collect();
        if !missing.is_empty() && ct.contains("</Types>") {
            parts.insert("[Content_Types].xml".into(), ct.replacen("</Types>", &format!("{}</Types>", missing.concat()), 1).into_bytes());
            fixes.push(format!("{} content types added for parts that had none", missing.len()));
        }
    }
    // a part with no content type makes the package one Office repairs: such parts are another
    // file's (cross-linked clusters hold their headers), not this document's
    let ct = String::from_utf8_lossy(&parts["[Content_Types].xml"]).to_lowercase();
    let untyped: Vec<String> = parts
        .keys()
        .filter(|n| *n != "[Content_Types].xml")
        .filter(|n| {
            let l = n.to_lowercase();
            let ext = l.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('/'));
            !ct.contains(&format!("partname=\"/{l}\"")) && ext.map_or(true, |e| !ct.contains(&format!("extension=\"{e}\"")))
        })
        .cloned()
        .collect();
    if !untyped.is_empty() {
        for n in &untyped {
            parts.remove(n);
        }
        let shown: Vec<&str> = untyped.iter().take(3).map(|s| s.as_str()).collect();
        fixes.push(format!("{} parts with no content type left out, not this document's ({}{})", untyped.len(), shown.join(", "), if untyped.len() > 3 { ", ..." } else { "" }));
    }
    // _rels/.rels
    let main = main_part(kind);
    if !parts.get("_rels/.rels").map_or(false, |b| well_formed(b)) && !main.is_empty() && parts.contains_key(main) {
        let x = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"{main}\"/></Relationships>");
        parts.insert("_rels/.rels".into(), x.into_bytes());
        fixes.push("_rels/.rels rebuilt".to_string());
    }
    // relationships to lost parts
    let present: std::collections::HashSet<String> = parts.keys().cloned().collect();
    let rels: Vec<String> = parts.keys().filter(|n| n.ends_with(".rels")).cloned().collect();
    let mut kept_ids: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();
    for rn in rels {
        let x = String::from_utf8_lossy(&parts[&rn]).into_owned();
        let (y, n) = filter_elements(&x, "Relationship", |e| {
            if attr(e, "TargetMode").map_or(false, |m| m == "External") {
                return true;
            }
            attr(e, "Target").map_or(false, |t| present.contains(&resolve_target(&rn, &t)))
        });
        let ids: std::collections::HashSet<String> = {
            let mut v = std::collections::HashSet::new();
            filter_elements(&y, "Relationship", |e| {
                if let Some(id) = attr(e, "Id") {
                    v.insert(id);
                }
                true
            });
            v
        };
        kept_ids.insert(rn.clone(), ids);
        if n > 0 {
            parts.insert(rn.clone(), y.into_bytes());
            fixes.push(format!("{rn}: {n} relationships to lost parts removed"));
        }
    }
    // the workbook's sheets and the presentation's slides that are gone
    for (owner, rels_name, tag) in [("xl/workbook.xml", "xl/_rels/workbook.xml.rels", "sheet"), ("ppt/presentation.xml", "ppt/_rels/presentation.xml.rels", "p:sldId")] {
        let (Some(doc), Some(ids)) = (parts.get(owner), kept_ids.get(rels_name)) else { continue };
        let x = String::from_utf8_lossy(doc).into_owned();
        let (y, n) = filter_elements(&x, tag, |e| attr(e, "r:id").map_or(true, |id| ids.contains(&id)));
        if n > 0 {
            parts.insert(owner.to_string(), y.into_bytes());
            fixes.push(format!("{owner}: {n} references to lost {} removed", if tag == "sheet" { "sheets" } else { "slides" }));
        }
    }
    fixes
}

/// The extension says it is an Office document
pub fn is_office_name(path: &str) -> bool {
    let l = path.to_lowercase();
    [".docx", ".xlsx", ".pptx", ".docm", ".xlsm", ".pptm"].iter().any(|s| l.ends_with(s))
}

/// It starts like a ZIP, or holds a central directory somewhere
/// A ZIP: it starts with a ZIP signature, or the end of its directory is there, or a local
/// header's data ends where the next local header or the directory begins (looked for in the
/// first 256 MB): a ZIP with something before it, a self-extractor's stub or another file's
/// bytes, cut short so that its directory is gone. Such a one was rescued as a lone Deflate
/// stream, no file back (make_realworld.py's py_prepended.zip, 1 MB before it, cut: 0 of 428).
pub fn looks_like_zip(data: &[u8]) -> bool {
    if (data.len() >= 4 && data[0] == b'P' && data[1] == b'K' && (data[2] == 3 || data[2] == 5 || data[2] == 7))
        || zip::find_eocd(data).is_some()
    {
        return true;
    }
    let end = data.len().min(256 << 20);
    let mut i = 0;
    while let Some(p) = data[i..end].windows(4).position(|w| w == b"PK\x03\x04") {
        let at = i + p;
        if let Some(l) = parse_local(data, at) {
            let next = l.data_offset + l.csize as usize;
            if l.flags & 8 == 0 && l.csize > 0 && next + 4 <= data.len() && matches!(u32le(data, next), 0x0403_4b50 | 0x0201_4b50) {
                return true;
            }
        }
        i = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(x: &str) -> String {
        String::from_utf8(repair_xml(x.as_bytes())).unwrap()
    }

    #[test]
    fn well_formed_xml_comes_back_unchanged() {
        let x = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<w:document xmlns:w=\"u\"><w:body><w:p a=\"1\"/>t &amp; u</w:body></w:document>\n";
        assert_eq!(fix(x), x);
    }

    #[test]
    fn declarations_and_end_tags_from_garbage_are_dropped() {
        let x = "<?xml version=\"1.0\"?><doc><p>one</p></doc>garbage<?xml version=\"1.0\"?><p>two</p></doc>";
        assert_eq!(fix(x), "<?xml version=\"1.0\"?><doc><p>one</p>garbage<p>two</p></doc>");
    }

    #[test]
    fn duplicate_attributes_and_text_outside_the_root_are_dropped() {
        assert_eq!(fix("junk<a><b r=\"1\" r=\"2\">x</b><c/></a>"), "<a>x<c/></a>");
    }

    #[test]
    fn parts_without_a_content_type_get_one() {
        let mut parts: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let ct = "<Types xmlns=\"t\"><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"rels\" ContentType=\"r\"/><Default Extension=\"PNG\" ContentType=\"image/png\"/>\
<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/></Types>";
        parts.insert("[Content_Types].xml".into(), ct.as_bytes().to_vec());
        for n in ["_rels/.rels", "ppt/presentation.xml", "ppt/slideMasters/slideMaster1.xml", "ppt/slideMasters/_rels/slideMaster1.xml.rels", "ppt/media/image1.png",
                  "idlelib/pyparse.py", "idlelib/README"] {
            parts.insert(n.into(), b"<x/>".to_vec());
        }
        let fixes = make_openable(&mut parts, "pptx");
        let x = String::from_utf8(parts["[Content_Types].xml"].clone()).unwrap();
        assert!(x.contains("<Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/></Types>"), "{x}");
        assert_eq!(x.matches("PartName=\"/ppt/presentation.xml\"").count(), 1);
        assert!(fixes.iter().any(|f| f == "1 content types added for parts that had none"), "{fixes:?}");
        // parts of another file, with no content type, are left out; the image (by extension) stays
        assert!(!parts.contains_key("idlelib/pyparse.py") && !parts.contains_key("idlelib/README") && parts.contains_key("ppt/media/image1.png"));
        assert!(fixes.iter().any(|f| f.starts_with("2 parts with no content type left out")), "{fixes:?}");
    }

    #[test]
    fn no_document_without_its_main_part() {
        let dir = std::env::temp_dir().join(format!("gpuzip-office-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = |name: &str, status: Status, xml: &str| Recovered {
            name: name.into(), status, detail: String::new(), size: xml.len() as u64, from_scan: false, office_part: Some(xml.as_bytes().to_vec()),
        };
        // a Word document whose document.xml was lost: its styles alone are no document
        let mut entries = vec![entry("word/styles.xml", Status::Intact, "<w:styles xmlns:w=\"u\"/>"), entry("word/document.xml", Status::Lost, "")];
        let of = office_repair(&mut entries, Some(&dir), 0);
        assert!(of.rebuilt.is_none());
        assert!(of.package_fixes.iter().any(|f| f.starts_with("word/document.xml was lost")), "{:?}", of.package_fixes);
        // with it, there is one
        let mut entries = vec![entry("word/document.xml", Status::Intact, "<w:document xmlns:w=\"u\"><w:body/></w:document>")];
        let of = office_repair(&mut entries, Some(&dir), 0);
        assert!(of.rebuilt.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_start_where_the_stream_begins_is_tried_first() {
        // a destroyed local header of 40 bytes, a 28-byte extra field, the data, the directory
        let mut x = 1u32;
        let text: Vec<u8> = (0..600_000)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                b"abcdefgh ij\n"[(x >> 16) as usize % 12]
            })
            .collect();
        let mut comp = vec![0u8; text.len() + 1024];
        let n = libdeflater::Compressor::new(libdeflater::CompressionLvl::default()).deflate_compress(&text, &mut comp).unwrap();
        let mut sound = vec![0u8; 40];
        sound.extend((0..28u8).map(|i| i.wrapping_mul(37) + 1));
        sound.extend_from_slice(&comp[..n]);
        sound.extend_from_slice(b"PK\x01\x02");
        let t = Task {
            name: "0123456789".into(), raw_name: b"0123456789".to_vec(), data_offset: None, header_offset: 0, method: DEFLATED, flags: 0, crc: Some(crc32fast::hash(&text)),
            csize: Some(n as u64), size: Some(text.len() as u64), dos_time: 0, dos_date: 0, from_scan: false, cd_extra_len: 0, unix_mode: 0,
        };
        let order = |data: &[u8]| {
            let mut starts = vec![40, 49, 64, 68, 60];
            let enough = order_starts(data, &t, &mut starts, |s| lost_bytes(data, &t, s, data.len()) == 255);
            (starts, enough)
        };
        // the stream begins at 68: it alone is rescued, unless it gives back nothing
        assert_eq!(order(&sound), (vec![68, 40, 49, 64, 60], Status::Unverified));
        // zeros over the extra field and the stream's first 3,000 bytes: the stream begins nowhere,
        // 68 is where its data ends at the directory, and no start can give back more than a part
        let mut hole = sound.clone();
        hole[40..68 + 3000].fill(0);
        assert_eq!(order(&hole), (vec![68, 40, 49, 64, 60], Status::Partial));
        hole[40..68 + 3000].fill(0xff);
        assert_eq!(order(&hole), (vec![68, 40, 49, 64, 60], Status::Partial));
        // other data there: 68 is tried first, every start until one is intact
        let mut other = sound.clone();
        let mut y = 7u32;
        other[40..68 + 3000].iter_mut().for_each(|b| {
            y = y.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *b = (y >> 16) as u8;
        });
        assert_eq!(order(&other), (vec![68, 40, 49, 64, 60], Status::Intact));
        // a small entry: the whole stream, as long as the entry
        let small = Task { size: Some(1000), ..t };
        let n = libdeflater::Compressor::new(libdeflater::CompressionLvl::default()).deflate_compress(&text[..1000], &mut comp).unwrap();
        let data = [&sound[..68], &comp[..n]].concat();
        assert!(stream_begins(&data, &small, 68) && !stream_begins(&data, &small, 40));
        assert!(!stream_begins(&data, &Task { size: Some(999), ..small }, 68));
    }
}
