//! The display settings a client sets for itself, and the only place that knows what
//! their keys and values mean.
//!
//! The distinction that decides where these live: a model's context ceiling is a fact
//! about a conversation, so it sits in `config.toml` where a restart, a phone and the
//! next model process can all read the same number. "Show me what each step of the run
//! did" is a preference about this screen, and it is not a fact about the conversation
//! at all. It goes in the conversation database (`ui_prefs`, schema v12) rather than in
//! a browser's `localStorage`, because localStorage is per-origin — the desktop, a
//! browser on another port and a phone are three stores, and a preference only one of
//! them remembers is not a setting, it is a coincidence.
//!
//! Keys are not free-form. A table any client can invent keys in turns into a drawer:
//! the list below is the whole vocabulary, the settings screens render from it, and a
//! write to anything else is refused in words.

use crate::store::StoreOps;
use serde::Serialize;

pub struct UiPref {
    pub key: &'static str,
    /// What the control says. Short, and about the effect, not the mechanism: the
    /// transcript is the mechanism, the lines are the effect.
    pub label: &'static str,
    /// One line on what changing it changes, so the person does not have to try it.
    pub hint: &'static str,
    /// Which settings page renders it. A pref with no page is a control that exists
    /// nowhere, and a pref rendered on two pages is two controls for one setting.
    pub group: &'static str,
    pub default: &'static str,
    /// The whole vocabulary of values. A toggle is two of them.
    pub values: &'static [&'static str],
}

pub const UI_PREFS: &[UiPref] = &[
    UiPref {
        key: "verbose_blocks",
        label: "Show what each step did",
        hint: "On leaves a run's thinking and tool calls open, with their output under each line. \
               Off folds them to one line each, which is what most turns are worth.",
        group: "transcript",
        default: "off",
        values: &["off", "on"],
    },
    UiPref {
        key: "gateway_on_exit",
        label: "Keep devices working after this window closes",
        hint: "On hands the port to `cwoffee-gateway` — the same gateway as a separate process — when \
               the last window closes, so a phone stays connected with this app gone. Off stops the \
               gateway with the window.",
        group: "devices",
        default: "off",
        values: &["off", "on"],
    },
    // A pref joins this list only once the app acts on it. The second entry I wrote here
    // said "show the time on each message" and nothing in the transcript read it, which is
    // a control that does nothing: cut it rather than ship the promise.
];

pub fn known(key: &str) -> Option<&'static UiPref> {
    UI_PREFS.iter().find(|p| p.key == key)
}

/// A pref as a settings screen needs it: what to say, what is set now, and whether
/// that came from the person or from the default.
#[derive(Serialize)]
pub struct UiPrefView {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub group: &'static str,
    pub value: String,
    pub default: &'static str,
    pub is_default: bool,
    pub values: &'static [&'static str],
}

pub fn view(store: &dyn StoreOps) -> Result<Vec<UiPrefView>, String> {
    let mut out = Vec::with_capacity(UI_PREFS.len());
    for p in UI_PREFS {
        let stored = store.ui_pref(p.key).map_err(|e| e.to_string())?;
        let value = stored.clone().unwrap_or_else(|| p.default.to_string());
        out.push(UiPrefView {
            key: p.key,
            label: p.label,
            hint: p.hint,
            group: p.group,
            is_default: stored.is_none(),
            default: p.default,
            value,
            values: p.values,
        });
    }
    Ok(out)
}

/// Set one pref by hand, and say what it now means. The value is checked against the
/// pref's own vocabulary, and the answer is read back from the store: a settings screen
/// that echoed what it sent would keep showing a number the store refused to keep.
pub fn set(store: &dyn StoreOps, key: &str, value: &str) -> Result<String, String> {
    let p = match known(key) {
        Some(p) => p,
        None => {
            let names: Vec<&str> = UI_PREFS.iter().map(|p| p.key).collect();
            return Err(format!(
                "`{key}` is not a setting cwoffee knows, so nothing was written. The list is {}.",
                names.join(", ")
            ));
        }
    };
    if !p.values.contains(&value) {
        return Err(format!(
            "`{value}` is not a value `{}` takes: it is one of {}.",
            p.key,
            p.values.join(" or ")
        ));
    }
    store.set_ui_pref(key, value).map_err(|e| e.to_string())?;
    let back = store
        .ui_pref(key)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| p.default.to_string());
    if back != value {
        return Err(format!(
            "the store kept `{back}` for `{key}`, not `{value}` — nothing was changed, and the \
             screen should show what it actually holds"
        ));
    }
    Ok(format!("{}: {value}", p.label))
}

/// Leave it unset again, which is not the same as writing the default: an unset pref
/// follows the default if that ever moves.
pub fn clear(store: &dyn StoreOps, key: &str) -> Result<String, String> {
    let p = match known(key) {
        Some(p) => p,
        None => return Err(format!("`{key}` is not a setting cwoffee knows")),
    };
    store.clear_ui_pref(key).map_err(|e| e.to_string())?;
    Ok(format!(
        "{}: back to unset, so it follows the default ({})",
        p.label, p.default
    ))
}

