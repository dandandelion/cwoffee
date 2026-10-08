//! `sheet_read` - look inside a spreadsheet without changing it.
//!
//! Stage 1 of the spreadsheet workflow (see `temp-spreadsheet-plan.md`). Before this
//! tool the engine could *make* an .xlsx (`write_xlsx`) and was blind to one the user
//! already had: `read_file` refuses it as binary, and so does `edit_file`. A cowork
//! harness that cannot open the user's own workbook is not coworking.
//!
//! Two boundaries hold this stage:
//!
//! * **Read-only.** There is no mutation path in this module, so it cannot damage a
//!   file. Editing an .xlsx is a different problem - surgical OOXML plus a capability
//!   probe, because re-serializing a workbook silently drops formulas, number formats,
//!   merges, widths, charts and pivot tables - and it gets its own tool when the
//!   preservation test exists.
//! * **Bounded output.** One endpoint slot and a 262k window: an unbounded cell dump
//!   costs tens of thousands of prefill tokens and poisons the llama.cpp prefix cache
//!   for the rest of the session. Every read is paged, and the truncation line names
//!   the range to ask for next.
//!
//! Parsing is streaming (SAX over the zip entry), so a large sheet does not become
//! resident memory, and every part read is capped against a zip bomb. `zip` is already
//! a dependency, `quick-xml` is already in the workspace lock, and nothing here spawns
//! a process.

use std::collections::BTreeMap;
use std::io::{BufReader, Read};

use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{json, Value};
use zip::ZipArchive;

use super::path::{confine_permitted, display_rel, lexical_resolve};
use super::{clamp, CallRisk, Risk, Tool, ToolCtx, ToolResult};

/// Above this, say so rather than try. A 200 MB workbook is a database, and the
/// honest answer is a smaller export or an exec pipeline.
pub(crate) const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// Decompressed ceiling for one zip part. A bomb declares gigabytes; we refuse
/// rather than find out from the OOM killer.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;

const DEFAULT_MAX_CELLS: usize = 2000;
const HARD_MAX_CELLS: usize = 8000;

/// The line a bounded read ends with. It is also how the caller learns the result
/// was cut, so it is a constant rather than a string in two places.
const PAGING_MARK: &str = "# stopped after ";

/// Stop scanning a sheet after this many cells even for `structure`. Extents then
/// come from the file's own `<dimension>`, with a note saying the scan was capped.
const MAX_SCAN_CELLS: usize = 400_000;

// ---------------------------------------------------------------------------
// cell references

/// Excel's own grid limits. `nope` looks like a column name and is not one, and a
/// model that types a sheet name into `range` should be told rather than pointed at
/// column four million.
pub(crate) const MAX_COL: usize = 16_384; // XFD
const MAX_ROW: usize = 1_048_576;

/// 0-based column: "A" -> 0, "Z" -> 25, "AA" -> 26.
fn col_of(letters: &str) -> Option<usize> {
    let mut n: usize = 0;
    for b in letters.bytes() {
        let d = match b {
            b'A'..=b'Z' => b - b'A' + 1,
            b'a'..=b'z' => b - b'a' + 1,
            _ => return None,
        };
        n = n.checked_mul(26)? + d as usize;
    }
    if n == 0 || n > MAX_COL {
        None
    } else {
        Some(n - 1)
    }
}

pub(crate) fn col_name(idx: usize) -> String {
    let mut out = Vec::new();
    let mut n = idx + 1;
    while n > 0 {
        out.push((b'A' + ((n - 1) % 26) as u8) as char);
        n = (n - 1) / 26;
    }
    out.reverse();
    out.into_iter().collect()
}

/// `B7` -> (row 6, col 1). Absolute markers (`$B$7`) are accepted because models
/// copy them out of defined names.
fn split_ref(ref_text: &str) -> Option<(usize, usize)> {
    let t = ref_text.trim().replace('$', "");
    let letters: String = t.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = t.chars().skip_while(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let row: usize = digits.parse().ok()?;
    let col = col_of(&letters)?;
    if row == 0 || row > MAX_ROW {
        return None;
    }
    Some((row - 1, col))
}

pub(crate) struct Rect {
    pub(crate) r1: usize,
    pub(crate) c1: usize,
    pub(crate) r2: usize,
    pub(crate) c2: usize,
}

/// `A1:D12`, `B7`, `A:D`, `3:9`. Sheet-qualified input (`Sales!A1:D12`) is accepted
/// and the sheet part ignored, because that is how it appears in spreadsheets.
pub(crate) fn parse_rect(spec: &str) -> Result<Rect, String> {
    let spec = spec.trim();
    let body = match spec.find('!') {
        Some(i) => spec[i + 1..].trim(),
        None => spec,
    };
    if body.is_empty() {
        return Err("empty range".into());
    }
    let (a, b) = match body.split_once(':') {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (body, None),
    };
    let one = |s: &str| -> Result<(Option<usize>, Option<usize>), String> {
        if s.chars().all(|c| c.is_ascii_digit()) {
            let row: usize = s.parse().map_err(|_| format!("{s:?} is not a row"))?;
            if row == 0 || row > MAX_ROW {
                return Err(format!("{s:?} is not a row (1 to {MAX_ROW})"));
            }
            return Ok((Some(row - 1), None));
        }
        if s.chars().all(|c| c.is_ascii_alphabetic()) {
            return match col_of(s) {
                Some(c) => Ok((None, Some(c))),
                None => Err(format!("{s:?} is not a column (A to XFD)")),
            };
        }
        match split_ref(s) {
            Some((r, c)) => Ok((Some(r), Some(c))),
            None => Err(format!("{s:?} is not a cell reference")),
        }
    };
    let (r1, c1) = one(a)?;
    let (r2, c2) = match b {
        Some(s) => one(s)?,
        None => (r1, c1),
    };
    let open = usize::MAX / 4;
    let rect = Rect {
        r1: r1.unwrap_or(0),
        c1: c1.unwrap_or(0),
        r2: r2.unwrap_or(open),
        c2: c2.unwrap_or(open),
    };
    if rect.r2 < rect.r1 || rect.c2 < rect.c1 {
        return Err(format!(
            "range {spec:?} is reversed - write the top-left first, then the bottom-right"
        ));
    }
    Ok(rect)
}

// ---------------------------------------------------------------------------
// package plumbing

/// Parts an edit would have to preserve. Harmless to report today, and the same
/// inventory Stage 2 will refuse with.
#[derive(Debug, Default)]
struct Hazards {
    charts: usize,
    pivot_tables: usize,
    pivot_caches: usize,
    drawings: usize,
    media: usize,
    external_links: usize,
    query_tables: usize,
    macros: bool,
}

fn scan_hazards(names: impl Iterator<Item = String>) -> Hazards {
    let mut h = Hazards::default();
    for n in names {
        let n = n.to_ascii_lowercase();
        if n.contains("/charts/") || n.starts_with("xl/charts/") {
            h.charts += 1;
        } else if n.contains("/pivottables/") {
            h.pivot_tables += 1;
        } else if n.contains("/pivotcache/") {
            h.pivot_caches += 1;
        } else if n.contains("/drawings/") {
            h.drawings += 1;
        } else if n.contains("/media/") {
            h.media += 1;
        } else if n.contains("/externallinks/") {
            h.external_links += 1;
        } else if n.contains("/querytables/") {
            h.query_tables += 1;
        } else if n.ends_with(".bin") || n.contains("/vbaproject") {
            h.macros = true;
        }
    }
    h
}

pub(crate) struct SheetMeta {
    pub(crate) name: String,
    pub(crate) part: String,
    pub(crate) hidden: bool,
}

/// Read one zip part with a hard cap on the decompressed size.
fn read_part<T, F>(zip: &mut ZipArchive<std::fs::File>, name: &str, f: F) -> Result<T, String>
where
    F: FnOnce(&mut dyn Read) -> Result<T, String>,
{
    let mut entry = match zip.by_name(name) {
        Ok(e) => e,
        Err(_) => return Err(format!("{name} is missing from the workbook")),
    };
    let mut limited = (&mut entry).take(MAX_PART_BYTES);
    f(&mut limited)
}

/// What a `visit` callback sees: an open tag (also for self-closed tags) or a
/// close tag with the text accumulated since the open.
pub enum Visit<'a> {
    Open { tag: &'a str, attrs: &'a BTreeMap<String, String> },
    Close { tag: &'a str, text: &'a str },
}

