// Tool discovery: how many tool declarations the model is *told about*.
//
// Measured, not guessed: the twelve tool definitions cost ~12 KB of every prompt
// (`scripts/gateway-prompt-shape.mjs` reads the request bodies that actually leave the
// process). On a small local model with a small window that is a fifth of the room spent
// describing capabilities the turn will not use — and a longer menu does not make a small
// model choose better, it makes it choose slower and worse. So the coworker can start with a
// short menu (`Registry::lean_tool_names`) and reach the rest through this tool: it describes
// the job, the registry searches its own list, and the tools it finds are handed to the host
// as callable from the next step on.
//
// The search lives here rather than in the extension for the same reason the schemas live
// here: a TypeScript copy of the ranking is a second policy, and a model that searches two
// registries gets two answers. The words each tool answers to are curated deliberately —
// "Run a shell command" does not contain "terminal", and a model looking for a terminal
// should still find it.
use async_trait::async_trait;
use super::*;
use crate::provider::ToolDef;
use serde_json::json;

/// Search words a tool answers to, beyond its name and description.
///
/// Short on purpose: this is an index, not a second description. A curated list exists
/// because a model does not search in the words the tool author used: it says "total the
/// quantity column", the schema says `sheet_query`.
pub fn discovery_words(name: &str) -> &'static [&'static str] {
    match name {
        "exec" => &[
            "shell", "terminal", "command", "run", "bash", "python", "script", "install", "git", "cargo",
            "npm", "compile",
        ],
        "read_file" => &[
            "open", "view", "read", "text", "file", "cat", "lines", "document", "csv", "json",
            "source", "log",
        ],
        "write_file" => &[
            "save", "create", "overwrite", "file", "text", "new document", "export", "write",
            "replace file",
        ],
        "edit_file" => &[
            "patch", "change", "modify", "find and replace", "update", "fix", "rename", "edit",
            "diff", "in place",
        ],
        "write_xlsx" => &[
            "excel", "spreadsheet", "workbook", "csv", "table", "report", "output file", "xlsx",
            "deliverable",
        ],
        "sheet_read" => &[
            "excel", "spreadsheet", "workbook", "cells", "rows", "grid", "tab", "sheet", "range",
            "open a spreadsheet",
        ],
        "sheet_query" => &[
            "sql", "query", "filter", "sum", "total", "average", "group", "join", "sort",
            "spreadsheet", "count", "pivot", "column",
        ],
        "web_search" => &[
            "search", "internet", "find online", "news", "web", "look up", "reference",
            "current",
        ],
        "web_fetch" => &[
            "open", "url", "page", "read website", "http", "fetch", "html", "article",
        ],
        "download_file" => &[
            "download", "get file", "http", "url", "save file", "attachment", "link", "fetch a file",
        ],
        "recall" => &[
            "memory", "remember", "earlier", "another conversation", "history", "previously said",
            "what did we discuss", "last time",
        ],
        "ask_user" => &[
            "ask", "question", "clarify", "choose", "options", "confirm with the user", "decision",
            "ambiguous", "which one",
        ],
        _ => &[],
    }
}

/// How a tool's risk level reads to a person choosing whether to allow it.
fn risk_says(risk: Risk) -> &'static str {
    match risk {
        Risk::Harmless => "no approval needed",
        Risk::Workspace => "asks first unless you have allowed it in this conversation",
        Risk::Sensitive => "always asks first",
    }
}

/// One discovery card. `body` says whether it carries the argument schema: the model asks
/// for that when it is about to use the tool, and pays for it only then.
fn card(reg: &Registry, d: &ToolDef, body: bool) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), json!(d.name));
    m.insert("what".into(), json!(d.description));
    let risk = reg.get(&d.name).map(|t| t.risk()).unwrap_or_default();
    m.insert("risk".into(), json!(risk_says(risk)));
    if body {
        m.insert("arguments".into(), json!(d.parameters));
    }
    Value::Object(m)
}

/// Score one tool against a query. A name hit beats a curated-word hit, which beats a
/// description hit: "excel" should find `sheet_read` before it finds prose that happens to
/// mention a spreadsheet.
/// Words that carry no capability. Without this list, "the" matches almost every tool's
/// description and a query about memory ranks `exec` and `read_file` above `recall` — which
/// is not a ranking bug, it is a model being told the app cannot do what it asked.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "in", "of", "for", "on", "at", "my", "our", "your", "this", "that",
    "these", "those", "it", "is", "are", "was", "and", "or", "but", "with", "from", "into",
    "me", "you", "we", "they", "he", "she", "i", "its", "his", "her", "about", "some", "any",
    "can", "please", "how", "there", "here", "then", "so", "as", "be", "do", "does", "did",
    "just", "want", "need", "like", "get", "make", "use", "using",
];

