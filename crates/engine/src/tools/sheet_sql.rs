//! `sheet_query` - ask a question of a whole sheet instead of copying its cells.
//!
//! This module exists because of a transcript, not a diagram. Asked to describe a
//! 1000-row inventory workbook, the agent used `sheet_read` correctly - structure,
//! then a bounded block of values - and then, because it could total a column only
//! by looking at every row, it gave up on the tool suite and built its own:
//! `write_file` a 111-line Python script that unzips the package, parses
//! `sharedStrings.xml` and `sheet1.xml` by hand, and computes category totals.
//! Two more calls to fix a missing `openpyxl` and an assumed-persistent `/tmp`,
//! and the useful part of the answer cost seven tool calls and ~9k characters of
//! code in the prompt - code the model had to get exactly right, on a 3-bit
//! quant, against a file format it has never seen spelled consistently.
//!
//! The fix is not a better prompt. It is the missing capability: put the sheet in
//! front of a query engine. SQLite is already linked into the process for the
//! session store, so this costs no dependency, no subprocess, and no parser that
//! the model has to write correctly. One call replaces the script:
//!
//! ```text
//! sheet_query("01 Retail Inventory.xlsx",
//!   "SELECT category, COUNT(*) n, SUM(units_sold) units,
//!           SUM(units_sold * retail_price) revenue
//!    FROM t GROUP BY category ORDER BY revenue DESC")
//! ```
//!
//! Boundaries:
//!
//! * **Read-only.** Only one statement, and it must start with `SELECT`, `WITH` or
//!   `EXPLAIN`; `prepare` refuses a second statement; `ATTACH` is capped to zero at
//!   the engine level, so a query cannot reach for a file. The database is a fresh
//!   in-memory one holding only this sheet, so even a confused statement cannot
//!   damage anything the user cares about.
//! * **Bounded, twice.** A ceiling on cells loaded into the table, and a ceiling on
//!   result rows printed, with a note naming how to narrow rather than a dump.
//! * **Wall clock.** A progress handler interrupts a query after a few seconds -
//!   a cartesian product over 1000 rows is one keystroke away.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::types::Value as SqlValue;
use rusqlite::types::ValueRef;
use rusqlite::limits::Limit;
use rusqlite::{params_from_iter, Connection, ErrorCode};
use serde_json::{json, Value};
use zip::ZipArchive;

use super::path::{confine_permitted, display_rel};
use super::sheets::{
    date_styles, esc, parse_delimited, parse_rect, pick_delim, read_call_risk, read_sheet,
    shared_strings, workbook_meta, MAX_COL, MAX_FILE_BYTES, Rect, SheetMeta,
};
use super::{CallRisk, Risk, Tool, ToolCtx, ToolResult};

/// Cells loaded into the temporary table. Above this the sheet is a database and
/// the honest answer is to load part of it with `range`.
const LOAD_CELL_CEILING: usize = 400_000;

/// Result rows printed by default. An aggregate usually returns tens of rows; a
/// `SELECT *` returns thousands, and thousands of rows is a dump with extra steps.
const DEFAULT_MAX_ROWS: usize = 200;
const HARD_MAX_ROWS: usize = 1_000;

/// A query gets this much wall clock before it is interrupted.
const QUERY_BUDGET: Duration = Duration::from_millis(5_000);
/// How often the deadline is checked, in SQLite virtual-machine instructions.
const QUERY_OPS_BETWEEN_CHECKS: i32 = 25_000;

pub struct SheetQueryTool;

#[async_trait]
impl Tool for SheetQueryTool {
    fn name(&self) -> &str {
        "sheet_query"
    }

