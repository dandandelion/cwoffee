//! Which pi session file belongs to which cwoffee conversation.
//!
//! pi keeps conversations as JSONL files it owns; cwoffee keeps its sidebar,
//! transcript, and FTS index in SQLite. The two have to agree on which is which,
//! and the store has no column for a pi path — so the bridge keeps the pairing
//! beside itself, in a file, until Phase 4 moves it into the schema (where it
//! belongs, next to the messages it indexes).

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionMap {
    /// cwoffee session row id -> pi session file.
    by_row: HashMap<i64, String>,
    #[serde(skip)]
    path: PathBuf,
}

impl SessionMap {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut m = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<SessionMap>(&t).ok())
            .unwrap_or_default();
        m.path = path;
        m
    }

    pub fn get(&self, row: i64) -> Option<&str> {
        self.by_row.get(&row).map(String::as_str)
    }

    pub fn set(&mut self, row: i64, file: &str) {
        if self.by_row.get(&row).map(String::as_str) == Some(file) {
            return;
        }
        self.by_row.insert(row, file.to_string());
        self.save();
    }

    pub fn forget(&mut self, row: i64) {
        if self.by_row.remove(&row).is_some() {
            self.save();
        }
    }

    fn save(&mut self) {
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&self.path, text) {
            eprintln!("[bridge] could not write {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("cw-session-map-{}.json", std::process::id()));
        let mut m = SessionMap::open(&path);
        m.set(7, "/tmp/sessions/abc.jsonl");
        drop(m);
        let again = SessionMap::open(&path);
        assert_eq!(again.get(7), Some("/tmp/sessions/abc.jsonl"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let m = SessionMap::open(std::env::temp_dir().join("definitely-not-here.json"));
        assert_eq!(m.get(1), None);
    }
}
