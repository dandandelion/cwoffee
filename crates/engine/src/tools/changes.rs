//! What a file call did, described for the tool card rather than for the model.
//!
//! `+N -M` in the header and a git-style diff when the card is expanded. None of
//! this reaches the model: it lives in `ToolResult::changes`, which is UI
//! metadata on the same footing as `files`. That matters for two reasons. A diff
//! of a file the model just wrote would be the single most expensive possible
//! thing to put in a prompt (it re-reads the whole change), and the prompt bytes
//! are what the llama.cpp prefix cache is keyed on.
//!
//! The diff is computed against what was actually on disk a moment before the
//! write, not against what the model believed was there, so the numbers cannot
//! be talked into being reassuring.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::path::display_rel;

/// Above this many lines on either side we report counts only. Counting is
/// linear-ish; a full unified diff over a 200k-line file is a lot of work to
/// produce something nobody will scroll through.
const DIFF_LINE_LIMIT: usize = 4000;
/// Upper bound on the diff handed to the front end, in characters. The card is
/// scrollable, but a 5 MB string over IPC for one card is not worth it.
const DIFF_CHAR_LIMIT: usize = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// Workspace-relative for display, same rule as `OutputFile::path`.
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// Unified diff with three lines of context, or `None` when the file was too
    /// big to diff (counts are still correct) or when there was nothing to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// True when the file did not exist before this call, which is what makes a
    /// plain `+42` mean "created" rather than "42 lines added, nothing removed".
    #[serde(default)]
    pub created: bool,
}

/// Describe `new` against `old` (None = the file did not exist before).
pub fn line_change(workspace: &std::path::Path, path: &std::path::Path, old: Option<&str>, new: &str) -> FileChange {
    let created = old.is_none();
    let (added, removed) = match old {
        None => (count_lines(new), 0),
        Some(prev) if prev == new => (0, 0),
        Some(prev) => counts(prev, new),
    };

    let diff = match old {
        Some(prev)
            if added + removed > 0
                && count_lines(prev) <= DIFF_LINE_LIMIT
                && count_lines(new) <= DIFF_LINE_LIMIT =>
        {
            let mut out = String::new();
            for line in unified(prev, new) {
                if out.len() > DIFF_CHAR_LIMIT {
                    out.push_str("… diff truncated\n");
                    break;
                }
                out.push_str(&line);
                out.push('\n');
            }
            Some(out)
        }
        _ => None,
    };

    FileChange {
        path: display_rel(workspace, path),
        added,
        removed,
        diff,
        created,
    }
}

/// Lines in a string. A trailing newline ends the last line rather than starting
/// an empty one, so a normal text file of three lines counts as 3, not 4.
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let n = s.split('\n').count();
    if s.ends_with('\n') {
        n - 1
    } else {
        n
    }
}

/// Added and removed line counts, by content multiset.
///
/// Multiset difference is linear and cannot be blown up by an adversarial file,
/// which an LCS table can: a 9000x9000 LCS is 81M cells. The cost is that a pure
/// reordering of unchanged lines reads as no change, which for a "+N -M" badge is
/// the answer a person actually wants.
fn counts(old: &str, new: &str) -> (usize, usize) {
    let mut bag: HashMap<&str, i64> = HashMap::new();
    for line in split_lines(old) {
        *bag.entry(line).or_default() += 1;
    }
    let mut added = 0usize;
    for line in split_lines(new) {
        match bag.get_mut(line) {
            Some(n) if *n > 0 => *n -= 1,
            _ => added += 1,
        }
    }
    let removed: usize = bag.values().filter(|n| **n > 0).map(|n| *n as usize).sum();
    (added, removed)
}

fn split_lines(s: &str) -> impl Iterator<Item = &str> {
    let drop_last = s.ends_with('\n');
    let mut it = s.split('\n').peekable();
    std::iter::from_fn(move || match it.next() {
        Some(l) => {
            if drop_last && it.peek().is_none() {
                None
            } else {
                Some(l)
            }
        }
        None => None,
    })
}

