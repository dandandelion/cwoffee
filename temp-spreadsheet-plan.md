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

---

## Stage 1b: the question the reader could not answer (landed)

Stage 1 made the sheet visible. A real session then showed what visibility is
worth without queryability: the model read a 1000-row inventory workbook, could
not total a column, and wrote its own parser — 111 lines of Python that unzip an
.xlsx and parse `sharedStrings.xml`, plus 62 more for the aggregates, after two
calls wasted on `import openpyxl` and on assuming `/tmp` survives between `exec`
calls. It ended at the output cap with a degenerate tail.

What landed:

* **`sheet_query`** (appended after `sheet_read` in the fixed tool order, so the
  prompt diff is a pure suffix): loads the sheet or delimited file into an
  in-memory SQLite table `t` and runs one `SELECT`. `rownum` keeps the sheet's own
  row number; row 1 supplies the column names (`Unit Cost` → `unit_cost`, deduped
  as `unit_cost_2`); blanks are NULL; a column is numeric only if every value
  round-trips through the number, so `01234` stays `TEXT`; results are capped at
  200 rows (1000 max) with a note saying how to page.
* **Read-only as an engine fact**: one statement, `SELECT`/`WITH`/`EXPLAIN` only,
  `SQLITE_LIMIT_ATTACHED = 0`, progress handler with a 5 s wall clock. Each half
  has a negative test, including a cartesian blow-up that must be interrupted.
* **`sheet_read`'s "an edit here would have to preserve" line now counts styles and
  defined names.** It used to say "only the cells" about a workbook with an
  AutoFilter, blue bold headers and a custom `$#,##0.00` format — and the test that
  asserted that phrase was wrong, because a real writer always emits `styles.xml`.
  A hand-built package with no styles is now its own counter-test.
* **Values mode says a format is not a value**: a workbook with custom number
  formats gets `values are the stored numbers, not the formatted text (a money cell
  arrives as 9.72, not $9.72)`.
* **Artifact chips respect deletion**: a file the turn deleted produces no chip.
  Existence is checked when the turn closes; there is no "unproduce" tool result.
* **`exec` says what is ephemeral**: the description now states that each call gets
  a fresh filesystem and `/tmp` does not survive to the next call, so scratch files
  belong in the workspace. That assumption cost the model a call in the session
  above.

Live proof (real endpoint, one probe turn, cleaned up afterwards): three calls —
`sheet_read`, `sheet_query` for the group totals, `sheet_query` for the grand total
— `finish=stop` in 22.5 s, correct numbers (West 57 of 121), zero `exec`, zero
model-authored parser. Numbers and gates: `docs/BENCHMARKS.md`, "the model stopped
writing its own spreadsheet parser".

Cost of the two tools, measured: `sheet_read` 1,159 bytes + `sheet_query` 1,355
bytes, whole tool block 9,459 bytes (`the_tool_block_costs_what_it_says_it_costs`).

## Sampling: the answer, and what shipped

Question: does Cwoffee give the endpoint any sampling config, or leave the server's
defaults alone? Before this batch: nothing was sent at all (`ChatRequest` had two
fields, no code set them), so llama.cpp's own defaults decided — temperature 1.0,
top_p 0.95, top_k 20, min_p 0 on this machine's endpoint.

Now `[sampling]` in `config.toml` passes `temperature`, `top_p`, `top_k`, `min_p` and
`seed` through, and a key that is not written stays out of the request. No defaults
are invented: the temperature-1.0 degeneration in session 1300 is an argument for a
documented knob, not for quietly overriding somebody else's endpoint. The boot line
states both halves — what we send, and what the endpoint says it uses:

```
[cwoffee] sampling: nothing set, the endpoint decides | endpoint defaults: temperature=1 top_p=0.95 top_k=20 min_p=0
```

Defaults are read per key across `default_generation_settings.params` →
`default_generation_settings` → root (builds split them between all three).
Anthropic drops sampling when thinking is on, once-per-process in the log, because
that API rejects the combination without naming the cause.

## Stage 2 unchanged, still the gate for writes

Preservation fixture first (charts, merges, pivots, formulas, styles, tables,
validations, conditional formats), edit one cell, assert every untouched zip part
is byte-identical. Only then does an editing tool exist.

## Stage 1c: the cash-flow workbook, and the cap that cut it off

Session 1305 (`~/Downloads/cashflow`, `BES Cash Flow.xlsx`, 8 sheets, 63 KB) is the
most honest test of the two sheet tools, because nobody scripted it. The model was
asked to work out what the workbook does. Its calls, in order:

