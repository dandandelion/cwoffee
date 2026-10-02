//! `write_xlsx` - structured spreadsheet output.
//!
//! Separate from `write_file` on purpose. A spreadsheet is not text with a .xlsx
//! suffix: the value of the format is typed cells, and a model that has to write
//! a CSV blob and hope the conversion is right loses that. Taking `rows` as JSON
//! arrays means a number stays a number and sums work when the user opens the
//! file.
//!
//! rust_xlsxwriter writes the format natively, so this does not shell out to
//! LibreOffice and does not depend on it being installed.

use async_trait::async_trait;
use serde_json::{json, Value};

use rust_xlsxwriter::{Format, Workbook, Worksheet};

use super::path::confine;
use super::{Risk, Tool, ToolCtx, ToolResult};

pub struct WriteXlsxTool;

fn cell(ws: &mut Worksheet, row: u32, col: u16, v: &Value, fmt: Option<&Format>) -> Result<(), String> {
    let r = match v {
        // write_blank takes a format, not an Option, so the unformatted case
        // passes a default Format.
        Value::Null => match fmt {
            Some(f) => ws.write_blank(row, col, f),
            None => ws.write_blank(row, col, &Format::new()),
        },
        Value::Bool(b) => match fmt {
            Some(f) => ws.write_with_format(row, col, *b, f),
            None => ws.write(row, col, *b),
        },
        Value::Number(n) => {
            // Integers stay integers: writing 42 as 42.0 makes Excel show a
            // decimal and warns the user the number is stored as text-ish.
            if let Some(i) = n.as_i64() {
                match fmt {
                    Some(f) => ws.write_with_format(row, col, i, f),
                    None => ws.write(row, col, i),
                }
            } else if let Some(fv) = n.as_f64() {
                match fmt {
                    Some(f) => ws.write_with_format(row, col, fv, f),
                    None => ws.write(row, col, fv),
                }
            } else {
                return Err(format!("number out of range: {n}"));
            }
        }
        Value::String(s) => match fmt {
            Some(f) => ws.write_with_format(row, col, s.as_str(), f),
            None => ws.write(row, col, s.as_str()),
        },
        // A nested array/object has no cell representation. Serialising it to a
        // string keeps the data visible instead of dropping it silently.
        other => ws.write(row, col, other.to_string()),
    };
    r.map_err(|e| format!("xlsx write: {e}"))?;
    Ok(())
}

#[async_trait]
impl Tool for WriteXlsxTool {
    fn name(&self) -> &str {
        "write_xlsx"
    }

    fn description(&self) -> &str {
        "Create an .xlsx spreadsheet in the workspace. Pass sheets, each with rows \
         as arrays of values (numbers stay numbers). Use for tabular deliverables; \
         use write_file for prose."
    }

