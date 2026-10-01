//! Path resolution and workspace confinement.
//!
//! `Path::starts_with` on an unresolved path is not a containment check.
//! `/home/u/ws/../../etc/passwd`.starts_with("/home/u/ws") is **true**, because
//! `..` is just another path segment to Rust until something collapses it. Every
//! file-facing tool goes through `confine` so there is exactly one place that
//! decides what "inside the workspace" means.
//!
//! `canonicalize` is deliberately not used: it fails on paths that do not exist
//! yet (write_file creates them), and it resolves symlinks, which would make
//! behaviour differ between a real directory and a bind-mounted one.

use std::path::{Component, Path, PathBuf};

/// Collapse `.` and `..` textually, without touching the filesystem.
pub fn lexical_resolve(base: &Path, rel: &str) -> PathBuf {
    let rel_path = Path::new(rel);
    let mut out = if rel_path.is_absolute() {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    for comp in rel_path.components() {
        match comp {
            Component::RootDir => out = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            Component::Prefix(_) => {}
        }
    }
    out
}

/// Resolve `raw` against the workspace and reject anything that lands outside.
///
/// Returns the resolved path, or a message safe to hand back to the model — a
/// model that is told "outside the workspace" can correct itself, whereas a
/// generic error invites it to retry the same path forever.
pub fn confine(workspace: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty path".into());
    }
    // A NUL byte truncates the path at the syscall layer, which would make the
    // checked path and the opened path differ.
    if raw.contains('\0') {
        return Err("path contains a NUL byte".into());
    }
    let ws = lexical_resolve(workspace, ".");
    let p = lexical_resolve(&ws, raw);
    if p == ws || p.starts_with(&ws) {
        Ok(p)
    } else {
        Err(format!(
            "{raw:?} resolves to {}, which is outside the workspace {}. \
             Use a path relative to the workspace root.",
            p.display(),
            ws.display()
        ))
    }
}

/// Relative path for display, so tool cards do not print the whole absolute path.
pub fn display_rel(workspace: &Path, p: &Path) -> String {
    let ws = lexical_resolve(workspace, ".");
    match p.strip_prefix(&ws) {
        Ok(r) if !r.as_os_str().is_empty() => r.display().to_string(),
        _ => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_dot_and_dotdot() {
        let base = Path::new("/home/u/ws");
        assert_eq!(lexical_resolve(base, "a/b"), PathBuf::from("/home/u/ws/a/b"));
        assert_eq!(lexical_resolve(base, "a/../b"), PathBuf::from("/home/u/ws/b"));
        assert_eq!(lexical_resolve(base, "./x"), PathBuf::from("/home/u/ws/x"));
        assert_eq!(lexical_resolve(base, "/etc"), PathBuf::from("/etc"));
        assert_eq!(lexical_resolve(base, "../../etc"), PathBuf::from("/home/etc"));
    }

    /// The whole reason this module exists.
    #[test]
    fn dotdot_escape_is_rejected() {
        let ws = Path::new("/home/u/ws");
        for bad in [
            "../../etc/passwd",
            "../outside",
            "a/../../outside",
            "/etc/passwd",
            "/home/u/wsx/file",
            "subdir/../../outside",
        ] {
            let r = confine(ws, bad);
            assert!(r.is_err(), "{bad:?} was allowed: {:?}", r.ok());
        }
    }

    /// starts_with alone would let these through, which is the bug this guards.
    #[test]
    fn naive_starts_with_would_have_let_escapes_through() {
        let ws = Path::new("/home/u/ws");
        let sneaky = Path::new("/home/u/ws/../../etc/passwd");
        assert!(
            sneaky.starts_with(ws),
            "if this ever becomes false, Rust changed path semantics and \
             confine() needs a review"
        );
        assert!(confine(ws, "../../etc/passwd").is_err());
    }

    #[test]
    fn sibling_prefix_is_not_containment() {
        // /home/u/ws-evil must not count as inside /home/u/ws
        let ws = Path::new("/home/u/ws");
        assert!(confine(ws, "../ws-evil/x").is_err());
        assert!(confine(ws, "/home/u/ws-evil/x").is_err());
    }

    #[test]
    fn allowed_paths_resolve_inside() {
        let ws = Path::new("/home/u/ws");
        assert_eq!(confine(ws, ".").unwrap(), PathBuf::from("/home/u/ws"));
        assert_eq!(confine(ws, "file.txt").unwrap(), PathBuf::from("/home/u/ws/file.txt"));
        assert_eq!(confine(ws, "a/b/c").unwrap(), PathBuf::from("/home/u/ws/a/b/c"));
        assert_eq!(confine(ws, "a/../b").unwrap(), PathBuf::from("/home/u/ws/b"));
        assert_eq!(confine(ws, "/home/u/ws/abs").unwrap(), PathBuf::from("/home/u/ws/abs"));
    }

    #[test]
    fn rejects_nul_and_empty() {
        let ws = Path::new("/home/u/ws");
        assert!(confine(ws, "").is_err());
        assert!(confine(ws, "   ").is_err());
        assert!(confine(ws, "a\0b").is_err());
    }

    #[test]
    fn display_rel_shortens_workspace_paths() {
        let ws = Path::new("/home/u/ws");
        assert_eq!(display_rel(ws, Path::new("/home/u/ws/a/b.txt")), "a/b.txt");
        assert_eq!(display_rel(ws, Path::new("/home/u/ws")), "/home/u/ws");
    }
}