    fn description(&self) -> &str {
        "Answer a question about a whole spreadsheet or delimited file: totals, counts, \
         groups, filters, top-N. The sheet is loaded as a read-only SQLite table named t \
         (one row per sheet row, plus rownum, the original row number) and query runs one \
         SELECT against it. Column names come from row 1, lower-case with underscores; \
         blanks are NULL and numbers are numbers, so SUM/AVG/GROUP BY work. Read-only: \
         one statement only, and it must start with SELECT, WITH or EXPLAIN. Use this \
         instead of reading the cells and adding them up yourself. Handles .xlsx, .csv, .tsv."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative .xlsx, .csv or .tsv" },
                "query": {
                    "type": "string",
                    "description": "One SELECT (or WITH/EXPLAIN) over table t, SQLite syntax"
                },
                "sheet": {
                    "type": "string",
                    "description": "xlsx only: which sheet becomes t. Default: the first visible sheet"
                },
                "range": {
                    "type": "string",
                    "description": "Load only this rectangle, e.g. A1:F500. Default: the whole used range"
                },
                "header": {
                    "type": "boolean",
                    "description": "Use a header row for the column names (default true). false names every column c1..cN"
                },
                "header_row": {
                    "type": "integer",
                    "description": "Sheet row holding the column names, 1-based. Default: detected - a real workbook opens with a title block, so row 1 is often not the header. Rows above it are titles, not records"
                },
                "max_rows": {
                    "type": "integer",
                    "description": "Result rows to print; default 200, max 1000"
                },
                "delimiter": {
                    "type": "string",
                    "description": "csv/tsv only: one of , ; | or a tab"
                }
            },
            "required": ["path", "query"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    fn risk_for(&self, args: &Value, ctx: &ToolCtx) -> CallRisk {
        read_call_risk(args, ctx)
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &super::ExecSink) -> ToolResult {
        let raw = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("sheet_query: missing required string field \"path\""),
        };
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) => q.trim().to_string(),
            None => {
                return ToolResult::err(
                    "sheet_query: missing required string field \"query\" (one SELECT over table t)",
                )
            }
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
                "sheet_query queries .xlsx, .csv and .tsv; {raw:?} is .{ext}."
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
                "{} is {} MB, above this tool's {} MB ceiling. Query a smaller export, or \
                 use exec with a real database tool.",
                display_rel(&ctx.workspace, &path),
                meta.len() / (1024 * 1024),
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        let rect = match args.get("range").and_then(|v| v.as_str()) {
            Some(spec) if !spec.trim().is_empty() => match parse_rect(spec) {
                Ok(r) => Some(r),
                Err(e) => return ToolResult::err(format!("sheet_query: range {spec:?}: {e}")),
            },
            _ => None,
        };
        let header = match args.get("header_row").and_then(Value::as_u64) {
            Some(n) if n >= 1 => Header::Explicit((n - 1) as usize),
            Some(n) => {
                return ToolResult::err(format!(
                    "sheet_query: header_row {n} is not a sheet row; rows start at 1"
                ))
            }
            None => match args.get("header").and_then(Value::as_bool) {
                Some(false) => Header::None,
                _ => Header::Auto,
            },
        };
        let max_rows = args
            .get("max_rows")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).max(1).min(HARD_MAX_ROWS))
            .unwrap_or(DEFAULT_MAX_ROWS);

        let loaded = match ext.as_str() {
            "xlsx" => match load_xlsx(&path, args.get("sheet").and_then(|s| s.as_str()), rect.as_ref()) {
                Ok(l) => l,
                Err(e) => return ToolResult::err(format!("sheet_query: {e}")),
            },
            _ => {
                let delim = pick_delim(&ext, args.get("delimiter").and_then(|d| d.as_str()));
                match load_delimited(&path, delim, rect.as_ref()) {
                    Ok(l) => l,
                    Err(e) => return ToolResult::err(format!("sheet_query: {e}")),
                }
            }
        };
        if loaded.rows.is_empty() {
            return ToolResult::err(format!(
                "sheet_query: {:?} has no cells{}",
                loaded.sheet,
                if rect.is_some() { " in that range" } else { "" }
            ));
        }

        let rel = display_rel(&ctx.workspace, &path);
        let shape = match Shape::build(&loaded, header) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("sheet_query: {e}")),
        };

        let conn = match Connection::open_in_memory() {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("cannot start the query engine: {e}")),
        };
        // Engine-side, not text-side: with zero attached databases allowed, a query
        // cannot reach outside this throwaway in-memory database at all.
        let _ = conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0);
        if let Err(e) = shape.create_table(&conn) {
            return ToolResult::err(format!("cannot load the sheet into a table: {e}"));
        }
        let rows_loaded = match shape.insert_rows(&conn, &loaded) {
            Ok(n) => n,
            Err(e) => return ToolResult::err(format!("cannot load the sheet into a table: {e}")),
        };

        // One statement, and a read-only verb. rusqlite's `prepare` already rejects
        // a second statement; the verb guard is what makes "read-only" true rather
        // than "the database happens to be disposable".
        let stmt_sql = query.trim().trim_end_matches(';').trim().to_string();
        let verb = stmt_sql
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(verb.as_str(), "select" | "with" | "explain") {
            return ToolResult::err(format!(
                "sheet_query runs read-only queries: the statement has to start with SELECT, \
                 WITH or EXPLAIN (yours starts with {verb:?}). t holds {} rows x {} columns: {}",
                rows_loaded,
                shape.names.len(),
                shape.names.join(", ")
            ));
        }

        // SQLite's contract is the inverse of the intuitive one: the progress
        // callback interrupts when it returns *true*. Getting that backwards is a
        // silent no-op, so it is stated here rather than discovered later.
        let started = Instant::now();
        conn.progress_handler(QUERY_OPS_BETWEEN_CHECKS, Some(move || {
            started.elapsed() >= QUERY_BUDGET
        }));

        let mut stmt = match conn.prepare(&stmt_sql) {
            Ok(s) => s,
            Err(e) => {
                return ToolResult::err(format!(
                    "query failed: {e}\nthe columns of t are: rownum, {}",
                    shape.names.join(", ")
                ))
            }
        };

        let ncols = stmt.column_count();
        let out_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| if s.is_empty() { "?".to_string() } else { s.to_string() })
            .collect();
        let mut rows = match stmt.query([]) {
            Ok(r) => r,
            Err(e) => {
                if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) {
                    return ToolResult::err(format!(
                        "the query ran for {}s and was stopped - it is almost certainly a join \
                         without a condition or a sort over every row. Narrow it (WHERE, LIMIT) \
                         or load fewer columns with range.",
                        QUERY_BUDGET.as_secs()
                    ));
                }
                return ToolResult::err(format!(
                    "query failed: {e}\nthe columns of t are: rownum, {}",
                    shape.names.join(", ")
                ));
            }
        };

        let mut out = String::new();
        out.push_str(&format!(
            "# {rel} [{:?}] -> t: {} data rows x {} columns (rownum = the row's number in the sheet)\n",
            loaded.sheet, rows_loaded, shape.names.len()
        ));
        if !shape.header_note.is_empty() {
            out.push_str(&shape.header_note);
        }
        out.push_str(&format!("# columns: rownum | {}\n", shape.names.join(" | ")));
        if !loaded.note.is_empty() {
            out.push_str(&format!("# {}\n", loaded.note));
        }
        if !shape.blank_names.is_empty() {
            let list: Vec<&str> = shape.blank_names.iter().take(8).map(|s| s.as_str()).collect();
            out.push_str(&format!(
                "# note: {} of the header cells were empty, so those columns are named by position \
                 ({}). If the real header is a different row, pass header_row=N.\n",
                shape.blank_names.len(),
                list.join(", ")
            ));
        }
        out.push_str(&shape.header_nudge);
        out.push_str("# blanks are NULL; columns whose cells are all numbers are numeric, so SUM/AVG work\n");

        let mut shown = 0usize;
        let mut more = false;
        let mut body = String::new();
        body.push_str(&out_names.iter().map(|n| esc(n)).collect::<Vec<_>>().join("\t"));
        body.push('\n');
        loop {
            match rows.next() {
                Ok(Some(r)) => {
                    if shown >= max_rows {
                        more = true;
                        break;
                    }
                    let mut cells = Vec::with_capacity(ncols);
                    for i in 0..ncols {
                        cells.push(cell_text(r, i));
                    }
                    body.push_str(&cells.join("\t"));
                    body.push('\n');
                    shown += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) {
                        return ToolResult::err(format!(
                            "the query ran for {}s and was stopped part-way through printing {} rows. \
                             Add LIMIT {} to the query.",
                            QUERY_BUDGET.as_secs(),
                            shown,
                            max_rows
                        ));
                    }
                    return ToolResult::err(format!("query failed: {e}"));
                }
            }
        }
        out.push_str(&body);
        out.push_str(&format!("# {shown} result row(s)"));
        if more {
            out.push_str(&format!(
                " and more - add LIMIT {max_rows} OFFSET {shown} to page, or narrow the query"
            ));
        }
        out.push('\n');

        let mut res = ToolResult::ok(super::clamp(out, ctx.max_output_bytes).0);
        if more {
            res.truncated = true;
        }
        res
    }
}

/// One sheet, as cell text. The values come from `sheet_read`'s reader, so a date
/// cell arrives as `2025-01-15` and a formula cell arrives as the value Excel last
/// cached - which is what a total has to be computed over.
struct Loaded {
    sheet: String,
    rows: BTreeMap<usize, BTreeMap<usize, String>>,
    min_col: usize,
    max_col: usize,
    note: String,
}

