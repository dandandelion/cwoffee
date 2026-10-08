# Native spreadsheet workflow — plan (temp scratch file)

Written 2026-10-08 before Stage 1 lands. This is a working document, not product
documentation; the durable version moves into `docs/` once the stage it describes
exists.

## Why

Today the engine can *create* `.xlsx` (`write_xlsx`, typed cells) and can read
`.docx`/`.pdf` it generated, but it is **blind to any spreadsheet it did not just
make**: `read_file` hits `looks_binary` and refuses, `edit_file` refuses, and there
is no path at all for the file the user actually brought in. A "cowork" harness that
cannot look at the user's own workbook is not a coworker.

## Non-goals

- No formula **evaluation**. We are not a spreadsheet engine; Excel recalculates.
- No LibreOffice shell-out. `soffice` exists on this box and would guarantee
  fidelity for pivot caches, at 150-300 MB transient RSS per edit, non-deterministic
  output, and a hard external dependency. Against the lightweight premise. It stays
  a candidate for an explicit *user* action, never the tool's implementation.
- No arbitrary format strings. Formatting is a small vocabulary we own.
- Not seven tools. Schemas are prompt bytes, permanently in every request, and a
  quantized local model mis-picks among near-identical tool names.

## Architecture decision: surgical OOXML, not re-serialization

An `.xlsx` is a zip of XML parts. Mutations edit only the parts they understand and
leave every other part **byte-identical**. Re-loading and re-writing the whole
workbook (rust_xlsxwriter cannot load; calamine cannot write; gluing them means
rebuilding from whatever our reader understood) silently drops formulas, number
formats, merged cells, column widths, freeze panes, autofilters, conditional
formatting, data validation, comments, defined names, tables, charts, images and
pivot tables. One such clobbering is worse than not shipping the feature.

The invariant that makes this testable: **hash every zip part before and after a
mutation; every part we did not intend to touch must be identical.** That test is
the feature's licence to exist.

Dependency note: engine already carries `zip` 8 and hand-builds OOXML for `.docx`
(`tools/docs.rs`). Reading needs an XML parser: `quick-xml` 0.42, already in the
workspace lock (via `plist`), pure Rust, SAX-style so a large sheet can be streamed
instead of dommed. No new runtime process anywhere.

## Tool surface (three entries, not seven)

| tool | stage | purpose |
|---|---|---|
| `sheet_read(path, sheet?, range?, mode?, max_cells?, include_formulas?)` | **1** | structure + values, paged |
| `sheet_edit(path, action, …)` | 2-3 | `write_cells`, `insert_rows`, `delete_rows`, `add_sheet`, `rename_sheet`, `delete_sheet`, `freeze`, `column_width` |
| `write_xlsx(…)` | exists | whole-document creation from typed JSON |

Both new tools **append** to the fixed registry order (`tools/mod.rs`) — inserting
or reordering changes prompt bytes and invalidates every existing session's prefix
cache. Adding a tool costs one cold prefill turn per session; unavoidable, cheaper
than versioning the tool set.

## Bounded output, by construction

The endpoint has one slot and a 262k window; an unbounded `read_range` would cost
tens of thousands of prefill tokens and poison the prefix cache. So: default ~2000
cells, hard cap, and a named cursor ("call again with range A41:H80"), plus a file
size ceiling with an error that says what it is.

## Capability inventory, from Stage 1

`sheet_read` in `structure` mode also reports the parts a later mutation must be
careful about — charts, pivots, shared/array formulas, data validation, conditional
formats, merged cells, images — as counts. Read-only today; it is the probe Stage 2
will refuse with, and it lets the model (and the user) see the shape of the file
before anyone asks for an edit.

---

# Stage 1 (this change)

## What lands

1. `crates/engine/src/tools/sheets.rs` — a **read-only** xlsx reader:
   - zip parts: `[Content_Types].xml`, `xl/workbook.xml`, `xl/_rels/workbook.xml.rels`,
     `xl/sharedStrings.xml`, `xl/styles.xml`, `xl/worksheets/sheetN.xml`;
   - cell types: numbers, booleans, errors, shared strings (`t="s"`), inline strings
     (`t="inlineStr"`), formulas (`<f>` + cached `<v>`);
   - dates: `styles.xml` `cellXfs` → `numFmtId` → builtin 14-22/45-47 and custom
     codes containing `y/m/d/h/s`; Excel serial → ISO (1900 epoch, with the 1900
     leap-year bug handled below serial 60). A model reading `45678` learns nothing;
     `2025-01-15` it can use.
   - inventory: sheet names/visibility, `<dimension>`, row/column counts, merged-cell
     count, autofilter, tables (name + ref), defined names, and the risky-part counts.
   - streaming `Bytes` read of the sheet entry, not a DOM, so memory stays flat
     regardless of file size (gates with the PSS watchdog, not asserted).
2. `sheet_read` tool with `mode = structure | values`, `range` (A1:H20 style),
   `max_cells`, `include_formulas`.
3. **CSV/TSV through the same tool**: `.csv`/`.tsv` are tabular too. `sheet_read`
   accepts them, maps columns to letters, pages by rows. One call surface for
   "look at this table", which is what a model actually wants. (`read_file` keeps
   working on them as text; this is the paged, column-aware route.)
