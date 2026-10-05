//! One-time relocation from the pre-rebrand `cowry` paths.
//!
//! A rename that silently strands someone's history is a data-loss bug wearing
//! a branding costume. The old directories are moved, not copied-and-left, so
//! there is exactly one source of truth afterwards; if the move fails (different
//! filesystem) it falls back to a copy and says so in the log rather than
//! pretending nothing happened.
//!
//! Deliberately conservative: if a `cwoffee` directory already exists, nothing
//! is touched. Merging two histories by guessing is worse than leaving the old
//! one where it is and telling the user.

use std::path::{Path, PathBuf};

const OLD: &str = "cowry";
const NEW: &str = "cwoffee";

/// The default workspace folder was renamed with everything else, and that rename
/// happened *outside* the database: sessions created before it still name
/// `~/cowry-workspace`. Since the workspace now follows the session, every open of
/// one of those rows would be refused ("...which the engine refused: is not a
/// directory") and the sidebar would carry a group for a folder that no longer
/// exists. Rewriting the prefix is only safe under the signature of the rename
/// itself: the old folder is gone and the new one is there. If both exist, or the
/// old one is still a real directory someone is using, nothing is touched.
pub fn rehome_session_workspaces(db: &Path, old_dir: &Path, new_dir: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    if !db.is_file() {
        return notes;
    }
    if old_dir.exists() || !new_dir.exists() {
        return notes;
    }
    let (old, new) = (old_dir.display().to_string(), new_dir.display().to_string());
    let conn = match rusqlite::Connection::open(db) {
        Ok(c) => c,
        Err(e) => {
            notes.push(format!("could not re-home session workspaces in {}: {e}", db.display()));
            return notes;
        }
    };
    let _ = conn.pragma_update(None, "busy_timeout", 8000);

    // `LIKE old || '/%'` would treat `_` and `%` inside the path as wildcards, so
    // the prefix test is done with substrings instead.
    // `LIKE old || '/%'` would treat `_` and `%` inside the path as wildcards, so
    // the prefix test is done with substrings instead.
    let count = |table: &str, col: &str| -> i64 {
        let sql = format!(
            "SELECT count(*) FROM {table} WHERE {col} = ?1 \
             OR (substr({col}, 1, length(?1)) = ?1 AND substr({col}, length(?1) + 1, 1) = '/')"
        );
        conn.query_row(&sql, rusqlite::params![old], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
    };
    let sess_before = count("sessions", "workspace");
    let ws_before = count("workspaces", "path");

    // A row for the destination may already exist (`workspaces` keys on `path`), so
    // drop the doomed one first rather than failing the statement on a unique
    // constraint.
    conn.execute(
        "DELETE FROM workspaces WHERE path = ?2 AND EXISTS (SELECT 1 FROM workspaces WHERE path = ?1)",
        rusqlite::params![old, new],
    ).ok();
    if let Err(e) = conn.execute(
        "UPDATE sessions SET workspace = ?2 || substr(workspace, length(?1) + 1) \
         WHERE substr(workspace, 1, length(?1)) = ?1 AND substr(workspace, length(?1) + 1, 1) = '/'",
        rusqlite::params![old, new],
    )
    .and_then(|_| {
        conn.execute(
            "UPDATE sessions SET workspace = ?2 WHERE workspace = ?1",
            rusqlite::params![old, new],
        )
    }) {
        notes.push(format!("re-homing session workspaces failed: {e}"));
        return notes;
    }
    if let Err(e) = conn.execute(
        "UPDATE workspaces SET path = ?2 || substr(path, length(?1) + 1) \
         WHERE substr(path, 1, length(?1)) = ?1 AND substr(path, length(?1) + 1, 1) = '/'",
        rusqlite::params![old, new],
    )
    .and_then(|_| {
        conn.execute(
            "UPDATE workspaces SET path = ?2 WHERE path = ?1",
            rusqlite::params![old, new],
        )
    }) {
        notes.push(format!("re-homing workspace rows failed: {e}"));
        return notes;
    }
    if sess_before > 0 {
        notes.push(format!(
            "re-homed {sess_before} session(s) from {old} to {new}: the folder was renamed, \
             so their tools now point at the folder that exists"
        ));
    }
    if ws_before > 0 {
        notes.push(format!("renamed {ws_before} sidebar workspace row(s) {old} -> {new}"));
    }
    let _ = conn;
    notes
}

/// Env-reading wrapper: the default workspace lives under `$HOME`, and the pair is
/// only offered when the rename is the explanation.
pub fn rehome_default_workspaces() -> Vec<String> {
    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => return Vec::new(),
    };
    let old_dir = home.join(format!("{OLD}-workspace"));
    let new_dir = home.join(format!("{NEW}-workspace"));
    let data_base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    rehome_session_workspaces(&data_base.join(NEW).join("sessions.db"), &old_dir, &new_dir)
}

