//! Spreadsheet parts made valid again after damage. A damaged worksheet, once its XML is well
//! formed, can still hold what no spreadsheet does: a cell at "1A168", a number "172.t>", a
//! type "=", a shared string that does not exist. Excel refuses such a file or asks to repair
//! it, and libraries fail outright. The sheet is rebuilt from the cells that are valid, in
//! order; the rest are dropped and counted. A workbook whose main part was lost (a file cut
//! short loses it: Excel and openpyxl write it near the end) gets one listing the sheets found.

use std::collections::BTreeMap;

enum Ev<'a> {
    Start { name: &'a str, tag: &'a str, empty: bool },
    End(&'a str),
    Text(&'a str),
}

/// The tags and text of well-formed XML (comments, declarations and CDATA left out)
fn events(x: &str) -> Vec<Ev<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < x.len() {
        let Some(k) = x[i..].find('<') else {
            out.push(Ev::Text(&x[i..]));
            break;
        };
        if k > 0 {
            out.push(Ev::Text(&x[i..i + k]));
        }
        let start = i + k;
        let rest = &x[start..];
        let skip = if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|e| e + 3)
        } else {
            None
        };
        if let Some(e) = skip {
            i = start + e;
            continue;
        }
        // the end of the tag, quotes honoured
        let b = rest.as_bytes();
        let mut j = 1;
        let mut quote = 0u8;
        while j < b.len() {
            let c = b[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        let tag = &rest[..(j + 1).min(rest.len())];
        i = start + tag.len();
        if tag.starts_with("<?") || tag.starts_with("<!") {
            continue;
        }
        if let Some(n) = tag.strip_prefix("</") {
            out.push(Ev::End(n.trim_end_matches('>').trim()));
            continue;
        }
        let empty = tag.ends_with("/>");
        let body = &tag[1..tag.len() - if empty { 2 } else { 1 }];
        let name = body.split(|c: char| c.is_whitespace()).next().unwrap_or("");
        out.push(Ev::Start { name, tag, empty });
    }
    out
}

/// The value of an attribute in a start tag
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    loop {
        let k = rest.find(name)?;
        let before = rest[..k].chars().last();
        let after = rest[k + name.len()..].trim_start();
        if before.map_or(false, |c| c.is_whitespace()) && after.starts_with('=') {
            let v = after[1..].trim_start();
            let q = v.chars().next()?;
            if q != '"' && q != '\'' {
                return None;
            }
            let end = v[1..].find(q)?;
            return Some(&v[1..1 + end]);
        }
        rest = &rest[k + name.len()..];
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// "AB12" as (column, row), both from 1; within Excel's limits (XFD, 1048576)
fn cell_ref(r: &str) -> Option<(u32, u32)> {
    let letters = r.chars().take_while(|c| c.is_ascii_uppercase()).count();
    if letters == 0 || letters > 3 || letters == r.len() {
        return None;
    }
    let digits = &r[letters..];
    if !digits.chars().all(|c| c.is_ascii_digit()) || digits.starts_with('0') || digits.len() > 7 {
        return None;
    }
    let col = r[..letters].bytes().fold(0u32, |a, c| a * 26 + (c - b'A' + 1) as u32);
    let row: u32 = digits.parse().ok()?;
    (col <= 16384 && row >= 1 && row <= 1_048_576).then_some((col, row))
}

fn is_number(v: &str) -> bool {
    let v = v.trim();
    let b = v.strip_prefix('-').unwrap_or(v);
    let (mant, exp) = match b.find(['e', 'E']) {
        Some(k) => (&b[..k], Some(&b[k + 1..])),
        None => (b, None),
    };
    let mut parts = mant.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next().unwrap_or("");
    let digits_ok = int.chars().all(|c| c.is_ascii_digit()) && frac.chars().all(|c| c.is_ascii_digit()) && !(int.is_empty() && frac.is_empty());
    let exp_ok = exp.map_or(true, |e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && e.len() <= 3 && e.chars().all(|c| c.is_ascii_digit())
    });
    digits_ok && exp_ok && v.parse::<f64>().map_or(false, |f| f.is_finite())
}

