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
//!
//! Inside it lives one folder with the opposite job: `.cws/assistant_attachments/`, the
//! only place a file may be written so that the transcript attaches it. See
//! [`ATTACH_DIR`].

use std::path::{Path, PathBuf};

/// The folder's name. A leading dot keeps it out of the user's `ls` and out of the way
/// of tab-completion for their own files.
pub const SCRATCH_DIR: &str = ".cws";

/// The one folder whose files the transcript is allowed to *attach*.
///
/// The report this answers: a turn printed chips for probe files, `helper.py`s and
/// half-finished dumps — 150 kB of scaffolding presented as deliverables — because the
/// transcript had no way to tell "I wrote this in the workspace while working" from "I am
/// handing you this file". Placement answers it: an attachment is a decision the model
/// makes by writing into this folder, and the transcript attaches what is in it and
/// nothing else.
///
/// It sits under `.cws/` rather than beside it on purpose: the scratch folder is already
/// git-ignored, already outside the person's file listing, and already confined by the
/// same rules, so a hand-over never looks like a change to the person's own tree.
pub const ATTACH_DIR: &str = "assistant_attachments";

/// `.cws/assistant_attachments`, as a workspace-relative prefix.
pub fn attach_prefix() -> String {
    format!("{SCRATCH_DIR}/{ATTACH_DIR}")
}

/// Where a file the tool is about to write should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `.cws/` under the workspace: the model wanted this file, the person did not ask
    /// for it by name. Default for `download_file`.
    Scratch,
    /// The workspace proper, where the person will see it. For the file they asked for.
    Keep,
    /// `.cws/assistant_attachments/`: the model is handing this file to the person as an
    /// attachment, and this is the only place the transcript attaches from.
    Attach,
}

impl Place {
    /// Read the choice out of the tool arguments. Anything that is not an explicit
    /// "keep" is scratch: a tool that cannot tell should not be littering, and a model
    /// that means "the person asked for this" has to say so.
    pub fn of(args: &serde_json::Value) -> Place {
        match args.get("place").and_then(|v| v.as_str()) {
            Some(s) => {
                let s = s.trim().to_ascii_lowercase();
                if Self::is_attach_word(&s) {
                    Place::Attach
                } else if s == "keep" || s == "workspace" || s == "here" {
                    Place::Keep
                } else {
                    Place::Scratch
                }
            }
            None => Place::Scratch,
        }
    }

    /// The same word read for a tool whose path is already the person's folder —
    /// `write_file`, `write_xlsx`, `edit_file`. Their default stays where the path says,
    /// because rewriting a normal write into `.cws/` would move files the person named.
    /// Only an explicit "attach" redirects, and it redirects to one place.
    pub fn of_write(args: &serde_json::Value) -> Place {
        match args.get("place").and_then(|v| v.as_str()) {
            Some(s) if Self::is_attach_word(s.trim()) => Place::Attach,
            _ => Place::Keep,
        }
    }

    fn is_attach_word(s: &str) -> bool {
        s == "attach" || s == "attachment" || s == "attached" || s == "deliverable"
    }

    pub fn label(self) -> &'static str {
        match self {
            Place::Scratch => "scratch",
            Place::Keep => "workspace",
            Place::Attach => "attachment",
        }
    }
}

/// True when a workspace-relative path already points at the scratch folder.
pub fn is_scratch_rel(rel: &str) -> bool {
    let t = rel.trim().trim_start_matches("./");
    t == SCRATCH_DIR || t.starts_with(&format!("{SCRATCH_DIR}/"))
}

/// True when a workspace-relative path already points at the attachments folder.
pub fn is_attach_rel(rel: &str) -> bool {
    let t = rel.trim().trim_start_matches("./");
    let pre = attach_prefix();
    t == pre || t.starts_with(&format!("{pre}/"))
}