/// Move `cowry` -> `cwoffee` under the config and data roots. Returns human
/// readable notes for the caller to log.
pub fn migrate_legacy_dirs() -> Vec<String> {
    let mut notes = Vec::new();
    for (env_key, fallback, label) in [
        ("XDG_CONFIG_HOME", ".config", "config"),
        ("XDG_DATA_HOME", ".local/share", "data"),
    ] {
        let base = std::env::var(env_key)
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| match std::env::var("HOME") {
                Ok(h) => PathBuf::from(h).join(fallback),
                Err(_) => PathBuf::from(fallback),
            });
        notes.extend(migrate_under(&base, label));
    }
    // The directories first: the database has to exist under the new name before
    // rows inside it can be pointed at the renamed workspace folder.
    notes.extend(rehome_default_workspaces());
    notes
}

/// The env-reading above is deliberately split from the work here: mutating
/// process-global env vars from a test races every other test in the binary,
/// which turned a passing migration test into one that failed only in parallel
/// runs. Tests call this directly.
pub fn migrate_under(base: &Path, label: &str) -> Vec<String> {
    let mut notes = Vec::new();
    {
        let old = base.join(OLD);
        let new = base.join(NEW);
        if !old.exists() {
            return notes;
        }
        if new.exists() {
            // Both names exist. This is not rare: the new dir gets created the
            // first time the renamed app starts, and the old one still holds the
            // real history. Refusing to touch either would show the user an empty
            // session list and look exactly like their history was deleted.
            notes.extend(reconcile(&old, &new, label));
            return notes;
        }
        match std::fs::rename(&old, &new) {
            Ok(_) => notes.push(format!(
                "migrated {label} dir {} -> {}",
                old.display(),
                new.display()
            )),
            Err(_) => match copy_tree(&old, &new) {
                Ok(()) => notes.push(format!(
                    "copied {label} dir {} -> {} (rename failed, likely a different filesystem); \
                     the old copy is still there",
                    old.display(),
                    new.display()
                )),
                Err(e) => notes.push(format!(
                    "could not migrate {label} dir {}: {e}",
                    old.display()
                )),
            },
        }
    }
    notes
}

