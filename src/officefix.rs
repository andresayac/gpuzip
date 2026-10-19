//! Office documents made openable again after damage, beyond well-formed XML. A damaged part,
//! once repair_xml has made it well formed, can still hold what Word refuses: a paragraph inside
//! a run's text, text loose in the body, an attribute value from garbage. And a package can lose
//! the parts others depend on: a presentation its slide master, layout or theme (PowerPoint
//! writes them last, so a cut file loses them first), or the presentation part itself, which
//! PowerPoint refuses outright or "repairs". Word's stories are rebuilt from the elements that
//! keep to Word's structure, the text of the others kept as plain paragraphs; what a
//! presentation is missing is made, plainly. All of it was checked against Word and PowerPoint.

use crate::ziprescue::{attr, filter_elements, resolve_target};
use std::collections::{BTreeMap, HashSet};

// ---------------------------------------------------------------- a small XML tree

enum Node<'a> {
    El(El<'a>),
    Text(&'a str),
}

struct El<'a> {
    name: &'a str,
    tag: &'a str,          // the start tag as written
    span: (usize, usize),  // the whole element in the source
    kids: Vec<Node<'a>>,
}

impl<'a> El<'a> {
    fn prefix(&self) -> &'a str {
        self.name.split_once(':').map_or("", |(p, _)| p)
    }
    fn local(&self) -> &'a str {
        self.name.split_once(':').map_or(self.name, |(_, l)| l)
    }
    fn elements(&self) -> impl Iterator<Item = &El<'a>> {
        self.kids.iter().filter_map(|k| match k {
            Node::El(e) => Some(e),
            Node::Text(_) => None,
        })
    }
    fn is(&self, prefix: &str, local: &str) -> bool {
        self.prefix() == prefix && self.local() == local
    }
}

/// The document element of well-formed XML (repair_xml's output); None if there is none
fn parse(x: &str) -> Option<El<'_>> {
    let b = x.as_bytes();
    let mut stack: Vec<El> = Vec::new();
    let mut i = 0;
    while i < x.len() {
        let Some(k) = x[i..].find('<') else {
            if let Some(top) = stack.last_mut() {
                top.kids.push(Node::Text(&x[i..]));
            }
            break;
        };
        if k > 0 {
            if let Some(top) = stack.last_mut() {
                top.kids.push(Node::Text(&x[i..i + k]));
            }
        }
        let s = i + k;
        let rest = &x[s..];
        if rest.starts_with("<!--") {
            i = s + rest.find("-->").map_or(rest.len(), |e| e + 3);
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            let e = rest.find("]]>").map_or(rest.len(), |e| e + 3);
            if let Some(top) = stack.last_mut() {
                top.kids.push(Node::Text(&x[s..s + e]));
            }
            i = s + e;
            continue;
        }
        let mut j = s + 1;
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
        if j >= b.len() {
            return None;
        }
        let tag = &x[s..=j];
        i = j + 1;
        if tag.starts_with("<?") || tag.starts_with("<!") {
            continue;
        }
        if tag.starts_with("</") {
            let mut e = stack.pop()?;
            e.span.1 = i;
            match stack.last_mut() {
                Some(p) => p.kids.push(Node::El(e)),
                None => return Some(e),
            }
            continue;
        }
        let empty = tag.ends_with("/>");
        let body = &tag[1..tag.len() - if empty { 2 } else { 1 }];
        let name = body.split(|c: char| c.is_whitespace()).next().unwrap_or("");
        let e = El { name, tag, span: (s, i), kids: Vec::new() };
        if empty {
            match stack.last_mut() {
                Some(p) => p.kids.push(Node::El(e)),
                None => return Some(e),
            }
        } else {
            stack.push(e);
        }
    }
    None
}

/// The attributes of a start tag, as (name, value)
fn attributes(tag: &str) -> Vec<(&str, &str)> {
    let inner = tag.trim_start_matches('<').trim_end_matches('>').trim_end_matches('/');
    let mut r = inner[inner.find(char::is_whitespace).unwrap_or(inner.len())..].trim_start();
    let mut out = Vec::new();
    while let Some(eq) = r.find('=') {
        let name = r[..eq].trim();
        let v = r[eq + 1..].trim_start();
        let Some(q) = v.chars().next() else { break };
        let Some(end) = v[1..].find(q) else { break };
        out.push((name, &v[1..1 + end]));
        r = v[1 + end + 1..].trim_start();
    }
    out
}

/// A start tag as an opening one (`<w:p/>` becomes `<w:p>`)
fn open_tag(tag: &str) -> String {
    match tag.strip_suffix("/>") {
        Some(t) => format!("{}>", t.trim_end()),
        None => tag.to_string(),
    }
}

// ---------------------------------------------------------------- Word

/// Elements of WordprocessingML that only a few others may hold: a paragraph in a run, a run in
/// the body, a cell outside a row are what garbage makes, and what Word refuses
fn allowed_parents(local: &str) -> Option<&'static [&'static str]> {
    const BLOCK: &[&str] = &["body", "tc", "txbxContent", "sdtContent", "customXml", "hdr", "ftr", "footnote", "endnote", "comment", "ins", "del", "moveFrom", "moveTo", "docPartBody"];
    const INLINE: &[&str] = &["p", "hyperlink", "fldSimple", "ins", "del", "moveFrom", "moveTo", "smartTag", "customXml", "sdtContent", "rt", "rubyBase", "dir", "bdo"];
    Some(match local {
        "p" | "tbl" => BLOCK,
        "r" | "hyperlink" | "fldSimple" | "smartTag" => INLINE,
        "t" | "delText" | "instrText" | "delInstrText" | "fldChar" | "drawing" | "br" | "cr" | "sym" | "lastRenderedPageBreak" | "noBreakHyphen" | "softHyphen" | "footnoteReference"
        | "endnoteReference" | "commentReference" | "annotationRef" | "footnoteRef" | "endnoteRef" | "separator" | "continuationSeparator" | "pgNum" | "ptab" | "object" | "pict"
        | "ruby" => &["r"],
        "tab" => &["r", "tabs"],
        "pPr" => &["p", "pPrChange"],
        "rPr" => &["r", "pPr", "rPrChange", "sdtPr"],
        "tr" => &["tbl", "sdtContent", "customXml"],
        "tc" => &["tr", "sdtContent", "customXml"],
        "body" => &["document"],
        "sectPr" => &["body", "pPr", "sectPrChange"],
        _ => return None,
    })
}

/// Marks that may stand between blocks, runs, rows and cells alike
const RANGE_MARKS: &[&str] = &["bookmarkStart", "bookmarkEnd", "commentRangeStart", "commentRangeEnd", "permStart", "permEnd", "proofErr", "moveFromRangeStart", "moveFromRangeEnd",
                               "moveToRangeStart", "moveToRangeEnd", "customXmlInsRangeStart", "customXmlInsRangeEnd", "customXmlDelRangeStart", "customXmlDelRangeEnd",
                               "customXmlMoveFromRangeStart", "customXmlMoveFromRangeEnd", "customXmlMoveToRangeStart", "customXmlMoveToRangeEnd", "ins", "del", "moveFrom", "moveTo"];
/// What a story (the body, a header, a footer) holds directly, range marks aside
const BODY_KIDS: &[&str] = &["p", "tbl", "sdt", "customXml", "altChunk"];
/// What a paragraph holds, besides its properties and range marks
const P_KIDS: &[&str] = &["r", "hyperlink", "fldSimple", "smartTag", "customXml", "sdt", "subDoc", "dir", "bdo"];
/// What a run holds (ECMA-376 Part 1, 17.3.2)
const R_KIDS: &[&str] = &["rPr", "br", "t", "contentPart", "delText", "instrText", "delInstrText", "noBreakHyphen", "softHyphen", "dayShort", "monthShort", "yearShort", "dayLong",
                          "monthLong", "yearLong", "annotationRef", "footnoteRef", "endnoteRef", "separator", "continuationSeparator", "sym", "pgNum", "cr", "tab", "object", "pict",
                          "fldChar", "ruby", "footnoteReference", "endnoteReference", "commentReference", "drawing", "ptab", "lastRenderedPageBreak"];