fn useful(word: &str) -> bool {
    word.len() > 2 && !STOPWORDS.contains(&word)
}

fn score(name: &str, description: &str, needle: &str, words: &[String]) -> i64 {
    let n = name.to_ascii_lowercase();
    let d = description.to_ascii_lowercase();
    let keys = discovery_words(name);
    // A sum, not a max: a tool that answers four words in the question is a better answer
    // than one that answers a single word very strongly, and a max throws that away.
    let mut best: i64 = 0;
    for w in words {
        if !useful(w) {
            continue;
        }
        if n == *w {
            best = best.max(9);
        } else if n.contains(w.as_str()) {
            best = best.max(7);
        }
        if keys.iter().any(|k| *k == w) {
            best = best.max(8);
        } else if keys.iter().any(|k| k.contains(w.as_str()) || w.contains(*k)) {
            best = best.max(5);
        }
        if d.contains(w.as_str()) {
            best += 4;
        }
    }
    if best == 0 && needle.len() > 6 && d.contains(needle) {
        best = 3;
    }
    best
}

/// Rank the registry against a phrase. An empty query means the whole set, in registry
/// order — which is also how the host can ask "what exists at all".
pub fn discover(reg: &Registry, query: &str, limit: usize, body: bool) -> Vec<Value> {
    let q = query.trim().to_ascii_lowercase();
    let defs = reg.defs();
    let limit = limit.max(1);
    if q.is_empty() {
        return defs.iter().take(limit).map(|d| card(reg, d, body)).collect();
    }
    let words: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| useful(w))
        .map(|w| w.to_string())
        .collect();
    let mut hits: Vec<(i64, usize, Value)> = defs
        .iter()
        .enumerate()
        .map(|(i, d)| (score(&d.name, &d.description, &q, &words), i, d))
        .filter(|(s, _, _)| *s > 0)
        .map(|(s, i, d)| (s, i, card(reg, d, body)))
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.truncate(limit);
    hits.into_iter().map(|(_, _, v)| v).collect()
}

/// The tool that opens the rest of the registry.
///
/// It runs nowhere and touches nothing: it reads the registry this process already has. The
/// `activates` list on its result is the part the *host* acts on — the extension widens the
/// declared tool set with it, so the model is not promised a tool the host has no intention
/// of making callable.
pub struct ExploreTool;

#[async_trait]
impl Tool for ExploreTool {
    fn name(&self) -> &str {
        "explore"
    }

    fn description(&self) -> &str {
        "Look up what this app can do. Describe the job in a few words (\"total a column in a spreadsheet\", \"change one line in a file\", \"open a web page\") and it answers with the closest tools and what each costs to approve. The tools it names become available to you from your next call on. Not every tool is listed to you at the start, so absence from the list is not absence of capability."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "need": { "type": "string", "description": "what you are trying to do, in your own words. Omit to list every tool." },
                "limit": { "type": "integer", "description": "how many tools to return (default 6)" },
                "with_arguments": { "type": "boolean", "description": "include each tool's argument schema - ask for it when you mean to use the tool" }
            },
            "additionalProperties": false
        })
    }

    fn risk(&self) -> Risk {
        // Reading the tool list is not a reach: it changes nothing, and an approval card
        // here would only train a person to click through the one tool that decides what
        // they are approving.
        Risk::Harmless
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &super::ExecSink) -> ToolResult {
        let q = args
            .get("need")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let limit = args
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(6)
            .clamp(1, 12) as usize;
        let body = args
            .get("with_arguments")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // The same construction the host used, from the same config, so a card describes the
        // tool the user actually mounted — search provider and sandbox policy and all.
        let reg = Registry::with_v1_tools_exec(ctx.search_backend.clone(), ctx.exec_cfg.clone());
        let found = discover(&reg, &q, limit, body);
        if found.is_empty() {
            let known: Vec<String> = reg.defs().into_iter().map(|d| d.name).collect();
            return ToolResult::ok(format!(
                "Nothing in the tool set matches {q:?}. The full set is: {}. Try different words, or ask with `need` omitted to list them all.",
                known.join(", ")
            ));
        }
        let mut activates: Vec<String> = found
            .iter()
            .filter_map(|v| v.get("name").and_then(Value::as_str))
            .filter(|n| *n != "explore")
            .map(|n| n.to_string())
            .collect();
        let mut out = if q.is_empty() {
            format!("{} tool(s) in this app:", found.len())
        } else {
            format!("{} tool(s) fit {q:?}:", found.len())
        };
        for f in &found {
            let name = f.get("name").and_then(Value::as_str).unwrap_or("?");
            let what = f.get("what").and_then(Value::as_str).unwrap_or("");
            let risk = f.get("risk").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("\n- {name}: {what}\n  ({risk})"));
            if let Some(a) = f.get("arguments") {
                out.push_str(&format!("\n  arguments: {a}"));
            }
        }
        activates.sort();
        activates.dedup();
        if !activates.is_empty() {
            out.push_str(&format!(
                "\nThese are available from your next call: {}. Call them by the names above.",
                activates.join(", ")
            ));
        }
        out.push_str("\nIf none of these fit, ask again with different words.");
        let mut r = ToolResult::ok(out);
        // The capability claim, carried for the host rather than for the model: the extension
        // makes these callable, the transcript can say the call unlocked them, and the names
        // came out of the registry that owns them.
        r.activates = activates;
        r
    }
}