4. Risk: `Harmless` for inside-workspace reads (never prompts — prompting on benign
   reads trains users to click through the prompt that matters), `Sensitive` +
   named path for an escape, so the existing permit machinery applies unchanged.
5. Engine tests (fixtures generated in-test, no binary blobs committed):
   - round-trip a workbook written by `rust_xlsxwriter` (values, types, sheet names,
     formula + cached value);
   - a hand-built zip with `sharedStrings.xml` to pin shared-string reading, XML
     entities (`&amp;`, `&#10;`) and self-closing/empty cells;
   - a hand-built zip containing `xl/charts/chart1.xml` + a pivot part → inventory
     counts them;
   - date cells via builtin and custom numFmt; booleans; error cells;
   - range selection, paging cursor, `max_cells`, oversize refusal;
   - CSV/TSV: header row, quoted fields with commas/newlines, ragged rows, range;
   - escape rejection (same `confine_permitted` path as every file tool).

## Deliberately *not* in Stage 1

Any write to an `.xlsx`. Stage 1 has no mutation surface, so it cannot corrupt a
file; that is also why it is first.

## Stages after

- **Stage 2 — safe-subset writes.** `write_cells`, `add_sheet`, `rename_sheet`,
  `delete_sheet`, `freeze`, `column_width` as surgical part edits: sheet XML row/cell
  insertion, **`t="inlineStr"`** for new strings so `sharedStrings.xml` never has to
  be rewritten (no refcount surgery), `<dimension>` kept honest, `calcPr
  fullCalcOnLoad="1"` so Excel recalculates on open, style entries only from the
  vocabulary we own. Capability probe refuses (with a model-correctable reason that
  names the specific part) anything whose references the edit would invalidate.
- **Stage 3 — `insert_rows` / `delete_rows`.** The expensive one: reference
  translation across formula text (relative/absolute, ranges, quoted sheet names),
  `mergeCells`, `conditionalFormatting`, `dataValidation`, `hyperlink`, table `ref`,
  autofilter, defined names, chart series ranges, pivot cache ranges, shared/array
  formulas. `fullCalcOnLoad` recalculates; it does **not** shift references, so a
  wrong translation produces a workbook that recomputes confidently into garbage.
  Implemented only for the subset we can rewrite verifiably; otherwise refuse.
- **Stage 4 — UI.** `changes.rs` line diffs do not describe cell edits. Add a
  structured change record (`Sheet1 · 9 cells written · 3 rows inserted at 12`) as UI
  metadata on the same footing as `files`/`changes` — never in the prompt, which is
  what keeps the prefix cache intact.

## Cost estimate (labels matter: these are estimates)

- +1 engine dependency (`quick-xml` 0.42, already in the lock, pure Rust).
- ~+0.5-1.5 MB release binary (estimate; measured after the build).
- PSS: no new process; streaming reader means sheet size does not become resident
  size. Measured against the 400 MB gate, expected flat around the current 246 MB.
- Prompt bytes: one schema (~400-600 bytes), one cold prefill turn per existing
  session.

## Gates for this change

engine tests (filtered, under `scripts/cargo-guard.sh`), `svelte-check` if the UI
changes, full `./scripts/selftest.sh` against the **scripted endpoint** (never the
user's llama.cpp by default) with a `sheet-read` scenario, then production rebuild:
`render OK`, `PSS <= 400`, gateway smoke, search-smoke.

---

## Stage 1: what actually landed

Shipped as one tool, `sheet_read`, appended last in the fixed tool order (the
`tool_order_is_fixed` test proves the change is a pure suffix over prompt bytes).

Deviations from the plan above, and why:

- **The reader is our own SAX over the zip parts, on `quick-xml`, not
  `calamine`.** calamine would have been less code today and more divergence in
  Stage 2: an editor has to rewrite `sheet1.xml` and its rels in place, so the
  package model the reader builds is the one the editor will touch. It also keeps
  the dependency surface to a pure-Rust SAX with no new system library; the
  release binary is 16.4 MB and PSS did not move (243.1 MB).
- **`mode` defaults differ by file kind**: `.xlsx` defaults to `structure` unless
  `sheet`/`range` is given (an xlsx answer without sheet names, merges and hazards
  is how an edit goes wrong), CSV/TSV default to `values` (one table, no structure
  to discover).
- **`ToolResult.truncated` is the union** of the cell cap and the byte clamp. It
  used to be set only by the byte clamp, so a cell-capped read claimed to be
  complete on the wire.
- **Refusals point at the tool now**: `read_file` on `.xlsx` and `write_file` on
  `.xlsx` both name `sheet_read` / `write_xlsx` instead of only refusing.

Numbers: `tools::sheets` 19 tests; 1200-row × 39,873-byte CSV → 18,526 bytes and
500 rows under the default cap, with the next range named from the last row shown.
`SELFTEST ok` with the new check, and the negative control (`require` armed with
an impossible string) fails the run as it must.

Stage 2 starts at the preservation test, not at the editor: a fixture workbook
with charts, merges, pivot tables, formulas and styles, an edit to one cell, and an
assertion that every untouched part is byte-identical.