/// A run's properties (17.3.2.28)
const RPR_KIDS: &[&str] = &["rStyle", "rFonts", "b", "bCs", "i", "iCs", "caps", "smallCaps", "strike", "dstrike", "outline", "shadow", "emboss", "imprint", "noProof", "snapToGrid",
                            "vanish", "webHidden", "color", "spacing", "w", "kern", "position", "sz", "szCs", "highlight", "u", "effect", "bdr", "shd", "fitText", "vertAlign", "rtl",
                            "cs", "em", "lang", "eastAsianLayout", "specVanish", "oMath", "ins", "del", "moveFrom", "moveTo", "rPrChange"];
/// A paragraph's properties (17.3.1.26)
const PPR_KIDS: &[&str] = &["pStyle", "keepNext", "keepLines", "pageBreakBefore", "framePr", "widowControl", "numPr", "suppressLineNumbers", "pBdr", "shd", "tabs",
                            "suppressAutoHyphens", "kinsoku", "wordWrap", "overflowPunct", "topLinePunct", "autoSpaceDE", "autoSpaceDN", "bidi", "adjustRightInd", "snapToGrid",
                            "spacing", "ind", "contextualSpacing", "mirrorIndents", "suppressOverlap", "jc", "textDirection", "textAlignment", "textboxTightWrap", "outlineLvl",
                            "divId", "cnfStyle", "rPr", "sectPr", "pPrChange"];

/// The WordprocessingML children these elements may have: a run's properties loose in the
/// run, spacing loose in a paragraph, a name made by a flipped bit are garbage's doing
fn allowed_children(local: &str) -> Option<&'static [&'static [&'static str]]> {
    Some(match local {
        "p" => &[&["pPr"], P_KIDS, RANGE_MARKS],
        "r" => &[R_KIDS],
        "rPr" => &[RPR_KIDS],
        "pPr" => &[PPR_KIDS],
        "tbl" => &[&["tblPr", "tblGrid", "tr", "customXml", "sdt"], RANGE_MARKS],
        "tr" => &[&["tblPrEx", "trPr", "tc", "customXml", "sdt"], RANGE_MARKS],
        "tc" => &[&["tcPr", "p", "tbl", "customXml", "sdt", "altChunk"], RANGE_MARKS],
        _ => return None,
    })
}
/// The elements that hold text
const TEXT_HOLDERS: &[&str] = &["t", "delText", "instrText", "delInstrText", "fldData"];
/// Attributes that are measures or numbers wherever they stand
const NUMERIC_ATTRS: &[&str] = &["after", "before", "line", "left", "right", "firstLine", "hanging", "start", "end", "top", "bottom", "header", "footer", "gutter", "beforeLines",
                                 "afterLines", "leftChars", "rightChars", "firstLineChars", "hangingChars", "w", "h", "space", "sz", "num", "id"];
/// Elements whose w:val is a number
const NUMERIC_VALS: &[&str] = &["sz", "szCs", "kern", "position", "outlineLvl", "ilvl", "numId", "gridSpan", "w", "spacing"];
/// Elements whose w:val is on or off
const TOGGLES: &[&str] = &["b", "bCs", "i", "iCs", "caps", "smallCaps", "strike", "dstrike", "outline", "shadow", "emboss", "imprint", "vanish", "webHidden", "specVanish", "noProof",
                           "keepNext", "keepLines", "pageBreakBefore", "widowControl", "contextualSpacing", "snapToGrid", "bidi", "rtl", "cs"];