/// Both directories exist: keep whichever database actually has sessions.
///
/// The rule is deliberately narrow. If the new database has anything in it, that
/// is the live one and the old copy is left where it is. Only when the new one is
/// empty and the old one is not do we move the old files across, and the empty
/// set is kept aside rather than removed, so a wrong guess is recoverable.
fn reconcile(old_dir: &Path, new_dir: &Path, label: &str) -> Vec<String> {
    let mut notes = Vec::new();
    let old_db = old_dir.join("sessions.db");
    let new_db = new_dir.join("sessions.db");
    if !old_db.exists() {
        return notes;
    }
    let old_n = session_count(&old_db);
    let new_n = session_count(&new_db);
    match (old_n, new_n) {
        (Some(o), Some(n)) if o > 0 && n == 0 => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut moved = true;
            for suffix in ["", "-wal", "-shm"] {
                let src = new_db.with_extension(format!("db{suffix}"));
                if src.exists() {
                    let dst = src.with_extension(format!("db{suffix}.pre-cwoffee-{stamp}"));
                    if std::fs::rename(&src, &dst).is_err() {
                        moved = false;
                    }
                }
            }
            if !moved {
                notes.push(format!(
                    "could not set aside the empty {label} database; left both copies alone"
                ));
                return notes;
            }
            for suffix in ["", "-wal", "-shm"] {
                let src = old_db.with_extension(format!("db{suffix}"));
                if src.exists() {
                    let dst = new_dir.join(src.file_name().unwrap());
                    let _ = std::fs::rename(&src, &dst);
                }
            }
            let dst = new_dir.display().to_string();
            notes.push(format!(
                "{label}: the new database was empty and the legacy one held {o} session(s); moved the legacy database into {dst}. The empty copy is kept aside as *.pre-cwoffee-{stamp}."
            ));
        }
        (Some(o), Some(n)) => notes.push(format!(
            "{label}: legacy dir {} holds {o} session(s), current dir holds {n}; left both alone",
            old_dir.display()
        )),
        _ => notes.push(format!(
            "{label}: could not read one of the databases to compare them; left both alone"
        )),
    }
    notes
}