| # | call | why it made it |
|---|---|---|
| 1 | `exec ls` | see what is there |
| 2 | `sheet_read` structure | what sheets, what sizes |
| 3-9 | `sheet_read` values x7 | one sheet at a time |
| 10 | `exec` `import openpyxl` | hoping it exists (it does not) |
| 11 | `exec` unzip | give up on the tool |
| 12 | `exec` python XML dump | find merges, formulas |
| 13-14 | `exec` awk x2 | cross-sheet refs, layout |

Fourteen calls. The engine's `max_tool_iters` was 14. The turn did not finish
because the workbook was answered; it finished because the budget ran out and the
engine asked for a summary of unfinished work. Turn telemetry: 293,539 prompt
tokens, 231,728 cached, 3,194 completion.

What the tools could not express, in the model's own order of frustration:

1. **Flow.** "Learn the flow" means which sheet's formulas read which other sheet.
   `sheet_read` said `119 formula(s)`, `1 merged ranges` — counts. The model
   wanted names. It got them by `awk`-ing `mergeCells` and `<f>` out of raw XML.
2. **Layout.** `FINAL CANVASSING` is 29 columns with four blocks side by side. A
   tab grid asks the reader to count empty fields; the model printed
   `A1='First Canvassing' | B1=''` by hand instead.
3. **One call per workbook.** Eight sheets, eight calls, each paying prefill.
4. **Aggregation on a titled sheet.** `sheet_query` was never called in 1305. The
   reason is visible in one line of its old behaviour: the Canteen sheet's header
   is row 3, so `t`'s columns were `burgos_elementary_school | c2 | c3`.

### What shipped

- **`max_tool_iters`** in `config.toml`; absent or `0` means **no cap**. At the
  iteration where the old build stopped, an uncapped turn says once that it is
  uncapped. A cap that is set is honoured exactly (its note names the number).
- **Structure reports names, not just counts**: merge ranges listed
  (`merges: A1:F1`), rows 1-3 previewed instead of row 1, and a workbook-level
  flow graph (`"Local Fund " -> "Canteen" (2 formula reference(s))`).
  External-workbook refs (`[2]Budget!A1`) are counted, not resolved.
- **`sheet: "*"`** sweeps every visible sheet in one call, splitting the cell
  budget evenly and saying so; hidden sheets are skipped with a note.
- **`cell_addresses: true`** prints `1: A1=First Canvassing C1=01/16/2026` for
  wide / multi-block sheets.
- **Header detection in `sheet_query`**: rows are scored (width filled, text not
  numbers, distinct labels, data beneath), the chosen row is printed, and rows
  above it are not records. `header_row` forces one; `header: false` means none.

### Evidence on the real file (`crates/engine/examples/sheet_probe.rs`)

The example drives the tools with no model in the loop, which is how a tool
failure gets distinguished from a model failure:

```
sheet "Local Fund ": A1:V80 (80 rows x 22 cols, 917 non-empty cells) - 1 merged ranges, 119 formula(s) (110 shared...)
  merges: A1:F1
  row 2: Treasurer: | MERLY R. BAUTO | ... | Treasurer: | MERLY R. BAUTO | ...
  row 3: DATE | DESCRIPTION | CATEGORY | INCOME (In) | Expenses (Ex) | Cash Flow | ...
  row 4: 2024-08-09 | Special Education Fund | Starting Balance | 43000 | | 43000 | ...
# flow: no sheet reads from another - every sheet stands alone
```

That last line is a finding, not a failure: raw XML confirms **1** formula in the
whole package contains `!`, and it is a `#DIV/0!` error cell. The workbook's
sheets really are independent; the linkage is that the same layout is repeated,
three ledgers side by side on one sheet. The tool now says that in one call
instead of the model discovering it with `awk`.

The aggregation that 1305 could not do:

```
# header: sheet row 3 -> DATE | DESCRIPTION | CATEGORY | INCOME (In) | Expenses (Ex) | Cash Flow
# note: rows 1-2 sit above the header (titles, names, dates) so they are not rows of t.
# columns: rownum | date | description | category | income_in | expenses_ex | cash_flow
category  entries  income   expenses
NULL      293      76392    76378.75
```

`category` is NULL for all 293 rows because that column really is empty on
Canteen — an honest answer, and the kind the model had to assemble out of 60 rows
of tab-separated text before.

### Still open, deliberately

- **Shared formulas.** Of ~800 `<f>` elements on Canteen, only 198 carry text;
  the rest inherit from a shared master. Flow detection reads masters. The
  structure line states the split rather than overstating coverage.
- **No writes yet.** Stage 2's preservation fixture is still unbuilt.
- **Header detection is a heuristic.** It is reported so it can be argued with,
  and `header_row` is the escape hatch. A future `sheet_query` error could
  suggest the fix when a query names a column like `c2`.
