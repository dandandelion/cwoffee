//! Exporting a conversation as a Markdown document.
//!
//! This lives in the engine rather than the UI for the same reason the workspace
//! rules do: the UI is not the only client, and a "share" button that only behaves
//! in one of them is three behaviours to keep correct. The engine builds the
//! document and hands it back; it does not write it anywhere.
//!
//! An earlier version wrote the file into `<workspace>/exports/` and returned an
//! `OutputFile`, which made a share indistinguishable from model output: a chip in
//! the conversation, a file in the folder the tools work in, and a later turn
//! reading or "cleaning up" a document nobody had asked to create. Where the bytes
//! go is the human's decision — the desktop opens a save dialog, a browser
//! downloads — and the conversation stays free of anything the model can trip over.
//!
//! Reasoning text is deliberately left out. The header says how long the model
//! thought; the text itself is draft prose, often the bulk of the file, and a
//! document you send to someone is not where you want it.

use serde::{Deserialize, Serialize};
use crate::store::StoredMessage;

/// Slug for a filename, from a session title that may be any language, emoji, or
/// 300 characters of a first message. Filenames are not allowed to be the reason
/// an export fails, so this keeps what survives on a plain ext4 filesystem:
/// letters and digits of any script, single underscores between them, hard cap.
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut in_word = false;
    for ch in title.chars() {
        if chars >= 60 {
            break; // a char cap, not a byte cap: truncating bytes can cut a glyph
        }
        if ch.is_alphanumeric() {
            if !in_word {
                if !out.is_empty() && !out.ends_with('_') {
                    out.push('_');
                    chars += 1;
                }
                in_word = true;
            }
            for c in ch.to_lowercase() {
                out.push(c);
                chars += 1;
            }
        } else if in_word {
            out.push('_');
            chars += 1;
            in_word = false;
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "session".to_string()
    } else {
        out
    }
}