/// SAX walk over one XML part. One callback, not two, so a caller can keep state
/// across open and close without borrowing the same cell twice.
fn visit<R: Read>(reader: R, mut on: impl FnMut(Visit<'_>) -> Result<(), String>) -> Result<(), String> {
    // BufReader keeps this streaming: quick-xml reads in chunks, so a large sheet
    // inside a capped file never becomes one resident string.
    let mut reader = Reader::from_reader(BufReader::new(reader));
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    let mut text = String::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = e.name().as_ref().to_string();
                let attrs = attrs_of(&e);
                text.clear();
                on(Visit::Open { tag: &tag, attrs: &attrs })?;
            }
            Ok(Event::Empty(e)) => {
                let tag = e.name().as_ref().to_string();
                let attrs = attrs_of(&e);
                text.clear();
                on(Visit::Open { tag: &tag, attrs: &attrs })?;
                on(Visit::Close { tag: &tag, text: "" })?;
            }
            Ok(Event::Text(t)) => {
                let raw = t.into_inner();
                let un = quick_xml::escape::unescape(&raw)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| raw.to_string());
                text.push_str(&un);
            }
            Ok(Event::CData(t)) => {
                text.push_str(&t.into_inner());
            }
            Ok(Event::GeneralRef(r)) => {
                text.push_str(&resolve_ref(r.as_ref()));
            }
            Ok(Event::End(e)) => {
                let tag = e.name().as_ref().to_string();
                on(Visit::Close { tag: &tag, text: &text })?;
                text.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("unreadable xml: {e}")),
            _ => {}
        }
    }
    Ok(())
}

fn attrs_of(e: &quick_xml::events::BytesStart) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for a in e.attributes().flatten() {
        // Attribute values arrive still escaped: a sheet named `Q&A` is stored as
        // `Q&amp;A`, and the model should see the name the user sees.
        let v = a.value.into_owned();
        let v = quick_xml::escape::unescape(&v)
            .map(|c| c.into_owned())
            .unwrap_or(v);
        out.insert(a.key.as_ref().to_string(), v);
    }
    out
}

/// Resolve one entity reference. quick-xml reports `&amp;` as its own event rather
/// than folding it into the surrounding text, so ignoring that event quietly edits
/// the user's cell contents - `Q&A` becomes `QA`.
fn resolve_ref(name: &str) -> String {
    match name {
        "amp" => return "&".into(),
        "lt" => return "<".into(),
        "gt" => return ">".into(),
        "quot" => return "\"".into(),
        "apos" => return "'".into(),
        _ => {}
    }
    let numeric = name
        .strip_prefix("#x")
        .or_else(|| name.strip_prefix("#X"))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .or_else(|| name.strip_prefix('#').and_then(|d| d.parse::<u32>().ok()));
    match numeric.and_then(char::from_u32) {
        Some(c) => c.to_string(),
        // An entity we do not know stays visible as written. Inventing text here
        // would be worse than reporting something odd-looking.
        None => format!("&{name};"),
    }
}

/// A rels Target is relative to `xl/` (the parent of `xl/_rels/`), or absolute when
/// it starts with `/`. Segment stack, so `../x` leaves the `xl` directory instead of
/// producing a path that names it twice.
fn resolve_target(target: &str) -> String {
    let t = target.trim();
    if t.is_empty() {
        return String::new();
    }
    let segs: Vec<&str> = if let Some(abs) = t.strip_prefix('/') {
        abs.split('/').filter(|c| !c.is_empty() && *c != ".").collect()
    } else {
        let mut v = vec!["xl"];
        for comp in t.split('/') {
            match comp {
                "" | "." => {}
                ".." => {
                    v.pop();
                }
                c => v.push(c),
            }
        }
        v
    };
    if segs.is_empty() {
        return String::new();
    }
    // A target that walks above the package root is not a part we can open.
    if segs[0] == ".." {
        return String::new();
    }
    segs.join("/")
}

/// `xl/workbook.xml` and its rels, in the order the user sees the tabs. The sheet
/// element carries a *relationship id*, not a path, so the rels are read first.
pub(crate) fn workbook_meta(
    zip: &mut ZipArchive<std::fs::File>,
) -> Result<(Vec<SheetMeta>, Vec<String>), String> {
    let mut rels: BTreeMap<String, String> = BTreeMap::new();
    read_part(zip, "xl/_rels/workbook.xml.rels", |r| {
        visit(r, |ev| {
            if let Visit::Open { tag, attrs } = ev {
                if tag == "Relationship" {
                    if let (Some(id), Some(target)) = (attrs.get("Id"), attrs.get("Target")) {
                        rels.insert(id.clone(), target.clone());
                    }
                }
            }
            Ok(())
        })
    })?;

    let mut sheets = Vec::new();
    let mut defined: Vec<String> = Vec::new();
    let mut pending_name: Option<String> = None;
    read_part(zip, "xl/workbook.xml", |r| {
        visit(r, |ev| match ev {
            Visit::Open { tag, attrs } => {
                match tag {
                    "sheet" => {
                        let name = attrs.get("name").cloned().unwrap_or_else(|| "?".into());
                        let hidden = matches!(
                            attrs.get("state").map(|s| s.as_str()),
                            Some("hidden") | Some("veryHidden")
                        );
                        let rid = attrs
                            .iter()
                            .find(|(k, _)| k.as_str() == "id" || k.ends_with(":id"))
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        let part = resolve_target(rels.get(&rid).map(|s| s.as_str()).unwrap_or(""));
                        if part.is_empty() {
                            return Err(format!(
                                "sheet {name:?} points at relationship {rid:?}, which the workbook does not define"
                            ));
                        }
                        sheets.push(SheetMeta { name, part, hidden });
                    }
                    "definedName" => {
                        pending_name = attrs.get("name").cloned();
                    }
                    _ => {}
                }
                Ok(())
            }
            Visit::Close { tag, text } => {
                if tag == "definedName" {
                    if let Some(n) = pending_name.take() {
                        defined.push(format!("{n} -> {}", text.trim()));
                    }
                }
                Ok(())
            },
        })
    })?;
    if sheets.is_empty() {
        return Err("this workbook has no sheets".into());
    }
    Ok((sheets, defined))
}

/// `xl/sharedStrings.xml`: one entry per `<si>`, with rich-text runs concatenated -
/// the styling lives in `styles.xml` and a reader wants the text.
pub(crate) fn shared_strings(zip: &mut ZipArchive<std::fs::File>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if zip.by_name("xl/sharedStrings.xml").is_err() {
        return out;
    }
    let _ = read_part(zip, "xl/sharedStrings.xml", |r| {
        let mut current: Option<String> = None;
        visit(r, |ev| {
            match ev {
                Visit::Open { tag, .. } => {
                    if tag == "si" {
                        current = Some(String::new());
                    }
                }
                Visit::Close { tag, text } => match tag {
                    "t" => {
                        if let Some(s) = current.as_mut() {
                            s.push_str(text);
                        }
                    }
                    "si" => {
                        if let Some(s) = current.take() {
                            out.push(s);
                        }
                    }
                    _ => {}
                },
            }
            Ok(())
        })?;
        if let Some(s) = current {
            out.push(s);
        }
        Ok(())
    });
    out
}

/// Built-in number formats that mean a date or a time. The locale-specific ranges
/// are included because files produced outside en-US use them.
fn builtin_date_fmt(id: u32) -> bool {
    matches!(id, 14..=22 | 27..=36 | 45..=47 | 50..=58)
}

/// Is a custom format code date-ish? Approximate on purpose: Excel's format grammar
/// is large, and the failure mode here is a conversion the reader can see and
/// question, not a silently wrong number.
fn custom_date_fmt(code: &str) -> bool {
    let mut quoted = false;
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => {
                chars.next();
            }
            '[' => {
                while let Some(n) = chars.next() {
                    if n == ']' {
                        break;
                    }
                }
            }
            _ if !quoted && matches!(c, 'y' | 'Y' | 'm' | 'M' | 'd' | 'D' | 'h' | 'H' | 's' | 'S') => {
                return true
            }
            _ => {}
        }
    }
    false
}

/// Index -> is-date for `cellXfs`, which is what a cell's `s` attribute points at.
pub(crate) fn date_styles(zip: &mut ZipArchive<std::fs::File>) -> Vec<bool> {
    let mut out = Vec::new();
    if zip.by_name("xl/styles.xml").is_err() {
        return out;
    }
    let _ = read_part(zip, "xl/styles.xml", |r| {
        let mut custom: BTreeMap<u32, bool> = BTreeMap::new();
        let mut xf: Vec<bool> = Vec::new();
        let mut inside = false;
        visit(r, |ev| {
            if let Visit::Open { tag, attrs } = ev {
                match tag {
                    "numFmt" => {
                        if let (Some(id), Some(code)) = (
                            attrs.get("numFmtId").and_then(|v| v.parse::<u32>().ok()),
                            attrs.get("formatCode"),
                        ) {
                            custom.insert(id, custom_date_fmt(code));
                        }
                    }
                    "cellXfs" => inside = true,
                    "xf" if inside => {
                        let id = attrs
                            .get("numFmtId")
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(0);
                        xf.push(*custom.get(&id).unwrap_or(&builtin_date_fmt(id)));
                    }
                    _ => {}
                }
            }
            Ok(())
        })?;
        out = xf;
        Ok(())
    });
    out
}

