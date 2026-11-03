//! Cwoffee desktop host.
//!
//! Thin by design. Everything interesting lives in `crates/engine`; this file
//! wires one up and gets out of the way.

mod clipboard;
mod commands;
mod gpu;
mod page_embed;
mod page_view;
mod gateway_ctl;

/// Whether the kept browser in this window should be a **page in this window** rather than a browser
/// in another process whose pictures are relayed here. On unless asked otherwise, because the page is
/// the shape where a person and a model share one document.
///
/// Read as a switch with words in it, not as `== "0"`: `scripts/selftest.sh` exports `no` for the
/// board that grades the picture column, and a check written as "is it exactly zero" would have that
/// board start a page anyway and then grade it as if it had not.
fn page_in_window_asked() -> bool {
    match std::env::var("CWOFFEE_BROWSER_PAGE").map(|v| v.trim().to_lowercase()) {
        Ok(v) if matches!(v.as_str(), "0" | "no" | "off" | "false" | "never") => false,
        _ => true,
    }
}

/// A page written to answer the acceptance questions and nothing else: it prints to the console, it
/// throws on its own timer, it asks for two things that are not there, and it has a button whose
/// text changes when something clicks it. That "something" being the model is what is being tested.
const PROBE_PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>probe page</title>
<h1 id="t">probe page</h1>
<button id="b">press me</button>
<p id="out">press not yet</p>
<button id="boom">explode</button>
<button class="dup">capybara one</button>
<button class="dup">capybara two</button>
<a id="next" href="probe-page-2.html">next page</a>
<a id="dl" href="probe-download.zip" download>save the archive</a>
<a id="dls" href="probe-download.txt" download>show the text</a>
<a id="dl" href="probe-download.txt" download>save the text</a>
<img src="this-image-does-not-exist.png" alt="gone">
<script>
  // What the page itself saw of its own errors, so the probe can tell "WebKit never told anyone"
  // from "the bridge's listener did not run".
  window.__seen = []
  window.addEventListener('error', function (e) {
    window.__seen.push('win-event:' + e.message + ':' + e.filename + ':' + e.lineno)
  })
  window.onerror = function (m, s, l) { window.__seen.push('onerror:' + m + ':' + s + ':' + l) }
  window.addEventListener('unhandledrejection', function (e) { window.__seen.push('rejection:' + e.reason) })
  console.log('hello from the page')
  console.warn('a warning, not a failure')
  console.error('the page is unhappy')
  document.getElementById('b').addEventListener('click', function () {
    document.getElementById('out').textContent =
      'clicked by ' + (window.__cw ? 'cwoffee' : 'somebody')
  })
  document.getElementById('boom').addEventListener('click', function () { null.explode })
  setTimeout(function () { throw new Error('a page error on purpose') }, 150)
  Promise.reject(new Error('a rejection on purpose'))
  fetch('this-file-does-not-exist.json').catch(function () {})
</script>
<script>
  // A top-level ReferenceError, in its own block so it does not stop the one above.
  aFunctionThatIsNotThere()
</script>
"#;

/// The second page: it says where it came from, and then moves somewhere else on its own timer, so
/// the probe can prove the hub notices a navigation neither the person nor the model asked for.
const PROBE_PAGE_2: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>second page</title>
<h1>second page</h1>
<p>arrived from <span id="from"></span></p>
<script>
  document.getElementById('from').textContent = document.referrer || 'nowhere'
  setTimeout(function () { location.href = 'probe-page-3.html' }, 900)
</script>
"#;

const PROBE_PAGE_3: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>third page</title>
<h1>third page</h1>
<p>the page moved itself here</p>
"#;

/// Drive the page-in-the-window once, in order, and report each step as it answered. Written as one
/// straight line of calls because every part of this chain fails quietly: a view that was never
/// placed shows as an empty box, a bridge that never started shows as a step that timed out, and a
/// hub that never attached its driver shows as nothing at all.
/// A page floats over the window's own border, and the border is part of the window: the overlay's
/// fixed container deliberately sits at -1,-1 so the page can cover the frame, which means an ask for
/// (400,300) can land at (399,299). One CSS pixel of slack per axis, so the geometry claims grade the
/// page's box rather than the window chrome — and stop passing or failing on where the window opened.
fn near(got: u32, want: u32) -> bool {
    got.abs_diff(want) <= 1
}