/// Turn what the model asked for into the path that will actually be confined.
///
/// One function, used both by the risk verdict and by the write itself. If the two
/// resolved the destination separately, an approval card could describe one file while
/// the tool wrote another — which is exactly the kind of drift a permission prompt is
/// supposed to make impossible.
///
/// An absolute path is left alone for `Keep` and `Scratch`: it is an explicit instruction, and
/// the confinement layer is the thing that says no to it. Prefixing it with `.cws/` would
/// silently rewrite a request the user was shown.
///
/// **`Attach` is the exception, and it is the folder.** `Place::Attach` is a promise about where
/// the bytes land — `.cws/assistant_attachments/`, the one place the transcript hands a file over
/// — so a destination that leaves that folder while still carrying the promise is the bug this
/// function used to allow: `download_file` with `place: "attach"` and an absolute or workspace-root
/// `path` wrote `<ws>/report.pdf`, got labelled `FileKind::Attachment` from the *place*, and the
/// transcript drew the hand-over container with a Save control on a file nobody had agreed to put
/// in the hand-over folder. An absolute `path` with `place: "attach"` now keeps its file name and
/// moves into the folder, and `place_note` says it moved.
pub fn destination_for(raw: &str, place: Place) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return match place {
            Place::Scratch => SCRATCH_DIR.to_string(),
            Place::Attach => attach_prefix(),
            Place::Keep => String::new(),
        };
    }
    if matches!(place, Place::Attach) && Path::new(raw).is_absolute() {
        // Only the name travels. An absolute path's directory part is somebody else's
        // arrangement — `/tmp/dl/x.pdf` is not a folder the person chose to hand over from.
        let name = Path::new(raw)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.to_string());
        return format!("{}/{name}", attach_prefix());
    }
    // `Keep` and `Scratch` still hand an absolute path to the confinement layer untouched:
    // rewriting a destination the person was shown in an approval card is the thing that prompt
    // exists to prevent. `Attach` is the one placement whose folder *is* the decision, so it is
    // the one that re-places instead of trusting the argument.
    if matches!(place, Place::Keep) || Path::new(raw).is_absolute() {
        return raw.to_string();
    }
    let rel = raw.trim_start_matches("./");
    // An attachment asked for by name (`"path": ".cws/assistant_attachments/x.html"`)
    // is the same destination as one asked for by place, so it must not be prefixed
    // twice. Checked first: `.cws/assistant_attachments/x` is also a scratch path.
    if is_attach_rel(rel) {
        return rel.to_string();
    }
    if matches!(place, Place::Attach) {
        // Already inside the folder (a path typed out in full) stays where it is; anything
        // else — the workspace proper, another scratch path, a bare name — lands in the
        // folder. There is no branch of this function that returns `Attach` with a
        // destination outside it, which is what makes "the transcript attaches from the
        // folder alone" a fact of the write rather than of the argument.
        return if is_attach_rel(rel) {
            rel.to_string()
        } else {
            format!("{}/{rel}", attach_prefix())
        };
    }
    if is_scratch_rel(rel) {
        return rel.to_string();
    }
    format!("{SCRATCH_DIR}/{rel}")
}

/// The workspace's scratch folder, without touching the disk.
pub fn scratch_root(workspace: &Path) -> PathBuf {
    workspace.join(SCRATCH_DIR)
}

/// The attachments folder, without touching the disk.
pub fn attach_root(workspace: &Path) -> PathBuf {
    workspace.join(attach_prefix())
}

/// The largest attachment the gateway is willing to hand over as one JSON reply.
///
/// Base64 grows a payload by a third, and a command reply is one JSON string the
/// client parses in one go. Beyond this the honest answer is "open it from the
/// conversation's folder" rather than a browser tab that dies parsing.
pub const MAX_INLINE_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// An attachment small enough to travel inside a command reply.
#[derive(Debug, Clone)]
pub struct InlineAttachment {
    pub name: String,
    pub mime: &'static str,
    pub bytes: u64,
    /// Standard alphabet, padded.
    pub b64: String,
}

/// Whether an attachment fits the reply, and if not, how big it is.
///
/// The size travels because the answer has to be actionable: "too big" is a dead end,
/// "this file is 12 MB and sits at .cws/assistant_attachments/x.zip in that
/// conversation's folder, which you can open in your own file manager" is a way forward.
#[derive(Debug, Clone)]
pub enum InlineFit {
    Inlined(InlineAttachment),
    TooBig { bytes: u64, limit: u64 },
}