/// What styling a workbook carries. Two reasons to count it: an edit that
/// re-serializes the package loses it, and a value grid that prints `9.72` for a
/// cell the user sees as `$9.72` should say so rather than let the model explain
/// the file by its storage.
#[derive(Default)]
pub(crate) struct StyleSummary {
    pub(crate) custom_fmts: usize,
    pub(crate) cell_fmts: usize,
    pub(crate) fonts: usize,
    pub(crate) fills: usize,
}

pub(crate) fn style_summary(zip: &mut ZipArchive<std::fs::File>) -> StyleSummary {
    let mut out = StyleSummary::default();
    if zip.by_name("xl/styles.xml").is_err() {
        return out;
    }
    let _ = read_part(zip, "xl/styles.xml", |r| {
        visit(r, |ev| {
            if let Visit::Open { tag, attrs: _ } = ev {
                match tag {
                    // numFmt entries are the ones a viewer wrote down by hand;
                    // the built-in ids are not declared in the part at all.
                    "numFmt" => out.custom_fmts += 1,
                    "xf" => out.cell_fmts += 1,
                    "font" => out.fonts += 1,
                    "fill" => out.fills += 1,
                    _ => {}
                }
            }
            Ok(())
        })
    });
    out
}

/// Excel serial -> calendar date. Day 1 is 1900-01-01, and Excel counts a
/// 1900-02-29 that never existed, so serials >= 61 sit one above a naive count.
fn serial_to_date(serial: f64) -> String {
    if !serial.is_finite() || serial < 0.0 || serial > 2_958_466.0 {
        return format!("{serial}");
    }
    let whole = serial.floor();
    let frac = serial - whole;
    let days = whole as i64;
    // 1900-01-01 is epoch day -25567, and Excel counts a 1900-02-29 that never
    // existed, so serials at or above 61 sit one above a naive day count.
    const EPOCH_1900: i64 = -25_567;
    let (y, m, d) = match days {
        0 => return "1900-01-00?".into(),
        60 => return "1900-02-29?".into(),
        1..=59 => civil_from_days(EPOCH_1900 + days - 1),
        _ => civil_from_days(EPOCH_1900 + days - 2),
    };
    let secs = (frac * 86_400.0).round() as i64;
    if secs <= 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}

/// Howard Hinnant's days-from-civil, inverted. Cheaper than a date dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// sheet scan

#[derive(Debug, Default)]
struct SheetStat {
    dimension: Option<String>,
    min_row: usize,
    max_row: usize,
    min_col: usize,
    max_col: usize,
    cells: usize,
    merged: usize,
    merge_declared: Option<usize>,
    autofilter: Option<String>,
    tables: usize,
    dv: usize,
    cf: usize,
    formulas: usize,
    shared_formulas: usize,
    array_formulas: usize,
    capped_scan: bool,
}

pub(crate) struct SheetContent {
    /// row index -> column index -> rendered text
    pub(crate) rows: BTreeMap<usize, BTreeMap<usize, String>>,
    stat: SheetStat,
    pub(crate) truncated: bool,
}