/// Unified diff with three lines of context.
///
/// Deliberately simple: trim the common prefix and suffix, LCS only the middle,
/// then keep three lines of context around each change. Real edits are local, so
/// the middle is usually a handful of lines and the quadratic table never gets
/// near its bound. When the middle really is huge the output degrades to "N lines
/// replaced" rather than stalling the tool call - the `+N -M` header is still
/// exact, and that is the number a person actually reads.
fn unified(old: &str, new: &str) -> Vec<String> {
    const CONTEXT: usize = 3;
    /// Bound on the LCS table. 800 x 800 cells is ~2.5 MB; past that we stop
    /// trying to be precise about *which* lines moved.
    const MAX_MIDDLE: usize = 800;

    let a: Vec<&str> = split_lines(old).collect();
    let b: Vec<&str> = split_lines(new).collect();

    let mut start = 0usize;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end_a = a.len();
    let mut end_b = b.len();
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    let mid_a = &a[start..end_a];
    let mid_b = &b[start..end_b];

    if mid_a.len() * mid_b.len() > MAX_MIDDLE * MAX_MIDDLE {
        return vec![format!(
            "@@ -{},{} +{},{} @@ replaced {} lines with {} (diff too large to show)",
            start + 1,
            mid_a.len(),
            start + 1,
            mid_b.len(),
            mid_a.len(),
            mid_b.len()
        )];
    }

    // Edit script over the middle, wrapped in the untouched lines either side so
    // line numbering is a straight walk.
    let mut script: Vec<(char, &str)> = Vec::with_capacity(a.len() + b.len());
    script.extend(a[..start].iter().map(|l| (' ', *l)));
    // One LCS table over the middle, then walk it. Recomputing the LCS at every
    // step would be quadratic *inside* the walk and can stall a tool call on a
    // few thousand lines.
    let (n, m) = (mid_a.len(), mid_b.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if mid_a[i] == mid_b[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }
    let at = |i: usize, j: usize| dp[i * (m + 1) + j];
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if mid_a[i] == mid_b[j] {
            script.push((' ', mid_a[i]));
            i += 1;
            j += 1;
        } else if at(i + 1, j) >= at(i, j + 1) {
            script.push(('-', mid_a[i]));
            i += 1;
        } else {
            script.push(('+', mid_b[j]));
            j += 1;
        }
    }
    while i < mid_a.len() {
        script.push(('-', mid_a[i]));
        i += 1;
    }
    while j < mid_b.len() {
        script.push(('+', mid_b[j]));
        j += 1;
    }
    script.extend(a[end_a..].iter().map(|l| (' ', *l)));

    // Which script entries matter: any change, plus CONTEXT lines either side.
    let keep = {
        let mut k = vec![false; script.len()];
        for (idx, (tag, _)) in script.iter().enumerate() {
            if *tag == ' ' {
                continue;
            }
            let from = idx.saturating_sub(CONTEXT);
            let to = (idx + CONTEXT).min(script.len() - 1);
            for slot in k.iter_mut().take(to + 1).skip(from) {
                *slot = true;
            }
        }
        k
    };

    // Walk once more, numbering lines and cutting the kept runs into hunks.
    let mut out: Vec<String> = Vec::new();
    let (mut old_ln, mut new_ln) = (1usize, 1usize);
    let mut idx = 0usize;
    while idx < script.len() {
        if !keep[idx] {
            match script[idx].0 {
                '-' => old_ln += 1,
                '+' => new_ln += 1,
                _ => {
                    old_ln += 1;
                    new_ln += 1;
                }
            }
            idx += 1;
            continue;
        }
        let hunk_from = idx;
        let (h_start_old, h_start_new) = (old_ln, new_ln);
        let (mut h_old, mut h_new) = (0usize, 0usize);
        let mut lines: Vec<String> = Vec::new();
        while idx < script.len() && keep[idx] {
            let (tag, text) = script[idx];
            lines.push(format!("{tag}{text}"));
            match tag {
                '-' => {
                    h_old += 1;
                    old_ln += 1;
                }
                '+' => {
                    h_new += 1;
                    new_ln += 1;
                }
                _ => {
                    h_old += 1;
                    h_new += 1;
                    old_ln += 1;
                    new_ln += 1;
                }
            }
            idx += 1;
        }
        let _ = hunk_from;
        out.push(format!(
            "@@ -{},{} +{},{} @@",
            h_start_old, h_old, h_start_new, h_new
        ));
        out.extend(lines);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_new_file_counts_every_line_as_added() {
        let c = line_change(Path::new("/ws"), Path::new("/ws/a.md"), None, "one\ntwo\nthree\n");
        assert_eq!((c.added, c.removed, c.created), (3, 0, true), "{c:?}");
        // Created files get no diff: the whole file is the diff, and the card
        // already has the file.
        assert!(c.diff.is_none());
    }

    #[test]
    fn a_replacement_shows_both_sides_and_a_diff() {
        let c = line_change(
            Path::new("/ws"),
            Path::new("/ws/a.md"),
            Some("keep\nchange me\ntail\n"),
            "keep\nchanged\ntail\n",
        );
        assert_eq!((c.added, c.removed, c.created), (1, 1, false));
        let d = c.diff.clone().expect("diff");
        assert!(d.contains("-change me"), "{d}");
        assert!(d.contains("+changed"), "{d}");
        assert!(d.contains(" keep"), "{d}");
    }

    #[test]
    fn writing_the_same_content_back_is_zero_and_zero() {
        let c = line_change(Path::new("/ws"), Path::new("/ws/a.md"), Some("same\n"), "same\n");
        assert_eq!((c.added, c.removed), (0, 0));
        assert!(c.diff.is_none(), "nothing changed, so nothing to show");
    }

    /// The counts stay honest even when the diff is refused for size.
    #[test]
    fn a_huge_change_still_reports_counts_without_a_diff() {
        let old = "x\n".repeat(9000);
        let new = format!("{}z\n", "x\n".repeat(4500));
        let c = line_change(Path::new("/ws"), Path::new("/ws/big.txt"), Some(&old), &new);
        assert!(c.added > 0 && c.removed > 0, "{c:?}");
        assert!(c.diff.is_none(), "no diff for a file this big");
    }

    #[test]
    fn paths_are_workspace_relative() {
        let c = line_change(Path::new("/ws"), Path::new("/ws/deep/note.md"), None, "a\n");
        assert_eq!(c.path, "deep/note.md");
    }
}