/// A number, or a measure ("12.5pt", "50%")
fn number_like(v: &str) -> bool {
    let v = v.strip_prefix('-').unwrap_or(v);
    let unit = ["mm", "cm", "in", "pt", "pc", "pi", "%"].iter().find(|u| v.ends_with(*u)).map_or(0, |u| u.len());
    let n = &v[..v.len() - unit];
    let mut parts = n.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    !int.is_empty() && int.chars().all(|c| c.is_ascii_digit()) && frac.map_or(true, |f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
}

fn attrs_ok(e: &El, w: &str) -> bool {
    let local = e.local();
    attributes(e.tag).iter().all(|&(n, v)| {
        let Some((p, an)) = n.split_once(':') else { return true };
        if p != w {
            return true;
        }
        if an.is_empty() || !an.chars().all(|c| c.is_ascii_alphabetic()) {
            return false;
        }
        if an.starts_with("rsid") {
            return v.len() == 8 && v.chars().all(|c| c.is_ascii_hexdigit());
        }
        if NUMERIC_ATTRS.contains(&an) {
            return number_like(v);
        }
        if an == "val" && NUMERIC_VALS.contains(&local) {
            return number_like(v);
        }
        if an == "val" && TOGGLES.contains(&local) {
            return matches!(v, "true" | "false" | "1" | "0" | "on" | "off");
        }
        true
    })
}

/// The element keeps to Word's structure all through: names, places, text where text may be,
/// properties first, attribute values of their kind, a paragraph closing every table cell.
/// Other vocabularies inside (DrawingML, VML, math) are not judged, only what they hold.
fn clean(e: &El, parent: Option<&El>, w: &str) -> bool {
    if e.prefix().is_empty() {
        // no namespace: a story declares none by default, so this is a tag garbage made, unless
        // it declares its own (another vocabulary, not judged)
        return attributes(e.tag).iter().any(|(n, _)| *n == "xmlns");
    }
    if e.prefix() != w {
        return e.elements().all(|c| clean(c, Some(e), w));
    }
    let local = e.local();
    if local.is_empty() || !local.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if let (Some(ps), Some(p)) = (allowed_parents(local), parent) {
        if p.prefix() == w && !ps.contains(&p.local()) {
            return false;
        }
    }
    if !attrs_ok(e, w) {
        return false;
    }
    let holder = TEXT_HOLDERS.contains(&local);
    let kids = allowed_children(local);
    let mut first = true;
    for k in &e.kids {
        match k {
            Node::Text(t) => {
                if !holder && !t.trim().is_empty() {
                    return false;
                }
            }
            Node::El(c) => {
                if holder {
                    return false;
                }
                if let Some(lists) = kids {
                    if c.prefix() == w && !lists.iter().any(|l| l.contains(&c.local())) {
                        return false;
                    }
                }
                let props = (local == "p" && c.is(w, "pPr")) || (local == "r" && c.is(w, "rPr"));
                if props && !first {
                    return false;
                }
                first = false;
                if !clean(c, Some(e), w) {
                    return false;
                }
            }
        }
    }
    if local == "tc" && !e.elements().filter(|c| !c.is(w, "tcPr")).last().map_or(false, |c| c.is(w, "p")) {
        return false;
    }
    true
}

/// The text of the w:t elements inside, as written (escaped)
fn w_text(e: &El, w: &str, out: &mut String) {
    for k in &e.kids {
        if let Node::El(c) = k {
            if c.is(w, "t") {
                for t in &c.kids {
                    match t {
                        Node::Text(s) => out.push_str(s),
                        Node::El(g) => w_text(g, w, out),
                    }
                }
            } else {
                w_text(c, w, out);
            }
        }
    }
}

fn plain_run(text: &str, w: &str) -> String {
    format!("<{w}:r><{w}:t xml:space=\"preserve\">{text}</{w}:t></{w}:r>")
}

/// A paragraph from what keeps to Word's structure in it: its properties if they are sound
/// and first, the runs and other content that are sound; the text of the rest as plain runs
fn rebuild_paragraph(p: &El, w: &str, x: &str) -> String {
    let mut out = if attrs_ok(p, w) { open_tag(p.tag) } else { format!("<{w}:p>") };
    let mut first = true;
    for c in p.elements() {
        let was_first = std::mem::replace(&mut first, false);
        if c.is(w, "pPr") {
            if was_first && clean(c, Some(p), w) {
                out.push_str(&x[c.span.0..c.span.1]);
            }
            continue;
        }
        if c.prefix() == w && (P_KIDS.contains(&c.local()) || RANGE_MARKS.contains(&c.local())) && clean(c, Some(p), w) {
            out.push_str(&x[c.span.0..c.span.1]);
            continue;
        }
        let mut t = String::new();
        w_text(c, w, &mut t);
        if !t.is_empty() {
            out.push_str(&plain_run(&t, w));
        }
    }
    out.push_str(&format!("</{w}:p>"));
    out
}

/// The outermost paragraphs inside an element
fn paragraphs_in<'b, 'a>(e: &'b El<'a>, w: &str, out: &mut Vec<&'b El<'a>>) {
    for c in e.elements() {
        if c.is(w, "p") {
            out.push(c);
        } else {
            paragraphs_in(c, w, out);
        }
    }
}

/// A Word story rebuilt: what was kept as it was, rebuilt, and dropped
pub struct Story {
    pub xml: Vec<u8>,
    pub kept: usize,
    pub rebuilt: usize,
    pub dropped: usize,
}

/// A document, header or footer part rebuilt so Word opens it: every element of the body that
/// keeps to Word's structure as it was, a paragraph that does not from its sound content (its
/// other text as plain runs), anything else as plain paragraphs of its text. None when the part
/// is not one of these.
pub fn sanitize_story(xml: &[u8]) -> Option<Story> {
    let x = String::from_utf8_lossy(xml);
    let root = parse(&x)?;
    let w = root.prefix();
    let document = match root.local() {
        "document" => true,
        "hdr" | "ftr" => false,
        _ => return None,
    };
    let container = if document { root.elements().find(|e| e.is(w, "body")) } else { Some(&root) };
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n");
    out.push_str(&open_tag(root.tag));
    if document {
        out.push_str(&format!("<{w}:body>"));
    }
    let (mut kept, mut rebuilt, mut dropped) = (0, 0, 0);
    let mut blocks = String::new();
    let mut sect = String::new();
    if let Some(body) = container {
        let kids: Vec<&El> = body.elements().collect();
        for (i, b) in kids.iter().enumerate() {
            let is_w = b.prefix() == w;
            if is_w && b.local() == "sectPr" {
                // the section's properties close the body; one anywhere else is garbage
                if i + 1 == kids.len() && clean(b, Some(body), w) {
                    sect = x[b.span.0..b.span.1].to_string();
                    kept += 1;
                } else {
                    dropped += 1;
                }
                continue;
            }
            if is_w && (BODY_KIDS.contains(&b.local()) || RANGE_MARKS.contains(&b.local())) && clean(b, Some(body), w) {
                blocks.push_str(&x[b.span.0..b.span.1]);
                kept += 1;
                continue;
            }
            if b.is(w, "p") {
                blocks.push_str(&rebuild_paragraph(b, w, &x));
                rebuilt += 1;
                continue;
            }
            let mut ps = Vec::new();
            paragraphs_in(b, w, &mut ps);
            if !ps.is_empty() {
                for p in ps {
                    blocks.push_str(&rebuild_paragraph(p, w, &x));
                }
                rebuilt += 1;
                continue;
            }
            let mut t = String::new();
            w_text(b, w, &mut t);
            if t.is_empty() {
                dropped += 1;
            } else {
                blocks.push_str(&format!("<{w}:p>{}</{w}:p>", plain_run(&t, w)));
                rebuilt += 1;
            }
        }
    }
    if blocks.is_empty() {
        blocks.push_str(&format!("<{w}:p/>")); // a story holds at least one paragraph
    }
    out.push_str(&blocks);
    out.push_str(&sect);
    if document {
        out.push_str(&format!("</{w}:body>"));
    }
    out.push_str(&format!("</{}>", root.name));
    Some(Story { xml: out.into_bytes(), kept, rebuilt, dropped })
}

/// The damaged stories of a Word document rebuilt (document, headers, footers)
pub fn fix_word(parts: &mut BTreeMap<String, Vec<u8>>, damaged: &HashSet<&str>) -> Vec<String> {
    let mut fixes = Vec::new();
    let stories: Vec<String> = parts
        .keys()
        .filter(|n| damaged.contains(n.as_str()))
        .filter(|n| *n == "word/document.xml" || ((n.starts_with("word/header") || n.starts_with("word/footer")) && n.ends_with(".xml")))
        .cloned()
        .collect();
    for name in stories {
        let Some(s) = sanitize_story(&parts[&name]) else { continue };
        if s.rebuilt == 0 && s.dropped == 0 {
            continue;
        }
        parts.insert(name.clone(), s.xml);
        let mut what = vec![format!("{} elements kept as they were", s.kept)];
        if s.rebuilt > 0 {
            what.push(format!("{} rebuilt from their sound content and text", s.rebuilt));
        }
        if s.dropped > 0 {
            what.push(format!("{} dropped", s.dropped));
        }
        fixes.push(format!("{name}: rebuilt to Word's structure, {}", what.join(", ")));
    }
    fixes
}

// ---------------------------------------------------------------- packages

const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const PML_NS: &str = "xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"";
const GROUP: &str = "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>";

/// A plain Office theme: the colours and fonts Office starts with, three of each format style
pub fn plain_theme() -> String {
    let fill = "<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>";
    let accents: String = ["4472C4", "ED7D31", "A5A5A5", "FFC000", "5B9BD5", "70AD47"]
        .iter()
        .enumerate()
        .map(|(i, c)| format!("<a:accent{n}><a:srgbClr val=\"{c}\"/></a:accent{n}>", n = i + 1))
        .collect();
    format!(
        "{XML_HEAD}<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Office\"><a:themeElements>\
<a:clrScheme name=\"Office\"><a:dk1><a:sysClr val=\"windowText\" lastClr=\"000000\"/></a:dk1><a:lt1><a:sysClr val=\"window\" lastClr=\"FFFFFF\"/></a:lt1>\
<a:dk2><a:srgbClr val=\"44546A\"/></a:dk2><a:lt2><a:srgbClr val=\"E7E6E6\"/></a:lt2>{accents}\
<a:hlink><a:srgbClr val=\"0563C1\"/></a:hlink><a:folHlink><a:srgbClr val=\"954F72\"/></a:folHlink></a:clrScheme>\
<a:fontScheme name=\"Office\"><a:majorFont><a:latin typeface=\"Calibri Light\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont>\
<a:minorFont><a:latin typeface=\"Calibri\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme>\
<a:fmtScheme name=\"Office\"><a:fillStyleLst>{f3}</a:fillStyleLst><a:lnStyleLst>{l3}</a:lnStyleLst>\
<a:effectStyleLst>{e3}</a:effectStyleLst><a:bgFillStyleLst>{f3}</a:bgFillStyleLst></a:fmtScheme></a:themeElements>\
<a:objectDefaults/><a:extraClrSchemeLst/></a:theme>",
        f3 = fill.repeat(3),
        l3 = format!("<a:ln w=\"6350\">{fill}</a:ln>").repeat(3),
        e3 = "<a:effectStyle><a:effectLst/></a:effectStyle>".repeat(3),
    )
}

fn plain_master(layout_ids: &[(u64, String)]) -> String {
    let ids: String = layout_ids.iter().map(|(id, r)| format!("<p:sldLayoutId id=\"{id}\" r:id=\"{r}\"/>")).collect();
    format!(
        "{XML_HEAD}<p:sldMaster {PML_NS}><p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree>{GROUP}</p:spTree></p:cSld>\
<p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" \
accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/><p:sldLayoutIdLst>{ids}</p:sldLayoutIdLst></p:sldMaster>"
    )
}

fn plain_layout() -> String {
    format!("{XML_HEAD}<p:sldLayout {PML_NS} type=\"blank\" preserve=\"1\"><p:cSld name=\"Blank\"><p:spTree>{GROUP}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>")
}

/// The relationships part of a part: word/document.xml -> word/_rels/document.xml.rels
pub fn rels_name(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// A part's path relative to another part's folder, as relationship targets are written
fn relative(from_part: &str, to_part: &str) -> String {
    let from: Vec<&str> = from_part.split('/').collect();
    let from = &from[..from.len() - 1];
    let to: Vec<&str> = to_part.split('/').collect();
    let common = from.iter().zip(&to[..to.len() - 1]).take_while(|(a, b)| a == b).count();
    let mut out = "../".repeat(from.len() - common);
    out.push_str(&to[common..].join("/"));
    out
}

struct Rel {
    id: String,
    kind: String,   // the last segment of its type: slide, slideLayout, theme, ...
    target: String, // the part it names
}

/// A part's internal relationships
fn rels_of(parts: &BTreeMap<String, Vec<u8>>, part: &str) -> Vec<Rel> {
    let rn = rels_name(part);
    let Some(x) = parts.get(&rn) else { return Vec::new() };
    let x = String::from_utf8_lossy(x).into_owned();
    let mut out = Vec::new();
    filter_elements(&x, "Relationship", |e| {
        if attr(e, "TargetMode").map_or(true, |m| m != "External") {
            if let (Some(id), Some(t), Some(target)) = (attr(e, "Id"), attr(e, "Type"), attr(e, "Target")) {
                out.push(Rel { id, kind: t.rsplit('/').next().unwrap_or("").to_string(), target: resolve_target(&rn, &target) });
            }
        }
        true
    });
    out
}

/// The part named by a relationship of the given kind, if that part is present
fn related(parts: &BTreeMap<String, Vec<u8>>, part: &str, kind: &str) -> Vec<(String, String)> {
    rels_of(parts, part).into_iter().filter(|r| r.kind == kind && parts.contains_key(&r.target)).map(|r| (r.id, r.target)).collect()
}

/// An id no relationship of the part, and nothing in the part, uses yet
fn fresh_rel_id(parts: &BTreeMap<String, Vec<u8>>, part: &str) -> String {
    let mut max = 0u64;
    for p in [part.to_string(), rels_name(part)] {
        if let Some(b) = parts.get(&p) {
            let s = String::from_utf8_lossy(b);
            for (i, _) in s.match_indices("rId") {
                let d: String = s[i + 3..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = d.parse::<u64>() {
                    max = max.max(n);
                }
            }
        }
    }
    format!("rId{}", max + 1)
}

/// The id of `part`'s relationship of this kind to `target`, added if it has none. A part made
/// under a lost one's name brings back the relationships that named it: they are used, not
/// doubled (a master with two themes is one PowerPoint repairs)
fn ensure_rel(parts: &mut BTreeMap<String, Vec<u8>>, part: &str, kind: &str, target: &str) -> String {
    match related(parts, part, kind).into_iter().find(|(_, t)| t == target) {
        Some((id, _)) => id,
        None => add_rel(parts, part, kind, target),
    }
}

/// Adds a relationship from `part` to `target`; returns its id
fn add_rel(parts: &mut BTreeMap<String, Vec<u8>>, part: &str, kind: &str, target: &str) -> String {
    let id = fresh_rel_id(parts, part);
    let rn = rels_name(part);
    let el = format!("<Relationship Id=\"{id}\" Type=\"{REL}/{kind}\" Target=\"{}\"/>", relative(part, target));
    let x = parts.get(&rn).map(|b| String::from_utf8_lossy(b).into_owned()).filter(|x| x.contains("</Relationships>"));
    let x = match x {
        Some(x) => x.replacen("</Relationships>", &format!("{el}</Relationships>"), 1),
        None => format!("{XML_HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{el}</Relationships>"),
    };
    parts.insert(rn, x.into_bytes());
    id
}

/// Parts in a folder with a numbered name (ppt/slides/slide12.xml), in number order
fn numbered(parts: &BTreeMap<String, Vec<u8>>, prefix: &str) -> Vec<String> {
    let mut v: Vec<(u32, String)> = parts
        .keys()
        .filter_map(|n| Some((n.strip_prefix(prefix)?.strip_suffix(".xml")?.parse::<u32>().ok()?, n.clone())))
        .collect();
    v.sort();
    v.into_iter().map(|(_, n)| n).collect()
}

/// A numbered name not yet taken
fn free_name(parts: &BTreeMap<String, Vec<u8>>, prefix: &str) -> String {
    (1..).map(|k| format!("{prefix}{k}.xml")).find(|n| !parts.contains_key(n)).unwrap()
}

/// The largest master or layout id in use (they share one space, from 2^31)
fn max_master_id(parts: &BTreeMap<String, Vec<u8>>) -> u64 {
    let mut max = 2_147_483_647u64;
    for (n, b) in parts {
        if n == "ppt/presentation.xml" || (n.starts_with("ppt/slideMasters/") && n.ends_with(".xml")) {
            let x = String::from_utf8_lossy(b);
            for tag in ["p:sldMasterId", "p:sldLayoutId"] {
                filter_elements(&x, tag, |e| {
                    if let Some(id) = attr(e, "id").and_then(|v| v.parse::<u64>().ok()) {
                        max = max.max(id);
                    }
                    true
                });
            }
        }
    }
    max
}

/// A damaged theme can hold what no theme may (an `a:effecte` from garbage): it is replaced
/// by a plain one, losing only colours and fonts
pub fn replace_damaged_themes(parts: &mut BTreeMap<String, Vec<u8>>, damaged: &HashSet<&str>) -> Vec<String> {
    let themes: Vec<String> = parts.keys().filter(|n| damaged.contains(n.as_str()) && n.contains("/theme/theme") && n.ends_with(".xml")).cloned().collect();
    for t in &themes {
        parts.insert(t.clone(), plain_theme().into_bytes());
    }
    themes.into_iter().map(|t| format!("{t}: damaged, replaced by a plain theme")).collect()
}

/// A presentation PowerPoint opens: every slide on a layout, every layout on a master, every
/// master with a theme and listed by the presentation; a damaged master or layout replaced by
/// a plain one; a presentation part for slides whose own was lost. What is made is plain: the
/// slides keep their content, not the look their lost master gave them.
pub fn fix_presentation(parts: &mut BTreeMap<String, Vec<u8>>, damaged: &HashSet<&str>) -> Vec<String> {
    let mut fixes = Vec::new();
    let slides = numbered(parts, "ppt/slides/slide");
    let mut masters = numbered(parts, "ppt/slideMasters/slideMaster");
    if slides.is_empty() && !parts.contains_key("ppt/presentation.xml") {
        return fixes;
    }
    fixes.extend(fix_slides(parts, damaged));
    // damaged masters and layouts: their lists and references are what damage breaks
    for m in masters.clone() {
        if damaged.contains(m.as_str()) {
            parts.insert(m.clone(), plain_master(&[]).into_bytes());
            fixes.push(format!("{m}: damaged, replaced by a plain master"));
        }
    }
    for l in numbered(parts, "ppt/slideLayouts/slideLayout") {
        if damaged.contains(l.as_str()) {
            parts.insert(l.clone(), plain_layout().into_bytes());
            fixes.push(format!("{l}: damaged, replaced by a blank layout"));
        }
    }
    if masters.is_empty() {
        let m = "ppt/slideMasters/slideMaster1.xml".to_string();
        parts.insert(m.clone(), plain_master(&[]).into_bytes());
        fixes.push(format!("the slide masters were lost: {m} made"));
        masters.push(m);
    }
    let mut next_id = max_master_id(parts) + 1;
    // every master's theme; a theme no master uses is taken first, else a plain one made
    for m in &masters {
        if related(parts, m, "theme").is_empty() {
            let used: HashSet<String> = masters.iter().flat_map(|mm| related(parts, mm, "theme")).map(|(_, t)| t).collect();
            let theme = match numbered(parts, "ppt/theme/theme").into_iter().find(|t| !used.contains(t)) {
                Some(t) => t,
                None => {
                    let t = free_name(parts, "ppt/theme/theme");
                    parts.insert(t.clone(), plain_theme().into_bytes());
                    fixes.push(format!("{t}: made, the theme of {m} being lost"));
                    t
                }
            };
            ensure_rel(parts, m, "theme", &theme);
        }
    }
    // layouts whose master is gone go to the first master; a master has every layout on it
    for l in numbered(parts, "ppt/slideLayouts/slideLayout") {
        let m = match related(parts, &l, "slideMaster").into_iter().next() {
            Some((_, m)) => m,
            None => {
                ensure_rel(parts, &l, "slideMaster", &masters[0]);
                fixes.push(format!("{l}: its master lost, put on {}", masters[0]));
                masters[0].clone()
            }
        };
        if !related(parts, &m, "slideLayout").iter().any(|(_, t)| *t == l) {
            ensure_rel(parts, &m, "slideLayout", &l);
        }
    }
    // every master lists the layouts it has, and has one
    for m in &masters {
        let mut layouts = related(parts, m, "slideLayout");
        if layouts.is_empty() {
            let l = free_name(parts, "ppt/slideLayouts/slideLayout");
            parts.insert(l.clone(), plain_layout().into_bytes());
            ensure_rel(parts, &l, "slideMaster", m);
            let id = ensure_rel(parts, m, "slideLayout", &l);
            layouts.push((id, l.clone()));
            fixes.push(format!("{l}: a blank layout made for {m}"));
        }
        let x = String::from_utf8_lossy(&parts[m]).into_owned();
        let ids: HashSet<&str> = layouts.iter().map(|(id, _)| id.as_str()).collect();
        let (mut y, _) = filter_elements(&x, "p:sldLayoutId", |e| attr(e, "r:id").map_or(false, |id| ids.contains(id.as_str())));
        let listed: HashSet<String> = {
            let mut s = HashSet::new();
            filter_elements(&y, "p:sldLayoutId", |e| {
                s.extend(attr(e, "r:id"));
                true
            });
            s
        };
        let mut add = String::new();
        for (id, _) in &layouts {
            if !listed.contains(id) {
                add.push_str(&format!("<p:sldLayoutId id=\"{next_id}\" r:id=\"{id}\"/>"));
                next_id += 1;
            }
        }
        if !add.is_empty() {
            y = if let Some(k) = y.find("</p:sldLayoutIdLst>") {
                format!("{}{add}{}", &y[..k], &y[k..])
            } else if let Some(k) = y.find("<p:sldLayoutIdLst/>") {
                format!("{}<p:sldLayoutIdLst>{add}</p:sldLayoutIdLst>{}", &y[..k], &y[k + "<p:sldLayoutIdLst/>".len()..])
            } else if let Some(k) = y.find("</p:sldMaster>") {
                format!("{}<p:sldLayoutIdLst>{add}</p:sldLayoutIdLst>{}", &y[..k], &y[k..])
            } else {
                y
            };
        }
        if y != x {
            parts.insert(m.clone(), y.into_bytes());
        }
    }
    // every slide on a layout
    let first_layout = related(parts, &masters[0], "slideLayout")[0].1.clone();
    let mut relaid = 0;
    for s in &slides {
        if related(parts, s, "slideLayout").is_empty() {
            ensure_rel(parts, s, "slideLayout", &first_layout);
            relaid += 1;
        }
    }
    if relaid > 0 {
        fixes.push(format!("{relaid} slides whose layout was lost put on {first_layout}"));
    }
    // the presentation: made for the slides found if it was lost; its masters listed
    let pres = "ppt/presentation.xml";
    if !parts.contains_key(pres) {
        parts.remove(&rels_name(pres)); // what survived of its relationships names ids nothing uses now
        let mut ids = String::new();
        for (k, s) in slides.iter().enumerate() {
            let r = add_rel(parts, pres, "slide", s);
            ids.push_str(&format!("<p:sldId id=\"{}\" r:id=\"{r}\"/>", 256 + k));
        }
        parts.insert(
            pres.to_string(),
            format!("{XML_HEAD}<p:presentation {PML_NS}><p:sldMasterIdLst/><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx=\"12192000\" cy=\"6858000\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>")
                .into_bytes(),
        );
        if let Some((_, t)) = related(parts, &masters[0], "theme").into_iter().next() {
            ensure_rel(parts, pres, "theme", &t);
        }
        fixes.push(format!("{pres} was lost: one made for the {} slides found, in their numbers' order", slides.len()));
    }
    let listed = related(parts, pres, "slideMaster");
    let x = String::from_utf8_lossy(&parts[pres]).into_owned();
    let ids: HashSet<&str> = listed.iter().map(|(id, _)| id.as_str()).collect();
    let (mut y, gone) = filter_elements(&x, "p:sldMasterId", |e| attr(e, "r:id").map_or(false, |id| ids.contains(id.as_str())));
    let mut add = String::new();
    for m in &masters {
        let id = match listed.iter().find(|(_, t)| t == m) {
            Some((id, _)) if y.contains(&format!("r:id=\"{id}\"")) => continue,
            Some((id, _)) => id.clone(),
            None => ensure_rel(parts, pres, "slideMaster", m),
        };
        add.push_str(&format!("<p:sldMasterId id=\"{next_id}\" r:id=\"{id}\"/>"));
        next_id += 1;
    }
    if !add.is_empty() {
        y = if let Some(k) = y.find("</p:sldMasterIdLst>") {
            format!("{}{add}{}", &y[..k], &y[k..])
        } else if let Some(k) = y.find("<p:sldMasterIdLst/>") {
            format!("{}<p:sldMasterIdLst>{add}</p:sldMasterIdLst>{}", &y[..k], &y[k + "<p:sldMasterIdLst/>".len()..])
        } else {
            // the list comes first in the presentation
            match y.find("<p:presentation").and_then(|s| y[s..].find('>').map(|e| s + e + 1)) {
                Some(k) => format!("{}<p:sldMasterIdLst>{add}</p:sldMasterIdLst>{}", &y[..k], &y[k..]),
                None => y,
            }
        };
    }
    if y != x {
        parts.insert(pres.to_string(), y.into_bytes());
        if gone > 0 || !add.is_empty() {
            fixes.push(format!("{pres}: its list of masters made to name the {} present", masters.len()));
        }
    }
    fixes
}

// ---------------------------------------------------------------- PowerPoint slides

/// Names a slide commonly holds. With the names of the package's intact parts they are what a
/// damaged slide may hold: a bit flipped in a name (`a:masorlSlrMapping`) makes one no schema
/// knows, and PowerPoint repairs the file.
const SLIDE_NAMES: &[&str] = &[
    "p:sld", "p:cSld", "p:bg", "p:bgPr", "p:bgRef", "p:spTree", "p:nvGrpSpPr", "p:cNvPr", "p:cNvGrpSpPr", "p:nvPr", "p:grpSpPr", "p:sp", "p:nvSpPr", "p:cNvSpPr", "p:spPr",
    "p:style", "p:txBody", "p:pic", "p:nvPicPr", "p:cNvPicPr", "p:blipFill", "p:graphicFrame", "p:nvGraphicFramePr", "p:cNvGraphicFramePr", "p:xfrm", "p:grpSp", "p:cxnSp",
    "p:nvCxnSpPr", "p:cNvCxnSpPr", "p:ph", "p:clrMapOvr", "p:extLst", "p:ext", "p:custDataLst", "p:tags", "a:xfrm", "a:off", "a:ext", "a:chOff", "a:chExt", "a:prstGeom",
    "a:avLst", "a:gd", "a:custGeom", "a:gdLst", "a:ahLst", "a:cxnLst", "a:rect", "a:pathLst", "a:path", "a:moveTo", "a:lnTo", "a:arcTo", "a:quadBezTo", "a:cubicBezTo",
    "a:close", "a:pt", "a:noFill", "a:solidFill", "a:gradFill", "a:gsLst", "a:gs", "a:lin", "a:blipFill", "a:blip", "a:srcRect", "a:stretch", "a:fillRect", "a:tile",
    "a:pattFill", "a:fgClr", "a:bgClr", "a:grpFill", "a:ln", "a:prstDash", "a:round", "a:bevel", "a:miter", "a:headEnd", "a:tailEnd", "a:effectLst", "a:outerShdw",
    "a:innerShdw", "a:glow", "a:softEdge", "a:reflection", "a:srgbClr", "a:schemeClr", "a:sysClr", "a:prstClr", "a:alpha", "a:lumMod", "a:lumOff", "a:shade", "a:tint",
    "a:satMod", "a:bodyPr", "a:lstStyle", "a:p", "a:pPr", "a:r", "a:rPr", "a:t", "a:br", "a:fld", "a:endParaRPr", "a:latin", "a:ea", "a:cs", "a:sym", "a:hlinkClick",
    "a:hlinkHover", "a:buNone", "a:buChar", "a:buAutoNum", "a:buFont", "a:buClr", "a:buSzPct", "a:lnSpc", "a:spcBef", "a:spcAft", "a:spcPct", "a:spcPts", "a:tabLst",
    "a:tab", "a:defRPr", "a:normAutofit", "a:noAutofit", "a:spAutoFit", "a:highlight", "a:effectRef", "a:fillRef", "a:lnRef", "a:fontRef", "a:graphic", "a:graphicData",
    "a:picLocks", "a:spLocks", "a:grpSpLocks", "a:cxnSpLocks", "a:graphicFrameLocks", "a:stCxn", "a:endCxn", "a:extLst", "a:tbl", "a:tblPr", "a:tblGrid", "a:gridCol",
    "a:tr", "a:tc", "a:txBody", "a:tcPr", "a:tableStyleId", "a:masterClrMapping", "a:overrideClrMapping",
];
/// DrawingML attributes that are numbers (or percentages) wherever they stand
const SLIDE_NUMBERS: &[&str] = &["x", "y", "cx", "cy", "rot", "sz", "w", "lvl", "kern", "spc", "baseline", "marL", "marR", "indent", "lIns", "tIns", "rIns", "bIns", "pos", "ang",
                                 "dist", "blurRad"];

/// The element and all inside it are what a slide may hold: known names, text only in a:t,
/// numbers where numbers go. Vocabularies other than p:, a: and r: are not judged by name.
fn slide_clean(e: &El, names: &HashSet<String>) -> bool {
    let p = e.prefix();
    if p.is_empty() || (matches!(p, "p" | "a" | "r") && !names.contains(e.name)) {
        return false;
    }
    let numbers_ok = p != "a" || attributes(e.tag).iter().all(|(n, v)| !SLIDE_NUMBERS.contains(n) || number_like(v));
    let id_ok = e.name != "p:cNvPr" || attributes(e.tag).iter().all(|(n, v)| *n != "id" || number_like(v));
    if !numbers_ok || !id_ok {
        return false;
    }
    e.kids.iter().all(|k| match k {
        Node::Text(t) => e.name == "a:t" || t.trim().is_empty(),
        Node::El(c) => e.name != "a:t" && slide_clean(c, names),
    })
}

/// The text of an element's a:t elements, as paragraphs: one per a:p inside it
fn slide_text(e: &El, out: &mut Vec<String>) {
    if e.name == "a:p" {
        let mut s = String::new();
        fn ts(e: &El, s: &mut String) {
            for k in &e.kids {
                match k {
                    Node::Text(t) if e.name == "a:t" => s.push_str(t),
                    Node::El(c) => ts(c, s),
                    _ => {}
                }
            }
        }
        ts(e, &mut s);
        if !s.is_empty() {
            out.push(s);
        }
        return;
    }
    for c in e.elements() {
        slide_text(c, out);
    }
}

/// A damaged slide rebuilt: the shapes that are sound as they were, the others as plain text
/// boxes of their text (where they stood, if that is known), the colour mapping of its master;
/// animations and transitions are kept only if sound. None when it is no slide.
pub fn sanitize_slide(xml: &[u8], names: &HashSet<String>) -> Option<(Vec<u8>, usize)> {
    let x = String::from_utf8_lossy(xml);
    let root = parse(&x)?;
    if root.name != "p:sld" {
        return None;
    }
    let mut rebuilt = 0;
    let mut out = format!("{XML_HEAD}{}", open_tag(root.tag));
    let csld = root.elements().find(|e| e.name == "p:cSld");
    let tree = csld.and_then(|c| c.elements().find(|e| e.name == "p:spTree"));
    out.push_str(&csld.map_or("<p:cSld>".to_string(), |c| open_tag(c.tag)));
    if let Some(bg) = csld.and_then(|c| c.elements().find(|e| e.name == "p:bg")).filter(|b| slide_clean(b, names)) {
        out.push_str(&x[bg.span.0..bg.span.1]);
    }
    out.push_str("<p:spTree>");
    let shapes: Vec<&El> = tree.map_or(Vec::new(), |t| t.elements().collect());
    let head = |name: &str| shapes.iter().find(|e| e.name == name).filter(|e| slide_clean(e, names)).map(|e| x[e.span.0..e.span.1].to_string());
    match (head("p:nvGrpSpPr"), head("p:grpSpPr")) {
        (Some(a), Some(b)) => out.push_str(&format!("{a}{b}")),
        _ => out.push_str(GROUP),
    }
    for (k, s) in shapes.iter().filter(|e| e.name != "p:nvGrpSpPr" && e.name != "p:grpSpPr").enumerate() {
        if slide_clean(s, names) {
            out.push_str(&x[s.span.0..s.span.1]);
            continue;
        }
        rebuilt += 1;
        let mut paras = Vec::new();
        slide_text(s, &mut paras);
        if paras.is_empty() {
            continue;
        }
        let xfrm = s
            .elements()
            .find(|e| e.name == "p:spPr")
            .and_then(|sp| sp.elements().find(|e| e.name == "a:xfrm"))
            .filter(|e| slide_clean(e, names))
            .map_or(String::new(), |e| x[e.span.0..e.span.1].to_string());
        let body: String = paras.iter().map(|t| format!("<a:p><a:r><a:t>{t}</a:t></a:r></a:p>")).collect();
        out.push_str(&format!(
            "<p:sp><p:nvSpPr><p:cNvPr id=\"{}\" name=\"Recovered text\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr><p:spPr>{xfrm}</p:spPr>\
<p:txBody><a:bodyPr wrap=\"square\"/><a:lstStyle/>{body}</p:txBody></p:sp>",
            10000 + k
        ));
    }
    out.push_str("</p:spTree></p:cSld>");
    match root.elements().find(|e| e.name == "p:clrMapOvr").filter(|e| slide_clean(e, names)) {
        Some(c) => out.push_str(&x[c.span.0..c.span.1]),
        None => out.push_str("<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>"),
    }
    for name in ["p:transition", "p:timing", "p:extLst"] {
        if let Some(e) = root.elements().find(|e| e.name == name).filter(|e| slide_clean(e, names)) {
            out.push_str(&x[e.span.0..e.span.1]);
        }
    }
    out.push_str("</p:sld>");
    Some((out.into_bytes(), rebuilt))
}

/// The element names of the package's intact presentation parts, with the common ones
fn slide_names(parts: &BTreeMap<String, Vec<u8>>, damaged: &HashSet<&str>) -> HashSet<String> {
    let mut names: HashSet<String> = SLIDE_NAMES.iter().map(|s| s.to_string()).collect();
    fn walk(e: &El, names: &mut HashSet<String>) {
        names.insert(e.name.to_string());
        for c in e.elements() {
            walk(c, names);
        }
    }
    for (n, b) in parts {
        let pml = ["ppt/slides/", "ppt/slideLayouts/", "ppt/slideMasters/"].iter().any(|d| n.starts_with(d));
        if pml && n.ends_with(".xml") && !damaged.contains(n.as_str()) {
            if let Some(root) = parse(&String::from_utf8_lossy(b)) {
                walk(&root, &mut names);
            }
        }
    }
    names
}

/// The damaged slides rebuilt (sanitize_slide)
fn fix_slides(parts: &mut BTreeMap<String, Vec<u8>>, damaged: &HashSet<&str>) -> Vec<String> {
    let names = slide_names(parts, damaged);
    let mut fixes = Vec::new();
    for s in numbered(parts, "ppt/slides/slide") {
        if !damaged.contains(s.as_str()) {
            continue;
        }
        let Some((xml, rebuilt)) = sanitize_slide(&parts[&s], &names) else { continue };
        if xml != parts[&s] {
            parts.insert(s.clone(), xml);
            if rebuilt > 0 {
                fixes.push(format!("{s}: {rebuilt} shapes that could not stand made plain text boxes of their text"));
            }
        }
    }
    fixes
}

// ---------------------------------------------------------------- part names

/// Part names are one whatever their case (OPC), so a bit flipped in a name's case
/// (`ppt/Slides/_rels/slide2.xml.rels`) leaves the part where Office finds it; but this code
/// looks names up as written, and would make that part again: two parts of one name, which
/// PowerPoint repairs. Folders are spelled as most parts spell them, files as the content types
/// name them; a second part of the same name is left out.
pub fn canonical_case(parts: &mut BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut spellings: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for n in parts.keys() {
        if let Some((dir, _)) = n.rsplit_once('/') {
            let segs: Vec<&str> = dir.split('/').collect();
            for k in 1..=segs.len() {
                let d = segs[..k].join("/");
                *spellings.entry(d.to_lowercase()).or_default().entry(d).or_default() += 1;
            }
        }
    }
    let typed: Vec<String> = parts.get("[Content_Types].xml").map_or(Vec::new(), |b| {
        let x = String::from_utf8_lossy(b).into_owned();
        let mut v = Vec::new();
        filter_elements(&x, "Override", |e| {
            v.extend(attr(e, "PartName").map(|p| p.trim_start_matches('/').to_string()));
            true
        });
        v
    });
    let mut fixes = Vec::new();
    for n in parts.keys().cloned().collect::<Vec<_>>() {
        let mut want = match n.rsplit_once('/') {
            Some((dir, file)) => {
                let best = spellings[&dir.to_lowercase()].iter().max_by_key(|(_, c)| **c).map(|(s, _)| s.as_str()).unwrap_or(dir);
                format!("{best}/{file}")
            }
            None => n.clone(),
        };
        if let Some(t) = typed.iter().find(|t| t.eq_ignore_ascii_case(&want)) {
            want = t.clone();
        }
        if want == n {
            continue;
        }
        let part = parts.remove(&n).unwrap();
        if parts.keys().any(|k| k.eq_ignore_ascii_case(&want)) {
            fixes.push(format!("{n}: a second part named {want}, left out"));
        } else {
            fixes.push(format!("{n}: renamed {want}, its case changed by damage"));
            parts.insert(want, part);
        }
    }
    fixes
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"";

    fn story(body: &str) -> (String, Story) {
        let x = format!("<?xml version=\"1.0\"?><w:document {W}><w:body>{body}</w:body></w:document>");
        let s = sanitize_story(x.as_bytes()).unwrap();
        (String::from_utf8(s.xml.clone()).unwrap(), s)
    }

    #[test]
    fn sound_paragraphs_kept_and_broken_ones_rebuilt() {
        let good = "<w:p><w:pPr><w:spacing w:after=\"120\"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">One</w:t></w:r></w:p>";
        // what garbage made of two paragraphs: one inside the other's text, text loose in runs
        let broken = "<w:p><w:r><w:t>Two<w:p><w:r><w:t>Three</w:t></w:r></w:p></w:t></w:r><w:r>loose</w:r></w:p>";
        let sect = "<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr>";
        let (out, s) = story(&format!("{good}{broken}stray text<w:r7rterly/>{sect}"));
        assert!(out.contains(&format!("<w:body>{good}<w:p><w:r><w:t xml:space=\"preserve\">TwoThree</w:t></w:r></w:p>{sect}</w:body>")), "{out}");
        assert_eq!((s.kept, s.rebuilt, s.dropped), (2, 1, 1));
    }

    #[test]
    fn values_of_the_wrong_kind_and_misplaced_properties() {
        // an attribute that is not a number, properties after the run they belong before
        let (out, s) = story("<w:p><w:pPr><w:spacing w:after=\"12ce\"/></w:pPr><w:r><w:t>a</w:t></w:r></w:p><w:p><w:r><w:t>b</w:t></w:r><w:pPr><w:jc w:val=\"left\"/></w:pPr></w:p>");
        assert!(out.contains("<w:body><w:p><w:r><w:t>a</w:t></w:r></w:p><w:p><w:r><w:t>b</w:t></w:r></w:p></w:body>"), "{out}");
        assert_eq!(s.rebuilt, 2);
        assert!(!clean(&parse("<w:b w:val=\"maybe\"/>").unwrap(), None, "w"));
        assert!(clean(&parse("<w:ind w:left=\"-1.5cm\" w:rsidR=\"00A1B2C3\"/>").unwrap(), None, "w"));
    }

    #[test]
    fn properties_loose_in_a_run_and_tags_without_a_namespace() {
        // what bad sectors and resumed downloads left: w:b straight in a run, w:spacing straight
        // in a paragraph, a made-up w:ts, unprefixed tags
        let body = "<w:p><w:r><w:b/><w:t>a</w:t></w:r></w:p><w:p><w:spacing w:after=\"1\"/><w:r><w:t>b</w:t></w:r></w:p>\
<w:p><w:r><w:ts/><w:t>c</w:t></w:r></w:p><w:p><w:pPr><e4>x</e4></w:pPr><w:r><w:t>d</w:t></w:r></w:p><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>e</w:t></w:r></w:p>";
        let (out, s) = story(body);
        assert!(out.contains("<w:body><w:p><w:r><w:t xml:space=\"preserve\">a</w:t></w:r></w:p><w:p><w:r><w:t>b</w:t></w:r></w:p>\
<w:p><w:r><w:t xml:space=\"preserve\">c</w:t></w:r></w:p><w:p><w:r><w:t>d</w:t></w:r></w:p><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>e</w:t></w:r></w:p></w:body>"), "{out}");
        assert_eq!((s.kept, s.rebuilt), (1, 4));
        // another vocabulary that declares its namespace is not judged
        assert!(clean(&parse("<w:r><w:drawing><x xmlns=\"u\"><y/></x></w:drawing></w:r>").unwrap(), None, "w"));
    }

    #[test]
    fn a_table_with_a_broken_cell_becomes_its_paragraphs() {
        let tbl = "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>x</w:t></w:r></w:p></w:tc><w:tc><w:tcPr/></w:tc></w:tr></w:tbl>";
        let (out, _) = story(tbl);
        assert!(out.contains("<w:body><w:p><w:r><w:t>x</w:t></w:r></w:p></w:body>"), "{out}");
        let fine = "<w:tbl><w:tr><w:tc><w:tcPr/><w:p/></w:tc></w:tr></w:tbl>";
        assert!(story(fine).0.contains(fine));
    }

    #[test]
    fn an_empty_story_still_holds_a_paragraph() {
        let x = format!("<w:document {W}/>");
        let s = sanitize_story(x.as_bytes()).unwrap();
        assert!(String::from_utf8(s.xml).unwrap().ends_with("<w:body><w:p/></w:body></w:document>"));
    }

    fn deck(names: &[&str]) -> BTreeMap<String, Vec<u8>> {
        names.iter().map(|n| (n.to_string(), b"<x/>".to_vec())).collect()
    }

    fn text(parts: &BTreeMap<String, Vec<u8>>, n: &str) -> String {
        String::from_utf8(parts[n].clone()).unwrap()
    }

    #[test]
    fn presentation_made_for_slides_whose_own_was_lost() {
        let mut parts = deck(&["ppt/slides/slide10.xml", "ppt/slides/slide2.xml"]);
        let fixes = fix_presentation(&mut parts, &HashSet::new());
        let p = text(&parts, "ppt/presentation.xml");
        let rels = text(&parts, "ppt/_rels/presentation.xml.rels");
        // slide 2 before slide 10, a master listed, and a master, layout and theme made
        let (r2, r10) = (rels.find("slides/slide2.xml").unwrap(), rels.find("slides/slide10.xml").unwrap());
        assert!(r2 < r10, "{rels}");
        assert!(p.contains("<p:sldMasterIdLst><p:sldMasterId id=\"21474836") && p.contains("<p:sldId id=\"256\""), "{p}");
        assert!(rels.contains("Target=\"slideMasters/slideMaster1.xml\"") && rels.contains("Target=\"theme/theme1.xml\""), "{rels}");
        let m = text(&parts, "ppt/slideMasters/slideMaster1.xml");
        assert!(m.contains("<p:sldLayoutIdLst><p:sldLayoutId id=\""), "{m}");
        assert!(text(&parts, "ppt/slides/_rels/slide2.xml.rels").contains("Target=\"../slideLayouts/slideLayout1.xml\""));
        assert!(text(&parts, "ppt/slideLayouts/_rels/slideLayout1.xml.rels").contains("Target=\"../slideMasters/slideMaster1.xml\""));
        assert!(fixes.iter().any(|f| f.contains("was lost")), "{fixes:?}");
    }

    #[test]
    fn slides_whose_layout_was_lost_go_on_one_that_is_there() {
        let mut parts = deck(&["ppt/presentation.xml", "ppt/slides/slide1.xml", "ppt/slideMasters/slideMaster1.xml", "ppt/theme/theme1.xml"]);
        let sid = "<p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId2\"/></p:sldMasterIdLst>";
        parts.insert("ppt/presentation.xml".into(), format!("<p:presentation {PML_NS}>{sid}<p:sldIdLst/></p:presentation>").into_bytes());
        // the slide's relationships are lost; its picture still names rId2
        parts.insert("ppt/slides/slide1.xml".into(), b"<p:sld><a:blip r:embed=\"rId2\"/></p:sld>".to_vec());
        parts.insert("ppt/_rels/presentation.xml.rels".into(), format!("<Relationships><Relationship Id=\"rId2\" Type=\"{REL}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/></Relationships>").into_bytes());
        parts.insert("ppt/slideMasters/slideMaster1.xml".into(), plain_master(&[(2147483649, "rId1".into())]).into_bytes());
        parts.insert("ppt/slideMasters/_rels/slideMaster1.xml.rels".into(), format!("<Relationships><Relationship Id=\"rId2\" Type=\"{REL}/theme\" Target=\"../theme/theme1.xml\"/></Relationships>").into_bytes());
        fix_presentation(&mut parts, &HashSet::new());
        // a layout made for the master, listed with a new id in place of the lost one's
        let m = text(&parts, "ppt/slideMasters/slideMaster1.xml");
        assert!(m.contains("<p:sldLayoutIdLst><p:sldLayoutId id=\"2147483650\" r:id=\"rId3\"/></p:sldLayoutIdLst>"), "{m}");
        let r = text(&parts, "ppt/slides/_rels/slide1.xml.rels");
        assert!(r.contains("Id=\"rId3\"") && r.contains(&format!("{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"")), "{r}");
        // the presentation already listed its master
        assert!(text(&parts, "ppt/presentation.xml").contains(sid));
    }

    #[test]
    fn a_layout_that_outlived_its_master_is_listed_by_the_new_one() {
        let mut parts = deck(&["ppt/presentation.xml", "ppt/slides/slide1.xml", "ppt/slideLayouts/slideLayout1.xml"]);
        parts.insert("ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(), format!("<Relationships><Relationship Id=\"rId1\" Type=\"{REL}/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/></Relationships>").into_bytes());
        fix_presentation(&mut parts, &HashSet::new());
        assert!(!parts.contains_key("ppt/slideLayouts/slideLayout2.xml"));
        let r = text(&parts, "ppt/slideMasters/_rels/slideMaster1.xml.rels");
        assert!(r.contains("Target=\"../slideLayouts/slideLayout1.xml\""), "{r}");
        assert!(text(&parts, "ppt/slideMasters/slideMaster1.xml").contains("<p:sldLayoutId id="));
    }

    #[test]
    fn damaged_masters_layouts_and_themes_replaced() {
        let mut parts = deck(&["ppt/presentation.xml", "ppt/slides/slide1.xml", "ppt/slideMasters/slideMaster1.xml", "ppt/slideLayouts/slideLayout1.xml", "ppt/theme/theme1.xml", "xl/theme/theme1.xml"]);
        let damaged: HashSet<&str> = ["ppt/slideMasters/slideMaster1.xml", "ppt/slideLayouts/slideLayout1.xml", "ppt/theme/theme1.xml"].into_iter().collect();
        let f = replace_damaged_themes(&mut parts, &damaged);
        assert_eq!(f.len(), 1);
        assert!(text(&parts, "ppt/theme/theme1.xml").contains("<a:fmtScheme name=\"Office\">"));
        assert_eq!(text(&parts, "xl/theme/theme1.xml"), "<x/>");
        fix_presentation(&mut parts, &damaged);
        assert!(text(&parts, "ppt/slideMasters/slideMaster1.xml").contains("<p:clrMap "));
        assert!(text(&parts, "ppt/slideLayouts/slideLayout1.xml").contains("type=\"blank\""));
    }

    #[test]
    fn a_theme_made_under_a_lost_ones_name_is_not_related_twice() {
        // a cut file: the master's relationships survive, naming theme1, which was lost
        let mut parts = deck(&["ppt/presentation.xml", "ppt/slides/slide1.xml", "ppt/slideMasters/slideMaster1.xml", "ppt/slideLayouts/slideLayout1.xml"]);
        parts.insert("ppt/slideMasters/slideMaster1.xml".into(), plain_master(&[(2147483649, "rId1".into())]).into_bytes());
        parts.insert("ppt/slideMasters/_rels/slideMaster1.xml.rels".into(), format!("<Relationships><Relationship Id=\"rId1\" Type=\"{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/>\
<Relationship Id=\"rId2\" Type=\"{REL}/theme\" Target=\"../theme/theme1.xml\"/></Relationships>").into_bytes());
        fix_presentation(&mut parts, &HashSet::new());
        assert!(parts.contains_key("ppt/theme/theme1.xml"));
        assert_eq!(text(&parts, "ppt/slideMasters/_rels/slideMaster1.xml.rels").matches("/theme\"").count(), 1);
    }

    #[test]
    fn damaged_slides_keep_their_sound_shapes() {
        let names: HashSet<String> = SLIDE_NAMES.iter().map(|s| s.to_string()).collect();
        let shape = |id: u32, x: &str, t: &str| {
            format!("<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"s\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"{x}\" y=\"0\"/><a:ext cx=\"1\" cy=\"1\"/></a:xfrm></p:spPr>\
<p:txBody><a:bodyPr/><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:txBody></p:sp>")
        };
        let (good, bad) = (shape(2, "457200", "kept"), shape(3, "45720O", "flattened"));
        let x = format!("<p:sld {PML_NS}><p:cSld><p:spTree>{GROUP}{good}{bad}</p:spTree></p:cSld><p:clrMapOvr><a:masorlSlrMapping/></p:clrMapOvr></p:sld>");
        let (out, rebuilt) = sanitize_slide(x.as_bytes(), &names).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(rebuilt, 1);
        assert!(out.contains(&good), "{out}");
        assert!(out.contains("name=\"Recovered text\"/><p:cNvSpPr txBox=\"1\"/><p:nvPr/></p:nvSpPr><p:spPr></p:spPr><p:txBody><a:bodyPr wrap=\"square\"/><a:lstStyle/><a:p><a:r><a:t>flattened</a:t>"), "{out}");
        assert!(out.ends_with("</p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"), "{out}");
    }

    #[test]
    fn a_name_whose_case_was_flipped_is_put_back() {
        let mut parts = deck(&["ppt/slides/slide1.xml", "ppt/slides/slide2.xml", "ppt/slides/_rels/slide1.xml.rels", "ppt/Slides/_rels/slide2.xml.rels", "ppt/slides/sLide3.xml"]);
        parts.insert("[Content_Types].xml".into(), b"<Types><Override PartName=\"/ppt/slides/slide3.xml\" ContentType=\"t\"/></Types>".to_vec());
        let fixes = canonical_case(&mut parts);
        assert!(parts.contains_key("ppt/slides/_rels/slide2.xml.rels") && !parts.contains_key("ppt/Slides/_rels/slide2.xml.rels"));
        assert!(parts.contains_key("ppt/slides/slide3.xml"));
        assert_eq!(fixes.len(), 2, "{fixes:?}");
        // a second part of one name is left out
        let mut parts = deck(&["ppt/slides/slide1.xml", "ppt/slides/slide2.xml", "ppt/Slides/slide1.xml"]);
        canonical_case(&mut parts);
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn relationship_paths() {
        assert_eq!(rels_name("ppt/slides/slide1.xml"), "ppt/slides/_rels/slide1.xml.rels");
        assert_eq!(relative("ppt/slides/slide1.xml", "ppt/slideLayouts/slideLayout1.xml"), "../slideLayouts/slideLayout1.xml");
        assert_eq!(relative("ppt/presentation.xml", "ppt/slides/slide1.xml"), "slides/slide1.xml");
        assert_eq!(relative("ppt/slideMasters/slideMaster1.xml", "ppt/theme/theme1.xml"), "../theme/theme1.xml");
    }
}