pub fn install_into(reg: &mut Registry) {
    if reg.get("explore").is_none() {
        reg.register(Box::new(ExploreTool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registry {
        Registry::with_v1_tools()
    }

    #[test]
    fn the_short_menu_is_short_and_names_the_tools_that_open_the_rest() {
        let lean = Registry::lean_tool_names();
        assert_eq!(lean, vec!["explore".to_string(), "recall".to_string()]);
        let r = reg();
        for n in &lean {
            assert!(r.get(n).is_some(), "lean mode declares {n}, which is not registered");
        }
        let hidden = r.discoverable_tool_names();
        assert!(hidden.contains(&"exec".to_string()));
        assert!(hidden.contains(&"ask_user".to_string()));
        assert!(!hidden.contains(&"explore".to_string()));
    }

    #[test]
    fn a_job_described_in_human_words_finds_the_tool_that_does_it() {
        let r = reg();
        // The whole point: the model does not know the name, so it says the job.
        for (need, want) in [
            ("total the quantity column in my spreadsheet", "sheet_query"),
            ("change one line in an existing file", "edit_file"),
            ("open a web page and read it", "web_fetch"),
            ("what did I say about this in another conversation", "recall"),
            ("run a python script", "exec"),
            ("save a report as excel", "write_xlsx"),
            ("ask the user which option they want", "ask_user"),
        ] {
            let found = discover(&r, need, 6, false);
            let names: Vec<&str> = found
                .iter()
                .map(|v| v.get("name").and_then(Value::as_str).unwrap_or(""))
                .collect();
            assert!(
                names.contains(&want),
                "{need:?} found {names:?}, which does not include {want}"
            );
        }
    }

    #[test]
    fn the_first_hit_is_the_tool_that_does_the_thing_not_a_tool_that_mentions_it() {
        let r = reg();
        let found = discover(&r, "spreadsheet", 3, false);
        let first = found[0].get("name").and_then(Value::as_str).unwrap_or("");
        assert!(
            first == "sheet_read" || first == "sheet_query" || first == "write_xlsx",
            "a search for \"spreadsheet\" put {first} first"
        );
    }

    /// The trait's `run` is async and wants a live exec sink; tests borrow one and drop it.
    async fn run(args: Value) -> ToolResult {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ExploreTool.run(args, &ToolCtx::default(), &tx).await
    }

    #[tokio::test]
    async fn nothing_found_says_so_and_shows_what_there_is() {
        let out = run(json!({"need": "tune the guitar"})).await;
        let t = out.text();
        assert!(t.contains("Nothing in the tool set matches"), "{t}");
        assert!(t.contains("read_file"), "the answer should name what does exist: {t}");
        assert!(out.activates.is_empty(), "a failed search unlocks nothing");
    }

    #[test]
    fn an_empty_query_lists_the_set_rather_than_guessing() {
        let r = reg();
        let all = discover(&r, "", 20, false);
        assert_eq!(all.len(), r.defs().len());
    }

    #[tokio::test]
    async fn a_found_tool_is_the_one_the_host_can_make_callable() {
        let out = run(json!({"need": "read a text file"})).await;
        let t = out.text();
        assert!(t.contains("available from your next call"), "{t}");
        assert!(
            out.activates.contains(&"read_file".to_string()),
            "activates={:?} text={t}",
            out.activates
        );
        // `explore` itself is never in the list: it is already declared in every mode, and a
        // host told to activate the tool it is running would be reading its own output back.
        assert!(!out.activates.contains(&"explore".to_string()));
    }

    #[test]
    fn argument_schemas_cost_only_when_asked_for() {
        let r = reg();
        let cheap = discover(&r, "read a file", 3, false);
        let dear = discover(&r, "read a file", 3, true);
        let c = serde_json::to_string(&cheap).unwrap().len();
        let d = serde_json::to_string(&dear).unwrap().len();
        assert!(
            d > c * 2,
            "with_arguments should be the expensive option: {c} B vs {d} B"
        );
    }
}