fn session_count(db: &Path) -> Option<usize> {
    if !db.exists() {
        return Some(0);
    }
    crate::store::Store::open(db)
        .ok()
        .and_then(|st| st.list_sessions(500).ok())
        .map(|v| v.len())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("cwoffee_{tag}_{}_{}", std::process::id(), n))
    }

    /// The workspace rename left session rows pointing at a folder that no longer
    /// exists, and since the workspace follows the session, that turns every open
    /// into a refusal and puts a phantom group in the sidebar.
    #[test]
    fn sessions_left_behind_by_the_workspace_rename_follow_the_folder() {
        let base = root("wsmove");
        let old_dir = base.join("home/cowry-workspace");
        let new_dir = base.join("home/cwoffee-workspace");
        let db = base.join("data/cwoffee/sessions.db");
        std::fs::create_dir_all(new_dir.parent().unwrap()).unwrap();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        // The new folder exists, the old one does not: the signature of the rename.
        std::fs::create_dir_all(&new_dir).unwrap();

        {
            let st = crate::store::Store::open(&db).unwrap();
            st.create_session("one", new_dir.to_str().unwrap(), "m").unwrap();
            st.create_session("two", old_dir.to_str().unwrap(), "m").unwrap();
            st.create_session("three", &format!("{}/notes", old_dir.display()), "m")
                .unwrap();
            st.add_workspace(old_dir.to_str().unwrap()).unwrap();
        }

        let notes = rehome_session_workspaces(&db, &old_dir, &new_dir);
        assert!(
            notes.iter().any(|n| n.contains("re-homed 2 session")),
            "{notes:?}"
        );

        let st = crate::store::Store::open(&db).unwrap();
        let mut ws: Vec<String> = st
            .list_sessions(500)
            .unwrap()
            .into_iter()
            .map(|r| r.workspace)
            .collect();
        ws.sort();
        let want = vec![
            new_dir.to_str().unwrap().to_string(),
            new_dir.to_str().unwrap().to_string(),
            format!("{}/notes", new_dir.display()),
        ];
        assert_eq!(ws, want, "{notes:?}");
        // `list_workspaces` unions registered rows with folders that hold sessions,
        // so the `notes` subfolder shows up as its own group once a session lives
        // there. What must not survive is the old name.
        let paths: Vec<String> = st.list_workspaces().unwrap().into_iter().map(|w| w.path).collect();
        assert!(
            !paths.iter().any(|p| p.contains("cowry-workspace")),
            "the old folder is still in the sidebar: {paths:?} {notes:?}"
        );
        assert!(
            paths.contains(&new_dir.to_str().unwrap().to_string()),
            "{paths:?} {notes:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Both folders present means someone is *using* the old one - maybe a
    /// separate install, maybe a symlink. Rewriting rows would move their files
    /// under a folder they never chose.
    #[test]
    fn a_workspace_that_still_exists_is_left_alone() {
        let base = root("wskeep");
        let old_dir = base.join("home/cowry-workspace");
        let new_dir = base.join("home/cwoffee-workspace");
        let db = base.join("data/cwoffee/sessions.db");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        {
            let st = crate::store::Store::open(&db).unwrap();
            st.create_session("one", old_dir.to_str().unwrap(), "m").unwrap();
        }
        let notes = rehome_session_workspaces(&db, &old_dir, &new_dir);
        assert!(notes.is_empty(), "{notes:?}");
        let st = crate::store::Store::open(&db).unwrap();
        let rows = st.list_sessions(500).unwrap();
        assert_eq!(rows[0].workspace, old_dir.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The whole point of the module: an existing history must still be there
    /// under the new name, not merely "migrated" in the log.
    #[test]
    fn a_legacy_data_dir_moves_to_the_new_name() {
        let base = root("mig");
        let old_data = base.join(OLD);
        std::fs::create_dir_all(old_data.join("wal")).unwrap();
        std::fs::write(old_data.join("sessions.db"), b"history").unwrap();
        std::fs::write(old_data.join("wal/side"), b"x").unwrap();

        let notes = migrate_under(&base, "data");

        let new_data = base.join(NEW);
        assert!(new_data.exists(), "no migration: {notes:?}");
        assert_eq!(std::fs::read(new_data.join("sessions.db")).unwrap(), b"history");
        assert_eq!(std::fs::read(new_data.join("wal/side")).unwrap(), b"x");
        assert!(!old_data.exists(), "old dir left behind after a rename");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Both names present and the new one is an empty shell: that is the common
    /// case in practice, and the one that would otherwise show the user an empty
    /// session list after a rename.
    #[test]
    fn an_empty_new_database_does_not_shadow_a_populated_legacy_one() {
        let base = root("recon");
        let old_data = base.join(OLD);
        let new_data = base.join(NEW);
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::create_dir_all(&new_data).unwrap();

        // A real database with sessions on the legacy side.
        {
            let st = crate::store::Store::open(&old_data.join("sessions.db")).unwrap();
            st.create_session("keep me", "/tmp", "m").unwrap();
            st.create_session("and me", "/tmp", "m").unwrap();
        }
        // An empty one on the new side.
        {
            let _st = crate::store::Store::open(&new_data.join("sessions.db")).unwrap();
        }

        let notes = migrate_under(&base, "data");

        let st = crate::store::Store::open(&new_data.join("sessions.db")).unwrap();
        let sessions = st.list_sessions(50).unwrap();
        assert_eq!(sessions.len(), 2, "history did not survive: {notes:?}");
        assert!(
            notes.iter().any(|n| n.contains("moved the legacy database")),
            "{notes:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// If the new database has its own sessions, it is the live one. Guessing
    /// which history wins is not our call.
    #[test]
    fn a_populated_new_database_is_never_overwritten() {
        let base = root("keep");
        let old_data = base.join(OLD);
        let new_data = base.join(NEW);
        std::fs::create_dir_all(&old_data).unwrap();
        std::fs::create_dir_all(&new_data).unwrap();
        {
            let st = crate::store::Store::open(&old_data.join("sessions.db")).unwrap();
            st.create_session("old", "/tmp", "m").unwrap();
        }
        {
            let st = crate::store::Store::open(&new_data.join("sessions.db")).unwrap();
            st.create_session("current", "/tmp", "m").unwrap();
        }

        let notes = migrate_under(&base, "data");

        let st = crate::store::Store::open(&new_data.join("sessions.db")).unwrap();
        let titles: Vec<String> = st.list_sessions(50).unwrap().into_iter().map(|s| s.title).collect();
        assert_eq!(titles, vec!["current".to_string()], "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("left both alone")), "{notes:?}");

        let _ = std::fs::remove_dir_all(&base);
    }
}
