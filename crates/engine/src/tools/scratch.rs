//! The workspace's scratch folder: `.cws`.
//!
//! Every workspace cwoffee has ever touched ends up with `fetch.py`, `data2.csv`,
//! `tmp_notes.md` and three PDFs sitting in it next to the user's real files. Those
//! files were not deliverables; the model needed them to answer, fetched them for
//! reference, and left them where the user's own files live. That is clutter with a
//! cost: the user has to sort through the model's leftovers to find their own work, and
//! `git status` shows files nobody asked for.
//!
//! So a file the model wants *for itself* goes under `<workspace>/.cws/`, and only a
//! file the person asked for lands in the folder proper. The model makes the call — it
//! is the one that knows whether the person asked — but the default is scratch, because
//! "the model wanted it" is the common case and the expensive mistake is littering a
//! person's folder.
//!
//! The folder is deliberately unremarkable: an ordinary subdirectory of the workspace,
//! so the confinement rules apply to it unchanged, `read_file` reaches it by name, and a
//! user who wants to look inside can just `ls -a`. It carries its own `.gitignore`
//! saying `*` so a repository does not show the model's leftovers as untracked work.

use std::path::{Path, PathBuf};

/// The folder's name. A leading dot keeps it out of the user's `ls` and out of the way
/// of tab-completion for their own files.
pub const SCRATCH_DIR: &str = ".cws";

/// Where a file the tool is about to write should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `.cws/` under the workspace: the model wanted this file, the person did not ask
    /// for it by name. Default.
    Scratch,
    /// The workspace proper, where the person will see it. For the file they asked for.
    Keep,
}

impl Place {
    /// Read the choice out of the tool arguments. Anything that is not an explicit
    /// "keep" is scratch: a tool that cannot tell should not be littering, and a model
    /// that means "the person asked for this" has to say so.
    pub fn of(args: &serde_json::Value) -> Place {
        match args.get("place").and_then(|v| v.as_str()) {
            Some(s) => {
                let s = s.trim().to_ascii_lowercase();
                if s == "keep" || s == "workspace" || s == "here" {
                    Place::Keep
                } else {
                    Place::Scratch
                }
            }
            None => Place::Scratch,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Place::Scratch => "scratch",
            Place::Keep => "workspace",
        }
    }
}

/// True when a workspace-relative path already points at the scratch folder.
pub fn is_scratch_rel(rel: &str) -> bool {
    let t = rel.trim().trim_start_matches("./");
    t == SCRATCH_DIR || t.starts_with(&format!("{SCRATCH_DIR}/"))
}

/// Turn what the model asked for into the path that will actually be confined.
///
/// One function, used both by the risk verdict and by the write itself. If the two
/// resolved the destination separately, an approval card could describe one file while
/// the tool wrote another — which is exactly the kind of drift a permission prompt is
/// supposed to make impossible.
///
/// An absolute path is left alone: it is an explicit instruction, and the confinement
/// layer is the thing that says no to it. Prefixing it with `.cws/` would silently
/// rewrite a request the user was shown.
pub fn destination_for(raw: &str, place: Place) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return match place {
            Place::Scratch => SCRATCH_DIR.to_string(),
            Place::Keep => String::new(),
        };
    }
    if matches!(place, Place::Keep) || Path::new(raw).is_absolute() {
        return raw.to_string();
    }
    let rel = raw.trim_start_matches("./");
    if is_scratch_rel(rel) {
        return rel.to_string();
    }
    format!("{SCRATCH_DIR}/{rel}")
}

/// The workspace's scratch folder, without touching the disk.
pub fn scratch_root(workspace: &Path) -> PathBuf {
    workspace.join(SCRATCH_DIR)
}

/// Create `.cws` and its self-ignoring `.gitignore`.
///
/// The `.gitignore` is the part that makes the folder polite in a repository: without
/// it, every file the model fetches shows up as untracked work-in-progress, and a person
/// working in git starts adding `-u` flags to see their own changes. Written once, left
/// alone afterwards, and `*` (not `!.gitignore`) so the ignore file itself is invisible
/// too: nothing in here is meant to be committed.
pub fn ensure_scratch(workspace: &Path) -> Result<(), String> {
    let dir = scratch_root(workspace);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let ig = dir.join(".gitignore");
    if !ig.exists() {
        if let Err(e) = std::fs::write(&ig, "*\n") {
            // Not fatal: the folder works, the repository is merely noisier. Say so
            // rather than pretending the write succeeded.
            eprintln!("[cwoffee/scratch] cannot write {}: {e}", ig.display());
        }
    }
    Ok(())
}

/// One-line explanation for the model-facing result, so the placement is a stated fact
/// rather than a path the model has to interpret.
pub fn place_note(place: Place, rel: &str) -> String {
    match place {
        Place::Scratch => format!(
            "{rel} is under .cws/ — cwoffee's scratch folder, for files the model fetched \
             for its own use. If the person asked you for this file, move it into the \
             workspace proper or fetch it again with \"place\": \"keep\"."
        ),
        Place::Keep => format!(
            "{rel} is in the workspace proper, where the person will see it. That is right \
             when they asked for the file; something you only needed for reference belongs \
             in .cws/ instead."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_scratch_and_only_an_explicit_keep_lands_in_the_folder() {
        assert_eq!(Place::of(&serde_json::json!({})), Place::Scratch);
        assert_eq!(
            Place::of(&serde_json::json!({"place": "keep"})),
            Place::Keep
        );
        // A word the schema does not offer is not a licence to litter.
        assert_eq!(Place::of(&serde_json::json!({"place": "maybe"})), Place::Scratch);
        assert_eq!(
            Place::of(&serde_json::json!({"place": ""})),
            Place::Scratch,
            "an empty place is no decision, and no decision is scratch"
        );
    }

    #[test]
    fn a_relative_path_moves_under_the_scratch_folder_once() {
        assert_eq!(destination_for("report.pdf", Place::Scratch), ".cws/report.pdf");
        assert_eq!(
            destination_for("data/x.csv", Place::Scratch),
            ".cws/data/x.csv",
            "subdirectories travel with it"
        );
        assert_eq!(
            destination_for(".cws/report.pdf", Place::Scratch),
            ".cws/report.pdf",
            "no .cws/.cws"
        );
        assert_eq!(destination_for("./report.pdf", Place::Scratch), ".cws/report.pdf");
        assert_eq!(destination_for("report.pdf", Place::Keep), "report.pdf");
    }

    #[test]
    fn an_absolute_path_is_left_for_confinement_to_refuse() {
        // Prefixing an absolute path would rewrite a request the person was shown in an
        // approval card, and a rewritten destination is a destination nobody approved.
        assert_eq!(
            destination_for("/etc/passwd", Place::Scratch),
            "/etc/passwd",
            "the path the card showed is the path the tool confines"
        );
        assert!(is_scratch_rel(".cws/x"));
        assert!(!is_scratch_rel("x/.cws/y"));
    }

    #[test]
    fn the_scratch_folder_ignores_itself_in_git() {
        let ws = std::env::temp_dir().join(format!("cw-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        ensure_scratch(&ws).unwrap();
        let dir = scratch_root(&ws);
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), "*\n");
        // Idempotent: a second call does not rewrite or remove anything.
        std::fs::write(dir.join("keep.txt"), "x").unwrap();
        ensure_scratch(&ws).unwrap();
        assert!(dir.join("keep.txt").exists(), "creating the folder must not empty it");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
