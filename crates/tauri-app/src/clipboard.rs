//! An image copied by another program: the part of "paste an image" the webview
//! cannot do by itself.
//!
//! A screenshot taken with the desktop's own tool lands in the Wayland/X11
//! clipboard, not in a DOM paste event: WebKitGTK hands the page a paste with no
//! `files` and no image item, so `clipboardData` looks empty even though the
//! clipboard plainly holds a PNG. The browser path (`navigator.clipboard.read()`)
//! is not available in an embedded webview either. So the shell reads the
//! selection itself, with the tools the desktop already ships, and hands the page
//! the same shape a pasted file would have produced.
//!
//! Three selection shapes are worth handling, because all three are what people
//! actually copy:
//!   * an image payload (`image/png` from a screenshot tool, `image/jpeg` from an
//!     image viewer),
//!   * a copied *file* (Files/nautilus puts `text/uri-list` on the clipboard and
//!     no bytes at all - the honest move is to read the file it names, because
//!     the user pointed at it themselves),
//!   * plain text, which is not our business: it returns nothing and says so, and
//!     the paste event keeps doing what a paste event does.
//!
//! `wl-clipboard` and `xclip` are the only external binaries involved, both
//! ordinary desktop packages, both optional: with neither installed the answer is
//! a sentence in the composer, not a silent failure.

use serde::Serialize;

/// The engine's per-attachment ceiling (HostConfig::max_attachment_bytes). The
/// number is repeated here rather than read from the host, and that is a known
/// duplication: it exists so a 9 MB screenshot is refused in the composer with a
/// size in it instead of dying at the engine boundary seconds later. If the two
/// ever disagree, the engine still wins.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Image types the model can actually be shown. Anything else is reported back
/// as "the clipboard offered it, this app cannot send it", which is a different
/// complaint from "there is nothing on the clipboard".
const ACCEPTED: &[&str] = &["image/png", "image/jpeg", "image/jpg", "image/webp", "image/gif"];

#[derive(Debug, Clone, Serialize)]
pub struct ClipImage {
    pub mime: String,
    pub bytes: usize,
    pub data_b64: String,
    /// Where it came from: `clipboard:image/png` or `file:/home/u/x.png`. The UI
    /// shows this as the attachment name, so a screenshot pasted from grim is
    /// labelled rather than anonymous.
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClipProbe {
    pub image: Option<ClipImage>,
    /// Everything the clipboard offered. Always filled when a reader exists:
    /// it is the difference between "nothing was copied" and "you copied a 6 MB
    /// TIFF", and the composer says which one happened.
    pub offered: Vec<String>,
    /// Empty when the image arrived or when the clipboard simply had no image.
    /// Non-empty only when the user's intent was clearly an image.
    pub note: String,
}

/// Which external reader this desktop has. Wayland first: a Wayland session with
/// `xclip` present talks to the X clipboard, which is not where a Wayland
/// screenshot went.
#[derive(Debug, PartialEq, Eq)]
enum Reader {
    Wayland,
    X11,
    None,
}

fn pick_reader() -> Reader {
    let has = |bin: &str| {
        std::process::Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && has("wl-paste") {
        Reader::Wayland
    } else if std::env::var_os("DISPLAY").is_some() && has("xclip") {
        Reader::X11
    } else {
        Reader::None
    }
}

/// Prefer the accepted image types in order; a type we cannot send is not a
/// reason to pretend the clipboard was empty.
fn pick_mime(offered: &[String]) -> Option<String> {
    for want in ACCEPTED {
        if let Some(have) = offered.iter().find(|t| t.eq_ignore_ascii_case(want)) {
            return Some(have.clone());
        }
    }
    None
}

/// Extensions accepted as pasted image *files* (the uri-list path). Checked on a
/// copied file's name, because a file manager offers no MIME type.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "svg"];

fn image_ext(path: &str) -> Option<String> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if ext == path && !path.contains('.') {
        return None;
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        Some(match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg".to_string(),
            "tif" | "tiff" => "image/tiff".to_string(),
            other => format!("image/{other}"),
        })
    } else {
        None
    }
}