    fn risk(&self) -> Risk {
        Risk::Mutating
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative .xlsx path" },
                "sheets": {
                    "type": "array",
                    "description": "One entry per sheet",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Sheet name" },
                            "rows": {
                                "type": "array",
                                "description": "Rows of cell values",
                                "items": { "type": "array", "items": true }
                            }
                        },
                        "required": ["rows"]
                    }
                },
                "bold_header": {
                    "type": "boolean",
                    "description": "Bold the first row of each sheet (default true)"
                }
            },
            "required": ["path", "sheets"]
        })
    }

    async fn run(
        &self,
        args: Value,
        ctx: &ToolCtx,
        _sink: &super::ExecSink,
    ) -> ToolResult {
        let Some(raw) = args.get("path").and_then(|p| p.as_str()) else {
            return ToolResult::err("write_xlsx needs a `path` string");
        };
        let path = match confine(&ctx.workspace, raw) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };
        if path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() != Some("xlsx")
        {
            return ToolResult::err("write_xlsx requires a .xlsx extension");
        }
        let Some(sheets) = args.get("sheets").and_then(|s| s.as_array()) else {
            return ToolResult::err("write_xlsx needs `sheets`: an array of {name, rows}");
        };
        if sheets.is_empty() {
            return ToolResult::err("`sheets` must not be empty");
        }
        let bold_header = args
            .get("bold_header")
            .and_then(|b| b.as_bool())
            .unwrap_or(true);

        let mut wb = Workbook::new();
        let header = Format::new().set_bold();
        let mut summary = Vec::new();

        for (i, sheet) in sheets.iter().enumerate() {
            let Some(rows) = sheet.get("rows").and_then(|r| r.as_array()) else {
                return ToolResult::err(format!("sheet {i} needs a `rows` array"));
            };
            let ws = wb.add_worksheet();

            if let Some(name) = sheet.get("name").and_then(|n| n.as_str()) {
                // set_name validates length and illegal characters; surface the
                // model's bad name rather than silently renaming it.
                if let Err(e) = ws.set_name(name) {
                    return ToolResult::err(format!("sheet name {name:?}: {e}"));
                }
            }

            let mut widest: Vec<usize> = Vec::new();
            for (r, row) in rows.iter().enumerate() {
                let r = r as u32;
                // A bare value is treated as a one-cell row rather than an error:
                // small models emit `"rows": ["a","b"]` often enough that
                // refusing would just cost a retry.
                let cols: Vec<&Value> = match row {
                    Value::Array(items) => items.iter().collect(),
                    other => vec![other],
                };
                for (c, v) in cols.iter().enumerate() {
                    let fmt = if bold_header && r == 0 { Some(&header) } else { None };
                    if let Err(e) = cell(ws, r, c as u16, v, fmt) {
                        return ToolResult::err(e);
                    }
                    let text = match v {
                        Value::String(s) => s.len(),
                        other => other.to_string().len(),
                    };
                    if widest.len() < c + 1 {
                        widest.resize(c + 1, 0);
                    }
                    widest[c] = widest[c].max(text);
                }
            }

            // Column widths: rust_xlsxwriter has no autofit that matches how
            // these files are actually read, so size from the content and clamp.
            for (c, w) in widest.iter().enumerate() {
                let width = (*w as f64 + 2.0).clamp(8.0, 60.0);
                let _ = ws.set_column_width(c as u16, width);
            }

            summary.push(format!(
                "{}: {} rows x {} cols",
                ws.name(),
                rows.len(),
                widest.len()
            ));
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult::err(format!("cannot create {}: {e}", parent.display()));
                }
            }
        }

        // save() is sync and does its own IO; the workspace is small so blocking
        // here is cheaper than pulling in a blocking-pool hop for one call.
        match wb.save(&path) {
            Ok(_) => ToolResult::ok(format!(
                "wrote {} ({})",
                super::path::display_rel(&ctx.workspace, &path),
                summary.join("; ")
            )),
            Err(e) => ToolResult::err(format!("cannot write {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Per-test directories. Keying on process::id() collides across parallel
    /// tests in one binary: one test's cleanup deletes another's workspace, which
    /// shows up as a phantom failure in an unrelated case.
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn ctx() -> (ToolCtx, PathBuf) {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let ws = std::env::temp_dir().join(format!("cowry_xlsx_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        (
            ToolCtx {
                workspace: ws.clone(),
                max_output_bytes: 4096,
            },
            ws,
        )
    }

    async fn run(args: Value) -> (ToolResult, PathBuf) {
        let (c, ws) = ctx();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::tools::ExecChunk>();
        let r = WriteXlsxTool.run(args, &c, &tx).await;
        (r, ws)
    }

    #[tokio::test]
    async fn writes_a_readable_workbook_with_typed_cells() {
        let (res, ws) = run(json!({
            "path": "report.xlsx",
            "sheets": [{
                "name": "Q1",
                "rows": [
                    ["region", "units", "rate", "active"],
                    ["north", 42, 1.5, true],
                    ["south", 7, 0.25, false]
                ]
            }]
        }))
        .await;
        assert!(res.ok, "{}", res.text());
        assert!(res.text().contains("3 rows x 4 cols"), "got {}", res.text());

        let bytes = std::fs::read(ws.join("report.xlsx")).unwrap();
        assert_eq!(&bytes[0..2], b"PK");
        let cur = std::io::Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(cur).expect("xlsx is a zip");
        let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
        assert!(
            names.iter().any(|n| n.contains("sheet1.xml")),
            "no sheet part: {names:?}"
        );
        let mut sheet = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("xl/worksheets/sheet1.xml").unwrap(), &mut sheet)
            .unwrap();
        // Typed cells: <v> without t="s" is numeric.
        assert!(sheet.contains("<v>42</v>"), "42 not stored as a number: {sheet}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn rejects_paths_outside_the_workspace() {
        let (res, _) = run(json!({
            "path": "../escape.xlsx",
            "sheets": [{ "rows": [["a"]] }]
        }))
        .await;
        assert!(!res.ok, "escape accepted");
    }

    #[tokio::test]
    async fn rejects_the_wrong_extension_rather_than_renaming() {
        let (res, _) = run(json!({
            "path": "report.csv",
            "sheets": [{ "rows": [["a"]] }]
        }))
        .await;
        assert!(!res.ok && res.text().contains(".xlsx"), "got {}", res.text());
    }

    #[tokio::test]
    async fn missing_sheets_is_an_actionable_error() {
        let (res, _) = run(json!({ "path": "a.xlsx" })).await;
        assert!(!res.ok && res.text().contains("sheets"), "got {}", res.text());
    }

    #[tokio::test]
    async fn bare_values_become_single_cell_rows() {
        let (res, ws) = run(json!({
            "path": "a.xlsx",
            "sheets": [{ "rows": ["one", "two"] }]
        }))
        .await;
        assert!(res.ok, "got {}", res.text());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn nested_values_are_kept_as_text_not_dropped() {
        let (res, ws) = run(json!({
            "path": "a.xlsx",
            "sheets": [{ "rows": [[{ "a": 1 }]] }]
        }))
        .await;
        assert!(res.ok, "got {}", res.text());
        // Search every part rather than assuming where the writer put the
        // string: rust_xlsxwriter may use shared strings or inline strings, and
        // pinning the test to one of them tests the library's layout, not ours.
        let bytes = std::fs::read(ws.join("a.xlsx")).unwrap();
        let cur = std::io::Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(cur).unwrap();
        let mut all = String::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut f, &mut buf).ok();
            all.push_str(&buf);
        }
        assert!(all.contains("{&quot;a&quot;:1}") || all.contains("{\"a\":1}"),
            "nested value lost from the package");
        std::fs::remove_dir_all(&ws).ok();
    }
}