fn load_xlsx(path: &Path, sheet_arg: Option<&str>, rect: Option<&Rect>) -> Result<Loaded, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut zip = ZipArchive::new(file).map_err(|e| format!("not a readable .xlsx package: {e}"))?;
    let (sheets, _defined) = workbook_meta(&mut zip)?;
    if sheets.is_empty() {
        return Err("this workbook declares no sheets".into());
    }
    let picked: &SheetMeta = match sheet_arg {
        None => sheets.iter().find(|s| !s.hidden).unwrap_or(&sheets[0]),
        Some(want) => {
            let w = want.trim();
            sheets
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(w))
                .or_else(|| {
                    sheets
                        .iter()
                        .find(|s| s.name.to_ascii_lowercase().contains(&w.to_ascii_lowercase()))
                })
                .ok_or_else(|| {
                    let all = sheets
                        .iter()
                        .map(|s| format!("{:?}", s.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("there is no sheet {want:?} in this workbook. sheets: {all}")
                })?
        }
    };
    let note = if sheet_arg.is_none() && sheets.len() > 1 {
        format!(
            "this workbook has {} sheets; t holds {:?}. pass sheet to pick another",
            sheets.len(),
            picked.name
        )
    } else {
        String::new()
    };
    let ss = shared_strings(&mut zip);
    let styles = date_styles(&mut zip);
    let content = read_sheet(
        &mut zip,
        &picked.part,
        &ss,
        &styles,
        rect,
        LOAD_CELL_CEILING,
        false,
    )?;
    let (min_col, max_col) = extent_cols(&content.rows);
    Ok(Loaded {
        sheet: picked.name.clone(),
        rows: content.rows,
        min_col,
        max_col,
        note,
    })
}

fn load_delimited(path: &Path, delim: char, rect: Option<&Rect>) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {} as text: {e}", path.display()))?;
    let parsed = parse_delimited(&text, delim);
    let mut rows: BTreeMap<usize, BTreeMap<usize, String>> = BTreeMap::new();
    for (i, fields) in parsed.iter().enumerate() {
        let mut m: BTreeMap<usize, String> = BTreeMap::new();
        for (j, v) in fields.iter().enumerate() {
            if !v.is_empty() {
                m.insert(j, v.clone());
            }
        }
        // A completely empty line is not a row; leaving it in would let a trailing
        // blank line become a header of NULLs.
        if !m.is_empty() {
            if let Some(r) = rect {
                if i < r.r1 || i > r.r2 {
                    continue;
                }
                m.retain(|c, _| *c >= r.c1 && *c <= r.c2);
                if m.is_empty() {
                    continue;
                }
            }
            rows.insert(i, m);
        }
    }
    let (mut min_col, mut max_col) = extent_cols(&rows);
    if let Some(r) = rect {
        min_col = min_col.max(r.c1);
        max_col = max_col.max(r.c2);
    }
    Ok(Loaded {
        sheet: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sheet")
            .to_string(),
        rows,
        min_col,
        max_col,
        note: String::new(),
    })
}

fn extent_cols(rows: &BTreeMap<usize, BTreeMap<usize, String>>) -> (usize, usize) {
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    for m in rows.values() {
        if let (Some(a), Some(b)) = (m.keys().next(), m.keys().next_back()) {
            lo = lo.min(*a);
            hi = hi.max(*b);
        }
    }
    if lo == usize::MAX {
        (0, 0)
    } else {
        (lo, hi.min(MAX_COL - 1))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Integer,
    Real,
    Text,
}

/// How the column names were chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
    /// Pick the row that looks like a header (see `pick_header_row`).
    Auto,
    /// A sheet row the caller named, stored 0-based like `Loaded::rows`.
    Explicit(usize),
    /// No header row: columns are `c1..cN` and every row is data.
    None,
}

