//! The palette a conversation is read in, and the two homes a choice about it can live in.
//!
//! `theme` in `config.toml` used to be read by nobody: the key was parsed, written by the settings
//! screen, and then ignored, while `ui/src/App.svelte` kept its own `localStorage` copy and read
//! `prefers-color-scheme` once at boot. That is one setting pretending to be two, and the failure it
//! produces is a phone tab that changes the desktop's colours, or a desktop that ignores the file a
//! person edited.
//!
//! The split this module draws, and every surface has to say it:
//!
//! * **the stored choice** is one value, in `config.toml`, read by every shell at start: the window,
//!   a gateway tab, the terminal. It is a fact about the app, so it lives where a restart, a phone
//!   and a `vim config.toml` all read the same thing.
//! * **a per-window override** is allowed — two windows against one app genuinely want different
//!   palettes — but only when the chrome says the word *override*, and only ever in that window's
//!   own store. An override that rewrote `config.toml` would be one window changing everybody else's
//!   setting, which is exactly the bug the split exists to prevent.
//!
//! `system` is not a third palette: it is a *subscription*. The client watches its own media query
//! and repaints. The host cannot resolve it for anybody, because "the system" is a different system
//! per client — so the stored value says "follow", and each surface answers for itself what it is
//! following. A terminal has no media query; it reads `COLORFGBG` where a terminal provides one and
//! says plainly when there is nothing to follow.
//!
//! The words here are engine-owned for the same reason `crate::steer` owns its words: a settings row
//! on a phone and a settings row on the desktop must not paraphrase which home a choice lives in, and
//! a paraphrase is how "stored on the host" and "this window only" end up reading identically.

use serde::{Deserialize, Serialize};

/// The stored choice. `System` is the default because it is the honest one for a fresh install: the
/// app has not been told what this person's screen wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Light,
    Dark,
    /// Follow this client's own `prefers-color-scheme`, live.
    System,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::System
    }
}

impl Theme {
    /// The config word. `system` is accepted with and without the hyphenated spellings a person
    /// types into a file by hand, for the same reason `MidTurn::parse` is loose: a word that is
    /// silently ignored here leaves the palette in `localStorage` and the setting dead in the file.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "light" | "auto_light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "system" | "follow" | "os" => Some(Self::System),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    /// True when the value is a palette rather than a subscription: a surface can paint it without
    /// asking anything of the client's environment.
    pub fn is_fixed(self) -> bool {
        matches!(self, Self::Light | Self::Dark)
    }

    /// The settings row's own sentence about where this choice lives. Naming the file is the point:
    /// a person who changes it in Settings and then edits `config.toml` is looking at the same value
    /// and needs to be able to prove it.
    pub fn setting_note(self) -> String {
        match self {
            Self::Light | Self::Dark => format!(
                "Stored on this host: `theme = \"{}\"` in the config file, which is what the window, \
                 a device tab and the terminal all start from. A window may still carry its own \
                 override, and it says so on the screen that sets it.",
                self.word()
            ),
            Self::System => "Stored on this host: `theme = \"system\"` in the config file, which is \
                 not a palette but a subscription — each surface follows the colour scheme its own \
                 system prefers, live. A phone tab and a desktop window can therefore disagree, and \
                 both are obeying the one stored choice. A window may still carry its own override, \
                 and that changes only that window."
                .to_string(),
        }
    }

    /// The About line: which value is the host's, and which is this client's. Two numbers in one
    /// sentence is the whole claim — a tab's theme must not change the desktop's, and a reader can
    /// only check that if the screen says which is which.
    pub fn about_says(stored: Theme, window: Option<Theme>) -> String {
        match window {
            Some(w) if w != stored => format!(
                "palette: this window shows {} here as an override; the host still stores {} in \
                 the config file, and the override does not change it",
                w.word(),
                stored.word()
            ),
            Some(w) => format!(
                "palette: this window shows {} and the host stores {} too — no override here",
                w.word(),
                stored.word()
            ),
            None => format!(
                "palette: no override in this window; the host stores {} in the config file{}",
                stored.word(),
                if stored == Theme::System { ", which resolves from this system's own preference" } else { "" }
            ),
        }
    }

    /// What a window says when it takes an override. The word *override* has to be on the screen
    /// that sets it: a control that looks like the setting and isn't it is the failure this split
    /// exists to avoid.
    pub fn override_says(window: Theme, stored: Theme) -> String {
        format!(
            "this window only: {} here, and the host still stores {} for every other surface",
            window.word(),
            stored.word()
        )
    }

    /// The sentence for a window that *drops* its override and goes back to the stored choice.
    pub fn cleared_says(stored: Theme) -> String {
        format!(
            "this window's override is gone; it follows the host's stored {} again",
            stored.word()
        )
    }

