//! The file a page *offers*, as opposed to the file a page *is*.
//!
//! A browser normally tells its embedder when a download starts, and the embedder chooses where the
//! bytes go. WebKitGTK stopped doing that in 2.40 — the `download-started` signal wry connects has
//! not been emitted for a `file:` or ordinary same-page link since — so in cwoffee's window a
//! `download` link is just a navigation: the address moves to the file, the page the person was
//! looking at is gone, and nothing was saved. That is the shape of the bug this module exists for.
//!
//! The page bridge therefore *cancels* the navigation of a click on a `download` link and records
//! what was offered, and this module puts the bytes where a person can open them: the workspace's
//! own `.cwoffee/downloads`, the same folder the webview's download handler was pointed at. One
//! folder, one naming rule, whether the file arrived through wry's handler or through here.
//!
//! ## What is not allowed
//!
//! * A page choosing a path. The name is a basename with any separator dropped, and the destination
//!   is always under the folder below. `../../.bashrc` has to be impossible rather than checked for.
//! * A page reading the disk. A `file:` offer is only fetched when the file is **inside** the
//!   workspace — the same boundary the panel's address box and the model's file tools use. Outside
//!   it the answer is a refusal naming the rule, not a silent failure.
//! * An endless response. The bytes are capped; a page that offers a 40 GB archive gets the first
//!   slice refused rather than filling the disk.

use std::path::{Path, PathBuf};

/// The cap on one offered file. Large enough for a document, a spreadsheet, an image and a modest
/// archive; small enough that "the page offered the ISO of a distro" is a refusal and not a stall.
pub const MAX_OFFER_BYTES: u64 = 512 * 1024 * 1024;

/// The folder a page's offers land in.
pub fn dir_for(workspace: &Path) -> PathBuf {
    workspace.join(".cwoffee/downloads")
}

/// A name from an address, with nothing that could climb out of the folder.
///
/// Query and fragment are not part of a filename, a trailing separator is not a name, and a name
/// containing `/` or `..` is not a name at all — it is an attempt. What is left is prefixed rather
/// than rejected when nothing usable survives, because "download" with a counter is a file the
/// person can look at and an empty name is not.
pub fn name_for(url: &str) -> String {
    let cut = url.split_once(['#', '?']).map(|(head, _)| head).unwrap_or(url);
    let tail = cut.rsplit('/').next().unwrap_or("");
    let decoded = percent_decode(tail);
    let clean: String = decoded
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0'))
        .collect();
    let clean = clean.trim_matches(|c: char| c == '.' || c == ' ');
    if clean.is_empty() || clean == ".." {
        return "download".to_string();
    }
    clean.chars().take(180).collect()
}

/// The only `%XX` decoding this needs: a filename with a space or a comma in it is ordinary, and
/// doing it here keeps a page from naming `probe%2E%2E` at us.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = &b[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(hex), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `file:` offer's path, if it is one this app may read.
pub fn local_source(url: &str, workspace: &Path) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let p = PathBuf::from(rest);
    let real = p.canonicalize().ok()?;
    real.starts_with(workspace).then_some(real)
}

/// One name that does not collide with what is already there. A page offering the same file twice
/// during a session is normal — a form resubmitted, a link clicked again — and overwriting the
/// first one loses the thing the person was looking at.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..1000 {
        let c = dir.join(format!("{stem} ({n}){ext}"));
        if !c.exists() {
            return c;
        }
    }
    candidate
}