/// Which row holds the column names.
///
/// Real spreadsheets open with a title block, not a header: row 1
/// "BURGOS ELEMENTARY SCHOOL", row 2 the treasurer's name, row 3 the actual
/// `DATE | DESCRIPTION | CATEGORY | INCOME | EXPENSES`. Taking the first non-empty
/// row named the columns `burgos_elementary_school`, `c2`, `c3` - which made a tool
/// meant to stop models writing their own parser unusable on precisely the
/// workbooks that need it. So score the early rows: a header is wide, it is text
/// rather than numbers, its names are distinct, and data follows it.
fn pick_header_row(rows: &BTreeMap<usize, BTreeMap<usize, String>>, cols: &[usize]) -> Option<usize> {
    let mut best: Option<(f64, usize)> = None;
    for (idx, (row, cells)) in rows.iter().enumerate() {
        // Only the early rows can be a header; scanning all of them costs nothing
        // but invites a data row full of text to win.
        if idx >= 20 {
            break;
        }
        let filled: Vec<&String> = cols
            .iter()
            .filter_map(|c| cells.get(c))
            .filter(|v| !v.trim().is_empty())
            .collect();
        if filled.len() < 2 {
            continue;
        }
        // A row of pure numbers is a row of a matrix, not a header. Without this,
        // "1,2,3" over "4,5,6" turns 1/2/3 into column names and the note starts
        // telling the model that row 1 looks like a header.
        if !filled.iter().any(|v| v.chars().any(|c| c.is_alphabetic())) {
            continue;
        }
        let ratio = filled.len() as f64 / cols.len().max(1) as f64;
        if ratio < 0.34 {
            continue;
        }
        let numeric = filled
            .iter()
            .filter(|v| v.trim().parse::<f64>().is_ok())
            .count() as f64;
        let texty = 1.0 - numeric / filled.len() as f64;
        let mut seen: Vec<String> = Vec::new();
        for v in &filled {
            let k = v.trim().to_ascii_lowercase();
            if !seen.iter().any(|s| s == &k) {
                seen.push(k);
            }
        }
        let distinct = seen.len() as f64 / filled.len() as f64;
        let follows = rows
            .range((std::ops::Bound::Excluded(*row), std::ops::Bound::Unbounded))
            .next()
            .map(|(_, next)| {
                if cols.iter().any(|c| next.get(c).map(|v| !v.trim().is_empty()).unwrap_or(false)) {
                    1.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        let score = 1.6 * ratio + 1.6 * texty + 0.4 * distinct + 0.5 * follows;
        // Higher wins; on a tie the earlier row does, because a sheet rarely has
        // two plausible headers and the first is the one the author meant.
        if best.map(|(b, _)| score > b + 1e-9).unwrap_or(true) {
            best = Some((score, *row));
        }
    }
    best.map(|(_, r)| r)
}

/// The table shape: one column per sheet column, named from row 1 when there is
/// one. Names are normalised (`Unit Cost` -> `unit_cost`) because a model quoting
/// `"Retail Price"` correctly on every call is a worse bet than a name it can type.
struct Shape {
    names: Vec<String>,
    cols: Vec<usize>,
    kinds: Vec<Kind>,
    /// The row whose cells became the column names. It is not a data row: leaving
    /// it in adds a phantom record, and a `COUNT(*)` that includes the header is
    /// off by one in a way that looks like the file's fault.
    header_row: Option<usize>,
    /// Rows above the header: a title block, not records. They are not loaded, and
    /// the output says so, because silently dropping rows is worse than naming it.
    skipped_above: Vec<usize>,
    /// Column names that came from an empty header cell. Named by position, and the
    /// list has to be the real one: the note used to carry the literal `(c7, c8)`
    /// from an example, which matched nothing and taught the model to distrust it.
    blank_names: Vec<String>,
    header_note: String,
    /// A one-line push back when the caller passed `header:false` and the sheet has
    /// a header the detector can see. Positional columns are workable but the named
    /// form is what makes GROUP BY readable, so it is worth one line.
    header_nudge: String,
}

impl Shape {
    fn build(loaded: &Loaded, header: Header) -> Result<Shape, String> {
        if loaded.max_col + 1 - loaded.min_col > MAX_COL {
            return Err(format!(
                "this sheet spans more than {MAX_COL} columns; use range to pick some"
            ));
        }
        let cols: Vec<usize> = (loaded.min_col..=loaded.max_col).collect();
        if cols.len() > 1024 {
            return Err(format!(
                "this sheet has {} columns, which SQLite will not hold as one table. \
                 Use range to pick a block.",
                cols.len()
            ));
        }
        // Which row names the columns. `Auto` scores the early rows rather than
        // trusting row 1, because row 1 of a real workbook is usually a title.
        let header_row = match header {
            Header::None => None,
            Header::Explicit(r) => {
                if loaded.rows.contains_key(&r) {
                    Some(r)
                } else {
                    return Err(format!(
                        "header_row {}: that sheet row has no cells. first row with content: row {}",
                        r + 1,
                        loaded.rows.keys().next().copied().map(|k| k + 1).unwrap_or(0)
                    ));
                }
            }
            // The fallback covers a single-column list, where the scorer's
            // two-filled-cells rule cannot fire; it is limited to rows with a letter
            // for the same reason.
            Header::Auto => pick_header_row(&loaded.rows, &cols).or_else(|| {
                loaded
                    .rows
                    .iter()
                    .find(|(_, m)| m.values().any(|v| v.chars().any(|c| c.is_alphabetic())))
                    .map(|(r, _)| *r)
            }),
        };
        let mut names = Vec::with_capacity(cols.len());
        let mut seen: Vec<String> = Vec::new();
        let mut blank_names: Vec<String> = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            let raw = header_row
                .and_then(|r| loaded.rows.get(&r))
                .and_then(|m| m.get(c))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let mut name = normalize(&raw, i);
            if raw.is_empty() && header_row.is_some() {
                blank_names.push(name.clone());
            }
            let mut n = 2;
            while seen.iter().any(|s| s == &name) {
                name = format!("{name}_{}", n);
                n += 1;
            }
            seen.push(name.clone());
            names.push(name);
        }
        // Anything above the header is a title or a label, not a record: leaving it
        // in puts "BURGOS ELEMENTARY SCHOOL" in a GROUP BY and adds 2 to COUNT(*).
        let skipped_above: Vec<usize> = header_row
            .map(|h| loaded.rows.keys().copied().filter(|r| *r < h).collect())
            .unwrap_or_default();
        let data: Vec<usize> = loaded
            .rows
            .keys()
            .copied()
            .filter(|r| Some(*r) != header_row && !skipped_above.contains(r))
            .collect();
        let kinds = cols
            .iter()
            .map(|c| sample_kind(&loaded.rows, &data, *c))
            .collect();
        let header_note = match (header, header_row) {
            (Header::Auto, Some(h)) => {
                let label: Vec<String> = cols
                    .iter()
                    .map(|c| {
                        loaded
                            .rows
                            .get(&h)
                            .and_then(|m| m.get(c))
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| "·".into())
                    })
                    .collect();
                let mut note = format!("# header: sheet row {} -> {}\n", h + 1, label.join(" | "));
                let skipped = loaded.rows.keys().copied().filter(|r| *r < h).count();
                if skipped > 0 {
                    note.push_str(&format!(
                        "# note: rows 1-{skipped} sit above the header (titles, names, dates) so they are not rows of t. If they are data, pass header_row=1; if you only want part of the sheet, pass range.\n"
                    ));
                }
                note
            }
            (Header::Auto, None) => String::new(),
            (Header::Explicit(h), _) => format!("# header: sheet row {} as you specified\n", h + 1),
            (Header::None, _) => "# header: none, so columns are c1..cN and every row is data\n".to_string(),
        };
        // `header:false` with a visible header row: the model usually passes it
        // because the automatic header looked wrong once, and then spends the rest of
        // the turn counting rows instead of naming columns. Say which row the detector
        // sees, and how to ask for it.
        let header_nudge = match header {
            Header::None => match pick_header_row(&loaded.rows, &cols) {
                Some(h) => {
                    let mut label: Vec<String> = cols
                        .iter()
                        .take(8)
                        .map(|c| {
                            loaded
                                .rows
                                .get(&h)
                                .and_then(|m| m.get(c))
                                .map(|v| v.trim().to_string())
                                .filter(|v| !v.is_empty())
                                .unwrap_or_else(|| "·".into())
                        })
                        .collect();
                    if cols.len() > 8 {
                        label.push(format!("+{} more", cols.len() - 8));
                    }
                    let line = label.join(" | ");
                    let line = if line.len() > 120 {
                        format!("{}...", &line[..120])
                    } else {
                        line
                    };
                    format!(
                        "# note: header:false, so the columns are c1..cN and row {} is data. \
                         Row {} looks like a header ({}) - drop header:false, or pass \
                         header_row={}, to query the columns by name.\n",
                        h + 1,
                        h + 1,
                        line,
                        h + 1
                    )
                }
                None => String::new(),
            },
            _ => String::new(),
        };
        Ok(Shape {
            names,
            cols,
            kinds,
            header_row,
            skipped_above,
            blank_names,
            header_note,
            header_nudge,
        })
    }

    fn create_table(&self, conn: &Connection) -> rusqlite::Result<()> {
        let cols: Vec<String> = self
            .names
            .iter()
            .zip(self.kinds.iter())
            .map(|(n, k)| format!("\"{n}\" {}", ddl_type(*k)))
            .collect();
        conn.execute_batch(&format!("CREATE TABLE t (rownum INTEGER, {})", cols.join(", ")))
    }

    fn insert_rows(&self, conn: &Connection, loaded: &Loaded) -> rusqlite::Result<usize> {
        let placeholders: Vec<String> = (0..self.names.len() + 1)
            .map(|i| format!("?{}", i + 1))
            .collect();
        let quoted: Vec<String> = self.names.iter().map(|n| format!("\"{n}\"")).collect();
        let sql = format!(
            "INSERT INTO t (rownum, {}) VALUES ({})",
            quoted.join(", "),
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut n = 0usize;
        for (row, cells) in &loaded.rows {
            if Some(*row) == self.header_row || self.skipped_above.contains(row) {
                continue;
            }
            let mut vals: Vec<SqlValue> = Vec::with_capacity(self.names.len() + 1);
            vals.push(SqlValue::Integer(*row as i64 + 1));
            for (c, kind) in self.cols.iter().zip(self.kinds.iter()) {
                let raw = cells.get(c).map(|s| s.as_str()).unwrap_or("");
                vals.push(coerce(raw, *kind));
            }
            if vals[1..].iter().all(|v| matches!(v, SqlValue::Null)) {
                continue;
            }
            stmt.execute(params_from_iter(vals.iter()))?;
            n += 1;
        }
        Ok(n)
    }
}

fn ddl_type(k: Kind) -> &'static str {
    match k {
        Kind::Integer => "INTEGER",
        Kind::Real => "REAL",
        Kind::Text => "TEXT",
    }
}

fn normalize(raw: &str, idx: usize) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    let s = s.trim_matches('_').to_string();
    if s.is_empty() {
        format!("c{}", idx + 1)
    } else if s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        format!("c_{s}")
    } else {
        s
    }
}