    /// Resolve `system` from a terminal's `COLORFGBG`. `15;0` is light text on a dark background and
    /// `0;15` the other way round; anything else, including no value at all, is not evidence.
    ///
    /// A terminal has no media query, so this is the only honest signal available, and a missing one
    /// is reported rather than guessed at: a default that reads as "followed the system" when nothing
    /// was followed is the claim this refuses to make.
    pub fn resolve_colorfgbg(stored: Theme, value: Option<&str>) -> Resolved {
        if stored.is_fixed() {
            return Resolved { theme: stored, says: format!("the stored choice says {}", stored.word()) };
        }
        let Some(v) = value else {
            return Resolved {
                theme: Theme::Dark,
                says: "the stored choice is system, and this terminal says nothing about its \
                       background (no COLORFGBG), so the dark palette is a default and not a \
                       followed preference"
                    .to_string(),
            };
        };
        let last = v.split(';').next_back().unwrap_or("").trim();
        match last {
            "7" | "15" => Resolved {
                theme: Theme::Light,
                says: format!("the stored choice is system, and COLORFGBG={v} says a light background"),
            },
            "0" | "8" => Resolved {
                theme: Theme::Dark,
                says: format!("the stored choice is system, and COLORFGBG={v} says a dark background"),
            },
            _ => Resolved {
                theme: Theme::Dark,
                says: format!(
                    "the stored choice is system, and COLORFGBG={v} is not a background this app \
                     reads; the dark palette is a default and not a followed preference"
                ),
            },
        }
    }
}

/// A palette plus the sentence about how it got chosen. `says` is what a status line or a boot note
/// prints, because "dark" alone would claim a followed preference that may be a default.
pub struct Resolved {
    pub theme: Theme,
    pub says: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_words_and_the_ones_that_are_not_them() {
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("Dark"), Some(Theme::Dark));
        assert_eq!(Theme::parse("system"), Some(Theme::System));
        assert_eq!(Theme::parse("follow"), Some(Theme::System));
        assert_eq!(Theme::parse("auto-light"), Some(Theme::Light));
        assert_eq!(Theme::parse("midnight"), None, "a word nobody means must not become a default");
        assert_eq!(Theme::parse("loud"), None, "theme_loud is a retired key, not a palette");
        assert_eq!(Theme::parse("aggressive"), None);
    }

    /// `system` is a subscription, and a surface that treats it as a palette stops listening.
    #[test]
    fn only_the_two_palettes_are_fixed() {
        assert!(Theme::Light.is_fixed());
        assert!(Theme::Dark.is_fixed());
        assert!(!Theme::System.is_fixed());
    }

    /// About has to name both homes, and say which one an override is. A line that only prints
    /// "dark" is how a phone tab appears to have changed the desktop.
    #[test]
    fn about_names_both_homes_and_says_which_one_the_window_is() {
        let plain = Theme::about_says(Theme::Dark, None);
        assert!(plain.contains("no override"), "{plain}");
        assert!(plain.contains("stores dark"), "{plain}");
        let over = Theme::about_says(Theme::Dark, Some(Theme::Light));
        assert!(over.contains("override"), "{over}");
        assert!(over.contains("light here"), "{over}");
        assert!(over.contains("still stores dark"), "{over}");
        assert!(over.contains("does not change it"), "{over}");
        let same = Theme::about_says(Theme::Light, Some(Theme::Light));
        assert!(same.contains("no override here"), "{same}");
        let sys = Theme::about_says(Theme::System, None);
        assert!(sys.contains("this system's own preference"), "{sys}");
    }

    /// An override says the word `override` and says what the host still stores.
    #[test]
    fn an_override_says_it_is_an_override() {
        let s = Theme::override_says(Theme::Light, Theme::Dark);
        assert!(s.contains("this window only"), "{s}");
        assert!(s.contains("still stores dark"), "{s}");
        assert!(Theme::cleared_says(Theme::System).contains("follows the host's stored system again"));
    }

    /// The settings row says where the value lives, in the file's own spelling.
    #[test]
    fn the_settings_note_names_the_file_and_the_word_in_it() {
        let s = Theme::Dark.setting_note();
        assert!(s.contains("theme = \"dark\""), "{s}");
        assert!(s.contains("config file"), "{s}");
        assert!(s.contains("override"), "{s}");
        let sys = Theme::System.setting_note();
        assert!(sys.contains("is not a palette"), "{sys}");
        assert!(sys.contains("live"), "{sys}");
        assert!(sys.contains("disagree"), "{sys}");
    }

    /// `system` on a terminal is resolved from what the terminal actually says, and silence is
    /// reported as silence rather than claimed as a followed preference.
    #[test]
    fn a_terminal_that_says_nothing_is_said_to_say_nothing() {
        let dark = Theme::resolve_colorfgbg(Theme::System, Some("15;0"));
        assert_eq!(dark.theme, Theme::Dark);
        assert!(dark.says.contains("dark background"), "{}", dark.says);
        let light = Theme::resolve_colorfgbg(Theme::System, Some("0;15"));
        assert_eq!(light.theme, Theme::Light);
        assert!(light.says.contains("light background"), "{}", light.says);
        let none = Theme::resolve_colorfgbg(Theme::System, None);
        assert!(none.says.contains("not a followed preference"), "{}", none.says);
        let odd = Theme::resolve_colorfgbg(Theme::System, Some("nonsense"));
        assert!(odd.says.contains("not a background this app reads"), "{}", odd.says);
        let fixed = Theme::resolve_colorfgbg(Theme::Light, None);
        assert_eq!(fixed.theme, Theme::Light);
        assert!(fixed.says.contains("the stored choice says light"), "{}", fixed.says);
    }

    /// The wire spelling is the config spelling is the settings word.
    #[test]
    fn one_word_three_places() {
        for t in [Theme::Light, Theme::Dark, Theme::System] {
            let v = serde_json::to_value(t).unwrap();
            assert_eq!(v.as_str().unwrap(), t.word(), "{t:?}");
            assert_eq!(Theme::parse(t.word()), Some(t));
        }
    }
}