/// `text/uri-list` lines to paths. Only `file:` URIs, percent-decoded, because
/// that is what a desktop puts there when someone copies a file.
fn uris_to_paths(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let rest = l.strip_prefix("file://")?;
            let decoded = percent_decode(rest);
            Some(if decoded.is_empty() { rest.to_string() } else { decoded })
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal base64: the engine expects unpadded-free standard alphabet, which is
/// what a data: URL carries. (No external crate enters the binary for this.)
pub fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn run(bin: &str, args: &[&str], stdin_empty: bool) -> Result<(Vec<u8>, Vec<u8>), String> {
    use std::io::Write;
    use std::process::{Stdio, Command};
    let mut c = Command::new(bin);
    c.args(args)
        .stdin(if stdin_empty { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("cannot run {bin}: {e}"))?;
    if stdin_empty {
        // An immediate write-then-close: wl-paste and xclip both read stdin when
        // given a pipe, and leaving it open would hang them.
        if let Some(mut si) = child.stdin.take() {
            si.write_all(&[]).ok();
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{bin} failed: {e}"))?;
    Ok((out.stdout, out.stderr))
}

/// Read one image off the clipboard, whatever shape it is in.
pub async fn read_image() -> ClipProbe {
    tokio::task::spawn_blocking(read_image_blocking)
        .await
        .unwrap_or_else(|e| ClipProbe { image: None, offered: Vec::new(), note: format!("clipboard reader failed: {e}") })
}

fn read_image_blocking() -> ClipProbe {
    let mut p = ClipProbe::default();
    match pick_reader() {
        Reader::None => {
            p.note = "no clipboard reader found: install wl-clipboard (Wayland) or xclip (X11) \
                      to paste images from other programs"
                .into();
            return p;
        }
        Reader::Wayland => {
            let (types, _) = match run("wl-paste", &["-l"], true) {
                Ok(v) => v,
                Err(e) => {
                    p.note = e;
                    return p;
                }
            };
            p.offered = String::from_utf8_lossy(&types)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if let Some(mime) = pick_mime(&p.offered) {
                let (bytes, _) = match run("wl-paste", &["-n", "-t", mime.as_str()], true) {
                    Ok(v) => v,
                    Err(e) => {
                        p.note = e;
                        return p;
                    }
                };
                let source = format!("clipboard:{mime}");
                return finish(p, mime, bytes, source);
            }
            if let Some(pos) = p.offered.iter().position(|t| t == "text/uri-list") {
                let _ = pos;
                let (bytes, _) = match run("wl-paste", &["-n", "-t", "text/uri-list"], true) {
                    Ok(v) => v,
                    Err(e) => {
                        p.note = e;
                        return p;
                    }
                };
                return from_uris(p, &String::from_utf8_lossy(&bytes));
            }
        }
        Reader::X11 => {
            let (targets, _) = match run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"], true) {
                Ok(v) => v,
                Err(e) => {
                    p.note = e;
                    return p;
                }
            };
            p.offered = String::from_utf8_lossy(&targets)
                .split_whitespace()
                .map(|s| s.trim_matches('"').to_string())
                .filter(|s| s != "TARGETS" && s != "TIMESTAMP" && s != "SAVE_TARGETS")
                .collect();
            if let Some(mime) = pick_mime(&p.offered) {
                let (bytes, _) = match run(
                    "xclip",
                    &["-selection", "clipboard", "-t", mime.as_str(), "-o"],
                    true,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        p.note = e;
                        return p;
                    }
                };
                let source = format!("clipboard:{mime}");
                return finish(p, mime, bytes, source);
            }
            if p.offered.iter().any(|t| t == "text/uri-list") {
                let (bytes, _) = match run(
                    "xclip",
                    &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
                    true,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        p.note = e;
                        return p;
                    }
                };
                return from_uris(p, &String::from_utf8_lossy(&bytes));
            }
        }
    }
    p
}

/// A copied file is read from disk: the user pointed at it, and the clipboard
/// carried a name rather than bytes.
fn from_uris(mut p: ClipProbe, list: &str) -> ClipProbe {
    let paths = uris_to_paths(list);
    for path in &paths {
        if let Some(mime) = image_ext(path) {
            match std::fs::read(path) {
                Ok(bytes) => return finish(p, mime, bytes, format!("file:{path}")),
                Err(e) => {
                    p.note = format!("the clipboard named {path} and it could not be read: {e}");
                    return p;
                }
            }
        }
    }
    p
}

fn finish(mut p: ClipProbe, mime: String, bytes: Vec<u8>, source: String) -> ClipProbe {
    if bytes.is_empty() {
        p.note = format!("the clipboard offered {mime} and sent no bytes");
        return p;
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        p.note = format!(
            "that image is {:.1} MB and this app sends images up to {} MB per attachment",
            bytes.len() as f64 / 1_048_576.0,
            MAX_IMAGE_BYTES / 1_048_576,
        );
        return p;
    }
    let declared = mime;
    // A file named .png that holds a TIFF is sent as what it is, not what it is
    // called: the sniff below overrides the guessed type for the common cases.
    let mime = sniff(&bytes, declared.clone());
    if !ACCEPTED.iter().any(|a| a.eq_ignore_ascii_case(&mime)) {
        p.note = format!(
            "the clipboard offered {} ({} kB); this app sends PNG, JPEG, WebP and GIF to the model",
            mime,
            bytes.len() / 1024
        );
        return p;
    }
    let name = source.clone();
    p.image = Some(ClipImage {
        mime,
        bytes: bytes.len(),
        data_b64: b64_encode(&bytes),
        source: name,
    });
    p
}

/// Magic-number sniff for the handful of formats that can actually arrive.
fn sniff(b: &[u8], fallback: String) -> String {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png".into()
    } else if b.starts_with(b"\xff\xd8\xff") {
        "image/jpeg".into()
    } else if b.starts_with(b"GIF8") {
        "image/gif".into()
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp".into()
    } else if b.starts_with(b"BM") {
        "image/bmp".into()
    } else if b.starts_with(b"<svg") || b.starts_with(b"<?xml") {
        // An SVG pasted as a file is vector text; sending it as an image would
        // show the model nothing, so it is named as what it is.
        "image/svg+xml".into()
    } else {
        fallback
    }
}

