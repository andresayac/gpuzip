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

use crate::rescue::{EXACT, Rescue};
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
}

#[derive(Default)]
pub struct OfficeOutcome {
    pub kind: String, // docx, xlsx, pptx, or "office"
    pub repaired_parts: Vec<String>,
    pub text_file: Option<PathBuf>,
    pub text_chars: usize,
    pub rebuilt: Option<PathBuf>,
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
    let name = if flags & 0x800 != 0 || raw.is_ascii() {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        raw.iter().map(|&b| if b < 0x80 { b as char } else { '\u{fffd}' }).collect()
    };
    let mut csize = u32le(data, h + 18) as u64;
    let mut size = u32le(data, h + 22) as u64;
    // Zip64 in the local header's extra field
    let extra = &data[h + 30 + nlen..h + 30 + nlen + xlen];
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
        name: name.replace('\\', "/"),
        dos_time: u16le(data, h + 10),
        dos_date: u16le(data, h + 12),
    })
}

/// Every plausible local file header in the file. A ZIP stored inside an entry (a wheel, a
/// jar) shows its own headers in the data; one that lies inside an entry whose size is known
/// is skipped, as are the entries nested in it.
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
                if known_size {
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

fn write_file(root: Option<&Path>, name: &str, bytes: &[u8], dos_time: u16, dos_date: u16) -> Result<(), String> {
    let Some(root) = root else { return Ok(()) };
    let rel = zip::safe_relpath(name).ok_or("path leaves the output folder; skipped")?;
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = File::create(&path).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    if let Some(t) = zip::dos_time(dos_date, dos_time) {
        let _ = f.set_modified(t);
    }
    Ok(())
}

struct Task {
    name: String,
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
    let mut covered: Vec<usize> = Vec::new(); // local header offsets the directory names
    match zip::read_central_directory(data) {
        Ok(cd) => {
            o.cd_ok = true;
            for e in &cd {
                let h = e.data_offset as usize; // the local header, as the directory says
                let local = by_offset.get(&h);
                covered.push(h);
                tasks.push(Task {
                    name: e.name.clone(),
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
                });
            }
        }
        Err(e) => {
            o.cd_error = e;
            o.notes.push(format!("the central directory cannot be read ({}): the entries come from the local headers found by scanning", o.cd_error));
        }
    }
    // entries only the scan knows
    for l in &locals {
        if covered.contains(&l.offset) {
            continue;
        }
        let bit3 = l.flags & 8 != 0;
        tasks.push(Task {
            name: l.name.clone(),
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
        });
    }
    tasks.sort_by_key(|t| t.header_offset);
    let office = tasks.iter().any(|t| t.name == "[Content_Types].xml");

    for t in tasks {
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
        if t.method != STORED && t.method != DEFLATED {
            rec.status = Status::Unsupported;
            rec.detail = format!("method {} is not read here", zip::method_name(t.method));
            o.entries.push(rec);
            continue;
        }
        // where the data starts: from the local header, or guessed when that header is destroyed
        let mut starts: Vec<usize> = Vec::new();
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
            }
        }
        let mut best: Option<(Status, Vec<u8>, String)> = None;
        for start in starts {
            let limit = next_header_after(start);
            let (status, bytes, detail) = recover_entry(data, lib, &t, start, limit, fill);
            let better = match &best {
                None => true,
                Some((s, _, _)) => (status as u8) < (*s as u8),
            };
            if better {
                best = Some((status, bytes, detail));
            }
            if status == Status::Intact {
                break;
            }
        }
        if let Some((status, bytes, detail)) = best {
            rec.status = status;
            rec.size = bytes.len() as u64;
            if !detail.is_empty() {
                rec.detail = if rec.detail.is_empty() { detail } else { format!("{}; {detail}", rec.detail) };
            }
            if status != Status::Lost {
                if let Err(e) = write_file(root, &t.name, &bytes, t.dos_time, t.dos_date) {
                    o.failed.push((t.name.clone(), e));
                }
                if office && keep_parts {
                    rec.office_part = Some(bytes);
                }
            }
        }
        o.entries.push(rec);
    }
    if office {
        o.office = Some(office_repair(&mut o.entries, root, fill));
    }
    o
}