/// A column is numeric only if *every* non-empty cell in it parses as a number. A
/// column with one text value in it stays text, which is what preserves a leading
/// zero in a code like `01234` - silently turning that into `1234` is how a data
/// tool becomes a data-corruption tool between the read and the write.
fn sample_kind(rows: &BTreeMap<usize, BTreeMap<usize, String>>, data: &[usize], col: usize) -> Kind {
    let mut kind = Kind::Integer;
    for r in data {
        let v = match rows.get(r).and_then(|m| m.get(&col)) {
            Some(v) => v.as_str(),
            None => continue,
        };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        if let Ok(i) = v.parse::<i64>() {
            // The digits have to round-trip: `01234` parses as 1234, and a column
            // of codes, postcodes or account numbers turned into integers loses the
            // thing that made it an identifier.
            if i.to_string() == v {
                continue;
            }
            return Kind::Text;
        }
        if let Ok(f) = v.parse::<f64>() {
            if f.is_finite() {
                kind = Kind::Real;
                continue;
            }
        }
        return Kind::Text;
    }
    kind
}

fn coerce(raw: &str, kind: Kind) -> SqlValue {
    let t = raw.trim();
    if t.is_empty() {
        return SqlValue::Null;
    }
    match kind {
        Kind::Integer => match t.parse::<i64>() {
            Ok(i) => SqlValue::Integer(i),
            Err(_) => SqlValue::Text(raw.to_string()),
        },
        Kind::Real => match t.parse::<f64>() {
            Ok(f) if f.is_finite() => SqlValue::Real(f),
            _ => SqlValue::Text(raw.to_string()),
        },
        Kind::Text => SqlValue::Text(raw.to_string()),
    }
}

fn cell_text(row: &rusqlite::Row<'_>, i: usize) -> String {
    match row.get_ref(i) {
        Ok(ValueRef::Null) => "NULL".to_string(),
        Ok(ValueRef::Integer(v)) => v.to_string(),
        Ok(ValueRef::Real(v)) => fmt_real(v),
        Ok(ValueRef::Text(t)) => esc(&String::from_utf8_lossy(t)),
        Ok(ValueRef::Blob(_)) => "<binary>".to_string(),
        Err(_) => "?".to_string(),
    }
}

