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
            notes.push(format!(
                "{label}: the new database was empty and the legacy one held {o} session(s);                  moved the legacy database into {}. The empty copy is kept aside as                  *.pre-cwoffee-{stamp}.",
                new_dir.display(),
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