/// Read an attachment for a client that cannot reach the disk — a phone, a browser tab
/// on the gateway. The desktop does not need this: it copies the file straight to
/// wherever the person said (see `save_attachment_as` in the app shell).
pub fn inline_attachment(workspace: &Path, rel: &str) -> Result<InlineFit, String> {
    inline_attachment_limited(workspace, rel, MAX_INLINE_ATTACHMENT_BYTES)
}

/// The same, with the ceiling as an argument so the size rule is testable without
/// writing 8 MB into a temp directory.
pub fn inline_attachment_limited(workspace: &Path, rel: &str, limit: u64) -> Result<InlineFit, String> {
    let full = attachment_path(workspace, rel)?;
    let len = std::fs::metadata(&full).map_err(|e| format!("{rel}: {e}"))?.len();
    if len > limit {
        return Ok(InlineFit::TooBig { bytes: len, limit });
    }
    let bytes = std::fs::read(&full).map_err(|e| format!("cannot read {rel}: {e}"))?;
    Ok(InlineFit::Inlined(InlineAttachment {
        name: full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.to_string()),
        mime: super::mime_for(&full),
        bytes: len,
        b64: base64(&bytes),
    }))
}

/// A size a person can read. Used in the one error that needs it — "this attachment is
/// too big for the network route" — where "12582912 bytes" is the kind of number that
/// makes someone assume the app is broken rather than tell them what to do.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 1;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    // One decimal only when there is a fraction to say. "8.0 MB" in a sentence about a
    // file a person cannot download reads like a machine talking; "8 MB" reads like a
    // person telling them what to do next.
    let num = if v.fract() == 0.0 { format!("{:.0}", v) } else { format!("{:.1}", v) };
    format!("{num} {}", UNITS[i])
}

/// Standard-alphabet base64 with padding.
///
/// Written here rather than taken from a crate for one reply: `base64` sits in the
/// lockfile only as somebody else's transitive dependency, and making that a direct
/// dependency of the gateway ties our wire format to whichever version they happen to
/// use. The alphabet is 64 characters and the tests are three known answers.
pub fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Resolve the file behind an attachment control, against the folder of the
/// conversation that made it.
///
/// Both rules live here rather than in a front end, because the path arrives from a
/// tool result the model wrote. A client that checked nothing would let a turn offer
/// `~/.ssh/id_rsa` as an "attachment", and the answer to that is not a client that
/// behaves; it is a host that refuses.
///
/// `rel` must be the workspace-relative path of a file under
/// [`.cws/assistant_attachments`](ATTACH_DIR) — not absolute, and not a traversal.
pub fn attachment_path(workspace: &Path, rel: &str) -> Result<PathBuf, String> {
    let clean = rel.trim().trim_start_matches("./");
    if clean.is_empty() || clean.contains('\0') {
        return Err("no attachment path given".to_string());
    }
    if Path::new(clean).is_absolute() || !is_attach_rel(clean) {
        return Err(format!(
            "{rel} is not under {}/ — only a file the assistant wrote there can be handed \
             over from the transcript",
            attach_prefix()
        ));
    }
    let full = super::path::confine(workspace, clean).map_err(|e| e.to_string())?;
    let md = std::fs::metadata(&full).map_err(|_| missing_note(rel, workspace, None))?;
    if !md.is_file() {
        return Err(format!("{rel} is not a file"));
    }
    Ok(full)
}

/// Find a turn's file, and say what was searched when it is not there.
///
/// A transcript row is a *record*: the tool wrote the file into the folder the
/// conversation lived in at the time. The folder the host is working in now can be a
/// different one — the app restored this conversation from another workspace, or you are
/// reading an old conversation while a new one is open — and resolving the record against
/// the current folder then fails on a file that is sitting, unmodified, in the folder the
/// conversation belongs to. `save_attachment_as` already resolves through the row for
/// exactly this reason; a click that resolves through the host is the same bug with a
/// friendlier name, and its old answer was a lie: `{rel} no longer exists`.
///
/// So the conversation's own folder is tried first, the host's folder second (a session
/// moved between folders legitimately, and the file moved with it), and a miss names every
/// folder searched instead of claiming anything about the file's history.
pub fn turn_file_path(
    conversation: Option<&Path>,
    host: &Path,
    rel: &str,
) -> Result<PathBuf, String> {
    let clean = rel.trim().trim_start_matches("./");
    if clean.is_empty() || clean.contains('\0') || Path::new(clean).is_absolute() {
        return Err(if clean.is_empty() {
            "no file path given".to_string()
        } else {
            format!("{rel} is not a workspace-relative path")
        });
    }
    let mut searched: Vec<PathBuf> = Vec::new();
    for ws in [conversation, Some(host)].into_iter().flatten() {
        if searched.iter().any(|p| p == ws) {
            continue;
        }
        searched.push(ws.to_path_buf());
        // `confine` is the whole containment rule: a traversal in a model-authored path
        // resolves to nothing here rather than to a file outside the folder.
        if let Ok(full) = super::path::confine(ws, clean) {
            if full.is_file() {
                return Ok(full);
            }
        }
    }
    Err(missing_note(rel, &searched[0], searched.get(1).cloned()))
}