/// Whole numbers print without a decimal point, and six decimals is the display
/// ceiling: `3833172.6300000003` is SQLite being exact about a sum of floats, and
/// repeating that in the answer is noise the model then has to reason over. A
/// caller who wants full precision asks for it with CAST or ROUND.
fn fmt_real(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ExecSink;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;
    use tokio::sync::mpsc;
    use zip::write::FileOptions;
    use zip::CompressionMethod;

    fn tmp(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "cwoffee_sheet_sql_{}_{}_{}",
            std::process::id(),
            tag,
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx_in(d: &Path) -> ToolCtx {
        ToolCtx {
            workspace: d.to_path_buf(),
            max_output_bytes: 64 * 1024,
            ..Default::default()
        }
    }

    fn sink() -> ExecSink {
        mpsc::unbounded_channel().0
    }

    async fn call(ctx: &ToolCtx, args: Value) -> super::super::ToolResult {
        SheetQueryTool.run(args, ctx, &sink()).await
    }

    fn why(r: &super::ToolResult) -> String {
        if r.ok {
            r.output.clone()
        } else {
            r.error.clone().unwrap_or_else(|| "<no error text>".into())
        }
    }

    fn csv(d: &Path, name: &str, body: &str) -> PathBuf {
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn inventory(d: &Path) -> PathBuf {
        csv(
            d,
            "inv.csv",
            "Product ID,Product Name,Category,Unit Cost,Units Sold,Closing Stock,Stock Status\n\
             PRD-1001,Wine Glasses,Home & Kitchen,23.78,118,17,Low Stock\n\
             PRD-1002,Eye Mask,Beauty,15.67,10,76,In Stock\n\
             PRD-1003,Band,Sports,5.59,195,124,In Stock\n\
             PRD-1004,Tray,Home & Kitchen,8.22,8,24,In Stock\n\
             PRD-1005,Soap,Grocery,6.74,6,15,In Stock\n",
        )
    }

    #[tokio::test]
    async fn a_group_by_answers_what_a_dump_of_cells_could_not() {
        let d = tmp("group");
        inventory(&d);
        let ctx = ctx_in(&d);
        let r = call(
            &ctx,
            json!({
                "path": "inv.csv",
                "query": "SELECT category, COUNT(*) AS n, SUM(units_sold) AS units, \
                          SUM(units_sold * unit_cost) AS cost FROM t GROUP BY category ORDER BY cost DESC"
            }),
        )
        .await;
        assert!(r.ok, "{}", why(&r));
        let out = why(&r);
        // Home & Kitchen: 118*23.78 + 8*8.22 = 2806.04 + 65.76 = 2871.80
        assert!(out.contains("Home & Kitchen"), "{out}");
        assert!(out.contains("2871.8"), "sum of a whole column:\n{out}");
        assert!(out.contains("n"), "{out}");
        // The header names the table, so the next call can be written from it.
        assert!(out.contains("t: 5 data rows x 7 columns"), "{out}");
        assert!(out.contains("product_id | product_name | category"), "{out}");
    }

    #[tokio::test]
    async fn numbers_are_numbers_and_a_code_column_keeps_its_zero() {
        let d = tmp("types");
        csv(
            &d,
            "codes.csv",
            "code,qty,note\n01234,3,x\n00007,4,y\n",
        );
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "codes.csv", "query": "SELECT code, qty FROM t ORDER BY rownum"})).await;
        assert!(r.ok, "{}", why(&r));
        let out = why(&r);
        // Every cell of `code` parses as an integer, and it is still text: the
        // leading zero is the data. `qty` has no such tell and is an integer.
        assert!(out.contains("01234"), "leading zero lost:\n{out}");
        assert!(out.contains("\t3"), "{out}");
        let sum = call(
            &ctx,
            json!({"path": "codes.csv", "query": "SELECT SUM(qty) AS total, AVG(qty) AS avg_qty FROM t"}),
        )
        .await;
        assert!(sum.ok, "{}", why(&sum));
        let s = why(&sum);
        assert!(s.contains("7"), "AVG/SUM over a numeric column:\n{s}");
        assert!(s.contains("3.5"), "{s}");
    }

    #[tokio::test]
    async fn rownum_is_the_row_number_in_the_sheet_not_the_position_in_the_result() {
        let d = tmp("rownum");
        inventory(&d);
        let ctx = ctx_in(&d);
        let r = call(
            &ctx,
            json!({"path": "inv.csv", "query": "SELECT rownum, product_name FROM t WHERE closing_stock <= 17 ORDER BY closing_stock"}),
        )
        .await;
        assert!(r.ok, "{}", why(&r));
        let out = why(&r);
        // rows 2 (PRD-1001, 17) and 6 (PRD-1005, 15) in the file, 1-based.
        assert!(out.contains("6\tSoap") && out.contains("2\tWine Glasses"), "{out}");
    }

    #[tokio::test]
    async fn the_result_is_bounded_and_says_how_to_page() {
        let d = tmp("bound");
        let body = std::iter::once("n,name".to_string())
            .chain((1..=500).map(|i| format!("{i},row-{i}")))
            .collect::<Vec<_>>()
            .join("\n");
        csv(&d, "many.csv", &body);
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "many.csv", "query": "SELECT * FROM t"})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(r.truncated, "500 rows printed under a 200-row cap");
        let out = why(&r);
        let printed = out.lines().filter(|l| l.contains("\trow-")).count();
        assert_eq!(printed, 200, "rows printed under a 200-row cap:\n{out}");
        assert!(out.contains("add LIMIT 200 OFFSET 200"), "{out}");
        let paged = call(
            &ctx,
            json!({"path": "many.csv", "query": "SELECT n FROM t ORDER BY n LIMIT 200 OFFSET 200"}),
        )
        .await;
        assert!(paged.ok, "{}", why(&paged));
        assert!(why(&paged).contains("201\n"), "the page the hint names is the page you get:\n{}", why(&paged));
    }

    #[tokio::test]
    async fn writes_and_extra_statements_are_refused_before_they_run() {
        let d = tmp("guard");
        inventory(&d);
        let ctx = ctx_in(&d);
        for q in [
            "UPDATE t SET units_sold = 0",
            "DROP TABLE t",
            "INSERT INTO t VALUES (9,9)",
            "ATTACH '/etc/passwd' AS p",
            "DELETE FROM t",
            "CREATE TABLE u (a)",
        ] {
            let r = call(&ctx, json!({"path": "inv.csv", "query": q})).await;
            assert!(!r.ok, "{q} was not refused");
            let msg = why(&r);
            assert!(msg.contains("SELECT"), "{q}: {msg}");
            assert!(msg.contains("product_name"), "the refusal should name the columns: {msg}");
        }
        let two = call(
            &ctx,
            json!({"path": "inv.csv", "query": "SELECT 1; DROP TABLE t"}),
        )
        .await;
        assert!(!two.ok, "a second statement was accepted");
    }

    #[tokio::test]
    async fn a_bad_column_name_tells_you_what_the_columns_are() {
        let d = tmp("badcol");
        inventory(&d);
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "inv.csv", "query": "SELECT units_sold FROM t GROUP BY cat"})).await;
        assert!(!r.ok);
        let msg = why(&r);
        assert!(msg.contains("no such column: cat"), "{msg}");
        assert!(msg.contains("unit_cost"), "columns should be listed: {msg}");
    }

    #[tokio::test]
    async fn header_false_names_columns_by_position() {
        let d = tmp("noheader");
        csv(&d, "plain.csv", "a,1\nb,2\n");
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "plain.csv", "query": "SELECT c1, c2 FROM t", "header": false})).await;
        assert!(r.ok, "{}", why(&r));
        assert!(why(&r).contains("c1 | c2"), "{}", why(&r));
        assert!(why(&r).contains("2 data rows"), "{}", why(&r));
    }

    #[tokio::test]
    async fn blank_header_cells_are_named_by_position_and_said_so() {
        let d = tmp("blankhead");
        csv(&d, "bh.csv", "Name,,Qty\nThing,2025-01-15,3\n");
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "bh.csv", "query": "SELECT name, c2, qty FROM t"})).await;
        assert!(r.ok, "{}", why(&r));
        let out = why(&r);
        assert!(out.contains("header cells were empty"), "{out}");
        // The note names the columns it actually produced. It used to carry "(c7,
        // c8)" from an example, which matched no column in any file.
        assert!(out.contains("(c2)"), "{out}");
        assert!(!out.contains("c7, c8"), "a hardcoded example leaked into the note: {out}");
        // It must not push the model toward turning headers off, which is what the
        // old wording did and what session 1306 then did for a whole turn.
        assert!(!out.contains("Pass header:false"), "{out}");
        assert!(out.contains("2025-01-15"), "an unlabelled date column should still arrive:\n{out}");

        // With no header row there are no blank header cells, so the note cannot
        // appear alongside "# header: none" - session 1306 printed both at once.
        let r = call(
            &ctx,
            json!({"path": "bh.csv", "header": false, "query": "SELECT COUNT(*) AS n FROM t"}),
        )
        .await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        assert!(out.contains("# header: none"), "{out}");
        assert!(!out.contains("header cells were empty"), "{out}");
    }

    /// One column is the case the scorer's "at least two filled cells" rule cannot
    /// see. The name row must still not become a record: a COUNT over a list of
    /// three cities that answers 4 is the kind of wrong number that survives into a
    /// report.
    #[tokio::test]
    async fn a_single_column_list_keeps_its_name_out_of_the_records() {
        let d = tmp("onecol");
        csv(&d, "cities.csv", "City\nBerlin\nLima\nQuito\n");
        let ctx = ctx_in(&d);
        let r = call(&ctx, json!({"path": "cities.csv", "query": "SELECT COUNT(*) AS n FROM t"})).await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        assert!(out.contains("rownum | city"), "{out}");
        assert!(
            out.trim().ends_with("# 1 result row(s)") && out.contains("\n3"),
            "three cities, not four:\n{out}"
        );
    }

    /// `header:false` is not wrong, but on a sheet with a visible header it costs the
    /// model named columns, and session 1306 paid that for every call of a long turn.
    #[tokio::test]
    async fn header_false_on_a_sheet_with_a_visible_header_is_pushed_back_on_once() {
        let d = tmp("nudged");
        let xlsx = d.join("cashflow.xlsx");
        titled_workbook(&xlsx);
        let ctx = ToolCtx { workspace: d.clone(), ..Default::default() };
        let r = call(
            &ctx,
            json!({"path": "cashflow.xlsx", "header": false, "query": "SELECT COUNT(*) AS n FROM t"}),
        )
        .await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        assert!(out.contains("# header: none"), "{out}");
        assert!(out.contains("looks like a header"), "{out}");
        assert!(out.contains("DATE | DESCRIPTION"), "{out}");
        assert!(out.contains("header_row=3"), "{out}");
        // One line, not a lecture: these bytes are in every result.
        assert_eq!(out.lines().filter(|l| l.starts_with("# note:")).count(), 1, "{out}");

        // A sheet with nothing header-shaped gets no nudge.
        let d2 = tmp("nudged2");
        csv(&d2, "plain.csv", "1,2,3\n4,5,6\n");
        let ctx2 = ctx_in(&d2);
        let r = call(&ctx2, json!({"path": "plain.csv", "header": false, "query": "SELECT COUNT(*) AS n FROM t"})).await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        assert!(!out.contains("looks like a header"), "{out}");
    }

    #[tokio::test]
    async fn range_limits_what_is_loaded() {
        let d = tmp("range");
        csv(&d, "wide.csv", "a,b,c\n1,2,3\n4,5,6\n7,8,9\n");
        let ctx = ctx_in(&d);
        let r = call(
            &ctx,
            json!({"path": "wide.csv", "query": "SELECT a, b FROM t", "range": "A1:B4"}),
        )
        .await;
        assert!(r.ok, "{}", why(&r));
        let out = why(&r);
        assert!(out.contains("2 columns"), "{out}");
        assert!(out.contains("3 data rows"), "{out}");
        let missing = call(
            &ctx,
            json!({"path": "wide.csv", "query": "SELECT c FROM t", "range": "A1:B4"}),
        )
        .await;
        assert!(!missing.ok, "column C was loaded despite the range");
    }

    #[tokio::test]
    async fn an_aggregate_over_xlsx_matches_the_reader_that_showed_the_cells() {
        // Built with the same reader `sheet_read` uses, so a total in the query
        // engine and a value in a values grid cannot disagree about the file.
        let d = tmp("xlsx");
        let xlsx = d.join("hand.xlsx");
        std::fs::write(
            &xlsx,
            b"not really a zip", // replaced below
        )
        .unwrap();
        // Write a real package with the zip crate rather than a fixture blob.
        {
            let f = std::fs::File::create(&xlsx).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts: FileOptions<'_, ()> =
                FileOptions::default().compression_method(CompressionMethod::Stored);
            let wb = "<?xml version=\"1.0\"?><workbook><sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>";
            let rels = "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>";
            let cells = (1..=6)
                .map(|i| {
                    if i == 1 {
                        format!(
                            r#"<row r="1"><c r="A1" t="inlineStr"><is><t>grp</t></is></c><c r="B1" t="inlineStr"><is><t>qty</t></is></c></row>"#
                        )
                    } else {
                        format!(
                            r#"<row r="{i}"><c r="A{i}" t="inlineStr"><is><t>g</t></is></c><c r="B{i}"><v>{}</v></c></row>"#,
                            i * 10
                        )
                    }
                })
                .collect::<String>();
            let sheet = format!("<?xml version=\"1.0\"?><worksheet><sheetData>{cells}</sheetData></worksheet>");
            zw.start_file("xl/workbook.xml", opts).unwrap();
            zw.write_all(wb.as_bytes()).unwrap();
            zw.start_file("xl/_rels/workbook.xml.rels", opts).unwrap();
            zw.write_all(rels.as_bytes()).unwrap();
            zw.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
            zw.write_all(sheet.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        let ctx = ctx_in(&d);
        let r = call(
            &ctx,
            json!({"path": "hand.xlsx", "query": "SELECT COUNT(*) AS n, SUM(qty) AS total FROM t WHERE rownum > 1"}),
        )
        .await;
        assert!(r.ok, "{}", why(&r));
        let out = why(&r);
        // 20+30+40+50+60 = 200
        assert!(out.contains("200"), "{out}");
        assert!(out.contains("5"), "{out}");
        assert!(out.contains("\"Data\""), "{out}");
    }

    #[tokio::test]
    async fn a_query_that_outlives_its_budget_is_stopped_and_explained() {
        let d = tmp("slow");
        let body = std::iter::once("n".to_string())
            .chain((1..=400).map(|i| format!("{i}")))
            .collect::<Vec<_>>()
            .join("\n");
        csv(&d, "slow.csv", &body);
        let ctx = ctx_in(&d);
        let r = call(
            &ctx,
            json!({"path": "slow.csv", "query": "SELECT a.n AS x, b.n AS y FROM t a, t b, t c, t d ORDER BY x"}),
        )
        .await;
        assert!(!r.ok, "a cartesian product over 400 rows ran to completion:\n{}", why(&r));
        assert!(why(&r).contains("stopped"), "{}", why(&r));
    }

    /// Write a small package whose first rows are a **title block**, the way school
    /// and office workbooks actually look: row 1 the institution, row 2 the person
    /// in charge, row 3 the header, rows 4+ the ledger.
    fn titled_workbook(path: &Path) {
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts: FileOptions<'_, ()> =
            FileOptions::default().compression_method(CompressionMethod::Stored);
        let add = |zw: &mut zip::ZipWriter<std::fs::File>, name: &str, body: &str| {
            zw.start_file(name, opts).unwrap();
            use std::io::Write;
            zw.write_all(body.as_bytes()).unwrap();
        };
        add(
            &mut zw,
            "xl/workbook.xml",
            r#"<?xml version="1.0"?><workbook><sheets><sheet name="Local Fund " sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        );
        add(
            &mut zw,
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        );
        let s = |col: &str, r: usize, v: &str| {
            format!(r#"<c r="{col}{r}" t="inlineStr"><is><t>{v}</t></is></c>"#)
        };
        let n = |col: &str, r: usize, v: f64| format!(r#"<c r="{col}{r}"><v>{v}</v></c>"#);
        let sheet = format!(
            r#"<?xml version="1.0"?><worksheet><sheetData>
<row r="1">{a1}</row>
<row r="2">{a2}{b2}</row>
<row r="3">{a3}{b3}{c3}{d3}{e3}</row>
<row r="4">{a4}{b4}{c4}{d4}</row>
<row r="5">{a5}{b5}{c5}{d5}</row>
<row r="6">{a6}{b6}{c6}{d6}</row>
</sheetData></worksheet>"#,
            a1 = s("A", 1, "BURGOS ELEMENTARY SCHOOL"),
            a2 = s("A", 2, "Treasurer:"),
            b2 = s("B", 2, "MERLY R. BAUTO"),
            a3 = s("A", 3, "DATE"),
            b3 = s("B", 3, "DESCRIPTION"),
            c3 = s("C", 3, "CATEGORY"),
            d3 = s("D", 3, "INCOME (In)"),
            e3 = s("E", 3, "Expenses (Ex)"),
            a4 = s("A", 4, "2024-09-06"),
            b4 = s("B", 4, "Starting Balance"),
            c4 = s("C", 4, "Opening"),
            d4 = n("D", 4, 1000.0),
            a5 = s("A", 5, "2024-09-09"),
            b5 = s("B", 5, "Books"),
            c5 = s("C", 5, "MOOE"),
            d5 = n("D", 5, 250.0),
            a6 = s("A", 6, "2024-09-11"),
            b6 = s("B", 6, "Snacks"),
            c6 = s("C", 6, "MOOE"),
            d6 = n("D", 6, 120.0),
        );
        add(&mut zw, "xl/worksheets/sheet1.xml", &sheet);
        zw.finish().unwrap();
    }

    /// The failure that made `sheet_query` useless on a real workbook: its header
    /// was row 3, the detector used row 1, and the column names became
    /// `burgos_elementary_school`, `c2`, `c3` - so the model gave up on the tool and
    /// unzipped the package instead.
    #[tokio::test]
    async fn a_header_below_a_title_block_is_found_and_the_title_is_not_a_record() {
        let d = tmp("titled");
        let xlsx = d.join("cashflow.xlsx");
        titled_workbook(&xlsx);
        let ctx = ToolCtx {
            workspace: d.clone(),
            ..Default::default()
        };
        let r = call(
            &ctx,
            json!({"path": "cashflow.xlsx", "query": "SELECT category, SUM(income_in) AS income, COUNT(*) AS n FROM t GROUP BY category ORDER BY income DESC"}),
        )
        .await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        // The note quotes the sheet's own header text; the column list is the
        // normalised form a query has to type.
        assert!(
            out.contains("# header: sheet row 3 -> DATE | DESCRIPTION | CATEGORY | INCOME (In) | Expenses (Ex)"),
            "the tool must say which row it used: {out}"
        );
        assert!(
            out.contains("# columns: rownum | date | description | category | income_in | expenses_ex"),
            "the queryable names must be shown: {out}"
        );
        assert!(
            out.contains("rows 1-2 sit above the header"),
            "skipping rows must be stated, not silent: {out}"
        );
        // MOOE 370 over two rows, Opening 1000 over one - and no phantom row for
        // the school name, which is what a title row becomes if it is loaded.
        assert!(out.contains("MOOE\t370\t2"), "{out}");
        assert!(out.contains("Opening\t1000\t1"), "{out}");
        assert!(!out.to_lowercase().contains("burgos"), "the title block leaked into the result: {out}");
    }

    #[tokio::test]
    async fn header_row_and_header_false_are_honoured_exactly_as_said() {
        let d = tmp("explicit");
        let xlsx = d.join("cashflow.xlsx");
        titled_workbook(&xlsx);
        let ctx = ToolCtx {
            workspace: d.clone(),
            ..Default::default()
        };

        // Named by the caller: row 1, which is the title. The tool obeys, says it
        // obeyed, and the title becomes the first column's name.
        let r = call(
            &ctx,
            json!({"path": "cashflow.xlsx", "header_row": 1, "query": "SELECT COUNT(*) AS n FROM t"}),
        )
        .await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        assert!(out.contains("as you specified"), "{out}");
        assert!(out.contains("burgos_elementary_school"), "{out}");
        // Every row is data now, and the count says so: 5 rows, not 3.
        assert!(out.contains("5"), "header_row=1 should keep every row: {out}");

        // No header at all: c1..cN and nothing dropped.
        let r = call(
            &ctx,
            json!({"path": "cashflow.xlsx", "header": false, "query": "SELECT COUNT(*) AS n FROM t"}),
        )
        .await;
        let out = why(&r);
        assert!(r.ok, "{out}");
        assert!(out.contains("# header: none"), "{out}");
        assert!(out.contains("c1 | c2 | c3 | c4 | c5"), "{out}");
        // And the tool does not leave it there: it says which row it can see.
        assert!(out.contains("looks like a header"), "{out}");
        assert!(out.contains("6"), "header:false must load all 6 rows: {out}");

        // A row that does not exist is an error with the first real row in it, not a
        // silent fallback to a guess.
        let r = call(
            &ctx,
            json!({"path": "cashflow.xlsx", "header_row": 99, "query": "SELECT 1"}),
        )
        .await;
        let out = why(&r);
        assert!(!r.ok, "header_row 99 must not be accepted quietly");
        assert!(out.contains("row 1"), "{out}");
    }
}