/// The fact a host or a tool needs, resolved the same way the screen resolves it.
pub fn is_on(store: &dyn StoreOps, key: &str) -> bool {
    let p = match known(key) {
        Some(p) => p,
        None => return false,
    };
    let v = store
        .ui_pref(key)
        .ok()
        .flatten()
        .unwrap_or_else(|| p.default.to_string());
    // The first listed value that is not the default is treated as "on" for a toggle, so
    // adding a pref cannot mean every caller has to special-case its spelling.
    v != p.default
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn tmp() -> Store {
        let path = std::env::temp_dir().join(format!(
            "cw-prefs-{}-{}.db",
            std::process::id(),
            UI_PREFS.len()
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        Store::open(&path).unwrap()
    }

    /// A pref nobody acts on is a control that lies about doing something, so the list
    /// stays as short as the app's ability to honour it.
    #[test]
    fn every_listed_pref_is_one_the_app_actually_reads() {
        // verbose_blocks is read by the transcript (ui/src/lib/transcript.ts, setVerbose);
        // gateway_on_exit is read by the window's close path (crates/tauri-app/src/main.rs,
        // which calls gateway_ctl::handoff before the last window goes away). An entry here
        // without a reader on the other side is a switch wired to nothing.
        assert_eq!(UI_PREFS.len(), 2, "a new pref has to be wired in the app as well");
        // And a group is a settings page: `ui/src/App.svelte` renders `transcript` under
        // Appearance and `devices` under Devices. A third group name would be a pref no
        // screen shows.
        for p in UI_PREFS {
            assert!(
                matches!(p.group, "transcript" | "devices"),
                "{}: group {:?} is not a page any settings screen renders",
                p.key,
                p.group
            );
        }
    }

    #[test]
    fn the_vocabulary_is_short_and_every_pref_names_its_default() {
        assert!(!UI_PREFS.is_empty());
        let mut keys: Vec<&str> = UI_PREFS.iter().map(|p| p.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), UI_PREFS.len(), "two prefs claim the same key");
        for p in UI_PREFS {
            assert!(
                p.values.contains(&p.default),
                "{}'s default `{}` is not one of its own values",
                p.key,
                p.default
            );
            // A hint a person can act on: it says what changes, not what the field is.
            assert!(
                p.hint.split_whitespace().count() >= 8,
                "{} explains nothing: {:?}",
                p.key,
                p.hint
            );
            assert!(!p.label.contains('_'), "{}: labels are prose, not keys", p.key);
        }
    }

    #[test]
    fn a_pref_round_trips_and_an_unknown_key_changes_nothing() {
        let s = tmp();
        // Unset first, and the screen reports the default while saying it is the default.
        let v = view(&s).unwrap();
        let vb = v.iter().find(|p| p.key == "verbose_blocks").unwrap();
        assert_eq!(vb.value, "off");
        assert!(vb.is_default, "unset has to be distinguishable from the default");
        assert!(
            !is_on(&s, "verbose_blocks"),
            "an unset pref is not 'on'"
        );

        let say = set(&s, "verbose_blocks", "on").unwrap();
        assert!(say.contains("Show what each step did"), "{say}");
        assert!(is_on(&s, "verbose_blocks"));
        let v = view(&s).unwrap();
        let vb = v.iter().find(|p| p.key == "verbose_blocks").unwrap();
        assert_eq!(vb.value, "on");
        assert!(!vb.is_default, "a written pref is no longer the default");

        // A key nobody declared is refused, and the table stays empty of it.
        let e = set(&s, "theme_aggressive", "on").unwrap_err();
        assert!(e.contains("not a setting"), "{e}");
        assert!(e.contains("verbose_blocks"), "{e}: the list has to be named");
        assert_eq!(
            s.ui_prefs_all().unwrap().len(),
            1,
            "a refused write left a row: {:?}",
            s.ui_prefs_all().unwrap()
        );

        // A value outside the pref's vocabulary is refused the same way.
        let e2 = set(&s, "verbose_blocks", "yes please").unwrap_err();
        assert!(e2.contains("off or on"), "{e2}");
        assert_eq!(s.ui_pref("verbose_blocks").unwrap().unwrap(), "on");

        // Un-setting is not the same as writing the default, and the screen can tell.
        let say2 = clear(&s, "verbose_blocks").unwrap();
        assert!(say2.contains("unset"), "{say2}");
        assert!(s.ui_pref("verbose_blocks").unwrap().is_none());
        assert!(view(&s).unwrap().iter().find(|p| p.key == "verbose_blocks").unwrap().is_default);
    }

    /// The point of putting these in the conversation database instead of the browser:
    /// the store answers the same way for the desktop, another port's browser and a
    /// phone, because it is one file the app owns.
    #[test]
    fn the_pref_survives_reopening_the_store() {
        let path = std::env::temp_dir().join(format!("cw-prefs-reopen-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
        {
            let s = Store::open(&path).unwrap();
            set(&s, "verbose_blocks", "on").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert!(is_on(&s, "verbose_blocks"), "the setting did not outlive the process");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }
}