/// Fetch what the page offered and put it in `dir`.
///
/// The answer is the path on success, and a sentence on failure — a sentence that says which rule
/// stopped it, because "the download failed" is not something a person or a model can act on.
pub async fn save(url: &str, workspace: &Path) -> Result<PathBuf, String> {
    let dir = dir_for(workspace);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let name = name_for(url);
    let dest = unique(&dir, &name);

    if let Some(src) = local_source(url, workspace) {
        // A local page offering a local file. Copy it rather than re-reading it through a URL
        // layer, and keep the cap: the boundary that matters is the folder, and the cap is about
        // the disk.
        let meta = std::fs::metadata(&src).map_err(|e| format!("could not read {}: {e}", src.display()))?;
        if meta.len() > MAX_OFFER_BYTES {
            return Err(format!(
                "{} is {} bytes, more than the {} bytes cwoffee will copy for a page",
                src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                meta.len(),
                MAX_OFFER_BYTES
            ));
        }
        std::fs::copy(&src, &dest).map_err(|e| format!("could not copy {}: {e}", src.display()))?;
        return Ok(dest);
    }

    if url.starts_with("http://") || url.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| format!("no way to ask for the file: {e}"))?;
        let mut res = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("the request for {url} did not answer: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("{url} answered {}", res.status()));
        }
        if let Some(len) = res.content_length() {
            if len > MAX_OFFER_BYTES {
                return Err(format!(
                    "{url} says it is {len} bytes, more than the {MAX_OFFER_BYTES} bytes cwoffee will take for a page"
                ));
            }
        }
        let mut got: u64 = 0;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| format!("the body of {url} stopped: {e}"))? {
            got += chunk.len() as u64;
            if got > MAX_OFFER_BYTES {
                return Err(format!(
                    "{url} went past {MAX_OFFER_BYTES} bytes of body, which cwoffee will not save"
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        std::fs::write(&dest, &bytes).map_err(|e| format!("could not write {}: {e}", dest.display()))?;
        return Ok(dest);
    }

    Err(format!(
        "cwoffee does not fetch a file from {url}: only http(s) and a {} page inside the workspace",
        "file:"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_from_an_address_cannot_climb() {
        assert_eq!(name_for("http://x/a/b/report.pdf"), "report.pdf");
        assert_eq!(name_for("http://x/a/b/report.pdf?tok=1#p2"), "report.pdf");
        assert_eq!(name_for("http://x/%2E%2E%2Fetc%2Fpasswd"), "etcpasswd");
        assert_eq!(name_for("file:///tmp/../../etc/passwd"), "passwd");
        assert_eq!(name_for("http://x/a/.."), "download");
        assert_eq!(name_for("http://x/a/"), "download");
        assert_eq!(name_for("http://x/a//"), "download");
        assert_eq!(name_for("http://x/a/\0evil"), "evil");
        let long = name_for(&format!("http://x/{}", "a".repeat(400)));
        assert_eq!(long.chars().count(), 180, "a name is kept, not dropped");
    }

    #[test]
    fn a_local_offer_is_only_read_from_inside_the_folder() {
        let dir = std::env::temp_dir().join(format!("cwoffee-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let inside = dir.join("data.csv");
        std::fs::write(&inside, "a,b\n").unwrap();
        let ws = dir.canonicalize().unwrap();
        assert_eq!(local_source(&format!("file://{}", inside.display()), &ws).as_deref(), Some(inside.as_path()));
        assert_eq!(local_source("file:///etc/passwd", &ws), None, "outside the folder is not offered");
        assert_eq!(local_source("http://x/a.zip", &ws), None, "not a local file at all");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_offered_local_file_is_copied_into_the_downloads_folder() {
        let dir = std::env::temp_dir().join(format!("cwoffee-dl-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("offered.txt");
        std::fs::write(&src, "the bytes the page offered\n").unwrap();
        let ws = dir.canonicalize().unwrap();
        let got = save(&format!("file://{}", src.display()), &ws).await.unwrap();
        assert!(got.starts_with(dir_for(&ws)), "{} is in the downloads folder", got.display());
        assert_eq!(std::fs::read_to_string(&got).unwrap(), "the bytes the page offered\n");
        // The same offer twice does not overwrite the first answer.
        let again = save(&format!("file://{}", src.display()), &ws).await.unwrap();
        assert_ne!(again, got, "the second one is a different file");
        assert!(again.to_string_lossy().contains(" (2)"), "{} is numbered", again.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_address_that_is_not_a_file_says_which_rule_refused_it() {
        let dir = std::env::temp_dir().join(format!("cwoffee-dl-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let why = save("javascript:alert(1)", &dir).await.unwrap_err();
        assert!(why.contains("does not fetch"), "{why}");
        let why = save("file:///etc/passwd", &dir).await.unwrap_err();
        assert!(why.contains("inside the workspace"), "{why}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