/// The sentence for a record that points at bytes nobody can find.
///
/// It says which folders were searched, in the order they were searched, and it stops at
/// that. "No longer exists" was the old wording and it was not supportable: the only thing
/// this function knows is that the folders it was given do not hold the file. The file may
/// be in a third folder, the conversation may have been moved, or the record may be older
/// than the disk — the reader's next step is a folder list, not a story about deletion.
fn missing_note(rel: &str, first: &Path, second: Option<PathBuf>) -> String {
    let folders = match second {
        Some(b) => format!("{} or {}", first.display(), b.display()),
        None => format!("{}", first.display()),
    };
    format!(
        "{rel} is not in {folders} — this conversation's record says a turn wrote it, and the          folders that record could point at do not hold it. It was moved, deleted, or written          while the conversation lived in another folder."
    )
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
        Place::Attach => format!(
            "{rel} is in .cws/assistant_attachments/ — the transcript attaches it to this \n             turn, and the person chooses where to save it. That is the right place for a file \n             you are handing over. Something you only needed for yourself belongs in .cws/, and a \n             file they asked to have in their own folder belongs there. A `path` outside that \n             folder is moved into it when `place` is `attach`: the folder is the decision to \n             hand the file over, so the placement cannot be a label on a file that stayed elsewhere."
        ),
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

    /// A transcript row is a record of where a turn wrote something, and the folder the host is
    /// working in now is not always that folder. Resolving the record against the current one is
    /// how a click on a file that exists answers `no longer exists`.
    #[test]
    fn a_turn_file_is_found_in_the_folder_its_conversation_lives_in() {
        let root = std::env::temp_dir().join(format!("cw-turnfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let conv = root.join("conversation-folder");
        let host = root.join("host-folder");
        std::fs::create_dir_all(conv.join(".cws/assistant_attachments")).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(conv.join("three-feature-prompt-3.md"), b"# Task\n").unwrap();

        // The conversation's own folder wins, even though the host's folder is the one the
        // process is working in.
        let got = turn_file_path(Some(&conv), &host, "three-feature-prompt-3.md").unwrap();
        assert_eq!(got, conv.join("three-feature-prompt-3.md"), "{got:?}");

        // A file that only the host folder holds is still found — a conversation can move
        // between folders and take its files with it.
        std::fs::write(host.join("moved.md"), b"moved\n").unwrap();
        let got = turn_file_path(Some(&conv), &host, "moved.md").unwrap();
        assert_eq!(got, host.join("moved.md"), "{got:?}");

        // Both folders are the same place: searched once, and the refusal says one folder.
        let got = turn_file_path(Some(&conv), &conv, "three-feature-prompt-3.md").unwrap();
        assert_eq!(got, conv.join("three-feature-prompt-3.md"));

        // Nothing found: the sentence names every folder it looked in, and makes no claim about
        // whether the file still exists anywhere.
        let err = turn_file_path(Some(&conv), &host, "gone.md").unwrap_err();
        assert!(err.contains(&conv.display().to_string()), "must name the conversation folder: {err}");
        assert!(err.contains(&host.display().to_string()), "must name the host folder: {err}");
        assert!(!err.contains("no longer exists"), "that claim is not what this knows: {err}");
        let one = turn_file_path(None, &host, "gone.md").unwrap_err();
        assert!(one.contains(&host.display().to_string()) && !one.contains(&conv.display().to_string()), "{one}");
        assert!(!one.contains("no longer exists"), "{one}");

        // Containment still holds: a record cannot point outside the folder it resolved in, and
        // an absolute path in a tool result is not a workspace-relative one.
        assert!(turn_file_path(Some(&conv), &host, "../../../etc/hostname").is_err());
        assert!(turn_file_path(Some(&conv), &host, "/etc/hostname").is_err());
        assert!(turn_file_path(Some(&conv), &host, "   ").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The hand-over rule and the record rule disagree on purpose: `attachment_path` refuses
    /// anything outside `.cws/assistant_attachments/` before it looks at the disk, because that
    /// control offers to copy a file out, while a click only opens what the record names.
    #[test]
    fn a_missing_hand_over_says_which_folder_it_searched() {
        let root = std::env::temp_dir().join(format!("cw-attach-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".cws/assistant_attachments")).unwrap();
        let err =
            attachment_path(&root, ".cws/assistant_attachments/hand.xlsx").unwrap_err();
        assert!(err.contains(&root.display().to_string()), "names the folder searched: {err}");
        assert!(!err.contains("no longer exists"), "{err}");
        // Still refused before the disk for a path that is not in the folder at all.
        let refused = attachment_path(&root, "three-feature-prompt-3.md").unwrap_err();
        assert!(refused.contains("is not under .cws/assistant_attachments"), "{refused}");
        let _ = std::fs::remove_dir_all(&root);
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
    fn an_attachment_is_placed_in_one_folder_and_only_that_folder() {
        assert_eq!(Place::of(&serde_json::json!({ "place": "attach" })), Place::Attach);
        assert_eq!(
            Place::of(&serde_json::json!({ "place": "attachment" })),
            Place::Attach,
            "the word the schema advertises and the word the model reaches for are both accepted"
        );
        assert_eq!(
            destination_for("report.html", Place::Attach),
            ".cws/assistant_attachments/report.html"
        );
        assert_eq!(
            destination_for(".cws/assistant_attachments/report.html", Place::Attach),
            ".cws/assistant_attachments/report.html",
            "no doubled prefix when the model names the folder itself"
        );
        assert_eq!(
            destination_for("tables/q2.xlsx", Place::Attach),
            ".cws/assistant_attachments/tables/q2.xlsx",
            "subfolders travel with it"
        );
        // `write_file` keeps its own default: its path is the person's folder unless the
        // call says otherwise, because rewriting a named write into .cws/ would move a
        // file the person spelled out.
        assert_eq!(Place::of_write(&serde_json::json!({})), Place::Keep);
        assert_eq!(Place::of_write(&serde_json::json!({ "place": "attach" })), Place::Attach);
        assert_eq!(
            Place::of_write(&serde_json::json!({ "place": "keep" })),
            Place::Keep,
            "an unknown place on a write is no decision, and no decision leaves the path alone"
        );
        assert!(is_attach_rel(".cws/assistant_attachments/x"));
        assert!(!is_attach_rel(".cws/x"));
        assert!(!is_attach_rel("src/assistant_attachments/x"));
        assert_eq!(attach_prefix(), ".cws/assistant_attachments");
    }

    /// `Place::Attach` is a promise about a folder, not a label on a result. Every branch of
    /// `destination_for` with `Attach` lands inside `.cws/assistant_attachments/`, including the
    /// ones that used to hand back a workspace-root or absolute destination — which is how a file
    /// nobody agreed to hand over got a Save control in the transcript.
    #[test]
    fn an_attach_destination_never_lands_outside_the_hand_over_folder() {
        for raw in [
            "report.pdf",
            "./report.pdf",
            "sub/report.pdf",
            "/tmp/dl/report.pdf",
            "/etc/passwd",
            ".cws/report.pdf",
            "docs/x.html",
            "",
        ] {
            let got = destination_for(raw, Place::Attach);
            assert!(is_attach_rel(&got), "`{raw}` with place=attach landed at {got}");
            assert!(!got.contains("//"), "double separator: {got}");
        }
        // The name survives; somebody else's directory part does not.
        assert_eq!(destination_for("/tmp/dl/report.pdf", Place::Attach), ".cws/assistant_attachments/report.pdf");
        // A path already in the folder is not prefixed twice.
        assert_eq!(
            destination_for(".cws/assistant_attachments/x.html", Place::Attach),
            ".cws/assistant_attachments/x.html"
        );
        // `Keep` and `Scratch` still show the person the path they were shown, so the approval
        // card and the write agree; confinement is what says no.
        assert_eq!(destination_for("/etc/passwd", Place::Scratch), "/etc/passwd");
        assert_eq!(destination_for("/etc/passwd", Place::Keep), "/etc/passwd");
        // And the note says the folder is the decision, so a model that asked for a root path
        // with place=attach learns where the bytes actually went.
        assert!(place_note(Place::Attach, ".cws/assistant_attachments/report.pdf").contains("moved into it"));
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
    fn a_size_that_fits_in_a_sentence() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(12 * 1024 * 1024), "12 MB");
        assert_eq!(human_bytes(MAX_INLINE_ATTACHMENT_BYTES), "8 MB");
    }

    #[test]
    fn base64_matches_the_answers_everyother_decoder_already_agrees_on() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The two bytes that need padding in the middle of a stream, and the byte that
        // tests the high bit of the alphabet's last index.
        assert_eq!(base64(&[0xff, 0xef, 0x00]), "/+8A");
        // Round trip through the decoder the browser will use: `atob` in the UI, so the
        // check is that the alphabet and the padding are the standard ones.
        assert_eq!(base64(&[0, 1, 2, 253, 254, 255]), "AAEC/f7/");
    }

    #[test]
    fn an_attachment_can_only_be_read_out_of_the_attachments_folder() {
        let ws = std::env::temp_dir().join(format!("cw-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        ensure_scratch(&ws).unwrap();
        let dir = attach_root(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hand.html"), b"<html>hi</html>").unwrap();

        let got = attachment_path(&ws, ".cws/assistant_attachments/hand.html").unwrap();
        assert_eq!(got, dir.join("hand.html"), "the folder's own file resolves");

        // The rule is the folder, so a file anywhere else in the workspace is refused —
        // including one with a deliverable name, which is exactly the confusion the
        // folder exists to end.
        std::fs::write(ws.join("report.html"), b"<html></html>").unwrap();
        let err = attachment_path(&ws, "report.html").unwrap_err();
        assert!(err.contains("assistant_attachments"), "not explained: {err}");
        // Scratch material is not an attachment either.
        std::fs::write(scratch_root(&ws).join("probe.py"), b"print(1)").unwrap();
        assert!(attachment_path(&ws, ".cws/probe.py").is_err());
        // Traversal, absolute path and a missing file are all refusals, not surprises.
        assert!(attachment_path(&ws, ".cws/assistant_attachments/../../../etc/passwd").is_err());
        assert!(attachment_path(&ws, "/etc/passwd").is_err());
        assert!(attachment_path(&ws, ".cws/assistant_attachments/gone.html").is_err());

        // The same rule, on the route a phone uses: bytes come back only for a file in
        // that folder, and a file above the ceiling says how big it is instead of
        // arriving as a reply the client cannot parse.
        let got = match inline_attachment(&ws, ".cws/assistant_attachments/hand.html").unwrap() {
            InlineFit::Inlined(a) => a,
            InlineFit::TooBig { bytes, limit } => panic!("a 15-byte attachment does not exceed {limit}: {bytes}"),
        };
        assert_eq!(got.name, "hand.html");
        assert_eq!(got.mime, "text/html");
        assert_eq!(got.bytes, 15);
        assert_eq!(got.b64, base64(b"<html>hi</html>"));
        assert!(inline_attachment(&ws, "report.html").is_err(), "a workspace file is not an attachment route");
        match inline_attachment_limited(&ws, ".cws/assistant_attachments/hand.html", 8).unwrap() {
            InlineFit::TooBig { bytes, limit } => {
                assert_eq!((bytes, limit), (15, 8), "the refusal does not state the numbers: {bytes} {limit}")
            }
            InlineFit::Inlined(_) => panic!("a 15-byte file fit in an 8-byte reply"),
        }
        let _ = std::fs::remove_dir_all(&ws);
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