fn stamp() -> String {
    // Seconds since the epoch, formatted in UTC by hand: no chrono dependency for
    // one filename, and a fixed-width stamp sorts correctly on its own.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Days since 1970-01-01 -> civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

/// One English line per known tool, mirroring the UI's collapsed tool cards so the
/// document reads the way the conversation looked. An unknown tool keeps its id:
/// inventing a sentence for something we do not know is worse than naming it.
fn tool_line(name: &str, args: &serde_json::Value) -> String {
    let subject = ["path", "file", "url", "query"]
        .iter()
        .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
        .unwrap_or("");
    let head = match name {
        "write_file" => format!("wrote {subject}"),
        "write_xlsx" => format!("wrote {subject}"),
        "edit_file" => format!("edited {subject}"),
        "read_file" => format!("read {subject}"),
        "download_file" => format!("downloaded {subject}"),
        "web_fetch" => format!("fetched {subject}"),
        "web_search" => format!("searched for {subject}"),
        "exec" => "ran a command".to_string(),
        "ask_user" => "asked the user something".to_string(),
        other => format!("called {other}"),
    };
    head
}

/// The document. Structure mirrors the transcript: who said what, in order, with
/// the tool calls that happened between, and nothing that was only ever prompt
/// plumbing (no environment block, no reasoning text, no tool-call ids).
pub fn markdown_document(
    title: &str,
    workspace: &str,
    msgs: &[StoredMessage],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!(
        "_Exported from Cwoffee on {} UTC. Working folder: `{workspace}`._\n\n",
        stamp()
    ));

    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        match m.role.as_str() {
            "user" => {
                out.push_str("## You\n\n");
                out.push_str(m.content.trim());
                out.push_str("\n\n");
                i += 1;
            }
            "assistant" => {
                // One agent turn is often several stored rows - one per tool call -
                // and each of them used to become its own section, so a shared
                // document had `## Cwoffee` five times and "Tools used:" three times
                // for one paragraph of prose. A run of assistant rows is one speaker
                // turn, so it is rendered as one.
                let mut j = i;
                let mut text: Vec<&str> = Vec::new();
                let mut tools: Vec<String> = Vec::new();
                let mut files: Vec<&crate::tools::OutputFile> = Vec::new();
                let mut think_ms = 0u64;
                while j < msgs.len() && msgs[j].role == "assistant" {
                    let r = &msgs[j];
                    if !r.content.trim().is_empty() {
                        text.push(r.content.trim());
                    }
                    if r.reasoning_ms > think_ms {
                        think_ms = r.reasoning_ms;
                    }
                    let calls: Vec<serde_json::Value> =
                        serde_json::from_str(&r.tool_calls_json).unwrap_or_default();
                    for c in calls {
                        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let args: serde_json::Value = c
                            .get("arguments")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        // Arguments arrive as a JSON string in some providers and an
                        // object in others; accept both rather than losing the line.
                        let parsed = match &args {
                            serde_json::Value::String(s) => {
                                serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                            }
                            other => other.clone(),
                        };
                        tools.push(tool_line(name, &parsed));
                    }
                    for f in &r.files {
                        if !files.iter().any(|e| e.path == f.path) {
                            files.push(f);
                        }
                    }
                    j += 1;
                }
                if !text.is_empty() || !tools.is_empty() || !files.is_empty() {
                    out.push_str("## Cwoffee\n\n");
                    for t in &text {
                        out.push_str(t);
                        out.push_str("\n\n");
                    }
                    if think_ms > 0 && !text.is_empty() {
                        out.push_str(&format!(
                            "_<model thought for {}s before answering; that reasoning is not included.>_\n\n",
                            think_ms / 1000
                        ));
                    }
                    if !tools.is_empty() {
                        out.push_str("Tools used:\n\n");
                        for t in tools {
                            out.push_str(&format!("- {t}\n"));
                        }
                        out.push('\n');
                    }
                    if !files.is_empty() {
                        out.push_str("Files produced:\n\n");
                        for f in files {
                            out.push_str(&format!("- `{}`\n", f.path));
                        }
                        out.push('\n');
                    }
                }
                i = j;
            }
            "tool" => {
                // The result of a call is in the assistant row's tool_calls and in
                // the diff/changes; the raw stdout of a command is not worth a
                // section of its own in a document meant for a person.
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}
/// The document behind "Share as Markdown": a suggested file name and the text,
/// written nowhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareDoc {
    /// Suggested file name, safe to pre-fill a save dialog with: the session
    /// title slugged, plus a timestamp so two shares of one conversation are
    /// two files rather than one overwrite.
    pub name: String,
    pub markdown: String,
}

pub fn share_document(title: &str, workspace: &str, msgs: &[StoredMessage]) -> ShareDoc {
    ShareDoc {
        name: format!("{}-{}.md", slug(title), stamp()),
        markdown: markdown_document(title, workspace, msgs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn msg(role: &str, content: &str) -> StoredMessage {
        StoredMessage {
            id: 0,
            session_id: 1,
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn ws(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cwoffee_share_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn slug_survives_titles_that_are_not_filenames() {
        assert_eq!(slug("Q3 Report"), "q3_report");
        assert_eq!(slug("../../etc/passwd"), "etc_passwd");
        assert_eq!(slug("!!!"), "session");
        let weird = slug("café ☕ — naïve ✓");
        assert!(weird.chars().all(|c| c.is_alphanumeric() || c == '_'), "{weird}");
        assert!(slug(&"x".repeat(400)).len() <= 62);
    }

    #[test]
    fn the_document_is_readable_and_hides_plumbing() {
        let mut a = msg("assistant", "Here is the summary.\n\nDone.");
        a.reasoning_ms = 12_000;
        a.tool_calls_json = serde_json::json!([
            { "name": "write_file", "arguments": "{\"path\":\"notes/x.md\"}" }
        ])
        .to_string();
        a.files = vec![crate::tools::OutputFile {
            path: "notes/x.md".into(),
            name: "x.md".into(),
            mime: "text/markdown".into(),
            bytes: 12,
            kind: crate::tools::FileKind::Edit,
        }];
        let doc = markdown_document("Trip plan", "/home/u/ws", &[msg("user", "plan lisbon?"), a]);
        assert!(doc.contains("## You"));
        assert!(doc.contains("plan lisbon?"));
        assert!(doc.contains("Here is the summary."));
        assert!(doc.contains("wrote notes/x.md"), "{doc}");
        assert!(doc.contains("notes/x.md"));
        assert!(doc.contains("thought for 12s"));
        assert!(
            !doc.contains("tool_call_id") && !doc.contains("cwd="),
            "prompt plumbing leaked into the export"
        );
    }

    #[test]
    fn an_empty_assistant_row_that_only_ran_tools_still_shows_up() {
        let mut a = msg("assistant", "");
        a.tool_calls_json =
            serde_json::json!([{ "name": "exec", "arguments": { "cmd": "ls" } }]).to_string();
        let doc = markdown_document("t", "/w", &[a]);
        assert!(doc.contains("ran a command"), "{doc}");
        assert!(!doc.contains("## Cwoffee\n\n\n"));
    }

    #[test]
    fn a_run_of_assistant_rows_is_one_speaker_turn() {
        // One agent turn is stored as one row per tool call. Rendered row by row, a
        // shared document said "Cwoffee" five times and "Tools used:" three times for
        // one paragraph of prose, which is the first thing anyone notices about it.
        let mut mid = msg("assistant", "");
        mid.tool_calls_json = serde_json::json!([
            { "name": "exec", "arguments": { "cmd": "ls" } },
            { "name": "read_file", "arguments": { "path": "notes.md" } }
        ])
        .to_string();
        let mut last = msg("assistant", "done, and here is why");
        last.reasoning_ms = 4200;
        let rows = [msg("user", "have a look"), mid, last];
        let doc = markdown_document("t", "/w", &rows);
        assert_eq!(doc.matches("## Cwoffee").count(), 1, "{doc}");
        assert_eq!(doc.matches("Tools used:").count(), 1, "{doc}");
        assert!(doc.contains("ran a command"), "{doc}");
        assert!(doc.contains("read notes.md"), "{doc}");
        assert!(doc.contains("done, and here is why"), "{doc}");
        assert!(doc.contains("thought for 4s"), "{doc}");
        assert_eq!(doc.matches("thought for").count(), 1, "{doc}");
    }

    #[test]
    fn a_share_name_is_a_plain_file_name_whatever_the_title_is() {
        // The name goes into a save dialog, so a title full of separators must
        // not become a path. There is no workspace to escape into any more.
        let rows = [msg("user", "hi"), msg("assistant", "yo")];
        for title in ["../../etc/passwd", "a/b\\c", "", "  ", "Coffee, tea & the ★ thing"] {
            let d = share_document(title, "/tmp/ws", &rows);
            assert!(!d.name.contains('/'), "name was {:?}", d.name);
            assert!(!d.name.contains('\\'), "name was {:?}", d.name);
            assert!(d.name.ends_with(".md"), "name was {:?}", d.name);
            assert!(d.markdown.starts_with('#'), "document looked like {:?}", &d.markdown[..40.min(d.markdown.len())]);
        }
    }

    #[test]
    fn sharing_writes_nothing_anywhere() {
        // The complaint this answers: a share produced a file in the workspace,
        // which a later turn then treated like its own output. Building a share
        // document must leave the filesystem alone.
        let base = ws("share-leaves-nothing");
        let rows = [msg("user", "hi"), msg("assistant", "yo")];
        let before = std::fs::read_dir(&base).map(|d| d.count()).unwrap_or(0);
        let d = share_document("a conversation", &base.display().to_string(), &rows);
        assert!(d.markdown.contains("yo"));
        let after = std::fs::read_dir(&base).map(|r| r.count()).unwrap_or(0);
        assert_eq!(before, after, "sharing created files in {}", base.display());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The store is the only source: an export has to contain what a reopened
    /// session shows, or "share" quietly exports less than the user can see.
    #[test]
    fn a_share_covers_what_the_store_holds() {
        use crate::provider::Message;
        let store = Store::in_memory().unwrap();
        let sid = store.create_session("Notes", "/tmp", "m").unwrap();
        store.append_message(sid, &Message::user("make a list")).unwrap();
        store
            .append_message(sid, &Message::assistant("here it is"))
            .unwrap();
        let rows = store.messages(sid).unwrap();
        let doc = share_document("Notes", "/tmp", &rows);
        assert!(doc.name.starts_with("notes-"), "name was {:?}", doc.name);
        assert!(doc.markdown.contains("make a list"));
        assert!(doc.markdown.contains("here it is"));
    }
}