/// One entry's data from `start`, bounded by `limit`: (status, bytes, detail)
fn recover_entry(data: &[u8], lib: &Rescue, t: &Task, start: usize, limit: usize, fill: u8) -> (Status, Vec<u8>, String) {
    if start >= data.len() {
        return (Status::Lost, Vec::new(), "data lies beyond the end of the file".into());
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
            return (Status::Partial, bytes, format!("cut short: {n} of {} bytes", size.unwrap_or(0)));
        }
        match crc {
            Some(c) if crc32fast::hash(&bytes) == c => (Status::Intact, bytes, String::new()),
            Some(_) => (Status::Unverified, bytes, "stored data, CRC mismatch: damaged somewhere inside".into()),
            None => (Status::Unverified, bytes, "no CRC known".into()),
        }
    } else {
        let end = match t.csize {
            Some(c) if c > 0 => (start as u64 + c).min(data.len() as u64) as usize,
            _ => limit,
        };
        let slice = &data[start..end];
        let mut out = Vec::with_capacity(t.size.unwrap_or(slice.len() as u64 * 3) as usize);
        let mut unknown = 0u64;
        let report = lib.rescue_raw(slice, (t.size, t.crc), fill, |d, m| {
            out.extend_from_slice(d);
            unknown += m.iter().filter(|&&x| x != EXACT).count() as u64;
            true
        });
        let report = match report {
            Ok(r) => r,
            Err(e) => return (Status::Lost, Vec::new(), e),
        };
        let s = &report.stats;
        let cut = t.csize.map_or(false, |c| ((end - start) as u64) < c) || s.truncated != 0;
        if s.damaged > 0 || unknown > 0 || cut {
            let mut d = Vec::new();
            if s.damaged > 0 {
                d.push(format!("{} damaged block{}", s.damaged, if s.damaged == 1 { "" } else { "s" }));
            }
            if unknown > 0 {
                d.push(format!("{} bytes unknown", unknown));
            }
            if cut {
                d.push("cut short".into());
            }
            return (Status::Partial, out, d.join(", "));
        }
        // in step throughout: the CRC decides
        let crc = t.crc.or_else(|| {
            // bit 3: the descriptor follows the stream
            data_descriptor(data, start + s.input_end as usize).map(|(c, _, _)| c)
        });
        match crc {
            Some(c) if crc32fast::hash(&out) == c && t.size.map_or(true, |z| z == out.len() as u64) => (Status::Intact, out, String::new()),
            Some(_) => (Status::Unverified, out, "decodes in step but the CRC does not match: damage the decoder could not see".into()),
            None => (Status::Unverified, out, "no CRC known".into()),
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
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        if bytes[i] != b'<' {
            // text: keep what XML allows, up to the next tag; an '&' must begin a whole entity
            let j = text[i..].find('<').map(|k| i + k).unwrap_or(n);
            let seg = &text[i..j];
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
            if tag.ends_with("?>") || tag.starts_with("<!DOCTYPE") {
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
            // close down to the matching element; an unmatched end tag is dropped
            if let Some(pos) = stack.iter().rposition(|s| s == name) {
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
    loop {
        r = r.trim_start();
        if r.is_empty() {
            break;
        }
        let an: String = r.chars().take_while(|&c| is_name_char(c)).collect();
        if an.is_empty() {
            return None;
        }
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
                if let Err(err) = write_file(root, &e.name, part, 0, 0) {
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
        let mut zipped = Vec::new();
        let mut central = Vec::new();
        let mut count = 0u16;
        for e in entries.iter() {
            let Some(part) = &e.office_part else { continue };
            if e.status == Status::Lost || e.status == Status::Unsupported {
                continue;
            }
            let name = e.name.as_bytes();
            let crc = crc32fast::hash(part);
            let offset = zipped.len() as u32;
            let mut local = Vec::new();
            local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            local.extend_from_slice(&[20, 0, 0, 8, 0, 0, 0, 0, 0, 0]); // version 2.0, UTF-8 names, stored, no time
            local.extend_from_slice(&crc.to_le_bytes());
            local.extend_from_slice(&(part.len() as u32).to_le_bytes());
            local.extend_from_slice(&(part.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name);
            zipped.extend_from_slice(&local);
            zipped.extend_from_slice(part);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 8, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(part.len() as u32).to_le_bytes());
            central.extend_from_slice(&(part.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0u8; 12]); // extra, comment, disk, attributes
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
            count += 1;
        }
        if count > 0 {
            let cd_offset = zipped.len() as u32;
            zipped.extend_from_slice(&central);
            zipped.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
            zipped.extend_from_slice(&[0, 0, 0, 0]);
            zipped.extend_from_slice(&count.to_le_bytes());
            zipped.extend_from_slice(&count.to_le_bytes());
            zipped.extend_from_slice(&(central.len() as u32).to_le_bytes());
            zipped.extend_from_slice(&cd_offset.to_le_bytes());
            zipped.extend_from_slice(&0u16.to_le_bytes());
            let p = r.join(format!("recovered.{}", of.kind));
            if std::fs::write(&p, &zipped).is_ok() {
                of.rebuilt = Some(p);
            }
        }
    }
    of
}

/// The extension says it is an Office document
pub fn is_office_name(path: &str) -> bool {
    let l = path.to_lowercase();
    [".docx", ".xlsx", ".pptx", ".docm", ".xlsm", ".pptm"].iter().any(|s| l.ends_with(s))
}

/// It starts like a ZIP, or holds a central directory somewhere
pub fn looks_like_zip(data: &[u8]) -> bool {
    (data.len() >= 4 && data[0] == b'P' && data[1] == b'K' && (data[2] == 3 || data[2] == 5 || data[2] == 7))
        || zip::find_eocd(data).is_some()
}