/// Put the page harness's own fixture back on the selection.
///
/// The selection is desktop-wide and a board runs for a quarter of an hour: a person copying
/// an email, or one of the board's own checks copying an answer, replaces the fixture image
/// with text, and the paste check then reports "nothing sendable" about a fixture that
/// stopped existing mid-run. The board names its file in `CWOFFEE_SELFTEST_CLIP_IMG`; with
/// that variable unset this does nothing, so a running app cannot be talked into overwriting
/// what the user copied. It refuses an image the model cannot be shown, for the same reason
/// `finish` refuses it on the way in.
pub fn stage_fixture() -> Result<serde_json::Value, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let path = match std::env::var("CWOFFEE_SELFTEST_CLIP_IMG") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Err("this run staged no clipboard fixture: CWOFFEE_SELFTEST_CLIP_IMG is unset".to_string()),
    };
    let meta = std::fs::metadata(&path).map_err(|e| format!("the fixture {path} cannot be read: {e}"))?;
    if !meta.is_file() {
        return Err(format!("CWOFFEE_SELFTEST_CLIP_IMG={path} is not a file"));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read {path}: {e}"))?;
    let mime = sniff(&bytes, image_ext(&path).unwrap_or_else(|| "application/octet-stream".into()));
    if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/webp" | "image/gif") {
        return Err(format!(
            "the fixture {path} is {mime}, which the model cannot be shown; the board should stage a PNG"
        ));
    }
    // wl-copy first, because this desktop runs Wayland; xclip for an X11 session.
    //
    // Neither tool's process is the one that ends up owning the selection: both fork a
    // resident child that holds the offer until something replaces it. `read_image` can pipe
    // and wait because its tools (`wl-paste`, `xclip -o`) exit; these do not. Capturing their
    // stdout would keep the pipe open through that resident child and `wait` would never
    // return — which is how a self-test verb hung a whole page harness. So stdout goes to
    // null, and stderr to a file: a diagnosis that does not hold a pipe open.
    let err_log = std::env::temp_dir().join("cwoffee-selftest-clipboard.err");
    let attempts: [(&str, Vec<&str>); 2] = [
        ("wl-copy", vec!["--type", mime.as_str()]),
        ("xclip", vec!["-selection", "clipboard", "-t", mime.as_str(), "-mode", "i"]),
    ];
    let mut failures = Vec::new();
    for (bin, args) in attempts {
        let stderr_file = std::fs::File::create(&err_log).ok();
        let spawned = Command::new(bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(match stderr_file {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{bin}: {e}"));
                continue;
            }
        };
        if let Some(mut si) = child.stdin.take() {
            if let Err(e) = si.write_all(&bytes) {
                failures.push(format!("{bin}: could not hand over the bytes: {e}"));
                continue;
            }
        }
        match child.wait() {
            Ok(st) if st.success() => {
                return Ok(serde_json::json!({
                    "path": path,
                    "mime": mime,
                    "bytes": bytes.len(),
                    "via": bin,
                }));
            }
            Ok(st) => failures.push(format!(
                "{bin}: exit {st}; see {}",
                err_log.display()
            )),
            Err(e) => failures.push(format!("{bin}: {e}")),
        }
    }
    Err(format!("nothing could take the fixture: {}", failures.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_alphabet_the_providers_expect() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        // Bytes that used to break naive encoders: >127 and NUL.
        assert_eq!(b64_encode(&[0x00, 0xff, 0x10]), "AP8Q");
        // Round trip through decode for a longer blob.
        let big: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let enc = b64_encode(&big);
        assert_eq!(enc.len(), (5000 + 2) / 3 * 4);
        assert!(enc.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn an_accepted_image_wins_over_the_rest_of_the_selection() {
        let offered = ["TEXT", "UTF8_STRING", "image/tiff", "image/png"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(pick_mime(&offered).as_deref(), Some("image/png"));
        // A TIFF-only selection is not "empty": the caller must be able to say
        // what was there.
        let offered = ["image/tiff".to_string()];
        assert_eq!(pick_mime(&offered), None);
        let offered = ["TEXT".to_string(), "UTF8_STRING".to_string()];
        assert_eq!(pick_mime(&offered), None);
    }

    #[test]
    fn a_copied_png_file_is_recognised_and_a_copied_mp4_is_not() {
        assert_eq!(image_ext("/home/u/Screenshot from 2026-10-12.png").as_deref(), Some("image/png"));
        assert_eq!(image_ext("/a/b/JPEG.JPG").as_deref(), Some("image/jpeg"));
        assert_eq!(image_ext("/a/b/clip.mp4"), None);
        assert_eq!(image_ext("Makefile"), None);
    }

    #[test]
    fn uri_list_lines_become_paths_spaces_and_all() {
        let list = "file:///home/u/Downloads/Screenshot%202026-10-12.png\nfile:///tmp/x.jpeg\n";
        let p = uris_to_paths(list);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], "/home/u/Downloads/Screenshot 2026-10-12.png");
        assert_eq!(p[1], "/tmp/x.jpeg");
        // A comment line and a non-file scheme are not something to open.
        let p = uris_to_paths("# comment\nhttp://example.com/a.png\nfile:///ok.png\n");
        assert_eq!(p, vec!["/ok.png"]);
    }

    #[test]
    fn a_pasted_image_is_described_by_its_bytes_not_its_name() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 1, 2, 3];
        assert_eq!(sniff(&png, "image/gif".into()).as_str(), "image/png");
        assert_eq!(sniff(b"\xff\xd8\xff\xe0xx", "application/octet-stream".into()), "image/jpeg");
        assert_eq!(sniff(b"GIF89a...", "image/png".into()), "image/gif");
        // A BMP the clipboard offered is real, and the model cannot be shown it.
        assert_eq!(sniff(b"BM....", "image/png".into()), "image/bmp");
    }

    /// The size refusal has to name both numbers: a silent drop of a big
    /// screenshot reads as "paste does not work".
    #[test]
    fn an_oversized_image_is_refused_with_both_sizes_in_the_message() {
        let p = ClipProbe::default();
        // 5 MB, comfortably past the ceiling: the message is for a person who
        // just screenshotted a 5K display, not for a boundary-condition log.
        let big = vec![0u8; 5 * 1024 * 1024];
        let out = finish(p, "image/png".into(), big, "clipboard:image/png".into());
        assert!(out.image.is_none());
        assert!(out.note.contains("5.0 MB"), "{}", out.note);
        assert!(out.note.contains("2 MB"), "{}", out.note);
        // A raw float in a user-facing sentence is not acceptable: this used to
        // read "2.0000009536743164 MB" for one byte past the limit.
        assert!(!out.note.contains("MB and this app sends images up to 2.0 MB"), "{}", out.note);
    }

    #[test]
    fn empty_bytes_say_so_rather_than_attaching_nothing() {
        let out = finish(ClipProbe::default(), "image/png".into(), Vec::new(), "x".into());
        assert!(out.image.is_none());
        assert!(out.note.contains("no bytes"), "{}", out.note);
    }

    /// A BMP copied from a GIMP canvas is a legitimate clipboard content; the
    /// honest answer is that this app cannot send it, not that the clipboard was
    /// empty.
    #[test]
    fn an_unsendable_image_names_the_formats_that_are_sendable() {
        let out = finish(ClipProbe::default(), "image/bmp".into(), b"BMxxxx".to_vec(), "c".into());
        assert!(out.image.is_none());
        assert!(out.note.contains("PNG, JPEG, WebP and GIF"), "{}", out.note);
    }
}