/// Read one worksheet part, keeping only cells inside `want` (all of them when
/// None, capped at `max_cells`).
pub(crate) fn read_sheet(
    zip: &mut ZipArchive<std::fs::File>,
    part: &str,
    ss: &[String],
    styles: &[bool],
    want: Option<&Rect>,
    max_cells: usize,
    include_formulas: bool,
) -> Result<SheetContent, String> {
    let mut out = SheetContent {
        rows: BTreeMap::new(),
        stat: SheetStat {
            min_row: usize::MAX,
            min_col: usize::MAX,
            ..Default::default()
        },
        truncated: false,
    };
    read_part(zip, part, |r| {
        let mut reader = Reader::from_reader(BufReader::new(r));
        reader.config_mut().check_end_names = false;
        let mut buf = Vec::new();

        let mut row_idx: usize = 0;
        // (row, col, type, style)
        let mut cell: Option<(usize, usize, String, u32)> = None;
        let mut vbuf = String::new();
        let mut fbuf = String::new();
        let mut isbuf = String::new();
        let mut in_v = false;
        let mut in_f = false;
        let mut in_is = false;
        let mut stored = 0usize;
        let mut stop = false;

        while !stop {
            buf.clear();
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let tag = e.name().as_ref().to_string();
                    let attrs = attrs_of(&e);
                    open(
                        &tag,
                        &attrs,
                        &mut out.stat,
                        &mut row_idx,
                        &mut cell,
                        &mut vbuf,
                        &mut fbuf,
                        &mut isbuf,
                        &mut in_v,
                        &mut in_f,
                        &mut in_is,
                    );
                }
                Ok(Event::Empty(e)) => {
                    let tag = e.name().as_ref().to_string();
                    let attrs = attrs_of(&e);
                    open(
                        &tag,
                        &attrs,
                        &mut out.stat,
                        &mut row_idx,
                        &mut cell,
                        &mut vbuf,
                        &mut fbuf,
                        &mut isbuf,
                        &mut in_v,
                        &mut in_f,
                        &mut in_is,
                    );
                    // A self-closed `<c r="A1"/>` is a real, empty cell: it still
                    // counts toward extents.
                    if tag == "c" {
                        close(
                            &tag,
                            "",
                            &mut out,
                            &mut cell,
                            &mut vbuf,
                            &mut fbuf,
                            &mut isbuf,
                            ss,
                            styles,
                            want,
                            max_cells,
                            include_formulas,
                            &mut stored,
                        );
                    }
                }
                Ok(Event::Text(t)) => {
                    let raw = t.into_inner();
                    let un = quick_xml::escape::unescape(&raw)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| raw.to_string());
                    if in_v {
                        vbuf.push_str(&un);
                    } else if in_f {
                        fbuf.push_str(&un);
                    } else if in_is {
                        isbuf.push_str(&un);
                    }
                }
                Ok(Event::CData(t)) => {
                    let raw = t.into_inner();
                    if in_v {
                        vbuf.push_str(&raw);
                    } else if in_f {
                        fbuf.push_str(&raw);
                    } else if in_is {
                        isbuf.push_str(&raw);
                    }
                }
                Ok(Event::GeneralRef(r)) => {
                    let c = resolve_ref(r.as_ref());
                    if in_v {
                        vbuf.push_str(&c);
                    } else if in_f {
                        fbuf.push_str(&c);
                    } else if in_is {
                        isbuf.push_str(&c);
                    }
                }
                Ok(Event::End(e)) => {
                    let tag = e.name().as_ref().to_string();
                    if tag == "v" {
                        in_v = false;
                    } else if tag == "f" {
                        in_f = false;
                    } else if tag == "is" {
                        in_is = false;
                    } else if tag == "c" {
                        close(
                            &tag,
                            "",
                            &mut out,
                            &mut cell,
                            &mut vbuf,
                            &mut fbuf,
                            &mut isbuf,
                            ss,
                            styles,
                            want,
                            max_cells,
                            include_formulas,
                            &mut stored,
                        );
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(format!("unreadable worksheet: {e}")),
                _ => {}
            }
            if let Some(q) = want {
                if row_idx > q.r2 {
                    stop = true;
                }
            }
            if out.stat.cells > MAX_SCAN_CELLS {
                out.stat.capped_scan = true;
                stop = true;
            }
        }
        Ok(())
    })?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn open(
    tag: &str,
    attrs: &BTreeMap<String, String>,
    stat: &mut SheetStat,
    row_idx: &mut usize,
    cell: &mut Option<(usize, usize, String, u32)>,
    vbuf: &mut String,
    fbuf: &mut String,
    isbuf: &mut String,
    in_v: &mut bool,
    in_f: &mut bool,
    in_is: &mut bool,
) {
    match tag {
        "dimension" => stat.dimension = attrs.get("ref").cloned(),
        "row" => {
            *row_idx = attrs
                .get("r")
                .and_then(|v| v.parse::<usize>().ok())
                .map(|v| v.saturating_sub(1))
                .unwrap_or(*row_idx + 1);
        }
        "c" => {
            let (r, c) = match attrs.get("r").and_then(|v| split_ref(v)) {
                Some(rc) => rc,
                // A missing ref is malformed; put it in the current row rather
                // than dropping the value.
                None => (*row_idx, stat.max_col),
            };
            let t = attrs.get("t").cloned().unwrap_or_else(|| "n".into());
            let s = attrs.get("s").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            *cell = Some((r, c, t, s));
            vbuf.clear();
            fbuf.clear();
            isbuf.clear();
        }
        "v" => *in_v = true,
        "f" => {
            *in_f = true;
            stat.formulas += 1;
            match attrs.get("t").map(|s| s.as_str()) {
                Some("shared") => stat.shared_formulas += 1,
                Some("array") => stat.array_formulas += 1,
                _ => {}
            }
        }
        "is" => *in_is = true,
        "mergeCell" => stat.merged += 1,
        "mergeCells" => {
            // The count attribute and the children say the same thing; adding them
            // would report twice as many merges as exist.
            if let Some(n) = attrs.get("count").and_then(|v| v.parse::<usize>().ok()) {
                stat.merge_declared = Some(n);
            }
        }
        "autoFilter" => stat.autofilter = attrs.get("ref").cloned(),
        "tablePart" => stat.tables += 1,
        "dataValidation" => stat.dv += 1,
        "conditionalFormatting" => stat.cf += 1,
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn close(
    tag: &str,
    _text: &str,
    out: &mut SheetContent,
    cell: &mut Option<(usize, usize, String, u32)>,
    vbuf: &mut String,
    fbuf: &mut String,
    isbuf: &mut String,
    ss: &[String],
    styles: &[bool],
    want: Option<&Rect>,
    max_cells: usize,
    include_formulas: bool,
    stored: &mut usize,
) {
    if tag != "c" {
        return;
    }
    let Some((r, c, t, s)) = cell.take() else { return };
    let text = render_cell(&t, vbuf, isbuf, s, styles, ss);
    let text = if !fbuf.is_empty() && include_formulas {
        let cached = if text.is_empty() {
            String::new()
        } else {
            format!(" [={text}]")
        };
        format!("={fbuf}{cached}")
    } else {
        text
    };
    let stat = &mut out.stat;
    if let Some(n) = stat.merge_declared {
        stat.merged = stat.merged.max(n);
    }
    stat.cells += 1;
    stat.min_row = stat.min_row.min(r);
    stat.max_row = stat.max_row.max(r);
    stat.min_col = stat.min_col.min(c);
    stat.max_col = stat.max_col.max(c);
    let inside = match want {
        Some(q) => r >= q.r1 && r <= q.r2 && c >= q.c1 && c <= q.c2,
        None => true,
    };
    if inside && !text.is_empty() {
        if *stored < max_cells {
            out.rows.entry(r).or_default().insert(c, text);
            *stored += 1;
        } else {
            out.truncated = true;
        }
    }
}

fn render_cell(t: &str, v: &str, inline: &str, style: u32, styles: &[bool], ss: &[String]) -> String {
    match t {
        "s" => match v.trim().parse::<usize>() {
            Ok(i) => ss.get(i).cloned().unwrap_or_else(|| format!("#string{i}?")),
            Err(_) => v.to_string(),
        },
        "inlineStr" => inline.to_string(),
        "b" => match v.trim() {
            "1" | "true" | "TRUE" => "TRUE".into(),
            _ => "FALSE".into(),
        },
        "e" | "d" | "str" => v.to_string(),
        _ => {
            // A number in a date-styled cell is a serial, and a model reading
            // "45678" learns nothing from it.
            if styles.get(style as usize).copied().unwrap_or(false) {
                match v.trim().parse::<f64>() {
                    Ok(n) if n > 0.0 => serial_to_date(n),
                    _ => v.to_string(),
                }
            } else {
                v.to_string()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// csv / tsv

/// RFC4180-shaped: quoted fields, doubled quotes inside them, newlines inside
/// quotes, CRLF tolerated, BOM stripped.
pub(crate) fn parse_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == delim {
            row.push(std::mem::take(&mut field));
        } else if c == '\r' {
            // A bare CR is a Mac-era line ending; treat it as nothing.
            if chars.peek() != Some(&'\n') {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
        } else if c == '\n' {
            row.push(std::mem::take(&mut field));
            rows.push(std::mem::take(&mut row));
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(std::mem::take(&mut field));
        rows.push(row);
    }
    while rows.last().is_some_and(|r| r.len() == 1 && r[0].is_empty()) {
        rows.pop();
    }
    rows
}

pub(crate) fn pick_delim(ext: &str, over: Option<&str>) -> char {
    if let Some(o) = over.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(c) = o.chars().next() {
            return c;
        }
    }
    match ext {
        "tsv" => '\t',
        _ => ',',
    }
}

// ---------------------------------------------------------------------------
// rendering for the model

/// The grid separator is a tab, so a tab or newline inside a cell has to be an
/// escape, and a literal backslash has to be doubled to be reversible.
pub(crate) fn esc(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

fn grid_lines(rows: &BTreeMap<usize, BTreeMap<usize, String>>, cols: &[usize]) -> String {
    let mut out = String::new();
    for (r, cells) in rows {
        out.push_str(&format!("{}:", r + 1));
        for c in cols {
            out.push('\t');
            if let Some(v) = cells.get(c) {
                out.push_str(&esc(v));
            }
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// the tool

/// Both sheet tools share one risk shape: reading inside the workspace is never
/// gated (a prompt on a benign read trains the user to click through the prompt
/// that matters), and a path outside it is named in the approval.
pub(crate) fn read_call_risk(args: &Value, ctx: &ToolCtx) -> CallRisk {
    let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if raw.trim().is_empty() || raw.contains('\0') {
        return CallRisk::at(Risk::Harmless);
    }
    let p = lexical_resolve(&ctx.workspace, raw);
    let ws = lexical_resolve(&ctx.workspace, ".");
    if p == ws || p.starts_with(&ws) {
        CallRisk::at(Risk::Harmless)
    } else {
        CallRisk {
            level: Risk::Sensitive,
            escapes: vec![p.display().to_string()],
        }
    }
}

pub struct SheetReadTool;

#[async_trait]
impl Tool for SheetReadTool {
    fn name(&self) -> &str {
        "sheet_read"
    }

    fn description(&self) -> &str {
        "Read a spreadsheet or delimited file without changing it. mode \"structure\" \
         lists sheets, sizes and what an edit would have to preserve; mode \"values\" \
         returns cells, tab-separated and paged. For a total, a count, a group or a \
         top-N across the whole file, use sheet_query rather than reading cells and \
         adding them up yourself. Handles .xlsx, .csv and .tsv."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative .xlsx, .csv or .tsv" },
                "sheet": { "type": "string", "description": "Sheet name (xlsx). Omit, with mode=structure, to list every sheet" },
                "range": { "type": "string", "description": "A1:D12, or B7, or A:D. Omit to start at the top-left" },
                "mode": { "type": "string", "enum": ["structure", "values"], "description": "Defaults to values when sheet or range is given, otherwise structure" },
                "max_cells": { "type": "integer", "description": "Cells to return; default 2000, max 8000" },
                "include_formulas": { "type": "boolean", "description": "Show each formula with the last value Excel cached" },
                "delimiter": { "type": "string", "description": "csv/tsv only: one of , ; | or a tab" }
            },
            "required": ["path"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    /// Reading a file inside the workspace is never gated: a prompt on a benign
    /// read trains the user to click through the prompt that matters. Outside it,
    /// the path is named and the normal per-call permit machinery applies.
    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        read_call_risk(args, ctx)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &super::ExecSink) -> ToolResult {
        let raw = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("sheet_read: missing required string field \"path\""),
        };
        let path = match confine_permitted(&ctx.workspace, raw, &ctx.permits) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(ext.as_str(), "xlsx" | "csv" | "tsv") {
            return ToolResult::err(format!(
                "sheet_read reads .xlsx, .csv and .tsv; {raw:?} is .{ext}. Export it to \
                 one of those, or use exec if you know what is inside."
            ));
        }
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return ToolResult::err(format!("cannot open {}: {e}", path.display())),
        };
        if !meta.is_file() {
            return ToolResult::err(format!("{} is not a file", path.display()));
        }
        if meta.len() > MAX_FILE_BYTES {
            return ToolResult::err(format!(
                "{} is {} MB, above this tool's {} MB ceiling. Read a smaller export, or \
                 use exec.",
                display_rel(&ctx.workspace, &path),
                meta.len() / (1024 * 1024),
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }

        let mode_arg = args
            .get("mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let sheet_arg = args.get("sheet").and_then(|v| v.as_str());
        let want_values = match mode_arg.as_deref() {
            Some("structure") => false,
            Some("values") => true,
            // An .xlsx needs describing - which sheets, how big, what an edit would
            // disturb. A CSV is one table; the useful default is the table itself.
            _ => ext != "xlsx" || sheet_arg.is_some() || args.get("range").and_then(|v| v.as_str()).is_some(),
        };
        let max_cells = args
            .get("max_cells")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_CELLS)
            .min(HARD_MAX_CELLS);
        let formulas = args
            .get("include_formulas")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let rect = match args.get("range").and_then(|v| v.as_str()) {
            Some(r) => match parse_rect(r) {
                Ok(q) => Some(q),
                Err(e) => return ToolResult::err(format!("sheet_read: {e}")),
            },
            None => None,
        };

        let built = if ext == "xlsx" {
            read_xlsx(
                &path,
                sheet_arg,
                rect.as_ref(),
                want_values,
                max_cells,
                formulas,
                ctx,
            )
        } else {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    return ToolResult::err(format!(
                        "cannot read {} as text: {e}; it may not be a delimited text file",
                        display_rel(&ctx.workspace, &path)
                    ))
                }
            };
            let delim = pick_delim(&ext, args.get("delimiter").and_then(|d| d.as_str()));
            let rows = parse_delimited(&text, delim);
            Ok(delimited_text(&path, &rows, rect.as_ref(), want_values, max_cells, ctx))
        };
        match built {
            Ok(text) => {
                let (text, clamped) = clamp(text, ctx.max_output_bytes);
                let mut res = ToolResult::ok(text);
                // Two different bounds can cut a read: the cell cap (which prints
                // its own range to ask for next) and the byte clamp. The wire flag
                // means "you did not see everything", so it is the union.
                res.truncated = clamped || res.output.contains(PAGING_MARK);
                res
            }
            Err(e) => ToolResult::err(format!("sheet_read: {e}")),
        }
    }
}

fn read_xlsx(
    path: &std::path::Path,
    sheet_arg: Option<&str>,
    rect: Option<&Rect>,
    want_values: bool,
    max_cells: usize,
    formulas: bool,
    ctx: &ToolCtx,
) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut zip = ZipArchive::new(file).map_err(|e| format!("not a readable .xlsx package: {e}"))?;
    let hazards = scan_hazards(zip.file_names().map(|s| s.to_string()));
    let (sheets, defined) = workbook_meta(&mut zip)?;
    let rel = display_rel(&ctx.workspace, path);

    let pick: Vec<&SheetMeta> = match sheet_arg {
        None => sheets.iter().collect(),
        Some(want) => {
            let w = want.trim();
            let hit = sheets
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(w))
                .or_else(|| sheets.iter().find(|s| s.name.to_ascii_lowercase().contains(&w.to_ascii_lowercase())));
            match hit {
                Some(s) => vec![s],
                None => {
                    let all = sheets
                        .iter()
                        .map(|s| format!("{:?}", s.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!("there is no sheet {want:?} in this workbook. sheets: {all}"));
                }
            }
        }
    };

    let mut out = String::new();
    out.push_str(&format!("# {rel} - {} sheet(s)\n", sheets.len()));
    let style_info = style_summary(&mut zip);
    if !want_values {
        out.push_str(&format!(
            "# an edit here would have to preserve: {}\n",
            preserve_line(&hazards, &style_info, defined.len())
        ));
        if !defined.is_empty() {
            out.push_str(&format!("# defined names: {}\n", defined.join(" | ")));
        }
    } else if style_info.custom_fmts > 0 {
        out.push_str(&format!(
            "# note: {} custom number format(s) in this workbook: values are the stored \
             numbers, not the formatted text (a money cell arrives as 9.72, not $9.72)\n",
            style_info.custom_fmts
        ));
    }

    let ss = shared_strings(&mut zip);
    let styles = date_styles(&mut zip);

    for sm in &pick {
        let content = read_sheet(
            &mut zip,
            &sm.part,
            &ss,
            &styles,
            if want_values { rect } else { None },
            max_cells,
            formulas,
        )?;
        let stat = &content.stat;
        let empty = stat.cells == 0;
        let (r1, r2, c1, c2) = if want_values {
            let rows: Vec<usize> = content.rows.keys().copied().collect();
            if rows.is_empty() {
                (0, 0, 0, 0)
            } else {
                let cs: Vec<usize> = rows.iter().flat_map(|r| content.rows[r].keys().copied()).collect();
                (
                    rows[0],
                    rows[rows.len() - 1],
                    cs.iter().copied().min().unwrap_or(0),
                    cs.iter().copied().max().unwrap_or(0),
                )
            }
        } else if empty {
            (0, 0, 0, 0)
        } else {
            (stat.min_row, stat.max_row, stat.min_col, stat.max_col)
        };

        if !want_values {
            let range = match (&stat.dimension, empty) {
                (Some(d), _) => d.clone(),
                (None, true) => "(no cells)".into(),
                (None, false) => format!("{}:{}{}", col_name(c1), r1 + 1, col_name(c2)),
            };
            if empty {
                out.push_str(&format!("\nsheet {:?}{}: {range}", sm.name, if sm.hidden { " [hidden]" } else { "" }));
            } else {
                out.push_str(&format!(
                    "\nsheet {:?}{}: {range} ({} rows x {} cols, {} non-empty cells)",
                    sm.name,
                    if sm.hidden { " [hidden]" } else { "" },
                    r2 - r1 + 1,
                    c2 - c1 + 1,
                    stat.cells
                ));
            }
            let mut extras = Vec::new();
            if stat.merged > 0 {
                extras.push(format!("{} merged ranges", stat.merged));
            }
            if let Some(af) = &stat.autofilter {
                extras.push(format!("autofilter {af}"));
            }
            if stat.tables > 0 {
                extras.push(format!("{} table(s)", stat.tables));
            }
            if stat.dv > 0 {
                extras.push(format!("{} data validation(s)", stat.dv));
            }
            if stat.cf > 0 {
                extras.push(format!("{} conditional format(s)", stat.cf));
            }
            if stat.formulas > 0 {
                let mut kinds = Vec::new();
                if stat.shared_formulas > 0 {
                    kinds.push(format!("{} shared", stat.shared_formulas));
                }
                if stat.array_formulas > 0 {
                    kinds.push(format!("{} array", stat.array_formulas));
                }
                let shared = if kinds.is_empty() {
                    String::new()
                } else {
                    format!(" ({}; they spill past the cell they sit in)", kinds.join(", "))
                };
                extras.push(format!("{} formula(s){shared}", stat.formulas));
            }
            if !extras.is_empty() {
                out.push_str(&format!(" - {}", extras.join(", ")));
            }
            out.push('\n');
            if stat.capped_scan {
                out.push_str("  note: extents come from the file's own record; the cell scan was capped\n");
            }
            // The first row is what lets a model talk about the columns at all.
            if let Some(head) = content.rows.get(&r1) {
                let cols: Vec<usize> = (c1..=(c1 + 40).min(c2)).collect();
                let cells: Vec<String> = cols
                    .iter()
                    .map(|c| head.get(c).map(|v| esc(v)).unwrap_or_default())
                    .collect();
                if cells.iter().any(|c| !c.is_empty()) {
                    out.push_str(&format!("  row {}: {}\n", r1 + 1, cells.join(" | ")));
                }
            }
        } else {
            let cols: Vec<usize> = (c1..=c2).collect();
            out.push_str(&format!(
                "# sheet {:?} rows {}-{} cols {}-{} - tab-separated, empty field = empty cell, \\t \\n \\\\ are escapes\n",
                sm.name,
                r1 + 1,
                r2 + 1,
                col_name(c1),
                col_name(c2)
            ));
            out.push_str(&grid_lines(&content.rows, &cols));
            if content.truncated {
                let span = (r2 - r1 + 1).max(1);
                let next = r2 + 2;
                out.push_str(&format!(
                    "{PAGING_MARK}{} cells - last row shown {}, ask again with range {}{}:{}{}\n",
                    max_cells,
                    r2 + 1,
                    col_name(c1),
                    next,
                    col_name(c2),
                    next + span - 1
                ));
            }
        }
    }
    Ok(out.trim_end().to_string())
}

/// What an edit would have to keep. Styles and defined names belong here: a
/// workbook whose header row is blue, bold and white-on-#4472C4 and whose money
/// cells carry a custom `$#,##0.00` format is not "only the cells", and a tool
/// that says "only the cells" invites the next stage to overwrite the file.
fn preserve_line(h: &Hazards, st: &StyleSummary, defined: usize) -> String {
    let mut bits = hazard_bits(h);
    if st.custom_fmts > 0 || st.cell_fmts > 1 || st.fonts > 1 || st.fills > 1 {
        bits.push(format!(
            "styles ({} cell format(s), {} custom number format(s), {} font(s), {} fill(s))",
            st.cell_fmts, st.custom_fmts, st.fonts, st.fills
        ));
    }
    if defined > 0 {
        bits.push(format!("{defined} defined name(s)"));
    }
    if bits.is_empty() {
        "only the cells".into()
    } else {
        bits.join(", ")
    }
}

fn hazard_bits(h: &Hazards) -> Vec<String> {
    let mut bits = Vec::new();
    if h.charts > 0 {
        bits.push(format!("{} chart(s)", h.charts));
    }
    if h.pivot_tables > 0 {
        bits.push(format!("{} pivot table(s)", h.pivot_tables));
    }
    if h.pivot_caches > 0 {
        bits.push(format!("{} pivot cache(s)", h.pivot_caches));
    }
    if h.drawings > 0 {
        bits.push(format!("{} drawing(s)", h.drawings));
    }
    if h.media > 0 {
        bits.push(format!("{} embedded image(s)", h.media));
    }
    if h.external_links > 0 {
        bits.push(format!("{} external link(s)", h.external_links));
    }
    if h.query_tables > 0 {
        bits.push(format!("{} query table(s)", h.query_tables));
    }
    if h.macros {
        bits.push("macros".into());
    }
    bits
}

fn delimited_text(
    path: &std::path::Path,
    rows: &[Vec<String>],
    rect: Option<&Rect>,
    want_values: bool,
    max_cells: usize,
    ctx: &ToolCtx,
) -> String {
    let rel = display_rel(&ctx.workspace, path);
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let narrowest = rows.iter().map(|r| r.len()).min().unwrap_or(0);
    if !want_values {
        let head = rows.first().cloned().unwrap_or_default();
        return format!(
            "# {rel} - {} rows x up to {cols} columns\n  row 1: {}\n  columns: {}",
            rows.len(),
            head.iter().map(|v| esc(v)).collect::<Vec<_>>().join(" | "),
            (0..cols.min(80)).map(|i| col_name(i)).collect::<Vec<_>>().join(" ")
        );
    }
    let last = rows.len().saturating_sub(1);
    let (r1, r2, c1, c2) = match rect {
        Some(q) => (q.r1, q.r2.min(last), q.c1, q.c2.min(cols.saturating_sub(1))),
        None => (0, last, 0, cols.saturating_sub(1)),
    };
    // Collect first, describe afterwards. A header that promises rows 1-4 and then
    // stops at row 2 because of the cell cap teaches the model to distrust the
    // header; a continuation hint that names row 5 when the last row shown was 2
    // quietly loses two rows.
    let mut map: BTreeMap<usize, BTreeMap<usize, String>> = BTreeMap::new();
    let mut used = 0usize;
    let mut truncated = false;
    'outer: for r in r1..=r2 {
        let row = match rows.get(r) {
            Some(r) => r,
            None => break,
        };
        for c in c1..=c2 {
            if used >= max_cells {
                truncated = true;
                break 'outer;
            }
            if let Some(v) = row.get(c) {
                if !v.is_empty() {
                    map.entry(r).or_default().insert(c, v.clone());
                    used += 1;
                }
            }
        }
    }
    let shown: Vec<usize> = map.keys().copied().collect();
    let (a1, a2) = match (shown.first(), shown.last()) {
        (Some(f), Some(l)) => (*f, *l),
        _ => (r1, r1),
    };
    let mut out = String::new();
    out.push_str(&format!(
        "# {rel} rows {}-{} cols {}-{} - tab-separated, empty field = empty cell, \\t \\n \\\\ are escapes\n",
        a1 + 1,
        a2 + 1,
        col_name(c1),
        col_name(c2)
    ));
    let cols_v: Vec<usize> = (c1..=c2).collect();
    out.push_str(&grid_lines(&map, &cols_v));
    if narrowest < cols {
        out.push_str(&format!(
            "# note: rows here have between {narrowest} and {cols} fields; a missing field reads as empty\n"
        ));
    }
    if truncated {
        // One row past the last one actually painted, and the same number of
        // columns: the model should be able to copy this range verbatim.
        let span = (a2 - a1 + 1).max(1);
        let next = a2 + 2;
        out.push_str(&format!(
            "{PAGING_MARK}{} cells - last row shown {}, ask again with range {}{}:{}{}\n",
            max_cells,
            a2 + 1,
            col_name(c1),
            next,
            col_name(c2),
            next + span - 1
        ));
    }
    out.trim_end().to_string()
}


// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ExecSink;
    use serde_json::json;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use tokio::sync::mpsc;
    use zip::write::FileOptions;
    use zip::CompressionMethod;

    fn ctx_in(dir: &Path) -> ToolCtx {
        ToolCtx {
            workspace: dir.to_path_buf(),
            max_output_bytes: 64 * 1024,
            ..Default::default()
        }
    }

    fn tmp(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "cwoffee_sheets_{}_{}_{}",
            std::process::id(),
            label,
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sink() -> ExecSink {
        mpsc::unbounded_channel().0
    }

    async fn call(ctx: &ToolCtx, args: Value) -> super::super::ToolResult {
        SheetReadTool.run(args, ctx, &sink()).await
    }

    /// What the tool said, ok or not. An assertion that prints only `output` prints
    /// nothing on a failure, which is how a broken tool comes to look like a broken
    /// test.
    fn why(r: &super::super::ToolResult) -> String {
        if r.ok {
            r.output.clone()
        } else {
            r.error.clone().unwrap_or_else(|| "<no error text>".into())
        }
    }

    /// A workbook written by the crate we already use for output.
    fn made_xlsx(dir: &Path) -> PathBuf {
        use rust_xlsxwriter::Workbook;
        let p = dir.join("made.xlsx");
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet();
        ws.set_name("Sales").unwrap();
        ws.write(0, 0, "Region").unwrap();
        ws.write(0, 1, "Units").unwrap();
        ws.write(0, 2, "Total").unwrap();
        ws.write(1, 0, "North").unwrap();
        ws.write(1, 1, 12).unwrap();
        ws.write_formula(1, 2, "=B2*3").unwrap();
        ws.write(2, 0, "South").unwrap();
        ws.write(2, 1, 7.5).unwrap();
        let ws2 = wb.add_worksheet();
        ws2.set_name("Notes").unwrap();
        ws2.write(0, 0, "hello").unwrap();
        wb.save(&p).unwrap();
        p
    }

    /// A package built by hand, so the test controls sharedStrings, styles, inline
    /// strings, booleans, errors, entities, and the parts an edit must respect.
    fn hand_xlsx(dir: &Path, parts: &[(&str, &str)]) -> PathBuf {
        let p = dir.join("hand.xlsx");
        let f = std::fs::File::create(&p).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts: FileOptions<'_, ()> =
            FileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in parts {
            w.start_file(*name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
        p
    }

    const WB: &str = concat!(
        r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        r#"<sheets><sheet name="Data" sheetId="1" r:id="rId1"/>"#,
        r#"<sheet name="Old" sheetId="2" state="hidden" r:id="rId2"/></sheets>"#,
        r#"<definedNames><definedName name="Totals">Data!$D$9</definedName></definedNames></workbook>"#
    );
    const WBRELS: &str = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#;
    const EMPTY2: &str = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#;

    #[tokio::test]
    async fn structure_lists_sheets_sizes_and_the_header_row() {
        let d = tmp("structure");
        made_xlsx(&d);
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "made.xlsx", "mode": "structure"})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(why(&r).contains("\"Sales\""), "{}", why(&r));
        assert!(r.output.contains("\"Notes\""), "{}", why(&r));
        // Without the header row a model cannot talk about the columns.
        assert!(r.output.contains("Region | Units | Total"), "{}", why(&r));
        // A real writer always emits styles.xml, so a generated workbook is not
        // "only the cells" and never was: it carries cell formats and fonts that a
        // careless edit would drop.
        assert!(
            r.output.contains("styles (") && r.output.contains("custom number format"),
            "{}",
            why(&r)
        );
    }

    #[tokio::test]
    async fn a_plain_package_is_still_only_the_cells() {
        // The counter-case, so the line above cannot be an always-true string.
        let d = tmp("plain");
        let sheet = concat!(
            r#"<?xml version="1.0"?><worksheet><sheetData>"#,
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row>"#,
            r#"</sheetData></worksheet>"#
        );
        let wb = r#"<?xml version="1.0"?><workbook><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<?xml version="1.0"?><Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
        hand_xlsx(
            &d,
            &[
                ("xl/workbook.xml", wb),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "hand.xlsx", "mode": "structure"})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.output.contains("only the cells"), "{}", why(&r));
    }

    #[tokio::test]
    async fn styled_values_say_that_a_format_is_not_a_value() {
        let d = tmp("moneyfmt");
        let styles = concat!(
            r#"<?xml version="1.0"?><styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="$#,##0.00"/></numFmts>"#,
            r#"<fonts count="2"><font/><font><b/></font></fonts><fills count="2"><fill/><fill/></fills>"#,
            r#"<cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs></styleSheet>"#
        );
        let sheet = concat!(
            r#"<?xml version="1.0"?><worksheet><sheetData>"#,
            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>Price</t></is></c></row>"#,
            r#"<row r="2"><c r="A2" s="1"><v>9.72</v></c></row>"#,
            r#"</sheetData></worksheet>"#
        );
        hand_xlsx(
            &d,
            &[
                ("xl/workbook.xml", WB),
                ("xl/_rels/workbook.xml.rels", WBRELS),
                ("xl/styles.xml", styles),
                ("xl/worksheets/sheet1.xml", sheet),
                ("xl/worksheets/sheet2.xml", EMPTY2),
            ],
        );
        let ctx = ctx_in(&d);
        let st = call(&ctx, json!({"path": "hand.xlsx", "mode": "structure"})).await;
        assert!(st.ok, "{}", why(&st));
        let line = why(&st);
        assert!(line.contains("styles ("), "{line}");
        assert!(line.contains("1 custom number format"), "{line}");
        assert!(line.contains("2 font(s)"), "{line}");
        assert!(line.contains("1 defined name(s)"), "{line}");
        let v = call(&ctx, json!({"path": "hand.xlsx", "sheet": "Data"})).await;
        assert!(v.ok, "{}", why(&v));
        let out = why(&v);
        assert!(out.contains("stored"), "the value grid should say a format is not a value:\n{out}");
        assert!(out.contains("9.72"), "{out}");
    }

    #[tokio::test]
    async fn values_are_tab_separated_and_range_bounded() {
        let d = tmp("values");
        made_xlsx(&d);
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "made.xlsx", "sheet": "Sales", "range": "A1:B3"})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.output.contains("1:\tRegion\tUnits"), "{}", why(&r));
        assert!(r.output.contains("2:\tNorth\t12"), "{}", why(&r));
        assert!(r.output.contains("3:\tSouth\t7.5"), "{}", why(&r));
        // Column C was outside the range, so the formula must not appear.
        assert!(!r.output.contains("B2*3"), "{}", r.output);
    }

    #[tokio::test]
    async fn formulas_arrive_with_the_cached_value_named() {
        let d = tmp("formulas");
        made_xlsx(&d);
        let ctx = ctx_in(&d);
        let r = call(
            &ctx,
            json!({"path": "made.xlsx", "sheet": "Sales", "range": "C2", "include_formulas": true}),
        )
        .await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.output.contains("=B2*3"), "{}", why(&r));
        // The cached part is labelled as a cache, not passed off as computed now.
        assert!(r.output.contains("[="), "{}", why(&r));
        let plain = call(&ctx, json!({"path": "made.xlsx", "sheet": "Sales", "range": "C2"})).await;
        assert!(!plain.output.contains("B2*3"), "{}", why(&plain));
    }

    #[tokio::test]
    async fn shared_strings_inline_booleans_errors_and_entities_all_survive() {
        let d = tmp("types");
        let sheet = concat!(
            r#"<?xml version="1.0"?><worksheet><dimension ref="A1:D3"/><sheetData>"#,
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>inline &amp; free</t></is></c>"#,
            r#"<c r="C1" t="b"><v>1</v></c><c r="D1" t="e"><v>#DIV/0!</v></c></row>"#,
            r#"<row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><v>42</v></c><c r="C2"/></row>"#,
            r#"<row r="3"><c r="A3" t="s"><v>2</v></c></row></sheetData></worksheet>"#
        );
        let sst = concat!(
            r#"<?xml version="1.0"?><sst count="4" uniqueCount="3"><si><t>Hea&amp;der</t></si>"#,
            r#"<si><r><t>rich</t></r><r><t> text</t></r></si><si><t>third
line</t></si></sst>"#
        );
        hand_xlsx(
            &d,
            &[
                ("xl/workbook.xml", WB),
                ("xl/_rels/workbook.xml.rels", WBRELS),
                ("xl/sharedStrings.xml", sst),
                ("xl/worksheets/sheet1.xml", sheet),
                ("xl/worksheets/sheet2.xml", EMPTY2),
            ],
        );
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "hand.xlsx", "sheet": "Data"})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.output.contains("Hea&der"), "{}", why(&r));
        assert!(r.output.contains("inline & free"), "{}", why(&r));
        assert!(r.output.contains("rich text"), "{}", why(&r));
        assert!(r.output.contains("TRUE"), "{}", why(&r));
        assert!(r.output.contains("#DIV/0!"), "{}", why(&r));
        assert!(r.output.contains("\t42"), "{}", why(&r));
        // A newline inside a value is escaped, never a real break in the grid.
        assert!(r.output.contains("third\\nline"), "{}", why(&r));
        // Row 2 has 3 columns, row 1 has 4: the extents are the union.
        assert!(r.output.contains("cols A-D"), "{}", why(&r));
    }

    #[tokio::test]
    async fn date_styled_numbers_become_iso_dates_and_others_stay_numbers() {
        let d = tmp("dates");
        let styles = concat!(
            r#"<?xml version="1.0"?><styleSheet><numFmts count="1"><numFmt numFmtId="164" formatCode="yyyy-mm"/></numFmts>"#,
            r#"<cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="164"/></cellXfs></styleSheet>"#
        );
        let sheet = concat!(
            r#"<?xml version="1.0"?><worksheet><sheetData><row r="1">"#,
            r#"<c r="A1"><v>45678</v></c><c r="B1" s="1"><v>45672</v></c><c r="C1" s="2"><v>44927</v></c>"#,
            r#"<c r="D1" s="1"><v>45672.5</v></c><c r="E1" s="1"><v>59</v></c></row></sheetData></worksheet>"#
        );
        hand_xlsx(
            &d,
            &[
                ("xl/workbook.xml", WB),
                ("xl/_rels/workbook.xml.rels", WBRELS),
                ("xl/styles.xml", styles),
                ("xl/worksheets/sheet1.xml", sheet),
                ("xl/worksheets/sheet2.xml", EMPTY2),
            ],
        );
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "hand.xlsx", "sheet": "Data"})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.output.contains("2025-01-15"), "{}", why(&r));
        assert!(r.output.contains("2025-01-15 12:00:00"), "{}", why(&r));
        assert!(r.output.contains("2023-01"), "{}", why(&r));
        // Untouched format: the serial the file actually holds.
        assert!(r.output.contains("\t45678\t"), "{}", why(&r));
        // Serial 59 is 1900-02-28; the phantom leap day is 60.
        assert!(r.output.contains("1900-02-28"), "{}", why(&r));
    }

    #[tokio::test]
    async fn an_edit_hazards_are_listed_before_anyone_asks_for_an_edit() {
        let d = tmp("hazards");
        let sheet = concat!(
            r#"<?xml version="1.0"?><worksheet><dimension ref="A1:C9"/><autoFilter ref="A1:C9"/><sheetData>"#,
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#,
            r#"<row r="2"><c r="B2"><f t="shared" ref="B2:B9" si="0">A2*2</f><v>2</v></c></row>"#,
            r#"</sheetData><mergeCells count="2"><mergeCell ref="A1:B1"/><mergeCell ref="C1:C2"/></mergeCells>"#,
            r#"<dataValidation type="whole"/><conditionalFormatting sqref="A2:A9"><cfRule type="cellIs"/></conditionalFormatting>"#,
            r#"<tableParts count="1"><tablePart r:id="rId3"/></tableParts></worksheet>"#
        );
        hand_xlsx(
            &d,
            &[
                ("xl/workbook.xml", WB),
                ("xl/_rels/workbook.xml.rels", WBRELS),
                ("xl/sharedStrings.xml", r#"<?xml version="1.0"?><sst><si><t>x</t></si></sst>"#),
                ("xl/worksheets/sheet1.xml", sheet),
                ("xl/worksheets/sheet2.xml", EMPTY2),
                ("xl/charts/chart1.xml", "<c:chartSpace/>"),
                ("xl/pivotTables/pivotTable1.xml", "<pivotTable/>"),
                ("xl/media/image1.png", "not a png"),
            ],
        );
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "hand.xlsx", "mode": "structure"})).await;
        assert!(r.ok, "{}", why(&r));
        for want in [
            "1 chart(s)",
            "1 pivot table(s)",
            "1 embedded image(s)",
            "2 merged ranges",
            "autofilter A1:C9",
            "1 table(s)",
            "1 data validation(s)",
            "1 conditional format(s)",
            "1 shared",
            "Totals -> Data!$D$9",
            "[hidden]",
        ] {
            assert!(r.output.contains(want), "missing {want:?} in:\n{}", why(&r));
        }
    }

    #[tokio::test]
    async fn paging_names_the_range_to_ask_for_next() {
        let d = tmp("paging");
        let rows: String = (1..=40)
            .map(|r| format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c><c r="B{r}"><v>{}</v></c></row>"#, r * 10))
            .collect();
        let sheet = format!(r#"<?xml version="1.0"?><worksheet><sheetData>{rows}</sheetData></worksheet>"#);
        hand_xlsx(
            &d,
            &[
                ("xl/workbook.xml", WB),
                ("xl/_rels/workbook.xml.rels", WBRELS),
                ("xl/worksheets/sheet1.xml", &sheet),
                ("xl/worksheets/sheet2.xml", EMPTY2),
            ],
        );
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "hand.xlsx", "sheet": "Data", "max_cells": 10})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.output.contains("stopped after 10 cells"), "{}", why(&r));
        // The point of the hint is that it can be copied verbatim: it has to name
        // the row after the last one actually painted, not the row the request
        // happened to ask for.
        let shown: Vec<usize> = r
            .output
            .lines()
            .filter_map(|l| l.trim().split(':').next().and_then(|n| n.parse::<usize>().ok()))
            .collect();
        let last = *shown.iter().max().unwrap();
        assert!(
            r.output.contains(&format!("ask again with range A{}:B", last + 1)),
            "hint does not continue from the last row shown ({last}):\n{}",
            why(&r)
        );
        // 10 cells is at most 10 data rows plus headers.
        assert!(
            r.output.matches(":\t").count() <= 11,
            "returned too much under a 10-cell cap:\n{}",
            why(&r)
        );
    }

    #[tokio::test]
    async fn the_default_bound_keeps_a_dump_out_of_the_prompt() {
        // A file shaped like something a user actually has: 1200 log rows. Read in
        // full it does not just cost prefill once - it sits in history and every
        // later turn pays for it, on an endpoint with one slot. The default cap is
        // the difference between a tool result and a bill.
        let d = tmp("bound");
        let text: String = (1..=1200)
            .map(|r| format!("{r},widget-{},{} us,region {}\n", r * 3, r * 17, r % 7))
            .collect();
        std::fs::write(d.join("log.csv"), &text).unwrap();
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({ "path": "log.csv" })).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.truncated, "a 1200-row read came back claiming to be complete");
        let shown: Vec<usize> = r
            .output
            .lines()
            .filter_map(|l| l.trim().split(':').next().and_then(|n| n.parse::<usize>().ok()))
            .collect();
        assert!(!shown.is_empty(), "no rows painted:\n{}", why(&r));
        // Four fields per row, 2000 cells by default: about 500 rows, not 1200.
        assert!(
            shown.len() <= DEFAULT_MAX_CELLS / 4 + 2,
            "returned {} rows under the default cap",
            shown.len()
        );
        let last = *shown.iter().max().unwrap();
        assert!(
            r.output
                .contains(&format!("ask again with range A{}:D", last + 1)),
            "hint does not continue from the last row shown ({last}):\n{}",
            why(&r)
        );
        assert!(
            r.output.len() < text.len(),
            "the bound is not a bound: {} of {} bytes",
            r.output.len(),
            text.len()
        );
        // Measured by this test: a 39,873-byte file comes back as 18,526 bytes and
        // 500 of its 1200 rows. The bound is on *cells*, so a table with short
        // fields costs proportionally more per row - what is guaranteed is that the
        // number is capped and that the next range is one tool call away, not that
        // the output is a fixed fraction of the file.
    }

    #[tokio::test]
    async fn csv_and_tsv_share_the_surface() {
        let d = tmp("csv");
        std::fs::write(
            d.join("a.csv"),
            "region,units,note\nNorth,12,\"two words, and \"\"quotes\"\"\"\nSouth,7,\n\n",
        )
        .unwrap();
        std::fs::write(d.join("b.tsv"), "a\tb\n1\t2\n").unwrap();
        std::fs::write(d.join("semi.csv"), "a;b\n1;2\n").unwrap();
        let ctx = ctx_in(&d);
        let s = call(&ctx, json!({"path": "a.csv", "mode": "structure"})).await;
        assert!(s.ok, "{}", why(&s));
        assert!(s.output.contains("3 rows"), "{}", why(&s));
        assert!(s.output.contains("region | units | note"), "{}", why(&s));
        let v = call(&ctx, json!({"path": "a.csv", "range": "A1:C3"})).await;
        assert!(v.ok, "{}", why(&v));
        assert!(v.output.contains("2:\tNorth\t12\ttwo words, and \"quotes\""), "{}", why(&v));
        let t = call(&ctx, json!({"path": "b.tsv"})).await;
        assert!(t.output.contains("1:\ta\tb"), "{}", why(&t));
        let semi = call(&ctx, json!({"path": "semi.csv", "delimiter": ";"})).await;
        assert!(semi.output.contains("2:\t1\t2"), "{}", why(&semi));
    }

    #[tokio::test]
    async fn ragged_rows_are_reported_not_invented() {
        let d = tmp("ragged");
        std::fs::write(d.join("r.csv"), "a,b,c\n1\n2,3\n").unwrap();
        let ctx = ctx_in(&d);
        let v = call(&ctx, json!({"path": "r.csv"})).await;
        assert!(v.output.contains("between 1 and 3 fields"), "{}", why(&v));
        assert!(v.output.contains("2:\t1"), "{}", why(&v));
    }

    #[tokio::test]
    async fn bad_input_fails_with_a_reason_the_model_can_act_on() {
        let d = tmp("bad");
        made_xlsx(&d);
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "made.xlsx", "sheet": "Nope"})).await;
        assert!(!r.ok);
        assert!(why(&r).contains("there is no sheet"), "{}", why(&r));
        assert!(why(&r).contains("\"Sales\""), "{}", why(&r));

        let r = call(&ctx, json!({"path": "made.xlsx", "range": "D1:A1"})).await;
        assert!(!r.ok && why(&r).contains("reversed"), "{}", why(&r));

        std::fs::write(d.join("notes.txt"), "hello").unwrap();
        let r = call(&ctx, json!({"path": "notes.txt"})).await;
        assert!(!r.ok && why(&r).contains("reads .xlsx, .csv and .tsv"), "{}", why(&r));

        let r = call(&ctx, json!({"path": "../../etc/passwd"})).await;
        assert!(!r.ok && why(&r).contains("outside the workspace"), "{}", why(&r));
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_package_says_so() {
        let d = tmp("notzip");
        std::fs::write(d.join("fake.xlsx"), "this is not a package").unwrap();
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "fake.xlsx"})).await;
        assert!(!r.ok, "{}", why(&r));
        assert!(why(&r).contains("not a readable .xlsx package"), "{}", why(&r));
    }

    #[tokio::test]
    async fn a_sheet_name_that_only_fuzzy_matches_is_not_guessed_wrong() {
        let d = tmp("fuzzy");
        made_xlsx(&d);
        let ctx = ctx_in(&d);
        let exact = call(&ctx, json!({"path": "made.xlsx", "sheet": "sales"})).await;
        assert!(exact.ok && exact.output.contains("Region"), "{}", exact.output);
        let miss = call(&ctx, json!({"path": "made.xlsx", "sheet": "Q4"})).await;
        assert!(!miss.ok, "{}", why(&miss));
    }


    #[test]
    fn refs_and_ranges_parse() {
        assert_eq!(col_of("A"), Some(0));
        assert_eq!(col_of("Z"), Some(25));
        assert_eq!(col_of("AA"), Some(26));
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(702), "AAA");
        assert_eq!(split_ref("B7"), Some((6, 1)));
        assert_eq!(split_ref("$B$7"), Some((6, 1)));
        let q = parse_rect("Sales!A2:D10").unwrap();
        assert_eq!((q.r1, q.c1, q.r2, q.c2), (1, 0, 9, 3));
        let q = parse_rect("C").unwrap();
        assert_eq!((q.c1, q.c2), (2, 2));
        let q = parse_rect("3:9").unwrap();
        assert_eq!((q.r1, q.r2), (2, 8));
        assert!(parse_rect("nope").is_err());
        assert!(parse_rect("B2:A2").is_err());
        assert!(parse_rect("").is_err());
    }

    #[test]
    fn serial_dates_match_excel_including_the_phantom_leap_day() {
        assert_eq!(serial_to_date(1.0), "1900-01-01");
        assert_eq!(serial_to_date(59.0), "1900-02-28");
        assert_eq!(serial_to_date(60.0), "1900-02-29?");
        assert_eq!(serial_to_date(61.0), "1900-03-01");
        assert_eq!(serial_to_date(45672.0), "2025-01-15");
        assert_eq!(serial_to_date(45678.0), "2025-01-21");
        assert_eq!(serial_to_date(44927.0), "2023-01-01");
        assert_eq!(serial_to_date(45672.5), "2025-01-15 12:00:00");
        assert_eq!(serial_to_date(1e12), "1000000000000");
    }

    #[test]
    fn rels_targets_resolve_against_xl() {
        assert_eq!(resolve_target("worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve_target("/xl/worksheets/s2.xml"), "xl/worksheets/s2.xml");
        assert_eq!(resolve_target("../external/x.xml"), "external/x.xml");
        assert_eq!(resolve_target(""), "");
    }

    #[test]
    fn date_format_guessing() {
        assert!(builtin_date_fmt(14));
        assert!(!builtin_date_fmt(0));
        assert!(custom_date_fmt("yyyy-mm-dd"));
        assert!(!custom_date_fmt("#,##0.00 \"dollars\""));
        assert!(!custom_date_fmt("0.0%"));
        assert!(!custom_date_fmt("General"));
    }

    #[test]
    fn csv_quoting_and_embedded_newlines() {
        let rows = parse_delimited("a,b\n\"x, y\",\"he said \"\"hi\"\"\"\n\"multi\nline\",z", ',');
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][0], "x, y");
        assert_eq!(rows[1][1], "he said \"hi\"");
        assert_eq!(rows[2][0], "multi\nline");
        assert_eq!(parse_delimited("a\tb\n1\t2\n", '\t').len(), 2);
        assert_eq!(parse_delimited("\u{feff}a,b\n1,2\n", ',')[0][0], "a");
    }

    #[test]
    fn escaping_is_reversible() {
        assert_eq!(esc("a\tb"), "a\\tb");
        assert_eq!(esc("a\\b"), "a\\\\b");
        assert_eq!(esc("line\nbreak"), "line\\nbreak");
    }
}