async fn probe_page(
    hub: &std::sync::Arc<tokio::sync::Mutex<cwoffee_engine::browser_hub::Hub>>,
    handle: &tauri::AppHandle,
    ws: &std::path::Path,
) -> String {
    use cwoffee_engine::browser_hub::Who;
    use serde_json::json;
    let mut got: Vec<(&'static str, bool, String)> = Vec::new();
    let page = ws.join(".cwoffee/probe-page.html");
    let _ = std::fs::create_dir_all(page.parent().unwrap_or(ws));
    if let Err(e) = std::fs::write(&page, PROBE_PAGE) {
        return format!("fail: could not write the probe page: {e}");
    }
    let dir = page.parent().unwrap_or(ws);
    for (name, body) in [
        ("probe-page-2.html", PROBE_PAGE_2),
        ("probe-page-3.html", PROBE_PAGE_3),
        ("probe-download.txt", "a file the probe page offers to the download handler\n"),
        // Bytes with an extension no browser displays, which is the shape of a real download.
        ("probe-download.zip", "PK\u{3}\u{4} not really a zip, but the browser does not know that"),
    ] {
        if let Err(e) = std::fs::write(dir.join(name), body) {
            return format!("fail: could not write {name}: {e}");
        }
    }
    let url = format!("file://{}", page.display());
    {
        let mut h = hub.lock().await;
        // Where the panel's box would be, if the panel were open.
        if let Err(e) = h.view_rect(140.0, 140.0, 620.0, 420.0).await {
            return format!("fail: view_rect: {e}");
        }
        match h.open(&url, Who::Person, true).await {
            Ok(out) => eprintln!("PAGE-PROBE opened: {out}"),
            Err(e) => return format!("fail: open: {e}"),
        }
        // 1. The model reads the page it is standing in front of.
        let act = h
            .act(json!({"do": "text"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        let text = act
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        got.push(("the model read the page's own text", text.contains("press not yet"), text));
        // 1b. One name, two matches. The pair is the claim: **looking** is answered by whichever
        // arrived ("has it appeared?" is not a question about which one), and **acting** is still
        // refused, because guessing harms an element the person may be looking at. A refusal that
        // names no candidates costs the model another guess.
        let looked = h
            .act(json!({"do": "wait_for", "target": "capybara"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        got.push((
            "waiting on a name the page has several of is answered, not refused",
            looked.get("ok").and_then(|v| v.as_bool()) == Some(true)
                && looked
                    .get("note")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .contains("2 things matched"),
            looked.to_string(),
        ));
        let aimed = h
            .act(json!({"do": "click", "target": "capybara"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        let why = aimed.get("why").and_then(|v| v.as_str()).unwrap_or("").to_string();
        got.push((
            "a click on that same name is refused, and shows what the candidates are",
            aimed.get("ok").and_then(|v| v.as_bool()) == Some(false)
                && why.contains("capybara one")
                && why.contains("capybara two")
                && why.contains("Say which one"),
            why,
        ));
        // 2. The model clicks. The person watching sees exactly this happen.
        let clicked = h
            .act(json!({"do": "click", "target": "press me"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        let after = h
            .act(json!({"do": "eval", "code": "document.getElementById('out').textContent"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        let changed = after.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
        got.push((
            "a click by the model changed the page for the person looking",
            changed == "clicked by cwoffee",
            format!("click={clicked:?} read back={changed:?}"),
        ));
        // 3. The page's own diagnostics reached the hub on the driver's own timer. Nobody asked.
        // What the page itself thinks it collected. If the hub is missing something the page has,
        // the driver is at fault; if the page never collected it, the bridge is. Note the throw in
        // the probe page: WebKit reports an uncaught exception to the *local* console (the
        // inspector's), which a script cannot intercept — so the page's own console.error and the
        // resource failure are what the bridge can prove. `window.__cw.di` is read through the
        // bridge's own record list, and the page's `__cwDi` is a copy written from inside the page.
        let own = h
            .act(json!({"do": "eval", "code": "JSON.stringify({fromBridge: window.__cw.di.map(function(d){return d.kind+':'+d.level+':'+(d.where||'')}), fromPage: (window.__seen||[])})"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        eprintln!("PAGE-PROBE {}", own.get("value").and_then(|v| v.as_str()).unwrap_or("?"));
        let st = h.peek().clone();
        eprintln!(
            "PAGE-PROBE the hub kept {} diagnostics: {}",
            st.diagnostics.len(),
            st.diagnostics
                .iter()
                .map(|d| format!(
                    "{}:{}:{}{}",
                    d.kind,
                    d.level,
                    d.text.chars().take(34).collect::<String>(),
                    d.url.as_deref().map(|u| format!(" @{u}")).unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(" | ")
        );
        let has = |kind: &str, level: &str| st.diagnostics.iter().any(|d| d.kind == kind && d.level == level);
        got.push(("a console.log reached the model as a message, not a failure", has("console", "info"), String::new()));
        got.push(("a console.error reached as a message at error level, not as an exception", has("console", "error"), String::new()));
        got.push((
            "a page error this browser will not name says that, and says where to read it",
            st.diagnostics
                .iter()
                .any(|d| {
                    d.kind == "error"
                        && (d.text.contains("would not say what")
                            || d.text.contains("a page error on purpose")
                            || d.text.contains("Cannot read"))
                }),
            format!(
                "{:?}",
                st.diagnostics
                    .iter()
                    .filter(|d| d.kind == "error")
                    .map(|d| format!("{} @{}", d.text, d.at.clone().unwrap_or_default()))
                    .collect::<Vec<_>>()
            ),
        ));
        got.push((
            "a failed network request reached with its url and status",
            st.diagnostics
                .iter()
                .any(|d| d.kind == "network" && d.url.as_deref().unwrap_or("").contains("does-not-exist")),
            format!("{:?}", st.diagnostics.iter().find(|d| d.kind == "network").cloned()),
        ));
        got.push((
            "a resource that did not load is its own kind of failure, not a console line",
            has("resource", "error"),
            String::new(),
        ));
        // 4. The page can be inspected by role and name, and that names the thing to aim at.
        let a11y = h
            .act(json!({"do": "a11y"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        // Text, not a bag of objects. The app prints a step's `value` as the words it gave back, so an
        // array here would be a read that reports success and says nothing — which is what the model
        // sees, and what this claim exists to catch. `total` still counts everything the page has.
        let said = a11y
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        got.push((
            "the page could be inspected by role and name",
            said.contains("button") && said.contains("press me"),
            format!(
                "{} line(s) of {} element(s), first: {:?}",
                said.lines().count(),
                a11y.get("total").and_then(|v| v.as_u64()).unwrap_or(0),
                said.lines().next()
            ),
        ));
        // 4b. A wait in the page waits, and says it did, in the page's own clock. This is the claim the
        // sidebar browser's `wait` used to fail: the step answered `ok` in three milliseconds, so every
        // measurement a model took across a wait read as a page whose timers had stopped.
        let t0 = h
            .act(json!({"do": "eval", "code": "Date.now()"}), Who::Model)
            .await
            .unwrap_or_default();
        let mark = t0.get("value").and_then(|v| v.as_str()).unwrap_or("0").to_string();
        let w = h
            .act(json!({"do": "wait", "ms": 600}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        let page_clock = h
            .act(
                json!({"do": "eval", "code": &format!("Date.now() - {mark}")}),
                Who::Model,
            )
            .await
            .unwrap_or_default();
        let moved = page_clock
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("0")
            .parse::<u64>()
            .unwrap_or(0);
        let said_ms = w.get("waited").and_then(|v| v.as_u64()).unwrap_or(0);
        got.push((
            "a wait waits, in the page and by the page's own clock",
            w.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) && said_ms >= 550 && moved >= 550,
            format!("the step says it waited {said_ms}ms; the page's clock moved {moved}ms"),
        ));
        // A thing that is not there is looked for, for the time the step was given, and then said for.
        let wf = h
            .act(json!({"do": "wait_for", "target": "a button that does not exist", "ms": 700}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        let why = wf.get("why").and_then(|v| v.as_str()).unwrap_or("").to_string();
        got.push((
            "a wait_for that gives up says how long it looked",
            !wf.get("ok").and_then(|v| v.as_bool()).unwrap_or(true) && why.contains("looked for"),
            why,
        ));
        // 5. Geometry: the page *is* the box it was given — where the widget is, not merely where it
        // was asked to be. The app's own panel is placing the same widget on its own timer (it is
        // running for real in this window), so the ask is re-issued until it is the one that landed,
        // and then both numbers are read: the widget's box and the page's own viewport have to be the
        // box, and a page that answers every command while sitting somewhere else is still broken.
        let mut size = "?".to_string();
        let mut at = (0, 0, 0, 0, false);
        // Twelve asks, not eight: the placement lands on the app's own timer, and a probe run straight
        // after a release build competes with a busy machine for it. A geometry claim that fails under
        // load and passes a minute later is worse than no claim — it teaches nobody to distrust it.
        //
        // And the window's panel is told to leave the box alone for the duration: it places the same
        // widget from its own layout, and two placers means whichever reported last won — the check
        // used to flake on whether the panel happened to redraw in the middle of it.
        let holds = {
            use tauri::Manager;
            handle.state::<AppState>().probe_holds_box.clone()
        };
        holds.store(true, std::sync::atomic::Ordering::SeqCst);
        for _ in 0..12 {
            let _ = h.view_rect(400.0, 300.0, 300.0, 200.0).await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let s0 = h.peek();
            if !(near(s0.placed_w, 300) && near(s0.placed_h, 200) && near(s0.placed_x, 400) && near(s0.placed_y, 300)) {
                continue;
            }
            let geom = h
                .act(json!({"do": "eval", "code": "innerWidth + 'x' + innerHeight"}), Who::Model)
                .await
                .unwrap_or_else(|e| json!({"ok": false, "why": e}));
            size = geom.get("value").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            at = (s0.placed_x, s0.placed_y, s0.placed_w, s0.placed_h, s0.placed_shown);
            if size == "300x200" {
                break;
            }
        }
        got.push(("the page is the box the panel asked for, 300x200", size == "300x200", format!("the page says it is {size}")));
        // The widget's own box, from the widget tree. A page can report the right viewport while its
        // widget is in the wrong place or not shown at all, and the person sees an empty frame; size
        // alone cannot see that, so the position is graded here too.
        got.push((
            "the page widget is the box, at the place the panel asked for",
            at.2 == 300 && at.3 == 200 && near(at.0, 400) && near(at.1, 300) && at.4,
            format!("widget {}x{} at {},{} shown={} (asked 300x200 at 400,300)", at.2, at.3, at.0, at.1, at.4),
        ));
        // The panel gets its box back: from here the window lays the page out again, which is what the
        // later claim about the app's own page filling the window depends on.
        holds.store(false, std::sync::atomic::Ordering::SeqCst);
        // 6. A page already on screen has no picture of itself. That is not an error; it is the
        //    point of doing this instead of streaming frames.
        let pic = h.frame(false).await.map(|_| ());
        let refused = match &pic {
            Ok(_) => "a picture of a page that is already on screen".to_string(),
            Err(e) => e.clone(),
        };
        got.push(("no screenshot is taken of a page that is on screen", pic.is_err(), refused));
        // 7. A link the person could have clicked: the model follows it, and the state that comes
        //    back is the page on screen, not the page it was told about.
        let link = h
            .act(json!({"do": "click", "target": "next page"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        // Polled, not timed: a navigation is a thing the page does on its own schedule, and this
        // page moves on again a moment after arriving. What is being claimed is that the hub stops
        // talking about the page the click left, so any page past the first is the answer.
        let mut moved = h.peek().clone();
        for _ in 0..40 {
            if !moved.url.ends_with("probe-page.html") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            moved = h.peek().clone();
        }
        got.push((
            "a click on a link moved the page the hub talks about",
            !moved.url.ends_with("probe-page.html"),
            format!("click={link:?} url={}", moved.url),
        ));
        // And the step that moved it says so. An old document stays scriptable for a moment after
        // its click, so a step can answer truthfully about a page that no longer exists; without the
        // word the model reads a correct answer as a failed click and tries the click again.
        got.push((
            "the step that navigated says the answer came from the page it left",
            link.get("navigated").and_then(serde_json::Value::as_bool).unwrap_or(false)
                && link
                    .get("url_after")
                    .and_then(serde_json::Value::as_str)
                    .map(|to| !to.is_empty() && to != link.get("url_from").and_then(serde_json::Value::as_str).unwrap_or(""))
                    .unwrap_or(false),
            format!("{link:?}"),
        ));
        // 8. And now the page moves *itself*, on its own timer, with no act in front of it. The hub
        //    has to notice, or every reader of it is looking at a page that is no longer there.
        let mut unasked = h.peek().clone();
        for _ in 0..40 {
            if unasked.url.ends_with("probe-page-3.html") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            unasked = h.peek().clone();
        }
        got.push((
            "a navigation nobody asked for reaches the hub on the driver's own timer",
            unasked.url.ends_with("probe-page-3.html"),
            format!("the hub still thinks the url is {}", unasked.url),
        ));
        let text3 = h
            .act(json!({"do": "text"}), Who::Model)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "why": e}));
        got.push((
            "and the model reads the page that is actually there now",
            text3
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("the page moved itself here"),
            format!("{text3:?}"),
        ));
        // Fragment history: the panel's buttons walk a page's own history, and a reload re-reads
        // the page that is open. Fragments are the case that exposes a driver that only ever looks
        // at document loads — the address changes and nothing is fetched, so a hub that trusts its
        // own last answer shows an address the page left behind.
        let p1 = format!("{url}#first");
        let _ = h.open(&p1, Who::Person, true).await;
        let _ = h.open(&format!("{url}#second"), Who::Person, true).await;
        let mut at2 = h.peek().clone();
        for _ in 0..24 {
            if at2.url.ends_with("#second") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            at2 = h.peek().clone();
        }
        got.push(("the panel knows the second address", at2.url.ends_with("#second"), format!("url={}", at2.url)));
        // **A page opened again is not a page that spoke again.** Every `browse` call passes a url, so
        // the driver sees an open on each one; when that open re-arms the cursor of a document that is
        // still the document, the log fills a second time with records the page had already handed
        // over — same text, same instant, new ids — and `health` counts a page's words twice. The
        // tell is a kind/text/timestamp triple that appears twice.
        let (twice, held) = {
            let all = h.peek().diagnostics_value(0, None, None, 500);
            let rows = all.get("records").and_then(|r| r.as_array()).cloned().unwrap_or_default();
            let mut seen: std::collections::HashSet<(String, i64, i64, String, String)> =
                std::collections::HashSet::new();
            let mut dup = 0usize;
            for r in &rows {
                // **What a replay looks like.** The page numbers its own records from one per
                // document, and the app hands out the ids a drawer keys rows by; a record that
                // arrives twice for one page shows up as the same page-side id, kind and text under
                // two app ids. The app's own record of a move is counted once per move already, so
                // it is not what this is looking for.
                let key = (
                    r.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    r.get("nav").and_then(|v| v.as_i64()).unwrap_or(-1),
                    r.get("seq").and_then(|v| v.as_i64()).unwrap_or(-1),
                    r.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    r.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                );
                if key.0 == "hub" {
                    continue;
                }
                if !seen.insert(key) {
                    dup += 1;
                }
            }
            (dup, rows.len())
        };
        got.push((
            "opening the page again does not replay the records it already gave",
            twice == 0,
            format!("{twice} of {held} record(s) arrived twice for the same page"),
        ));
        // The history has to exist before Back can walk it. A page opened by rebuilding the widget
        // arrives with a history of one and no way back, which looks like a button that does nothing.
        let hl = h
            .act(serde_json::json!({"do": "eval", "code": "String(history.length)"}), Who::Person)
            .await
            .unwrap_or_default();
        got.push((
            "the page kept a history to walk back through",
            hl.get("value").and_then(|v| v.as_str()).unwrap_or("0").parse::<u32>().unwrap_or(0) >= 2,
            serde_json::to_string(&hl.get("value")).unwrap_or_default(),
        ));
        let when = std::time::Instant::now();
        let backr = h.act(serde_json::json!({"do": "back"}), Who::Person).await;
        let took = when.elapsed();
        // Answered *now*, not after the walk's budget. The step's own answer is written into the document
        // `history.back()` is on its way out of, so the app has to notice the move and close the step —
        // which is what used to cost 20 seconds and a "the page did not answer" sentence.
        got.push((
            "Back is answered in the moment it happens, not after the step's whole budget",
            took.as_millis() < 3_000,
            format!("{took:?}; the answer was {}", backr.unwrap_or_default()),
        ));
        let mut back = h.peek().clone();
        for _ in 0..24 {
            if back.url.ends_with("#first") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            back = h.peek().clone();
        }
        got.push(
            ("Back takes one step, and the hub knows it did", back.url.ends_with("#first"), format!("url={}", back.url)),
        );
        let nav_after_back = h.peek().nav;
        let _ = h.act(serde_json::json!({"do": "reload"}), Who::Person).await;
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        let re = h.peek().clone();
        got.push(
            ("Reload re-reads the page that is open; it does not walk the history", re.url.ends_with("#first"), format!("url={}", re.url)),
        );
        got.push(
            (
                "and a reload of one page does not become a second page",
                re.nav == nav_after_back || re.nav == nav_after_back + 1,
                format!("nav {nav_after_back} → {}", re.nav),
            ),
        );
        // And where does the page think it is? Asked of the document itself, not of the app's
        // bookkeeping. If the embedding were rewriting or reloading the document behind the walk,
        // the page would be the first to notice, and this is the check that says so.
        let here = h
            .act(serde_json::json!({"do": "eval", "code": "location.href"}), Who::Model)
            .await
            .unwrap_or_else(|e| serde_json::json!({"value": e}))
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let said = h.peek().url.clone();
        got.push((
            "the page's own address is the address the app is holding",
            here.ends_with("#first") && said.ends_with("#first"),
            format!("the page says {here}, the app says {said}"),
        ));

        let st = h.state().await.unwrap_or_default();
        eprintln!(
            "PAGE-PROBE state url={} title={} nav={} live={} native={} console={} errors={} failed={}",
            st.url, st.title, st.nav, st.live, st.native, st.console.len(), st.page_errors.len(), st.requests_failed.len()
        );
    }
    // 7. The app's own page still fills the window. The browser floats over it; it does not
    //    displace the chat, and it does not cover it either.
    let app_size = match handle.get_webview_window("main") {
        Some(w) => {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let _ = w.eval_with_callback("String(innerWidth)+'x'+String(innerHeight)", move |v| { let _ = tx.send(v); });
            match tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await {
                Ok(Some(v)) => v.trim_matches('"').to_string(),
                _ => "?".to_string(),
            }
        }
        None => "there is no main window".to_string(),
    };
    let window_size = handle
        .get_webview_window("main")
        .and_then(|w| {
            let scale = w.scale_factor().unwrap_or(1.0);
            w.inner_size()
                .ok()
                // The window reports physical pixels; the page reports CSS pixels. Divide, or the
                // check fails on any display that is not 1x.
                .map(|s| format!("{}x{}", (s.width as f64 / scale).round() as u32, (s.height as f64 / scale).round() as u32))
        })
        .unwrap_or_else(|| "?".into());
    got.push((
        "the app's own page still fills the window",
        app_size == window_size,
        format!("the app page is {app_size}, the window is {window_size}"),
    ));
    // 8. A file the page offers. The report tells a model that `page_log kind: "download"` is where
    // the answer lives, so that kind has to exist and the file has to be somewhere the person can
    // open - and the page the model is driving has to still be there afterwards. WebKitGTK stopped
    // reporting downloads to its embedder, so the whole path (the bridge cancels the navigation, the
    // app fetches, the log answers) exists to replace a signal that no longer fires.
    let dir_dl = ws.join(".cwoffee/downloads");
    let mut h = hub.lock().await;
    let _ = std::fs::remove_dir_all(&dir_dl);
    let zipped = dir_dl.join("probe-download.zip");
    let offer = h
        .act(json!({"do": "click", "target": "save the archive"}), Who::Model)
        .await
        .unwrap_or_else(|e| json!({"ok": false, "why": e}));
    let mut saved = false;
    for _ in 0..24 {
        if zipped.exists() {
            saved = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let stayed = h.peek().url.clone();
    let dls = h.peek().diagnostics_since(0, Some("download"), None, 20);
    got.push((
        "a file the page offers is saved where the report says, and recorded in the log",
        saved && !dls.is_empty(),
        format!(
            "the click answered {} with note {}; {} download record(s) {}; looked for {}",
            offer.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            offer.get("note").and_then(|v| v.as_str()).unwrap_or("(none)"),
            dls.len(),
            serde_json::to_string(&dls.iter().map(|d| format!("{}: {}", d.level, d.text)).collect::<Vec<_>>()).unwrap_or_default(),
            zipped.display()
        ),
    ));
    // The offer is not a navigation. A click that takes the person's page away - and leaves it
    // showing a pile of bytes - is the failure this path replaces.
    got.push((
        "offering a file leaves the page the person was looking at where it was",
        stayed.contains("probe-page.html"),
        format!("the page is still at {stayed}"),
    ));
    // A file this browser *could* display is still a file the page asked to have saved: the page's
    // own `download` attribute decides, not whatever the renderer happens to be able to paint.
    let txt = dir_dl.join("probe-download.txt");
    let _ = h.act(json!({"do": "click", "target": "show the text"}), Who::Model).await;
    let mut put_too = false;
    for _ in 0..24 {
        if txt.exists() {
            put_too = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    got.push((
        "a file the browser could display is still saved, not painted into the page",
        put_too && h.peek().url.contains("probe-page.html"),
        format!("{}; the page is at {}", if put_too { "the file is there" } else { "no file" }, h.peek().url),
    ));
    let mut lines = Vec::new();
    for (name, ok, detail) in &got {
        eprintln!("PAGE-PROBE {} {name}", if *ok { "pass" } else { "FAIL" });
        if !detail.is_empty() {
            eprintln!("           {detail}");
        }
        if !ok {
            lines.push(format!("{name}: {detail}"));
        }
    }
    // `CWOFFEE_PAGE_PROBE_HOLD_MS=4000` leaves the page on screen for a look. The probe answers
    // "is the page in the box the panel asked for" with numbers, and numbers have been argued with;
    // the same question asked of a screenshot settles it. It holds the page, not the app: the exit
    // code and the printed verdict are unchanged.
    if let Ok(ms) = std::env::var("CWOFFEE_PAGE_PROBE_HOLD_MS").map(|v| v.trim().parse::<u64>().unwrap_or(0)) {
        if ms > 0 {
            // Put it where the panel's column would be rather than where the last check left it: the
            // hold answers "what does this look like to a person", and the last check's 300x200 box in
            // the middle of the window is the shape of the bug this whole path was chasing.
            let (mut w, mut h) = (420.0f64, 620.0f64);
            if let Some(win) = handle.get_webview_window("main") {
                if let Ok(s0) = win.inner_size() {
                    let sc = win.scale_factor().unwrap_or(1.0);
                    w = s0.width as f64 / sc;
                    h = s0.height as f64 / sc;
                }
            }
            let col = 400.0f64;
            let _ = hub.lock().await.view_rect((w - col - 16.0).max(0.0), 110.0, col, (h - 150.0).max(200.0)).await;
            eprintln!("PAGE-PROBE holding the page on screen for {ms} ms — a column of {col:.0}px at the right of a {w:.0}x{h:.0} window");
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
    if lines.is_empty() {
        format!("ok {} checks", got.len())
    } else {
        format!("fail {} of {}: {}", lines.len(), got.len(), lines.join(" // "))
    }
}

#[allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cwoffee_engine::approval::ApprovalAnswer;
use cwoffee_engine::host::{approval_event, question_event, HostConfig, HostOps, SessionHost};
use cwoffee_engine::provider::Provider;
use cwoffee_engine::store::{Store, StoreOps};
use cwoffee_engine::tools::Registry;
use cwoffee_engine::Config;
use tokio::sync::{mpsc, oneshot};

/// The provider entry this app writes into pi's provider file (see `pi_host`). Named
/// because the same word appears in the boot log, and a log that cannot be grepped for
/// is a log that will not be grepped.
const PI_PROVIDER: &str = "cwoffee";
use tauri::Manager;

use commands::AppState;

/// Locate a built frontend to serve over the network.
///
/// A packaged cwoffee embeds its assets in the binary, which the webview can
/// read and an HTTP listener cannot. So the gateway needs a real directory; the
/// candidates cover a dev checkout and an installed copy, and CWOFFEE_UI_DIST
/// overrides both.
fn ui_dist_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CWOFFEE_UI_DIST") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    // The path recorded when this binary was compiled, if it still exists. Preferred
    // over anything derived from the executable's location, because the executable's
    // location says where the build directory was, not which UI was embedded.
    if let Some(baked) = option_env!("CWOFFEE_UI_DIST_BAKED") {
        let p = PathBuf::from(baked);
        if p.is_dir() {
            return p;
        }
        eprintln!(
            "[cwoffee] the UI this binary was built with is gone ({}); falling back to looking near the executable",
            baked
        );
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    for cand in [
        exe.join("../../ui/dist"),
        exe.join("../ui/dist"),
        PathBuf::from("/usr/share/cwoffee/ui"),
    ] {
        if cand.is_dir() {
            return cand;
        }
    }
    exe.join("ui/dist")
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

/// Minimal flag parser. Tauri hands us its own arguments too, so unknown flags
/// are ignored rather than fatal.
fn flag(args: &[String], long: &str, short: Option<&str>) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if (a == long || short.map(|s| a == s).unwrap_or(false)) && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
    }
    None
}

/// The app's own conversation history, in one place. The bridge hands this path to the
/// model process so the `recall` tool can read the same file; two literals for one file is
/// how a tool ends up reading a database nobody writes to.
fn sessions_db_path() -> std::path::PathBuf {
    data_dir().join("sessions.db")
}

fn data_dir() -> PathBuf {
    PathBuf::from(env_or("XDG_DATA_HOME", &format!("{}/.local/share", std::env::var("HOME").unwrap_or_default())))
        .join("cwoffee")
}

/// The part of a gateway hand-off that does not change while the window is open.
///
/// These are the values a standalone gateway cannot recover by itself: the core and the endpoint
/// came from this command line or this environment, and the config path is whatever `--config`
/// named. Everything the user may have moved during the session — model, folder, mode, persona —
/// is read from the host at close time instead, in `handoff_ctx`.
struct HandoffStatic {
    core: String,
    url: String,
    dist: Option<std::path::PathBuf>,
    config_path: std::path::PathBuf,
}

/// The full argument set for a hand-off: the static half plus what the host holds **now**.
///
/// Read at close rather than remembered from boot. A conversation that changed model, folder,
/// mode or persona halfway through would otherwise be handed to a gateway configured for the
/// conversation the user started the window with.
fn handoff_ctx(
    st: &HandoffStatic,
    host: &dyn cwoffee_engine::host::HostOps,
) -> gateway_ctl::HandoffCtx {
    let hs = host.state();
    gateway_ctl::HandoffCtx {
        core: st.core.clone(),
        url: st.url.clone(),
        model: hs.model.clone(),
        workspace: hs.workspace.clone(),
        mode: hs.mode.as_str().to_string(),
        persona: hs.persona.as_str().to_string(),
        dist: st.dist.clone(),
        config_path: st.config_path.clone(),
    }
}

/// The "keep devices working after this window closes" setting, acted on.
///
/// Off is the default and the quiet path: the gateway is a task inside this process, so closing
/// the window stops it. On hands the port to the standalone gateway first, then lets the window
/// go — see `gateway_ctl::handoff` for why one listener cannot survive its own process. What
/// happened goes to the log and to the hand-off's own log file, because a process with no
/// terminal has no other audience, and the settings panel has already said what it would do.
fn handoff_at_close(state: &AppState, st: &HandoffStatic) {
    if !cwoffee_engine::prefs::is_on(state.store.as_ref(), "gateway_on_exit") {
        return;
    }
    match state.gateway.handoff(&handoff_ctx(st, state.host.as_ref())) {
        Ok(r) => eprintln!(
            "[cwoffee] gateway hand-off: {}; started as pid {} via {}; stop it with `{}`",
            r.note, r.pid, r.via, r.stop
        ),
        Err(e) => eprintln!(
            "[cwoffee] gateway NOT kept running: {e}\n[cwoffee] (the setting was on, so this \
             window tried to hand the port over and could not)"
        ),
    }
}

fn main() {
    // MUST be the first thing that happens: the GPU policy has to be in place
    // before GTK/EGL is initialised, or libglvnd hands us an NVIDIA context on a
    // GPU that is already ~97% full from inference. See PLAN.md §12.
    gpu::pin_intel_gpu();

    // WebKitGTK's own rendering path, also env-driven and also needed before the
    // webview exists (the web process inherits this environment at fork time).
    let webkit_mode = gpu::apply_webkit_mode();
    eprintln!("[cwoffee] webkit render path: {}", webkit_mode.name);
    if let Some(w) = webkit_mode.warning {
        eprintln!("[cwoffee] WARNING: {w}");
    }

    let report = gpu::report();
    eprintln!(
        "[cwoffee] EGL vendor pinned to: {}\n[cwoffee] GLX vendor: {}",
        report.egl_vendor, report.glx_vendor
    );
    if report.leaked_nvidia {
        eprintln!("[cwoffee] WARNING: this process holds an NVIDIA render node despite the pin");
    }

    // ---- configuration ----------------------------------------------------
    // precedence: CLI flags > env > config file > defaults
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = flag(&args, "--config", None);
    let (mut appcfg, loaded) = match Config::load(cfg_path.as_deref().map(std::path::Path::new)) {
        Ok(v) => v,
        Err(e) => {
            // A malformed config must not silently fall back to defaults: that
            // would send the agent at the wrong endpoint with no explanation.
            eprintln!("[cwoffee] {e}");
            std::process::exit(1);
        }
    };
    // The file the search panel writes back to. None from `load` means no file
    // existed yet, which is not "nowhere to write": the panel creates one at the
    // default path, so a provider chosen on a phone survives a restart.
    let config_path = loaded.clone().unwrap_or_else(Config::default_path);
    appcfg.apply_env();
    if let Some(p) = flag(&args, "--url", None) {
        appcfg.provider.base_url = p;
    }
    if let Some(p) = flag(&args, "--model", None) {
        appcfg.provider.model = p;
    }
    if let Some(p) = flag(&args, "-C", Some("--workspace")) {
        appcfg.workspace = Some(p);
    }
    if let Some(p) = flag(&args, "--mode", None) {
        appcfg.mode = Some(p);
    }
    if let Some(p) = &loaded {
        eprintln!("[cwoffee] config: {}", p.display());
    }

    // ---- engine wiring ----------------------------------------------------
    let url = appcfg.provider.base_url.clone();
    // Empty is allowed: adopt_first_model() below asks the endpoint. Hardcoding
    // a placeholder like "local" only works by luck with llama.cpp's alias and
    // silently breaks every other server.
    let configured_model = appcfg.provider.model.trim().to_string();
    let model = configured_model.clone();
    // Default the workspace to ~/cwoffee-workspace, not the process CWD. A GUI app
    // launched from a .desktop file or by a WM inherits an arbitrary CWD, and
    // under `tauri dev` that is crates/tauri-app - which would quietly make the
    // agent edit the wrong tree.
    let workspace = appcfg
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env_or("HOME", ".")).join("cwoffee-workspace"));
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        eprintln!("[cwoffee] cannot create workspace {}: {e}", workspace.display());
    }
    // Rename safety: move ~/.config/cowry and ~/.local/share/cowry to their
    // cwoffee equivalents before anything reads them, or the rebrand strands
    // every existing session.
    let migration_notes = cwoffee_engine::legacy::migrate_legacy_dirs();
    for note in &migration_notes {
        println!("[cwoffee] {note}");
    }

    let db_path = sessions_db_path();
    let mode = appcfg.mode();

    if let Err(e) = std::fs::create_dir_all(db_path.parent().unwrap_or_else(|| std::path::Path::new("."))) {
        eprintln!("[cwoffee] cannot create data dir: {e}");
    }

    let store = match Store::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[cwoffee] cannot open store at {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };

    let provider: std::sync::Arc<dyn Provider> = match appcfg.build_provider() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[cwoffee] {e}");
            std::process::exit(1);
        }
    };

    let mut cfg = HostConfig::new(model, workspace.clone());
    // A mode or thinking level chosen in the window is written back to this file, so the
    // next start - and the gateway on a phone - agrees with the one you just made.
    cfg.cfg_path = Some(config_path.clone());
    // The browser. One lookup, used twice on purpose: the registry below is built from the
    // same value the tools will read per turn, because an approval card has to describe the
    // tool that actually runs — and a start with no driver does not pay ~1.1 KB of prompt
    // for a door that is not there.
    let browser_cfg = appcfg.browser_cfg();
    cfg.browser = browser_cfg.clone();
    // The **kept** browser: one browser this process holds, so the window's sidebar can show a
    // page and a model's walk can continue where it left off instead of starting a new browser
    // per call. Mounted on every start, driver or no driver: the panel has to exist before
    // there is anything to show, and mounting spawns nothing — a browser appears at the first
    // `open` and closes itself a few minutes after the last touch (`[browser] idle_secs`).
    let browser_keep = cwoffee_engine::browser_hub::SessionCfg::from_browser_cfg(&browser_cfg);
    // Which copy of itself is running, and when the browser files beside it were last replaced. The
    // launcher runs an *installed* copy under the user's data dir, and a session tested against a
    // copy from before a change reads exactly like a change that did not work. That question
    // currently costs a person an hour of diffing mtimes, so it answers itself in the first lines.
    {
        let me = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("?"));
        let age = |p: &std::path::Path| -> String {
            let then = match p.metadata().and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => return "now".to_string(),
            };
            let mins = match then.elapsed() {
                Ok(d) => d.as_secs() / 60,
                Err(_) => return "from the future".to_string(),
            };
            match mins {
                0..=59 => format!("{mins}m ago"),
                60..=2879 => format!("{}h ago", mins / 60),
                bigger => format!("{}d ago", bigger / 1440),
            }
        };
        let drivers: Vec<std::path::PathBuf> =
            [&browser_cfg.driver, &browser_keep.session_driver].into_iter().flatten().cloned().collect();
        let newer: Vec<&std::path::PathBuf> = match me.metadata().and_then(|m| m.modified()) {
            Ok(built) => drivers
                .iter()
                .filter(|d| d.metadata().and_then(|m| m.modified()).map(|t| t < built).unwrap_or(false))
                .collect(),
            Err(_) => Vec::new(),
        };
        eprintln!(
            "[cwoffee] this copy: {} (replaced {}){}",
            me.display(),
            age(&me),
            if drivers.is_empty() {
                " — no browser drivers beside it".to_string()
            } else {
                format!("; browser drivers: {}", drivers.iter().map(|d| format!("{} ({})", d.display(), age(d))).collect::<Vec<_>>().join(", "))
            }
        );
        if !newer.is_empty() && std::env::var("CWOFFEE_INSTALLED").is_ok() {
            // The pair is installed together on purpose. One half left behind is a tool advertising
            // steps a browser has never heard of, and the model is told the page failed.
            eprintln!(
                "[cwoffee] WARNING: {} older than this app — the app and its browser drivers are installed together: re-run scripts/install-desktop.sh before believing a browser bug.",
                newer.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
            );
        }
    }
    if !browser_keep.usable() {
        eprintln!(
            "[cwoffee] kept browser: {}",
            browser_keep.off_reason.clone().unwrap_or_else(|| {
                "no browser/session.mjs here; the sidebar will say so".to_string()
            })
        );
    }
    let browser_hub = Arc::new(tokio::sync::Mutex::new(cwoffee_engine::browser_hub::Hub::mount(
        browser_keep.clone(),
    )));
    let browser_shared = tauri::async_runtime::block_on(async { browser_hub.lock().await.shared() });
    // The tools get the same handle the panel will use. One browser, one owner: a tool process
    // that opened its own would render a page the person watching the sidebar never sees.
    cfg.browser_route = Some(cwoffee_engine::tools::browser::BrowserRoute::in_process(
        Arc::clone(&browser_hub),
        browser_shared.clone(),
    ));
    cfg.mode = mode;
    // The thinking chip is persisted the same way the mode is, so it has to be read
    // back at boot or the file and the running app disagree from the first second.
    // A word that is not a level stops the app: an ignored typo here means the model
    // runs at a level nobody chose.
    match appcfg.reasoning_checked() {
        Ok(Some(r)) => cfg.reasoning = r,
        Ok(None) => {}
        Err(e) => {
            eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
            std::process::exit(1);
        }
    }
    cfg.context_budget = appcfg.context.target_tokens;
    // The prompt states what the sandbox may reach. Read from the same ExecCfg the
    // registry got, so the sentence and the sandbox cannot disagree: the model's
    // willingness to run `npm install` or `git pull` hinges on it.
    cfg.egress = appcfg.exec_cfg().net.mode;
    // None means the server's own output cap applies; see Config.
    cfg.max_tokens = appcfg.max_output_tokens;
    // 0 = no cap. The hardcoded 14 was cutting real tasks short; see Config.
    cfg.max_tool_iters = appcfg.max_tool_iters();
    // What a message typed during a turn promises — `mid_turn` in config.toml, and the same
    // word the composer announces and the settings page writes back.
    cfg.mid_turn = appcfg.mid_turn();
    // The palette is one value in `config.toml` and every surface reads it: this window, a device
    // tab on the gateway, the terminal. What a window may still do is carry an override, and its own
    // chrome says so — see `crate::theme`.
    cfg.theme = appcfg.theme();
    // Empty unless `[sampling]` says otherwise, in which case the keys go on every
    // request. Validated here rather than at request time: a bad range should stop
    // the app at boot, not bend the model's behaviour invisibly.
    match appcfg.sampling_checked() {
        Ok(s) => cfg.sampling = s,
        Err(e) => {
            eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
            std::process::exit(1);
        }
    }
    // How long a question card stays open. Validated at boot for the same reason
    // sampling is: a value that is nonsense should stop the app, not be ignored.
    match appcfg.question_timeout_checked() {
        Ok(d) => cfg.question_timeout = d,
        Err(e) => {
            eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
            std::process::exit(1);
        }
    }
    // Same rule for the sandbox's egress block: a mode word that does not parse is
    // a startup error naming itself, not a silent fall back to the default.
    if let Err(e) = appcfg.net_checked() {
        eprintln!("[cwoffee] bad {e} - fix it in {}", config_path.display());
        std::process::exit(1);
    }
    // Say what the sandbox will actually do. Without this line a user who set
    // `network = "open"` has no way to know the app honoured it.
    println!("[cwoffee] sandbox egress: {}", appcfg.exec_cfg().net.describe());
    // No env block from here on purpose: the host derives it from the folder a
    // turn actually runs in. A snapshot of the startup folder used to be shown to
    // sessions living somewhere else.
    cfg.env_override = None;
    // Third-party MCP servers, started here and now. Sequentially, each bounded by
    // `[mcp] timeout_secs`: a startup that waits on somebody else's program is not a
    // startup, and one broken server must not stop the others — it is recorded as dead
    // and the catalog says so, which is the honest version of "I could not start it".
    let mcp_cfg = appcfg.mcp_cfg();
    if let Err(e) = Config::mcp_problem(&mcp_cfg) {
        eprintln!("[cwoffee] bad [mcp] config: {e} - fix it in {}", config_path.display());
        std::process::exit(1);
    }
    // Whether anything spawns *now* is decided by the config; the hub itself is mounted either way.
    // That is a change from the old "no servers, no hub": the settings page can add a server while
    // the app is open, and a hub that only exists when the boot found servers would make that add
    // silently wait for a restart. An empty hub costs a lock and no children.
    let mcp_wanted = mcp_cfg.has_servers();
    if !mcp_wanted && !mcp_cfg.servers.is_empty() {
        eprintln!("[cwoffee] [mcp] lists {} server(s) and none is enabled; nothing spawned", mcp_cfg.servers.len());
    }
    // Fail-closed default: an unanswered prompt refuses rather than hanging the
    // turn forever.
    cfg.approval_timeout =
        std::time::Duration::from_secs(appcfg.approval_timeout_secs.unwrap_or(300));

    // Two cores behind one seam, chosen at boot (PI-CORE-PLAN decision 12: the desktop
    // joins in-process, it does not open a second localhost socket to talk to itself).
    // `CWOFFEE_CORE=pi` puts a `pi --mode rpc` child where the Rust engine used to be,
    // and everything after this point — the window's pump, the gateway handle, every
    // command — is identical, because it only ever spoke `HostOps`.
    let core = std::env::var("CWOFFEE_CORE").unwrap_or_else(|_| "rust".into());
    if core != "rust" && core != "pi" {
        eprintln!("[cwoffee] CWOFFEE_CORE={core:?} is neither \"rust\" nor \"pi\"; refusing to guess which core you meant");
        std::process::exit(1);
    }
    // Cowork or chat, resolved before the core starts: in chat the model process gets
    // no tools and a one-line prompt, which is not something you can change afterwards.
    // `appcfg.persona` is what the window writes when the user clicks the tab, so the
    // app comes back in the mode the user left it in.
    let persona_raw = std::env::var("CWOFFEE_PERSONA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| appcfg.persona.clone())
        .unwrap_or_else(|| "cowork".into());
    let persona = match cwoffee_engine::host::Persona::parse(&persona_raw) {
        Some(p) => p,
        None => {
            eprintln!("[cwoffee] persona: {persona_raw:?} is neither \"cowork\" nor \"chat\"; refusing to guess which mode you meant");
            std::process::exit(1);
        }
    };
    if persona == cwoffee_engine::host::Persona::Chat && core != "pi" {
        eprintln!(
            "[cwoffee] persona: chat is not available on the rust core — that core is the coworker prompt, \
             the tool registry and a workspace. Set CWOFFEE_CORE=pi for a tool-less chat, or persona = cowork."
        );
        // Worth naming where this line goes, because for a desktop launch there is no
        // terminal: the app exits at boot and the icon looks like a launcher that does
        // nothing. The reason is real, it is just filed somewhere nobody thinks to look.
        eprintln!(
            "[cwoffee] no window will appear from this. started from a .desktop file this output only \
             reaches the journal (journalctl --user -b | grep cwoffee.desktop); the core a menu launch \
             gets is recorded by scripts/install-desktop.sh, which reads persona from the config."
        );
        std::process::exit(1);
    }

    // Read the level the file asked for before `cfg` moves into the host: the boot
    // line states the dials the app actually started with, which is the only way a
    // drift between `config.toml` and the running app is visible in a log.
    let reasoning_at_boot = cfg.reasoning;
    let (host, receivers, questions, model_label, mcp_hub, mcp_route): (
        Arc<dyn HostOps>,
        Option<(
            cwoffee_engine::host::EventRx,
            tokio::sync::mpsc::UnboundedReceiver<cwoffee_engine::host::ApprovalRx>,
        )>,
        Option<tokio::sync::mpsc::UnboundedReceiver<cwoffee_engine::tools::ask::QuestionRx>>,
        String,
        Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>>,
        String,
    ) = tauri::async_runtime::block_on(async {
        if core == "pi" {
            match pi_host(
                &appcfg,
                &cfg,
                &store,
                &workspace,
                &configured_model,
                persona,
                // Always the config, even when it lists nothing: the tool host reaches the hub over
                // a socket, and a server added from the settings page has to be reachable the moment
                // it is written rather than after a restart.
                Some(&mcp_cfg),
                Some(&browser_hub),
                &config_path,
            )
            .await
            {
                Err(e) => {
                    eprintln!("[cwoffee] pi core did not start: {e}");
                    std::process::exit(1);
                }
                Ok((mut h, hub, route)) => {
                    // Receivers can only be taken once, and they need &mut: take them
                    // before the host becomes the Arc the window and gateway share.
                    let r = h.take_receivers();
                    let q = h.take_questions();
                    // The hub is the app's, not the model process's: on this core the tools
                    // ask it over a socket (`PiHostConfig::mcp_sock`). `mcp_status` reads the
                    // same Arc the tool calls go through, so the settings page cannot claim a
                    // server the process lost, and cannot claim the other core has none.
                    (
                        Arc::new(h) as Arc<dyn HostOps>,
                        r,
                        q,
                        configured_model.clone(),
                        hub,
                        route,
                    )
                }
            }
        } else {
            // The engine spawns tasks, so it must be constructed inside a tokio
            // context. Tauri's async runtime is tokio, so entering it here is enough;
            // calling SessionHost::spawn from bare `main` panics with "no reactor".
            let mut reg = Registry::with_v1_tools_browser(
                appcfg.search_backend(),
                appcfg.exec_cfg(),
                browser_cfg.usable().then(|| browser_cfg.clone()),
            );
            let mcp_hub: Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>> = Some({
                let hub = cwoffee_engine::mcp::Hub::start(&mcp_cfg).await;
                for (name, line) in hub.servers_named() {
                    // A menu-launched app has no terminal: the launcher's stderr lands in the
                    // journal, so this is the only place a dead server can be read.
                    eprintln!("[cwoffee] mcp {name}: {line}");
                }
                cfg.mcp_catalog = hub.catalog_block();
                Arc::new(tokio::sync::Mutex::new(hub))
            });
            if let Some(hub) = &mcp_hub {
                // Registered as *hidden*: the tools exist and are callable once named, and cost
                // no prompt bytes until `explore` names them. That is the whole economy of the
                // feature — a configured server must not tax every turn in every session.
                let catalog = hub.lock().await.catalog();
                cwoffee_engine::mcp::install_into(&mut reg, &catalog, hub);
                eprintln!(
                    "[cwoffee] mcp: {} third-party tool(s) registered, hidden behind explore",
                    catalog.len()
                );
            }
            let mut h = SessionHost::spawn(provider, reg, Arc::clone(&store), cfg);
            // No config file means no model id. Ask the endpoint rather than sending
            // an empty one and taking a 400.
            let adopted = h.adopt_first_model().await;
            if let Some(m) = &adopted {
                eprintln!("[cwoffee] no model configured; using the endpoint's first: {m}");
            }
            let r = h.take_receivers();
            let q = h.take_questions();
            // Read the adopted id from the return value, not host.state(): SetModel is
            // applied by the host task, so state() can still show the old value here.
            let label = adopted.unwrap_or_else(|| configured_model.clone());
            (
                Arc::new(h) as Arc<dyn HostOps>,
                r,
                q,
                label,
                mcp_hub,
                // The hub belongs to this process whatever the boot found: a server added later is
                // reached here, so the route line must not read "none" on an app that can start one.
                "this process".to_string(),
            )
        }
    });
    eprintln!(
        "[cwoffee] engine up: core={core} persona={} url={url} model={model_label} workspace={} mode={:?} reasoning={:?}",
        persona.as_str(), workspace.display(), mode, reasoning_at_boot
    );
    // The exec deadline used to be invisible: a [sandbox] block that parsed,
    // validated, and then went nowhere looked identical to one that worked.
    let ec = appcfg.exec_cfg();
    eprintln!(
        "[cwoffee] exec: ceiling {}s, sandbox={}",
        ec.timeout_secs, ec.sandbox_enabled
    );
    // Same reason: a search backend you cannot see is a backend you cannot tell
    // apart from one that is silently falling back. `describe()` is the same wording
    // the settings card shows, so the log and the UI cannot disagree.
    {
        let b = appcfg.search_backend();
        let hint = match &b {
            cwoffee_engine::tools::web::SearchBackend::DuckDuckGo =>
                " — starts answering with a bot challenge after a handful of queries; set [search] backend = \"auto\" to have Cwoffee run its own SearXNG",
            _ => "",
        };
        eprintln!("[cwoffee] search: {}{}", b.describe(), hint);
    }

    let (queue_tx, queue_rx) = mpsc::unbounded_channel();
    // A second fan-out for non-webview listeners (the gateway). The webview gets
    // its own ordered channel; a browser tab gets a broadcast subscription, so
    // two tabs and the window all see the same turn rather than competing for
    // one receiver.
    let (bcast_tx, _bcast_rx) = tokio::sync::broadcast::channel::<cwoffee_engine::EngineEvent>(512);
    // One map, shared by the pump (which parks reply senders) and the
    // respond_approval command (which resolves them). Two separate maps meant the
    // command always looked up an empty one.
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ApprovalAnswer>>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_pump = Arc::clone(&pending);
    // ask_user works the same way: the pump parks the reply sender, and
    // respond_question (from the webview or the gateway) resolves it.
    let pending_q: Arc<Mutex<HashMap<u64, oneshot::Sender<cwoffee_engine::tools::ask::Answer>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_q_pump = Arc::clone(&pending_q);

    // The pump starts at boot, not when the webview attaches: nothing is lost
    // during startup, and if the webview goes away the gate's timeout is what
    // stops the agent — not the absence of a forwarder.
    let mut question_rx = questions.expect("question receiver");
    if let Some((mut ev_rx, mut ap_rx)) = receivers {
        let qtx = queue_tx.clone();
        let btx = bcast_tx.clone();
        let pending_q_pump = Arc::clone(&pending_q_pump);
        tauri::async_runtime::spawn(async move {
            let (mut ev_open, mut ap_open, mut q_open) = (true, true, true);
            loop {
                tokio::select! {
                    ev = ev_rx.recv(), if ev_open => {
                        match ev {
                            Some(e) => {
                                // The webview queue is the load-bearing one; the
                                // broadcast has no listeners in the common case
                                // and its failure must not end the pump.
                                let _ = btx.send(e.clone());
                                if qtx.send(e).is_err() {
                                    break;
                                }
                            }
                            None => ev_open = false,
                        }
                    }
                    ap = ap_rx.recv(), if ap_open => {
                        match ap {
                            Some((req, reply)) => {
                                let id = req.id;
                                let _ = btx.send(approval_event(&req));
                                if qtx.send(approval_event(&req)).is_err() {
                                    // Nobody to ask: refuse rather than run unattended.
                                    let _ = reply.send(ApprovalAnswer::Deny);
                                    continue;
                                }
                                pending_pump.lock().unwrap().insert(id, reply);
                            }
                            None => ap_open = false,
                        }
                    }
                    qu = question_rx.recv(), if q_open => {
                        match qu {
                            Some((question, reply)) => {
                                let id = question.id;
                                let _ = btx.send(question_event(&question));
                                if qtx.send(question_event(&question)).is_err() {
                                    // Nobody to ask: tell the model that rather
                                    // than parking the turn until the timeout.
                                    drop(reply);
                                    continue;
                                }
                                pending_q_pump.lock().unwrap().insert(id, reply);
                            }
                            None => q_open = false,
                        }
                    }
                    else => break,
                }
                if !ev_open && !ap_open && !q_open {
                    break;
                }
            }
            eprintln!("[cwoffee] event pump finished");
        });
    }

    // The gateway controller exists whether or not the gateway is on. The footer
    // has to answer "is it running?" and "how many devices are on it?", and that
    // question has to be answerable about a gateway started from the environment
    // as well as one started by clicking the footer.
    // Which folder a local page may be opened from, asked of the engine at the moment it is asked
    // for. The person can switch the conversation's folder — or move the conversation to another
    // folder — with this window and its page still open; a guard holding the folder the app booted
    // with refuses a page in the folder they are standing in and calls it an escape.
    let ws_of: crate::page_embed::WsOf = {
        let host_of = Arc::clone(&host);
        std::sync::Arc::new(move || std::path::PathBuf::from(host_of.state().workspace))
    };

    let gw_handle = Arc::new(cwoffee_gateway::EngineHandle {
        host: Arc::clone(&host),
        store: Arc::clone(&store) as Arc<dyn StoreOps>,
        events: bcast_tx.clone(),
        pending: Arc::clone(&pending),
        pending_questions: Arc::clone(&pending_q),
        clients: gateway_ctl::client_gauge(),
        migration_notes: migration_notes.clone(),
        config_path: config_path.clone(),
        browser_cfg: browser_cfg.clone(),
        browser: Some(Arc::clone(&browser_hub)),
        browser_state: browser_shared.clone(),
        browser_keep: browser_keep.clone(),
        mcp: mcp_hub.clone(),
        mcp_route: mcp_route.clone(),
    });
    let gateway = Arc::new(gateway_ctl::GatewayCtl::new(gw_handle));

    let state = AppState {
        // Cloned, not moved: the window's page box asks the same host which folder the conversation
        // is in, and that question is asked after `host` has gone into the state.
        host: Arc::clone(&host),
        store,
        note_tx: queue_tx.clone(),
        queue_rx: Mutex::new(Some(queue_rx)),
        pending_approvals: Arc::clone(&pending),
        pending_questions: Arc::clone(&pending_q),
        rendered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        migration_notes,
        gateway: Arc::clone(&gateway),
        config_path: config_path.clone(),
        browser_cfg: browser_cfg.clone(),
        browser: Some(Arc::clone(&browser_hub)),
        browser_state: browser_shared.clone(),
        probe_holds_box: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        browser_keep: browser_keep.clone(),
        host_for_browser: Arc::clone(&host),
        mcp: mcp_hub,
        mcp_route,
    };

    // Boot-time gateway: off unless CWOFFEE_GATEWAY_PORT is set. Started through
    // the same controller the footer toggle uses, so the status line reports the
    // truth about an environment-started gateway too - port, bind address, token
    // source and live client count.
    if let Some(port) = std::env::var("CWOFFEE_GATEWAY_PORT")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let bind: std::net::IpAddr = env_or("CWOFFEE_GATEWAY_HOST", "127.0.0.1")
            .parse()
            .unwrap_or_else(|_| std::net::Ipv4Addr::LOCALHOST.into());
        let port = port.parse().unwrap_or(8099);
        let (drive, source) = gateway_ctl::drive_token();
        let watch = std::env::var("CWOFFEE_GATEWAY_WATCH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        // Started before the window exists rather than alongside it. The first
        // `gateway_status` read happens as the UI boots, and a status taken while
        // the accept loop is still being spawned says "off" about a gateway that is
        // about to be reachable - the one reading of this pill that could mislead.
        let ctl = Arc::clone(&gateway);
        if let Err(e) =
            tauri::async_runtime::block_on(ctl.start(bind, port, drive, watch, source))
        {
            eprintln!("[cwoffee] gateway not running: {e}");
        }
    }

    let quit_armed: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    let handoff_static = Arc::new(HandoffStatic {
        core: core.clone(),
        url: appcfg.provider.base_url.clone(),
        dist: Some(ui_dist_dir()),
        config_path: config_path.clone(),
    });
    // A flag, not a setting, so a script can watch a hand-off happen without driving a window:
    // hand the port over with this process's own values, print the verdict, and leave. Used by
    // scripts/gateway-handoff.mjs, which then asks the port who is answering it.
    if args.iter().any(|a| a == "--handoff-gateway") {
        match state.gateway.handoff(&handoff_ctx(&handoff_static, state.host.as_ref())) {
            Ok(r) => {
                eprintln!(
                    "[handoff] bound={} pid={} via={} binary={} stop=`{}`\n[handoff] {}",
                    r.bound, r.pid, r.via, r.binary, r.stop, r.note
                );
                // Leave the child alone; this process has no business owning it.
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("[handoff] failed: {e}");
                std::process::exit(1);
            }
        }
    }
    let rendered_flag = state.rendered.clone();
    let app = tauri::Builder::default()
        // Rendered markdown contains links to pages the model fetched. Those must
        // open in the user's browser, never in this webview (which would replace
        // the app), and never through a shell string we built ourselves.
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        // A webview that never paints leaves no trace: the engine logs look
        // healthy, WebKit processes are up, memory is fine. The only outward
        // symptom is a missing "render OK" line. So log what the webview was
        // actually given, at startup, before anyone has to guess.
        .setup(move |app| {
            // The kept browser in **this window** is a page in this window — a real WebKit page the
            // person clicks and the model drives, through the same line protocol the child-process
            // driver answers. It is the default; `CWOFFEE_BROWSER_PAGE=0` asks for the old
            // child-process browser instead, which is also what the one-shot `browse` walk still
            // uses, because a page in this window cannot be screenshotted and a vision model
            // sometimes needs a picture.
            let probe = std::env::args().any(|a| a == "--page-probe");
            // The probe is a claim about the page in the window, so it *makes* that claim true rather
            // than reporting a start that was configured differently: `page_in_window_asked` is
            // consulted by the board below, and a probe run over a picture-column start would grade
            // the wrong thing and print "ok" doing it.
            if page_in_window_asked() || probe {
                let hub = Arc::clone(&browser_hub);
                let handle = app.handle().clone();
                let ws_drv = ws_of.clone();
                tauri::async_runtime::spawn(async move {
                    let drv = page_view::PageDriver::new(&handle, ws_drv);
                    let mut h = hub.lock().await;
                    h.set_native(drv);
                    eprintln!("[cwoffee] kept browser: the page lives in this window; the model drives the same document you are looking at");
                });
            }
            // A one-shot probe for that path: attach the driver, put a page in the window, drive it
            // the way a model would, and report each step as it answered. Print and leave. This is
            // the fastest way to tell "the plumbing is wrong" from "the layout is wrong", which is
            // the pair of failures an in-window rectangle makes easy to confuse.
            if probe {
                let hub = Arc::clone(&browser_hub);
                let handle = app.handle().clone();
                // The folder the conversation is in **now**, which is the folder the page box's
                // guard confines pages to. Writing the probe page somewhere else would grade the
                // guard refusing a page it should allow.
                let ws = (ws_of)();
                std::thread::spawn(move || {
                    tauri::async_runtime::block_on(async move {
                        // Give the attach above a moment to land, and the window a moment to size.
                        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
                        let out = probe_page(&hub, &handle, &ws).await;
                        eprintln!("PAGE-PROBE {out}");
                        std::process::exit(if out.starts_with("ok") { 0 } else { 1 });
                    });
                });
            }
            // The window manager and the dock ask the *window* for an icon. A
            // frameless window built from a bare binary has none, so GNOME shows
            // its generic gear. The bundle icons only apply once the app is
            // installed with a .desktop file, which is not how this runs in dev.
            // The idle clock. It is started here rather than being consulted when somebody
            // looks, because "did the app leave a browser running?" is the objection the whole
            // kept-browser design has to answer, and a clock nobody starts is a promise. When
            // it fires, the window is told: a page vanishing from the panel with no reason
            // given looks like a crash.
            {
                let hub = Arc::clone(&browser_hub);
                let note = app.handle().state::<AppState>().note_tx.clone();
                let session = app.handle().state::<AppState>().host.state().session_id;
                tauri::async_runtime::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                        let why = hub.lock().await.reap_if_idle().await;
                        if let Some(why) = why {
                            eprintln!("[cwoffee] kept browser: {why}");
                            let _ = note.send(cwoffee_engine::EngineEvent::Note {
                                message: format!("the browser closed itself: {why}"),
                                session: Some(session),
                            });
                        }
                    }
                });
            }
            if let Some(w) = app.get_webview_window("main") {
                if let Err(e) = w.set_icon(tauri::include_image!("icons/64x64.png")) {
                    eprintln!("[cwoffee] window icon not set: {e}");
                }
                // The self-test measures layout and animation in a page the
                // compositor is painting. An occluded or minimized window stops
                // requestAnimationFrame dead and freezes every CSS animation at its
                // start value, at which point those checks measure the window
                // manager rather than the app - and the run used to discover that as
                // a fifteen-minute timeout with no verdict at all. So the armed
                // harness asks for the window it needs, and the log says so: a run
                // that moved your windows should say it moved your windows.
                if commands::selftest_armed() {
                    let _ = w.unminimize();
                    let _ = w.set_always_on_top(true);
                    let _ = w.set_focus();
                    eprintln!(
                        "[selftest] window un-minimized, pinned above others and focused: the layout and \
                         reveal checks need a page the compositor is actually painting"
                    );
                }
            }
            // One ask at startup is not enough, and the reason is worse than a slow frame clock.
            // When this window is covered, WebKitGTK does not merely stop firing requestAnimationFrame:
            // it freezes the page's timer loop entirely, so the harness cannot run even its own
            // watchdog. Two runs ended with no verdict and no error because of that - the log just
            // stopped, mid-chain, and the board clock answered 25 minutes later. So the *application*
            // keeps its own window on top for the length of an armed run: the page's timers are
            // precisely the thing that cannot be trusted when the window is gone.
            if commands::selftest_armed() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    // Wayland ignores always-on-top for a normal toplevel, so the only lever that
                    // actually moves this window in the stack is activate (focus). Taking focus back
                    // is rude, so it is rationed: at most one activation per minute, ten in a run.
                    // A hard cap of three was tried first and spent all of them in the first minute,
                    // on a window that had barely lost focus during startup, leaving nothing for the
                    // silence that arrived twenty minutes later - which is the one that costs the run
                    // its geometry checks.
                    //
                    // One per minute turned out to be too slow for the ordinary case: the terminal
                    // the harness was started from is a maximised window, it takes focus back every
                    // time a command prints, and a covered window is not painted at all. A run lost
                    // 29 of its geometry checks that way and came back with failures that were about
                    // the compositor. So: one per twenty seconds, thirty in a run - still only in an
                    // armed run, which is a test the person started on purpose.
                    let mut focuses = 0u32;
                    let mut last = std::time::Instant::now();
                    for i in 1..=120u64 {
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        let Some(win) = handle.get_webview_window("main") else { return };
                        if win.is_minimized().unwrap_or(false) {
                            // Nothing is painted into a minimised window at all, so putting it back
                            // is not a courtesy question.
                            let _ = win.unminimize();
                            let _ = win.set_focus();
                            eprintln!("[selftest] the window was minimised at t={}s; the run puts it back", i * 5);
                            last = std::time::Instant::now();
                            continue;
                        }
                        if focuses >= 30 || last.elapsed() < std::time::Duration::from_secs(20) {
                            continue;
                        }
                        if win.is_focused().unwrap_or(true) {
                            continue;
                        }
                        focuses += 1;
                        last = std::time::Instant::now();
                        let _ = win.set_focus();
                        eprintln!(
                            "[selftest] this window had lost focus (activation {} of 30 at t={}s); the run activates it, because a page nothing composites cannot be measured",
                            focuses,
                            i * 5
                        );
                    }
                });
            }
            let cfg = app.config();
            let cfg_build = cfg.build.clone();
            eprintln!(
                "[cwoffee] frontend_dist={:?} csp_hashes={}",
                cfg.build.frontend_dist,
                cfg.app.security.csp.is_some(),
            );
            // A webview that never paints leaves no trace. Process liveness,
            // memory and sockets all report healthy; the only outward symptom is
            // a missing "render OK" line, which is easy to miss in a log. So make
            // the absence itself an explicit failure.
            let painted = rendered_flag.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                if !painted.load(std::sync::atomic::Ordering::SeqCst) {
                    eprintln!(
                        "[cwoffee] SUSPECT: no render report after 8s - the window is probably blank. frontend_dist={:?}; a debug build loads devUrl and needs vite running.",
                        cfg_build.frontend_dist
                    );
                }
            });
            Ok(())
        })
        // Decision 15: quitting with a turn running is a question, not a silent kill.
        // The refusal rides on a `Note`, which the window already knows how to render —
        // so the policy needs no UI edit. A second close within 8 s means "stop it and
        // quit": abort, give the turn 2 s to settle so the transcript flushes, then let
        // the window close. A window that vanishes and leaves an agent working
        // unattended is the same failure the approval gate exists to prevent.
        .on_window_event(move |window, event| {
            let tauri::WindowEvent::CloseRequested { api, .. } = event else { return };
            let state = window.app_handle().state::<AppState>();
            let busy = state.host.state().busy;
            let mut armed = quit_armed.lock().unwrap();
            let already = armed.map(|t| t.elapsed() < std::time::Duration::from_secs(8)).unwrap_or(false);
            if !busy {
                // The window is going away for good. This is the only place the "keep devices
                // working" setting can be honoured, and it has to be before the process dies:
                // the gateway is a task in here.
                handoff_at_close(&state, &handoff_static);
                return;
            }
            if !already {
                *armed = Some(std::time::Instant::now());
                api.prevent_close();
                let st = state.host.state();
                let _ = state.note_tx.send(cwoffee_engine::EngineEvent::Note {
                    message: "a turn is still running — closing was refused. Close again within 8 seconds to stop it and quit.".into(),
                    session: Some(st.session_id),
                });
                return;
            }
            state.host.send(cwoffee_engine::host::HostCmd::Cancel);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while state.host.state().busy && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let still_busy = state.host.state().busy;
            eprintln!(
                "[cwoffee] quit asked while busy: aborted, {}the turn settled{}",
                if still_busy { "and " } else { "" },
                if still_busy { " — its children are being killed" } else { "" }
            );
            // Same as the calm path: the turn is settled, the window is going, so this is the
            // moment to hand the gateway over if the setting says so.
            handoff_at_close(&state, &handoff_static);
        })
        .invoke_handler(tauri::generate_handler![
            commands::attach,
            commands::start_turn,
            commands::pick_workspace,
            commands::set_workspace,
            commands::cancel_turn,
            commands::steer_turn,
            commands::withdraw_steer,
            commands::queue_state,
            commands::set_mid_turn,
            commands::set_theme,
            commands::selftest_config_text,
            commands::theme_words,
            commands::respond_approval,
            commands::respond_question,
            commands::set_mode,
            commands::set_reasoning,
            commands::set_persona,
            commands::tools_view,
            commands::list_models,
            commands::migration_notes,
            commands::gateway_status,
            commands::gateway_start,
            commands::gateway_stop,
            commands::gateway_handoff_plan,
            commands::set_model,
            commands::session_state,
            commands::list_sessions,
            commands::sessions_in,
            commands::ui_prefs,
            commands::set_ui_pref,
            commands::provider_status,
            commands::provider_test,
            commands::provider_probe,
            commands::provider_add,
            commands::provider_remove,
            commands::model_set,
            commands::model_forget,
            commands::search_messages,
            commands::session_messages,
            commands::message_attachments,
            commands::edit_message,
            commands::branch_conversation,
            commands::session_tree,
            commands::switch_leaf,
            commands::files_ledger,
            commands::file_diff,
            commands::snapshot_stats,
            commands::forget_copies,
            commands::selftest_file_bytes,
            commands::selftest_write_file,
            commands::checkpoint_preview,
        commands::browser_panel,
        commands::browser_open,
        commands::browser_act,
        commands::browser_frame,
        commands::browser_watch,
        commands::browser_viewport,
        commands::browser_view_rect,
        commands::browser_log,
        commands::browser_close,
        commands::mcp_status,
        commands::mcp_add_server,
        commands::mcp_set_server_enabled,
        commands::mcp_remove_server,
        commands::mcp_set_enabled,
        commands::mcp_reconcile,
        commands::browser_status,
        commands::tool_inventory,
            commands::checkpoint_revert,
            commands::share_markdown,
            commands::save_markdown_file,
            commands::save_attachment_as,
            commands::search_settings,
            commands::set_search_backend,
            commands::search_probe,
            commands::set_context_budget,
            commands::open_file,
            commands::new_session,
            commands::list_workspaces,
            commands::add_workspace,
            commands::remove_workspace,
            commands::set_session_web,
            commands::provider_set_primary,
            commands::about,
            commands::set_workspace_label,
            commands::set_workspace_color,
            commands::rename_session,
            commands::set_session_persona,
            commands::compact,
            commands::delete_session,
            commands::clear_session,
            commands::switch_session,
            commands::selftest_armed,
            commands::selftest_resize_window,
            commands::present_selftest_window,
            commands::selftest_window_state,
            commands::selftest_config,
            commands::gpu_report,
            commands::clipboard_image,
            commands::selftest_stage_clipboard,
            commands::render_report,
            commands::boot_log
        ]);

    if let Err(e) = app.run(tauri::generate_context!()) {
        eprintln!("[cwoffee] error while running: {e}");
        std::process::exit(1);
    }
}

/// Start a `pi --mode rpc` core for the desktop.
///
/// The `CWOFFEE_PI_*` variables are development switches for this experiment: the
/// packaging decisions (S2.5 for extensions, S4.1 for the pinned runtime) are what
/// replace them. Until then the boot line states exactly what was mounted, because a
/// guardrail that is silently off is the failure mode worth making visible.
async fn pi_host(
    appcfg: &Config,
    cfg: &HostConfig,
    store: &Arc<Store>,
    workspace: &std::path::Path,
    model: &str,
    persona: cwoffee_engine::host::Persona,
    mcp: Option<&cwoffee_engine::mcp::McpCfg>,
    // The app's kept browser. On this core the tools are separate processes, so they reach it
    // over a socket in the agent dir — the same shape as the MCP hub, for the same reason.
    browser: Option<&std::sync::Arc<tokio::sync::Mutex<cwoffee_engine::browser_hub::Hub>>>,
    // Which config file the endpoints live in. Passed rather than re-derived: `--config` is a
    // decision, and a pi core that read ~/.config/cwoffee while the app runs on another file would
    // mount a different set of endpoints than the one the person edited.
    config_path: &std::path::Path,
) -> Result<
    (
        cwoffee_bridge::PiHost,
        Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>>,
        String,
    ),
    String,
> {
    if model.trim().is_empty() {
        return Err(
            "a pi core needs a model id: pi selects by provider+model and does not adopt an endpoint's first model"
                .into(),
        );
    }
    let env_path = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty()).map(PathBuf::from);
    let bin = env_path("CWOFFEE_PI").unwrap_or_else(|| PathBuf::from("pi"));
    let agent_dir = env_path("CWOFFEE_PI_AGENT_DIR").unwrap_or_else(|| {
        let base = std::env::var("XDG_STATE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share")
            });
        base.join("cwoffee").join("pi")
    });
    let session_dir = env_path("CWOFFEE_PI_SESSION_DIR").unwrap_or_else(|| agent_dir.join("sessions"));
    std::fs::create_dir_all(&session_dir).map_err(|e| format!("cannot create {}: {e}", session_dir.display()))?;

    // Third-party MCP servers, on this core too. The hub cannot live in the tool host —
    // `cwtool` is one process per call, and a server it spawned would be a new server each
    // time, so a stateful one would answer correctly exactly once. It lives here, in the
    // process that outlives the calls, and the tool host reaches it over a socket in the
    // app's own state directory. The catalog is capped by the hub before it is handed out.
    let (mcp_hub, mcp_sock, mcp_catalog): (
        Option<Arc<tokio::sync::Mutex<cwoffee_engine::mcp::Hub>>>,
        Option<PathBuf>,
        String,
    ) = match mcp {
        Some(mcp_cfg) => {
            let hub = cwoffee_engine::mcp::Hub::start(mcp_cfg).await;
            for (name, line) in hub.servers_named() {
                eprintln!("[cwoffee/pi] mcp {name}: {line}");
            }
            let catalog = hub.catalog_block();
            let tools = hub.catalog().len();
            let hub = Arc::new(tokio::sync::Mutex::new(hub));
            let sock = agent_dir.join("cwoffee-mcp.sock");
            match cwoffee_engine::mcp::socket::serve(Arc::clone(&hub), sock.clone()).await {
                Ok(()) => {
                    eprintln!(
                        "[cwoffee/pi] mcp: {tools} third-party tool(s) reachable through {}; hidden behind explore until named",
                        sock.display()
                    );
                    (Some(hub), Some(sock), catalog)
                }
                // The servers are up and the app can see them. Handing out a path that does
                // not answer would make every tool call fail with a confusing "refused"; the
                // tools then say there is no hub to ask, which is the truth a model can act on.
                Err(e) => {
                    eprintln!(
                        "[cwoffee/pi] mcp: the hub is running but no tool process can reach it ({e}); \
                         MCP calls will refuse rather than guess at a server"
                    );
                    (Some(hub), None, catalog)
                }
            }
        }
        None => (None, None, String::new()),
    };
    // How a tool call reaches these servers, in the words `mcp_status` shows. On this core the
    // answer is never "this process": the tool host is a child of it and asks over the socket,
    // and a hub nobody can reach is a fact the page has to state rather than a green row.
    // The kept browser, on the pi core. It lives in the app (the window that shows it and the
    // process that outlives a tool call), and the tool processes reach it through this socket.
    // A browser held inside `cwtool` would be a new browser per call, and the page a person is
    // watching in the sidebar would not be the page being clicked.
    let browser_sock = match browser {
        Some(h) => {
            let sock = agent_dir.join("cwoffee-browser.sock");
            match cwoffee_engine::browser_hub::socket::serve(std::sync::Arc::clone(h), &sock).await {
                Ok(()) => {
                    // `serve` prints the path it bound. A second line about the same path would be
                    // two sentences for one fact, and they are the kind that drift apart.
                    Some(sock)
                }
                // The browser is real and the window can use it; only the tool processes are
                // cut off. Naming a path that does not answer would make every `session: true`
                // call fail with a refused connection instead of the truth: this run has no
                // road from a tool call to the app's browser.
                Err(e) => {
                    eprintln!("[cwoffee/pi] {e} - `browse` with `session: true` will say there is no kept browser to reach");
                    None
                }
            }
        }
        None => None,
    };

    let mcp_route = match (&mcp_hub, &mcp_sock) {
        (Some(_), Some(sock)) => format!("the app's hub, reached over {}", sock.display()),
        (Some(_), None) => "a hub no tool process can reach".to_string(),
        (None, _) => "none - no server is running in this start".to_string(),
    };

    // Never overwrite a provider the user or their own pi setup already put there. The
    // dev file exists so the experiment runs against cwoffee's own config; keeping an
    // existing one is what lets a real pi login be used instead.
    let models = agent_dir.join("models.json");
    // One probe against the endpoint, through the same call the Rust core uses, and
    // the window it states goes into pi's model entry and into the host state. Without
    // it there is no denominator: the metrics panel can only say "no window reported",
    // and a context meter with no ceiling is a gauge with no numbers on it.
    let window = match appcfg.build_provider() {
        Ok(p) => cwoffee_bridge::mount::probe_window(p.as_ref(), model).await,
        Err(e) => {
            println!("[cwoffee/pi] no window probe: {e}");
            None
        }
    };
    println!(
        "[cwoffee/pi] context window for {model}: {}",
        window.map(|w| w.to_string()).unwrap_or_else(|| "the endpoint did not say".into())
    );
    // Skipping the whole file because it exists used to be the rule, and it cost an
    // attachment: the first entry written had no `input`, and pi drops image blocks for a
    // model it thinks is text-only, without saying so. Now the entry this app owns is
    // refreshed every boot (merge, not clobber — see `mount::write_dev_provider`), and only
    // an explicit CWOFFEE_PI_KEEP_MODELS keeps cwoffee out of the file.
    if std::env::var("CWOFFEE_PI_KEEP_MODELS").is_ok() {
        println!(
            "[cwoffee/pi] CWOFFEE_PI_KEEP_MODELS is set: leaving {} alone. pi's entry, not cwoffee's, decides the window and which modalities a picture may travel through.",
            agent_dir.display()
        );
    } else {
        let key = appcfg.api_key().unwrap_or_else(|| "unused".into());
        let (path, mounted) = cwoffee_bridge::mount::write_dev_provider(
            &agent_dir,
            PI_PROVIDER,
            &appcfg.provider.base_url,
            model,
            &key,
            cwoffee_bridge::mount::pi_api_for_kind(&appcfg.provider.kind),
            &appcfg.provider.kind,
            window,
            // The thinking levels the probe found for this model. pi clamps any level the
            // entry does not declare, so a model that can think and is filed without this
            // list answers every conversation at its own template default.
            &appcfg
                .models
                .get(model)
                .map(|r| r.efforts.clone())
                .unwrap_or_default(),
        )
        .map_err(|e| format!("cannot write {}: {e}", models.display()))?;
        // What the boot line claims has to be what the function did. The old line printed
        // "(written to …)" on every path, including the one where the file was left pointing
        // at another endpoint — which is the shape of "the installed app cannot find the
        // model": a stale scripted provider entry in pi's file, the app asking pi for a model
        // that entry did not list, and turns ending with nothing on screen.
        match &mounted {
            cwoffee_bridge::mount::MountOutcome::Written => println!(
                "[cwoffee/pi] provider {PI_PROVIDER} -> {} (written to {})",
                appcfg.provider.base_url,
                path.display()
            ),
            cwoffee_bridge::mount::MountOutcome::Replaced { previous } => println!(
                "[cwoffee/pi] provider {PI_PROVIDER} -> {} (replaced the entry that pointed at {previous}; \
                 it did not list {model}, and pi cannot answer with a model it has never seen)",
                appcfg.provider.base_url,
            ),
            cwoffee_bridge::mount::MountOutcome::Foreign { base_url } => println!(
                "[cwoffee/pi] provider {PI_PROVIDER} was left as it is: {} lists {model}, so requests for \
                 {model} go there, not to {} in cwoffee's config. If that is not what you want, remove that \
                 provider from {}.",
                base_url,
                appcfg.provider.base_url,
                path.display()
            ),
        }
        // The file `--config` named, not the default path: an app started with an explicit
        // config must mount the endpoints written in *that* file, and the model switch re-mounts
        // from the same one. Reading the default file here meant `--config` chose the endpoints
        // for the Rust core and the pi core silently got whatever is in ~/.config/cwoffee.
        match cwoffee_bridge::mount::mount_provider_rows(&agent_dir, &config_path) {
            Ok(list) if !list.is_empty() => println!(
                "[cwoffee/pi] also mounted {} endpoint(s) from {}: {}",
                list.len(),
                config_path.display(),
                list.join(", ")
            ),
            Ok(_) => {}
            Err(e) => eprintln!(
                "[cwoffee/pi] could not mount the registered endpoints from {}: {e} — models on them will not be pickable",
                config_path.display()
            ),
        }
        // pi folds on its own schedule (`compaction.reserveTokens`, 16 384 by default,
        // chosen for coding sessions) while cwoffee's meter fills against a budget.
        // Writing the reserve makes them one number: the fold happens where the app says
        // the room is gone. No window probed means nothing to compute, and the boot line
        // says so rather than claiming a threshold that was never written.
        match window {
            Some(w) => {
                let budget =
                    cwoffee_bridge::mount::context_budget(cfg.context_budget, Some(w as usize));
                match cwoffee_bridge::mount::mount_settings(
                    &agent_dir,
                    PI_PROVIDER,
                    model,
                    budget,
                    w as usize,
                ) {
                    Ok(Some(sentence)) => println!("[cwoffee/pi] {sentence}"),
                    Ok(None) => println!("[cwoffee/pi] compaction: pi keeps its own defaults"),
                    Err(e) => println!("[cwoffee/pi] compaction: pi was NOT told where to fold: {e}"),
                }
            }
            None => println!(
                "[cwoffee/pi] compaction: the endpoint stated no window, so pi keeps its own \
                 folding default and the meter falls back to {}",
                cwoffee_bridge::mount::context_budget(cfg.context_budget, None)
            ),
        }
    }

    let srcs: Vec<PathBuf> = std::env::var("CWOFFEE_PI_EXT")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let exts = cwoffee_bridge::mount::mount_extensions(&agent_dir, &bin, &srcs)
        .map_err(|e| format!("mounting extensions failed: {e}"))?;
    let ext_count = exts.len();

    // pi's own coding tools stay off: the tools that matter here are cwoffee's, and the
    // sandbox and approvals live with them. CWOFFEE_PI_TOOLS exists to add ours, not to
    // drop the minus signs by omission.
    let tools: Vec<String> = match std::env::var("CWOFFEE_PI_TOOLS") {
        Ok(v) => v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Err(_) => vec!["-read".into(), "-bash".into(), "-edit".into(), "-write".into()],
    };
    let mut extra_env = Vec::new();
    if let Ok(v) = std::env::var("CWOFFEE_SHIM") {
        extra_env.push(("CWOFFEE_SHIM".to_string(), v));
    }
    let pinned = if std::env::var("CWOFFEE_PI_ANY").is_ok() {
        None
    } else {
        Some(cwoffee_bridge::process::PINNED_PI.to_string())
    };

    let host = cwoffee_bridge::PiHost::spawn(cwoffee_bridge::PiHostConfig {
        launch: cwoffee_bridge::PiLaunch {
            bin,
            workdir: workspace.to_path_buf(),
            // The endpoint pi starts on is the one that actually serves the default model. cwoffee
            // used to always start on its own `[provider]` entry, which made an endpoint added on
            // the settings screen unusable as a default: switching to it worked for the run, and the
            // next start was back on the old server. Print the decision, because a desktop launch
            // has no terminal and this is the one line that says which server is answering.
            provider: {
                let (name, why) =
                    cwoffee_bridge::mount::launch_provider(config_path, "cwoffee", &model);
                if let Some(s) = why {
                    println!("[cwoffee/pi] endpoint: {s}");
                }
                name
            },
            model: model.to_string(),
            agent_dir: agent_dir.clone(),
            session_dir,
            session_file: None,
            extensions: exts,
            tools: tools.clone(),
            // Chat shaping is the bridge's, per process: a chat starts with no
            // extension mounted and --no-tools.
            no_tools: false,
            system_prompt: None,
            extra_env,
            pinned_version: pinned,
        },
        store: Arc::clone(store) as Arc<dyn StoreOps>,
        mode: cfg.mode,
        reasoning: cfg.reasoning,
        approval_timeout: cfg.approval_timeout,
        question_timeout: cfg.question_timeout,
        idle_reap: std::time::Duration::from_secs(
            std::env::var("CWOFFEE_PI_IDLE_REAP")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        ),
        context_budget: cwoffee_bridge::mount::context_budget(cfg.context_budget, window.map(|w| w as usize)),
        budget_pin: cfg.context_budget,
        model_window: window.map(|w| w as usize),
        session_map: agent_dir.join("cwoffee-pi-sessions.json"),
        db_path: Some(sessions_db_path()),
        lean_tools: {
            let (c, _) = cwoffee_engine::config::Config::load(None).unwrap_or_default();
            c.lean_tools()
                || matches!(
                    std::env::var("CWOFFEE_PI_TOOL_MODE").ok().as_deref(),
                    Some("lean")
                )
        },
        persona,
        // The tab the user clicks writes `persona` back to the same config file the
        // app booted from, so the mode is a property of the install, not of one window.
        cfg_path: cfg.cfg_path.clone(),
        max_tool_iters: appcfg.max_tool_iters(),
        compact_timeout: appcfg.compact_timeout(),
        // What a message typed during a turn promises. Read from the file like every other
        // choice, so the composer's announcement is the host's answer and not this window's.
        mid_turn: appcfg.mid_turn(),
        // The palette every surface starts from, read from the same file. The window may carry an
        // override; that lives in this window's own store and never writes this value back.
        theme: appcfg.theme(),
        // A chat starts with no network reach at boot; the composer's Web chip grants it, per conversation.
        web: false,
        mcp_sock: mcp_sock.clone(),
        browser_sock: browser_sock.clone(),
    })
    .await
    .map_err(|e| format!("{e}"))?;
    println!(
        "[cwoffee/pi] core pi {} up in {}; {} extension(s); tools {}; mcp {}",
        host.pi_version,
        workspace.display(),
        ext_count,
        if tools.is_empty() { "pi defaults".to_string() } else { tools.join(",") },
        match &mcp_hub {
            // Named with the bytes it costs every turn, because that number is the whole
            // reason the catalog is a list of names rather than the tool declarations.
            Some(_) => format!(
                "hub up, catalog {}B of a {}B ceiling, {}",
                mcp_catalog.len(),
                cwoffee_engine::mcp::CATALOG_BYTE_BUDGET,
                if mcp_sock.is_some() {
                    "reachable by the tool host"
                } else {
                    "unreachable from the tool host"
                }
            ),
            None => "no server configured".to_string(),
        }
    );
    Ok((host, mcp_hub, mcp_route))
}
