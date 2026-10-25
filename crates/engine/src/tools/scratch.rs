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
/// An absolute path is left alone: it is an explicit instruction, and the confinement
/// layer is the thing that says no to it. Prefixing it with `.cws/` would silently
/// rewrite a request the user was shown.
pub fn destination_for(raw: &str, place: Place) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return match place {
            Place::Scratch => SCRATCH_DIR.to_string(),
            Place::Attach => attach_prefix(),
            Place::Keep => String::new(),
        };
    }
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
        return format!("{}/{rel}", attach_prefix());
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
    let md = std::fs::metadata(&full).map_err(|_| format!("{rel} no longer exists"))?;
    if !md.is_file() {
        return Err(format!("{rel} is not a file"));
    }
    Ok(full)
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
            "{rel} is in .cws/assistant_attachments/ — the transcript attaches it to this \n             turn, and the person chooses where to save it. That is the right place for a file \n             you are handing over. Something you only needed for yourself belongs in .cws/, and a \n             file they asked to have in their own folder belongs there."
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