const ERRORS: [&str; 8] = ["#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A", "#GETTING_DATA"];

/// A formula worth keeping: printable, with its quotes and brackets in balance
fn formula_ok(f: &str) -> bool {
    let mut depth = 0i32;
    let mut quote = false;
    for c in f.chars() {
        if c.is_control() {
            return false;
        }
        match c {
            '"' => quote = !quote,
            '(' if !quote => depth += 1,
            ')' if !quote => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !quote && !f.trim().is_empty()
}

/// What a sheet's cells may refer to: how many shared strings and cell formats there are
/// (None: the part is missing, so nothing may refer to it)
pub struct Limits {
    pub strings: Option<usize>,
    pub formats: Option<usize>,
}

/// How many shared strings and cell formats the recovered parts hold
pub fn limits(parts: &BTreeMap<String, Vec<u8>>) -> Limits {
    let strings = parts.get("xl/sharedStrings.xml").map(|b| {
        events(&String::from_utf8_lossy(b)).iter().filter(|e| matches!(e, Ev::Start { name: "si", .. })).count()
    });
    let formats = parts.get("xl/styles.xml").and_then(|b| {
        let x = String::from_utf8_lossy(b);
        let mut inside = false;
        let mut n = None;
        for e in events(&x) {
            match e {
                Ev::Start { name: "cellXfs", empty, .. } => {
                    inside = !empty;
                    n = Some(0);
                }
                Ev::End("cellXfs") => inside = false,
                Ev::Start { name: "xf", .. } if inside => *n.as_mut().unwrap() += 1,
                _ => {}
            }
        }
        n
    });
    Limits { strings, formats }
}

/// A worksheet rebuilt from its valid rows and cells, and what that changed
pub struct Sanitized {
    pub xml: Vec<u8>,
    pub dropped: usize,  // cells that could not stand
    pub restyled: usize, // cells whose format does not exist, given the default one
}

/// A worksheet rebuilt from its valid rows and cells; None when it has no worksheet element at
/// all. Only the cells are kept, not the layout around them (column widths, merged ranges),
/// whose damage would be as hard to see.
pub fn sanitize_sheet(xml: &[u8], lim: &Limits) -> Option<Sanitized> {
    let x = String::from_utf8_lossy(xml);
    let ev = events(&x);
    let root = ev.iter().find_map(|e| match e {
        Ev::Start { name, tag, .. } if *name == "worksheet" || name.ends_with(":worksheet") => Some(tag.trim_end_matches("/>").trim_end_matches('>')),
        _ => None,
    })?;
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n");
    out.push_str(root);
    out.push_str("><sheetData>");
    let mut dropped = 0usize;
    let mut restyled = 0usize;
    let mut i = 0;
    // to the sheet data
    while i < ev.len() && !matches!(ev[i], Ev::Start { name: "sheetData", .. }) {
        i += 1;
    }
    let mut last_row = 0u32;
    if i < ev.len() && !matches!(ev[i], Ev::Start { empty: true, .. }) {
        i += 1;
        while i < ev.len() {
            match &ev[i] {
                Ev::End("sheetData") => break,
                Ev::Start { name: "row", tag, empty } => {
                    let r = attr(tag, "r").and_then(|v| v.parse::<u32>().ok());
                    let row = match r {
                        Some(r) if r > last_row && r <= 1_048_576 => r,
                        None if attr(tag, "r").is_none() && last_row < 1_048_576 => last_row + 1,
                        _ => 0,
                    };
                    let mut cells = String::new();
                    let mut last_col = 0u32;
                    i += 1;
                    if !*empty {
                        while i < ev.len() {
                            match &ev[i] {
                                Ev::End("row") => break,
                                Ev::Start { name: "c", tag, empty } => {
                                    let (cell, next) = read_cell(&ev, i, tag, *empty);
                                    i = next;
                                    match cell.and_then(|c| render_cell(&c, row, &mut last_col, lim, &mut restyled)) {
                                        Some(s) => cells.push_str(&s),
                                        None => dropped += 1,
                                    }
                                    continue;
                                }
                                Ev::Start { empty: false, .. } => i = skip(&ev, i),
                                _ => {}
                            }
                            i += 1;
                        }
                    }
                    if row != 0 {
                        last_row = row;
                        if !cells.is_empty() {
                            out.push_str(&format!("<row r=\"{row}\">{cells}</row>"));
                        }
                    }
                }
                Ev::Start { empty: false, .. } => i = skip(&ev, i),
                _ => {}
            }
            i += 1;
        }
    }
    out.push_str("</sheetData></");
    out.push_str(root[1..].split(|c: char| c.is_whitespace()).next().unwrap_or("worksheet"));
    out.push('>');
    Some(Sanitized { xml: out.into_bytes(), dropped, restyled })
}

/// Past the element that starts at ev[i]: the index of its end tag
fn skip(ev: &[Ev], mut i: usize) -> usize {
    let mut depth = 0i32;
    while i < ev.len() {
        match &ev[i] {
            Ev::Start { empty: false, .. } => depth += 1,
            Ev::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

struct Cell {
    r: Option<String>,
    t: Option<String>,
    s: Option<String>,
    v: Option<String>,
    f: Option<(String, bool)>, // the formula, and whether it had attributes (shared, array)
    is: Option<String>,        // an inline string's text, escaped
}

/// The cell element at ev[i]: what it holds, and the index after it
fn read_cell(ev: &[Ev], i: usize, tag: &str, empty: bool) -> (Option<Cell>, usize) {
    let mut c = Cell { r: attr(tag, "r").map(String::from), t: attr(tag, "t").map(String::from), s: attr(tag, "s").map(String::from), v: None, f: None, is: None };
    if empty {
        return (Some(c), i + 1);
    }
    let mut j = i + 1;
    let mut ok = true;
    while j < ev.len() {
        match &ev[j] {
            Ev::End("c") => return (ok.then_some(c), j + 1),
            Ev::Start { name: "v", empty: false, .. } => {
                let mut text = String::new();
                j += 1;
                while j < ev.len() {
                    match &ev[j] {
                        Ev::Text(t) => text.push_str(t),
                        Ev::End("v") => break,
                        _ => ok = false,
                    }
                    j += 1;
                }
                c.v = Some(text);
            }
            Ev::Start { name: "f", tag, empty } => {
                let has_attrs = tag.trim_start_matches("<f").trim_end_matches("/>").trim_end_matches('>').trim().len() > 0;
                let mut text = String::new();
                if !*empty {
                    j += 1;
                    while j < ev.len() {
                        match &ev[j] {
                            Ev::Text(t) => text.push_str(t),
                            Ev::End("f") => break,
                            _ => ok = false,
                        }
                        j += 1;
                    }
                }
                c.f = Some((text, has_attrs));
            }
            Ev::Start { name: "is", empty: false, .. } => {
                let end = skip(ev, j);
                let mut text = String::new();
                let mut in_t = false;
                for e in &ev[j..end] {
                    match e {
                        Ev::Start { name: "t", empty: false, .. } => in_t = true,
                        Ev::End("t") => in_t = false,
                        Ev::Text(t) if in_t => text.push_str(t),
                        _ => {}
                    }
                }
                c.is = Some(text);
                j = end;
            }
            Ev::Start { empty: false, .. } => {
                ok = false;
                j = skip(ev, j);
            }
            Ev::Start { .. } => ok = false,
            Ev::Text(t) if !t.trim().is_empty() => ok = false,
            _ => {}
        }
        j += 1;
    }
    (None, j)
}

/// The cell as it may stand in row `row`, after the column `last_col`; None when it cannot
fn render_cell(c: &Cell, row: u32, last_col: &mut u32, lim: &Limits, restyled: &mut usize) -> Option<String> {
    if row == 0 {
        return None;
    }
    let r = c.r.as_deref()?;
    let (col, cr) = cell_ref(r)?;
    if cr != row || col <= *last_col {
        return None;
    }
    let t = c.t.as_deref().unwrap_or("n");
    // an empty value is a formula's result not yet computed: no value
    let v = c.v.as_deref().filter(|v| !v.is_empty()).map(unescape);
    let value_ok = match (t, v.as_deref()) {
        (_, None) => t != "inlineStr" || c.is.is_some(),
        ("n", Some(v)) => is_number(v),
        ("s", Some(v)) => v.trim().parse::<usize>().map_or(false, |k| lim.strings.map_or(false, |n| k < n)),
        ("b", Some(v)) => v == "0" || v == "1",
        ("e", Some(v)) => ERRORS.contains(&v),
        ("str", Some(_)) => true,
        ("d", Some(v)) => !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || "-:T.Z+".contains(c)),
        ("inlineStr", Some(_)) => c.is.is_some(),
        _ => false,
    };
    if !value_ok {
        return None;
    }
    *last_col = col;
    let mut s = format!("<c r=\"{r}\"");
    if let Some(st) = c.s.as_deref() {
        match st.parse::<usize>() {
            Ok(k) if lim.formats.map_or(false, |n| k < n) => s.push_str(&format!(" s=\"{k}\"")),
            _ => *restyled += 1,
        }
    }
    if t != "n" {
        s.push_str(&format!(" t=\"{t}\""));
    }
    s.push('>');
    if let Some((f, has_attrs)) = &c.f {
        // a shared or array formula names cells elsewhere that may be gone: kept only plain
        if !has_attrs && formula_ok(&unescape(f)) {
            s.push_str(&format!("<f>{f}</f>"));
        }
    }
    if t == "inlineStr" {
        s.push_str(&format!("<is><t>{}</t></is>", c.is.as_deref().unwrap_or("")));
    } else if let Some(v) = c.v.as_deref().filter(|v| !v.is_empty()) {
        s.push_str(&format!("<v>{v}</v>"));
    }
    s.push_str("</c>");
    Some(s)
}

/// A workbook for sheets whose own was lost: one entry per worksheet present, named by its
/// number, with the relationships it needs. Returns the sheet count.
pub fn synthesize_workbook(parts: &mut BTreeMap<String, Vec<u8>>) -> usize {
    let mut sheets: Vec<(u32, String)> = parts
        .keys()
        .filter_map(|n| {
            let k = n.strip_prefix("xl/worksheets/sheet")?.strip_suffix(".xml")?.parse::<u32>().ok()?;
            Some((k, n.clone()))
        })
        .collect();
    sheets.sort();
    if sheets.is_empty() {
        return 0;
    }
    let mut wb = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>",
    );
    let mut rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    let rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    for (i, (k, name)) in sheets.iter().enumerate() {
        let id = i + 1;
        wb.push_str(&format!("<sheet name=\"Sheet{k}\" sheetId=\"{id}\" r:id=\"rId{id}\"/>"));
        let target = name.strip_prefix("xl/").unwrap_or(name);
        rels.push_str(&format!("<Relationship Id=\"rId{id}\" Type=\"{rel}/worksheet\" Target=\"{target}\"/>"));
    }
    wb.push_str("</sheets></workbook>");
    let mut id = sheets.len();
    for (part, kind) in [("xl/styles.xml", "styles"), ("xl/sharedStrings.xml", "sharedStrings"), ("xl/theme/theme1.xml", "theme")] {
        if parts.contains_key(part) {
            id += 1;
            let target = part.strip_prefix("xl/").unwrap();
            rels.push_str(&format!("<Relationship Id=\"rId{id}\" Type=\"{rel}/{kind}\" Target=\"{target}\"/>"));
        }
    }
    rels.push_str("</Relationships>");
    parts.insert("xl/workbook.xml".into(), wb.into_bytes());
    parts.insert("xl/_rels/workbook.xml.rels".into(), rels.into_bytes());
    sheets.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> Limits {
        Limits { strings: Some(3), formats: Some(2) }
    }

    #[test]
    fn keeps_valid_cells_and_drops_impossible_ones() {
        let x = br#"<?xml version="1.0"?><worksheet xmlns="m"><dimension ref="A1:C3"/><sheetData>
<row r="1"><c r="A1" t="s"><v>2</v></c><c r="B1"><v>3.5</v></c><c r="C1"><v>172.t&gt;</v></c></row>
<row r="2"><c r="1A168"><v>1</v></c><c r="B2" t="="><v>1</v></c><c r="C2" t="s"><v>9</v></c><c r="D2" s="7"><v>-1e3</v></c></row>
<row r="2"><c r="A2"><v>5</v></c></row>
<row r="4"><c r="B4"><f>SUM(B1:B2)</f><v>4</v></c><c r="A4"><v>1</v></c><c r="C4"><f>SUM(B1</f><v>4</v></c><c r="D4"><f>B4*2</f><v></v></c></row>
</sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#;
        let r = sanitize_sheet(x, &lim()).unwrap();
        assert_eq!(r.restyled, 1);
        let (out, dropped) = (String::from_utf8(r.xml).unwrap(), r.dropped);
        assert!(out.contains(r#"<row r="1"><c r="A1" t="s"><v>2</v></c><c r="B1"><v>3.5</v></c></row>"#), "{out}");
        assert!(out.contains(r#"<row r="2"><c r="D2"><v>-1e3</v></c></row>"#), "{out}");
        assert!(out.contains(r#"<row r="4"><c r="B4"><f>SUM(B1:B2)</f><v>4</v></c><c r="C4"><v>4</v></c><c r="D4"><f>B4*2</f></c></row>"#), "{out}");
        assert!(!out.contains("mergeCell"));
        // C1, 1A168, B2, C2, the second row 2's A2, A4 out of order
        assert_eq!(dropped, 6, "{out}");
    }

    #[test]
    fn workbook_for_lost_main_part() {
        let mut parts = BTreeMap::new();
        parts.insert("xl/worksheets/sheet2.xml".to_string(), b"<worksheet/>".to_vec());
        parts.insert("xl/worksheets/sheet10.xml".to_string(), b"<worksheet/>".to_vec());
        parts.insert("xl/styles.xml".to_string(), b"<styleSheet/>".to_vec());
        assert_eq!(synthesize_workbook(&mut parts), 2);
        let wb = String::from_utf8(parts["xl/workbook.xml"].clone()).unwrap();
        assert!(wb.find("Sheet2").unwrap() < wb.find("Sheet10").unwrap());
        let rels = String::from_utf8(parts["xl/_rels/workbook.xml.rels"].clone()).unwrap();
        assert!(rels.contains(r#"Target="worksheets/sheet10.xml""#) && rels.contains(r#"Target="styles.xml""#));
    }

    #[test]
    fn counts_strings_and_formats() {
        let mut parts = BTreeMap::new();
        parts.insert("xl/sharedStrings.xml".to_string(), b"<sst><si><t>a</t></si><si><t>b</t></si></sst>".to_vec());
        parts.insert("xl/styles.xml".to_string(), b"<styleSheet><cellStyleXfs><xf/></cellStyleXfs><cellXfs count=\"3\"><xf/><xf/><xf></xf></cellXfs></styleSheet>".to_vec());
        let l = limits(&parts);
        assert_eq!((l.strings, l.formats), (Some(2), Some(3)));
    }
}
