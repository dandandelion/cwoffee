//! Tests for the kept-browser seam. They spawn `scripts/fake-browser-session.mjs`, so the
//! framing, the bookkeeping and the death path run over a real stdio pipe — a stub that
//! answers whatever the client expects would test the client's hopes, not its protocol.

use super::*;
use std::path::PathBuf;
use std::time::Duration;

fn fake_driver() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../scripts/fake-browser-session.mjs");
    p.canonicalize().expect("scripts/fake-browser-session.mjs exists")
}

fn node() -> String {
    std::env::var("CWOFFEE_NODE").unwrap_or_else(|_| "node".to_string())
}

fn hub(idle_secs: u64) -> Hub {
    Hub::mount(SessionCfg {
        session_driver: Some(fake_driver()),
        node: node(),
        idle_secs,
        headed: Some(false),
        native: None,
        ..Default::default()
    })
}

#[tokio::test]
async fn opening_records_who_opened_it_and_acting_records_who_touched_it_next() {
    let mut h = hub(600);
    // The person opened it. That fact outlives the model's later acts, because "this is
    // somebody's window" is what decides whether a model's call needs asking.
    let out = h.open("http://127.0.0.1:9/x", Who::Person, false).await.expect("opens");
    assert_eq!(out["url_after"], "http://127.0.0.1:9/x");
    let s = h.peek();
    assert!(s.alive && s.opened);
    assert_eq!(s.opened_by, Who::Person);
    assert_eq!(s.last_act, "opened http://127.0.0.1:9/x");

    let act = h
        .act(json!({"do": "click", "target": "More"}), Who::Model)
        .await
        .expect("act");
    assert_eq!(act["ok"], true);
    let s = h.peek();
    // Still the person's session…
    assert_eq!(s.opened_by, Who::Person);
    // …but the model moved it, and that is the fact a watching person needs.
    assert_eq!(s.last_actor, Who::Model);
    assert_eq!(s.last_act, "clicked \"More\"");
    assert!(s.says().contains("last touched by the model"), "{}", s.says());
    h.close("test over").await;
    assert!(!h.peek().alive);
    // The *hub's* reason is what a person reads, not the driver's: only the hub knows
    // whether this was the idle clock, the person's Close button, or the app exiting.
    assert_eq!(h.peek().why, "test over");
}

#[tokio::test]
async fn the_page_s_events_are_what_the_panel_shows_without_anybody_asking() {
    let mut h = hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    // The driver announced `nav` before it answered the command. The hub must have taken
    // that on its own: a panel that only updates when polled shows a stale page.
    let s = h.peek();
    assert_eq!(s.url, "http://127.0.0.1:9/page");
    // Two lines now, and that is the point: the driver sent one the old flat way and one inside a
    // structured record. A panel that showed only one of them would be showing the person half of
    // what the page said, depending on which driver build is running.
    assert_eq!(s.console.len(), 2, "the flat line and the record both reach the snapshot: {:?}", s.console);
    assert_eq!(s.console[0].kind, "error");
    // The level comes through as the column's own vocabulary, so a record's console warning looks
    // like the flat form's warning and the panel does not need to know which driver spoke.
    assert_eq!(s.console[1].kind, "warning", "and the record's level is kept: {:?}", s.console);
    assert!(
        s.page_errors.iter().any(|e| e.contains("undefined")),
        "the page's own error reached the snapshot: {:?}",
        s.page_errors
    );
    h.close("test over").await;
}

#[tokio::test]
async fn a_refused_page_is_said_as_the_page_s_own_refusal() {
    let mut h = hub(600);
    let e = h.open("fail://nope", Who::Model, false).await.expect_err("refused");
    assert_eq!(e, "the fake page refused");
    let s = h.peek();
    // The attempt is on the record, and so is the reason. A panel that showed nothing would
    // leave the person wondering whether they typed it at all.
    assert_eq!(s.last_act, "tried to open fail://nope");
    assert_eq!(s.why, "the fake page refused");
    h.close("test over").await;
}

#[tokio::test]
async fn a_browser_that_dies_answers_with_the_reason_instead_of_making_the_caller_wait() {
    let mut h = hub(600);
    let e = h
        .open("crash://gone", Who::Model, false)
        .await
        .expect_err("the browser died mid-open");
    // Not a timeout: the death event resolved the waiting request. The wait for a timeout
    // would be 45 seconds of a frozen panel for an answer that was already known.
    assert!(e.contains("went away"), "{e}");
    let started = std::time::Instant::now();
    let e2 = h
        .act(json!({"do": "click", "target": "x"}), Who::Model)
        .await
        .expect_err("and the next call knows it is dead");
    assert!(started.elapsed() < Duration::from_secs(5), "answered at once: {e2}");
    assert!(h.peek().why.contains("went away"), "{:?}", h.peek().why);
    h.close("test over").await;
}

#[tokio::test]
async fn the_idle_clock_closes_a_browser_nobody_is_using_and_says_the_number() {
    let mut h = hub(1);
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    // Not yet.
    assert!(h.reap_if_idle().await.is_none());
    tokio::time::sleep(Duration::from_millis(1_400)).await;
    let why = h.reap_if_idle().await.expect("closed by the clock");
    // The number a person configured is the number they are told, in the unit they set it
    // in: "1 second", never "0 minutes".
    assert!(why.contains("1 second"), "{why}");
    assert!(why.contains("closed itself"), "{why}");
    let s = h.peek();
    assert!(!s.alive);
    assert_eq!(s.why, why);
    assert!(s.says().contains(&why), "{}", s.says());
}

#[tokio::test]
async fn an_act_that_fails_is_recorded_as_failing() {
    let mut h = hub(600);
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    let out = h
        .act(json!({"do": "click", "target": "Maybe"}), Who::Model)
        .await
        .expect("answered");
    assert_eq!(out["ok"], false);
    assert!(
        h.peek().last_act.contains("failed"),
        "the panel has to say the click did not happen: {:?}",
        h.peek().last_act
    );
    h.close("test over").await;
}

#[tokio::test]
async fn a_frame_is_bytes_for_the_watcher_and_the_snapshot_counts_it() {
    let mut h = hub(600);
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    let f = h.frame(false).await.expect("a frame");
    assert_eq!(f.mime, "image/jpeg");
    assert_eq!(f.b64, "AAEC");
    assert_eq!(f.bytes, 3);
    assert_eq!(h.peek().frames_taken, 1);
    // A poller holding that frame must be able to learn there is nothing newer.
    assert_eq!(h.peek().seq, f.seq);
    assert_eq!(h.frame_seq(), f.seq);
    // An act changes the state without drawing anything: the poller compares `frame_seq`,
    // sees it has not moved, and asks for no bytes.
    h.act(json!({"do": "click", "target": "More"}), Who::Model).await.unwrap();
    assert!(h.peek().seq > f.seq, "the state moved on");
    assert_eq!(h.frame_seq(), f.seq, "and no new picture was taken to pay for");
    h.close("test over").await;
    // A closed session keeps no picture. The alternative is a panel that shows the last
    // page it saw while telling the person the browser is gone.
    assert!(h.frame_now().is_none());
}

#[tokio::test]
async fn a_risk_check_can_see_the_open_session_without_awaiting_a_lock() {
    // `ToolCtx::risk_for` is synchronous. The whole reason the state is mirrored in a
    // `RwLock` rather than living only behind the hub's async mutex is that a risk level
    // which degrades to "no session" because a lock was busy would be a risk level that
    // silently lies about whose browser is whose.
    let h = hub(600);
    let shared = h.shared();
    assert!(!Hub::peek_shared(&shared).alive);
    drop(h);

    let mut h = hub(600);
    let shared = h.shared();
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    let seen = std::sync::Arc::clone(&shared);
    let found = std::thread::spawn(move || Hub::peek_shared(&seen).alive)
        .join()
        .expect("a plain thread can read it");
    assert!(found, "a synchronous read sees the live session");
    h.close("test over").await;
}

#[test]
fn an_act_is_described_in_words_a_person_would_use() {
    assert_eq!(describe_act(&json!({"do": "click", "target": "More"})), "clicked \"More\"");
    assert_eq!(describe_act(&json!({"do": "fill", "target": "note"})), "typed into \"note\"");
    assert_eq!(describe_act(&json!({"do": "press", "key": "Enter"})), "pressed Enter");
    assert_eq!(describe_act(&json!({"do": "scroll", "px": 400})), "scrolled 400px");
    assert_eq!(describe_act(&json!({"do": "reload"})), "reloaded the page");
    // A picture click is said by the point it landed on, in page pixels. The panel shows this line,
    // so it is the record of where the person pointed — and the panel's scaling is the caller's,
    // which means the number a reader sees is the number the browser was handed.
    assert_eq!(describe_act(&json!({"do": "click_xy", "x": 120.6, "y": 200.2})), "clicked at (121, 200)");
    assert_eq!(
        describe_act(&json!({"do": "type_keys", "text": "hello"})),
        "typed 5 character(s) into the page"
    );
    // An `eval` is described by what it was for, not by the code: a panel does not need to
    // paint a JavaScript expression at a person who did not write one.
    assert_eq!(
        describe_act(&json!({"do": "eval", "code": "window.state.count"})),
        "read something out of the page"
    );
}

#[test]
fn the_idle_words_are_the_unit_the_person_configured() {
    assert_eq!(idle_words(300), "5 minutes");
    assert_eq!(idle_words(60), "1 minute");
    assert_eq!(idle_words(45), "45 seconds");
    assert_eq!(idle_words(1), "1 second");
    assert_eq!(idle_words(90), "1.5 minutes");
}

#[test]
fn an_environment_that_names_a_session_driver_wins_over_the_derived_path() {
    // The board runs both drivers out of `scripts/`, where the kept driver is not named
    // `session.mjs`. Without an override the board would be testing "no kept browser", and
    // every claim about the panel would be about a browser that was never started. The rule
    // is graded as a plain function: the variable itself is read by the caller, and mutating
    // a process-wide env var in a test would leak into every other test in this binary.
    let dir = std::env::temp_dir().join(format!("cwoffee-sessdrv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let beside = dir.join("session.mjs");
    let other = dir.join("elsewhere.mjs");
    for f in [&beside, &other] {
        std::fs::write(f, "//").unwrap();
    }

    // Named wins over the derived sibling.
    let got = resolve_session_driver(Some(&other), Some(&dir.join("driver.mjs")));
    assert_eq!(got.as_deref(), Some(other.as_path()), "the variable must win");

    // A named path that is not there is answered as missing — not as the sibling file. An
    // override that quietly falls back is how a broken `driver=` line ends up "working"
    // against a file nobody meant to run.
    let got = resolve_session_driver(Some(&dir.join("nope.mjs")), Some(&dir.join("driver.mjs")));
    assert_eq!(got, None, "a broken override must not fall back");

    // With nothing named, the sibling of the one-shot driver is what is used, and only if it
    // is a real file: no `session.mjs` means no kept browser, said out loud.
    let got = resolve_session_driver(None, Some(&dir.join("driver.mjs")));
    assert_eq!(got.as_deref(), Some(beside.as_path()));
    let got = resolve_session_driver(None, None);
    assert_eq!(got, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hub_with_no_driver_says_what_is_missing_and_how_to_put_it_back() {
    let h = Hub::mount(SessionCfg {
        session_driver: None,
        node: node(),
        idle_secs: 300,
        headed: None,
        native: None,
        ..Default::default()
    });
    assert!(!h.cfg().usable());
    let line = status_line(h.cfg(), &h.peek());
    assert!(line.contains("not mounted"), "{line}");
    assert!(line.contains("browser/session.mjs"), "{line}");
    assert!(line.contains("npm --prefix browser install"), "{line}");
    assert!(line.contains("npx playwright install"), "{line}");
    // And it promises no browser it does not have.
    assert!(!h.peek().alive);
    assert!(h.peek().why.contains("driver"), "{:?}", h.peek().why);
}

#[test]
fn the_panel_says_why_there_is_no_browser_rather_than_only_that_there_is_none() {
    // The three arms a person can land in, each in its own words. A single "no browser is
    // open." would be true in all three and useful in none: one is a switch they turned, one is
    // a file that is not installed, and one is a timeout they need to read the number of.
    let off = SessionCfg {
        session_driver: Some(PathBuf::from("/nowhere/session.mjs")),
        node: "node".to_string(),
        idle_secs: 300,
        headed: None,
        native: None,
        off_reason: Some("kept browsers are off: [browser] keep = false in cwoffee's config".to_string()),
    };
    let snap = Snapshot::default();
    let words = panel_says(&off, &snap);
    assert!(words.contains("switched off"), "{words}");
    assert!(words.contains("keep = false"), "{words}");

    let missing = SessionCfg {
        session_driver: None,
        node: "node".to_string(),
        idle_secs: 300,
        headed: None,
        native: None,
        off_reason: None,
    };
    let words = panel_says(&missing, &snap);
    assert!(words.contains("npm --prefix browser install"), "{words}");
    assert!(words.contains("npx playwright install chromium"), "{words}");
    // One sentence, not a sentence with a paragraph break baked into the literal.
    assert!(!words.contains("  "), "{words}");

    // A closed-after-idle browser gets the snapshot's own sentence, number included: the
    // difference between a timeout and a crash is the thing being asked about.
    let open = Snapshot {
        alive: true,
        opened: true,
        url: "http://x/y".to_string(),
        idle_deadline: Some(now_secs() + 120),
        ..Default::default()
    };
    assert!(panel_says(&missing, &open).contains("http://x/y"), "{}", panel_says(&missing, &open));
    let shut = Snapshot {
        alive: false,
        why: "closed itself after 4 seconds of nobody touching it".to_string(),
        ..Default::default()
    };
    let words = panel_says(&missing, &shut);
    assert!(words.contains("closed itself after 4 seconds"), "{words}");
    // Both facts, in one line: the page is gone because of the clock, *and* this start has no
    // driver to put it back with. A panel that said only one of the two sends the reader to
    // look at the wrong thing.
    assert!(words.contains("npx playwright install chromium"), "{words}");
    let words = panel_says(&off, &shut);
    assert!(words.contains("closed itself after 4 seconds"), "{words}");
    assert!(words.contains("keep = false"), "{words}");
}

#[test]
fn a_switched_off_kept_browser_is_reported_as_switched_off_not_as_broken() {
    // The two refusals need different sentences. "Install the driver" to a person who
    // deliberately set `keep = false` is a bug that reads like advice; and a panel that
    // shows a control which the config refuses is worse than one that says it is off.
    let h = Hub::mount(SessionCfg {
        session_driver: None,
        node: node(),
        idle_secs: 300,
        headed: None,
        native: None,
        off_reason: Some("kept browsers are off: [browser] keep = false in cwoffee's config".to_string()),
    });
    let line = status_line(h.cfg(), &h.peek());
    assert!(line.contains("switched off"), "{line}");
    assert!(line.contains("keep = false"), "{line}");
    assert!(!line.contains("npm --prefix"), "no install advice for a deliberate choice: {line}");
    assert!(h.peek().why.contains("keep = false"), "{:?}", h.peek().why);
}

#[test]
fn the_kept_driver_is_found_beside_the_one_shot_one_and_not_invented() {
    let dir = std::env::temp_dir().join(format!("cwoffee-sesscfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let one = dir.join("driver.mjs");
    let kept = dir.join("session.mjs");
    std::fs::write(&one, "// one-shot").unwrap();
    std::fs::write(&kept, "// kept").unwrap();
    let cfg = BrowserCfg {
        driver: Some(one.clone()),
        node: "node".into(),
        ..Default::default()
    };
    let s = SessionCfg::from_browser_cfg(&cfg);
    assert_eq!(s.session_driver.as_deref(), Some(kept.as_path()));
    // Take the kept driver away and the feature is *absent*, not half-present: a panel that
    // opens and then cannot explain why nothing is there is worse than no panel.
    std::fs::remove_file(&kept).unwrap();
    let s2 = SessionCfg::from_browser_cfg(&cfg);
    assert_eq!(s2.session_driver, None);
    assert!(!s2.usable());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_phone_that_cannot_drive_the_browser_is_told_so_in_the_payload() {
    let mut h = hub(600);
    let snap = h.peek();
    let desktop = status_payload(h.cfg(), &snap, true, true, "this window's own browser");
    assert_eq!(desktop["writable"], true);
    let phone = status_payload(h.cfg(), &snap, true, false, "the desktop's browser, over the socket");
    assert_eq!(phone["writable"], false);
    // The machine it belongs to is part of the payload, so a read-only row cannot read as
    // a control that is merely broken.
    assert_eq!(phone["route"], "the desktop's browser, over the socket");
    assert!(phone["says"].as_str().unwrap().contains("no browser is open"));
    // An open session says what is open and how long is left on the clock.
    h.open("http://127.0.0.1:9/page", Who::Model, false).await.unwrap();
    let snap = h.peek();
    let p = status_payload(h.cfg(), &snap, true, false, "here");
    assert_eq!(p["alive"], true);
    assert_eq!(p["url"], "http://127.0.0.1:9/page");
    assert_eq!(p["opened_by"], "model");
    assert!(p["idle_left"].as_i64().unwrap() > 0);
    assert!(p["says"].as_str().unwrap().contains("a browser is open at http://127.0.0.1:9/page"));
    // And the address is separated from what follows it. A page whose url ends in a fragment ran
    // straight into the next clause, so the panel showed `/page#firstlast touched by you` — a
    // string that is neither the address the person might copy nor a sentence worth reading.
    h.open("http://127.0.0.1:9/page#first", Who::Person, false).await.unwrap();
    let says = h.peek().says();
    assert!(says.contains("page#first — "), "{says}");
    assert!(!says.contains("firstlast") && !says.contains("firstnobody"), "{says}");
    h.close("test over").await;
}

#[tokio::test]
async fn an_adopted_config_changes_the_clock_without_losing_the_page() {
    // Installing a driver, or moving `[browser] idle_secs`, happens while the app is open.
    // A restart to apply either would make the settings row a lie about itself.
    let mut h = hub(600);
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    h.adopt(SessionCfg {
        session_driver: Some(fake_driver()),
        node: node(),
        idle_secs: 42,
        headed: Some(false),
        native: None,
        ..Default::default()
    });
    let s = h.peek();
    assert_eq!(s.idle_secs, 42);
    assert!(s.alive, "the browser it is describing is still the one that is open");
    h.close("test over").await;
}

#[tokio::test]
#[ignore = "needs a real Chromium: cargo test -p cwoffee-engine --lib -- --ignored --test-threads=1 browser"]
async fn the_real_browser_streams_frames_of_a_page_that_is_moving_on_its_own() {
    // The live view, end to end: the real driver, the real Chromium, and a page that repaints with
    // nothing acting on it. Everything else in this file grades the seam against a fake that answers
    // what the hub expects; this grades the claim a person actually makes — "the page is moving and
    // the panel shows it moving" — which no fake can produce, because a fake has no repaints.
    let driver = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../browser/session.mjs");
    let driver = driver.canonicalize().expect("browser/session.mjs exists");
    let page_dir = std::env::temp_dir().join(format!("cwoffee-live-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&page_dir).unwrap();
    let page = page_dir.join("moving.html");
    std::fs::write(
        &page,
        "<!doctype html><body style=\"margin:0;background:#fff\"><div id=c></div>\
         <script>let n=0;setInterval(()=>{const s=[];for(let i=0;i<60;i++)s.push('x'.repeat(1+Math.floor(Math.random()*40)));\
         document.getElementById('c').textContent=(n++)+' '+s.join(' ')},100)</script>",
    )
    .unwrap();
    // A headless shell is enough here and it runs on a board with no display; the live view is pixels
    // over a pipe, not a window.
    std::env::set_var("CWOFFEE_BROWSER_HEADLESS_ONLY", "1");
    let mut h = Hub::mount(SessionCfg {
        session_driver: Some(driver),
        node: node(),
        idle_secs: 600,
        headed: Some(false),
        native: None,
        ..Default::default()
    });
    let url = format!("file://{}", page.display());
    h.open(&url, Who::Person, true).await.expect("the page opens");
    assert!(h.peek().live == false, "nothing is streaming until somebody asks");

    let w = h.watch(true, Some(420)).await.expect("the real driver can stream");
    assert_eq!(w["live"], true, "Chromium's screencast starts: {:?}", w);
    // The box the watching panel showed is the box the stream is sized to. A page is 1280-ish wide
    // and the column that shows it is often 400px; streaming the page at its own width is a hundred
    // kilobytes of JPEG per picture travelling to a box that draws a third of it.
    assert_eq!(w["picture"]["width"], 420, "and the answer names the picture it will send: {w}");
    let mut small_bytes: u64 = 0;
    for _ in 0..120 {
        if let Some(f) = h.frame_now() {
            if f.live {
                small_bytes = f.bytes;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(small_bytes > 0, "a 420px stream still sends real pictures, not stubs");

    // No act, no screenshot request, no page event: only the page's own timer. A still-only panel
    // would show this page standing still, which is the whole reason the live view exists.
    let mut first: Option<u64> = None;
    for _ in 0..120 {
        if let Some(f) = h.frame_now() {
            if f.live {
                first = Some(f.pic);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pic = first.expect("the real browser volunteered a frame of a page that was moving");
    assert!(h.peek().live, "and the panel is told this is a live view");
    // ...and it keeps coming: a second frame with a newer id, still with nothing asked of it.
    let mut later = 0;
    for _ in 0..120 {
        let n = h.frame_pic();
        if n > pic {
            later = n;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(later > pic, "the stream stopped after one frame ({pic}) — a panel would freeze on it");

    // The same page with no width hint: the browser's own width, and measurably more bytes for the
    // same page. This is the cost claim the panel's box exists to keep down.
    let no_hint = h.watch(true, Some(0)).await.expect("a re-ask with no hint asks for the page");
    assert_eq!(no_hint["picture"]["width"], 1280, "no hint means the page's own width: {no_hint}");
    let mut big_bytes: u64 = 0;
    for _ in 0..120 {
        if let Some(f) = h.frame_now() {
            if f.live && f.bytes > small_bytes {
                big_bytes = f.bytes;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        big_bytes > small_bytes * 2,
        "streaming the page at its own width cost {big_bytes} bytes against {small_bytes} for the panel's box"
    );
    h.watch(true, Some(420)).await.expect("back to the box it is shown in");

    // Full window: the page is reshaped to the box, and the size the panel scales its clicks by is
    // the size the browser reports, not the one it was asked for.
    let out = h.set_viewport(900, 640).await.expect("the page resizes");
    assert_eq!(out["moved"], true);
    let s = h.peek();
    assert_eq!((s.viewport_w, s.viewport_h), (900, 640), "the panel's arithmetic reads this size");
    assert!(h.frame_now().map(|f| f.live).unwrap_or(false), "and the stream survived the resize");

    h.watch(false, None).await.unwrap();
    assert!(!h.peek().live, "stopping the watch stops the browser's work for a panel that is gone");
    h.close("e2e over").await;
    assert!(h.frame_now().is_none(), "and no picture of a closed browser is left on screen");
    let _ = std::fs::remove_dir_all(&page_dir);
}

#[test]
fn a_page_that_changed_without_a_picture_of_its_own_still_owes_the_viewer_one() {
    use super::needs_pic;
    // Nothing happened since the viewer's picture: no screenshot to prove it.
    assert!(!needs_pic(9, 9, true), "a viewer holding the newest picture of the newest state is done");
    // The act (a reload, a click) bumped the state and no picture followed it yet. This is the
    // case a comparison of `since` against the state alone answers with "nothing newer", which is
    // how a panel ends up painting the page as it was before the reload.
    assert!(needs_pic(9, 9, false), "the page changed and no picture shows it yet");
    // Someone else's viewer already took the picture for the new state; this one is behind it.
    assert!(needs_pic(9, 10, true), "a newer picture exists and this viewer does not hold it");
    // The live view drew three frames of a page that never changed state: the state sequence says
    // "nothing happened", the picture ids say the page is moving. Only the second one reaches a
    // person watching an animation.
    assert!(needs_pic(11, 14, true), "a repaint with no act in front of it still counts as new");
    // And the settled case, twice running, is what keeps a frequent poll cheap.
    assert!(!needs_pic(11, 11, true), "polling a quiet page takes no new pictures");
    assert!(!needs_pic(11, 11, true), "polling a quiet page takes no new pictures");
}

#[tokio::test]
async fn a_frame_the_browser_volunteers_reaches_the_viewer_with_an_id_of_its_own() {
    let mut h = hub(600);
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    assert!(!h.peek().live, "nobody has asked for the live view yet");
    assert!(h.frame_now().is_none(), "and nothing has been drawn");

    let w = h.watch(true, Some(400)).await.expect("the driver can stream");
    assert_eq!(w["live"], true);
    // The panel's box travels. A 1280-wide page streamed to a 400px column is a hundred-odd kilobytes
    // of JPEG per picture for a box that draws a third of it, so the ask carries the width and the
    // answer names the picture it will actually send.
    assert_eq!(w["picture"]["width"], 400, "the stream is sized to the box that shows it: {w}");
    assert!(h.peek().live, "the panel is told this is a live view, not a still");

    // The fake driver volunteers a frame a moment after `watch`, with no command in front of it —
    // the shape of a repaint. A panel that only ever learned about the page from acts would show
    // this page standing still.
    let mut seen: Option<Arc<Frame>> = None;
    for _ in 0..60 {
        if let Some(f) = h.frame_now() {
            seen = Some(f);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let f = seen.expect("a frame the browser offered lands in the slot a poller reads");
    assert!(f.live, "and it is known to be the browser's own frame, not one someone paid for");
    assert!(f.pic > 0, "a picture of its own, counted separately from the page state");
    assert_eq!(h.peek().frame_pic, f.pic);
    assert_eq!(h.peek().frames_taken, 1, "the snapshot counts the picture it stored");
    // A viewer holding the newest picture is done; one that is behind is owed bytes.
    assert!(!needs_pic(f.pic, h.frame_pic(), h.frame_pictures_state(h.peek().seq)));
    assert!(needs_pic(f.pic - 1, h.frame_pic(), true));
    // Re-asking with no new hint keeps the box the watcher already stated (the panel re-states the
    // ask whenever its box is measured, and must not forget the width it gave).
    let again = h.watch(true, None).await.expect("re-asking is not an error");
    assert_eq!(again["picture"]["width"], 400, "and the hint survived the re-ask: {again}");
    // A second volunteered picture gets a newer id rather than re-labelling the first: two frames of
    // a moving page are two pictures, and a viewer holding the older one has to be able to tell.
    let mut second = 0;
    for _ in 0..60 {
        let n = h.frame_pic();
        if n > f.pic {
            second = n;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(second > f.pic, "the stream is still coming; a panel would freeze on the first frame");
    assert_eq!(h.peek().frames_taken, 2, "and each picture is counted, not just the first");

    h.watch(false, None).await.unwrap();
    assert!(!h.peek().live, "the load note: a screencast runs while somebody is looking, and stops when they stop");
    h.close("test over").await;
    assert!(h.frame_now().is_none(), "and no picture outlives the browser it was a picture of");
    assert_eq!(h.peek().frame_pic, 0, "a fresh session starts its own picture count");
}

#[tokio::test]
async fn a_borrowed_process_cannot_start_the_stream_or_reshape_the_page() {
    // The socket door is how a tool process reaches the app's kept browser. It answers the verbs a
    // borrowed session needs — state, open, act, frame, close — and `watch` and `viewport` are not
    // among them: the stream and the shape of the page belong to whoever is *looking* at it. A
    // process that could switch a screencast on could leave a browser encoding for a panel that does
    // not exist, and one that could resize the page would be sizing a window it has never seen.
    let dir = std::env::temp_dir().join(format!("cwoffee-bhub-door-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("browser.sock");
    let _ = std::fs::remove_file(&sock);
    let hub = Arc::new(tokio::sync::Mutex::new(hub(600)));
    socket::serve(Arc::clone(&hub), &sock).await.expect("the door listens");
    let door = |req: Value| {
        let sock = sock.clone();
        async move { socket::ask(&sock, &req, Duration::from_secs(5)).await.expect("the door answers") }
    };

    let opened = door(json!({ "cmd": "open", "url": "http://127.0.0.1:9/x", "who": "model" })).await;
    assert_eq!(opened["ok"], true, "a borrowed session may still open a page: {opened}");
    for cmd in ["watch", "viewport"] {
        let r = door(json!({ "cmd": cmd, "on": true, "width": 1600, "height": 900 })).await;
        assert_eq!(r["ok"], false, "`{cmd}` is the panel's verb, not a borrowed process's: {r}");
        assert!(r["why"].as_str().unwrap().contains("does not answer"), "{r}");
    }
    let s = hub.lock().await.peek();
    assert!(!s.live, "and the refusal did not start a stream");
    assert_ne!((s.viewport_w, s.viewport_h), (1600, 900), "nor resized the page");
    drop(s);

    // The live view the *panel* turned on is still legible to the borrower: a tool process carries
    // the same mirror the app does, and the sentence it tells the model ("this page is on somebody's
    // screen, and it is moving") reads these fields rather than guessing.
    hub.lock().await.watch(true, None).await.expect("the panel may stream");
    let mut pic = 0;
    for _ in 0..80 {
        pic = hub.lock().await.frame_pic();
        if pic > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pic > 0, "the frame the browser volunteered has a picture id of its own");
    let v = door(json!({ "cmd": "state" })).await;
    assert_eq!(v["live"], true, "and a borrowed process reads that the view is live: {v}");
    assert_eq!(v["frame_pic"], pic, "and it can compare against the picture it holds: {v}");
    let f = door(json!({ "cmd": "frame" })).await;
    assert_eq!(f["ok"], true);
    assert!(f["pic"].as_u64().unwrap() >= pic, "a picture pulled over the door says which picture it is: {f}");
    // …and it is a picture *paid for now*, not one off the stream. A borrowed process asking for a
    // frame is a tool asking for a screenshot of the page as it stands; the panel's stream is a
    // picture up to a frame tick old, and the door does not silently substitute it.
    assert_eq!(f["live"], false, "{f}");

    door(json!({ "cmd": "close", "why": "the borrowed process is done" })).await;
    assert!(!hub.lock().await.peek().alive, "close still closes the browser it was asked to close");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resizing_the_page_to_the_panel_reports_the_size_that_actually_happened() {
    let mut h = hub(600);
    h.open("http://127.0.0.1:9/x", Who::Person, false).await.unwrap();
    let out = h.set_viewport(1600, 900).await.expect("a page can be reshaped");
    assert_eq!(out["moved"], true);
    let s = h.peek();
    assert_eq!((s.viewport_w, s.viewport_h), (1600, 900), "and the panel's click arithmetic reads the size that is really open, not the one it asked for");
    // A box too small to read is refused before it reaches the browser, rather than clamped into
    // something the person cannot aim at.
    assert!(h.set_viewport(100, 100).await.is_err());
    assert_eq!((h.peek().viewport_w, h.peek().viewport_h), (1600, 900), "and the refusal changes nothing");
    h.close("test over").await;
}

// --- a browser that is a page in the app's own window -------------------------------------

/// A page view living **in this process**: no child, no pipes, and the same line protocol. This
/// exists to prove the hub's rules do not depend on the transport. If the page in the window got
/// its own copy of the idle clock, the who-opened-it record and the panel's sentences, those rules
/// would drift apart the moment one side was edited — and "the browser you are looking at closed
/// itself and never said why" is the failure the whole module exists to prevent.
struct FakePage {
    out: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    seen: std::sync::Mutex<Vec<Value>>,
}

impl FakePage {
    fn new() -> Self {
        Self { out: std::sync::Mutex::new(None), seen: std::sync::Mutex::new(Vec::new()) }
    }
    fn emit(&self, v: Value) {
        if let Some(tx) = self.out.lock().unwrap().as_ref() {
            let _ = tx.send(v.to_string());
        }
    }
    fn cmds(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| c.get("cmd").and_then(|c| c.as_str()).map(str::to_string))
            .collect()
    }
}

impl NativeDriver for FakePage {
    fn attach(&self, out: tokio::sync::mpsc::UnboundedSender<String>) {
        *self.out.lock().unwrap() = Some(out);
        // A page view reports its size the way a driver does: from the window it is really in.
        self.emit(json!({"ev": "ready", "viewport": {"width": 400, "height": 620}}));
    }

    fn send(&self, line: &str) {
        let Ok(cmd) = serde_json::from_str::<Value>(line) else { return };
        let id = cmd.get("id").cloned().unwrap_or(Value::Null);
        self.seen.lock().unwrap().push(cmd.clone());
        match cmd.get("cmd").and_then(|c| c.as_str()).unwrap_or("") {
            "open" => {
                let url = cmd["url"].as_str().unwrap_or("").to_string();
                self.emit(json!({"ev": "nav", "url": url}));
                self.emit(json!({"ev": "console", "type": "log", "text": "the page spoke up"}));
                self.emit(json!({"id": id, "ok": true, "url_after": url, "title": "A page",
                    "viewport": {"width": 400, "height": 620}}));
            }
            "act" => self.emit(json!({"id": id, "ok": true, "url_after": "http://127.0.0.1:9/page"})),
            // Where the panel asked for the page, and where the widget tree put it. The fake answers
            // with the box it was given — a real one sometimes answers with something else, and the
            // hub's job is to carry the answer, not the request.
            "view_rect" => {
                let (w, h) = (cmd["width"].as_f64().unwrap_or(0.0), cmd["height"].as_f64().unwrap_or(0.0));
                self.emit(json!({"ev": "placed", "placed": {
                    "x": cmd["x"].as_f64().unwrap_or(0.0) as i64,
                    "y": cmd["y"].as_f64().unwrap_or(0.0) as i64,
                    "width": w as i64, "height": h as i64, "shown": w >= 1.0 && h >= 1.0
                }}));
                self.emit(json!({"id": id, "ok": true, "width": w as i64, "height": h as i64}));
            }
            "diag" => {
                // A page view can volunteer what it saw without anybody asking. The hub keeps it as
                // a record, which is what makes "the page printed something" and "a request 404'd"
                // two different sentences instead of one list of strings.
                self.emit(json!({"ev": "diag", "diag": cmd["diag"]}));
            }
            "state" => self.emit(json!({"id": id, "ok": true, "alive": true,
                "url": "http://127.0.0.1:9/page", "title": "A page", "live": false,
                "live_why": "the page is on screen itself; there is nothing to stream",
                "viewport": {"width": 400, "height": 620}})),
            "watch" => self.emit(json!({"id": id, "ok": true, "live": false,
                "why": "the page is on screen itself; there is nothing to stream"})),
            "frame" => self.emit(json!({"id": id, "ok": false,
                "why": "there is no picture of a page that is already on screen"})),
            "close" => {
                self.emit(json!({"id": id, "ok": true}));
                self.emit(json!({"ev": "closed", "why": "the panel put the page away"}));
            }
            _ => self.emit(json!({"id": id, "ok": false, "why": "the page view does not do that"})),
        }
    }

    fn quit(&self) {
        self.emit(json!({"ev": "closed", "why": "the page view was put away"}));
    }
}

fn native_hub(idle_secs: u64) -> (Hub, Arc<FakePage>) {
    let page = Arc::new(FakePage::new());
    let h = Hub::mount(SessionCfg {
        session_driver: None,
        idle_secs,
        native: Some(page.clone()),
        ..Default::default()
    });
    (h, page)
}

/// The hub writes its own `navigation` record for every move it hears about (see `note_move`). Tests
/// that count what the *page* said filter those out, so a count of the page's records stays a count of
/// the page's records.
fn page_records(diags: &[Diag]) -> Vec<Diag> {
    diags.iter().filter(|d| d.source != "hub").cloned().collect()
}

#[tokio::test]
async fn records_from_two_documents_never_arrive_with_the_same_id() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/one", Who::Person, false).await.unwrap();
    // A page counts its own records from one every time a document loads. The ids the app hands out
    // have to come from the app: two records with the same id break a drawer that keys its rows by id
    // and a reader that asks for "anything newer than 12".
    page.emit(json!({"ev": "diag", "diag": {"seq": 1, "t": 1, "kind": "console", "level": "info", "text": "first page speaks"}}));
    page.emit(json!({"ev": "nav", "url": "http://127.0.0.1:9/two", "title": "Two"}));
    // The hub's own record of that move rides along. A browsing model's first question is "did that
    // click take me somewhere", and a log that answers `kind: navigation` with nothing hides the one
    // fact the app is certain about.
    page.emit(json!({"ev": "diag", "diag": {"seq": 1, "t": 2, "kind": "console", "level": "info", "text": "second page speaks"}}));
    for _ in 0..40 {
        if page_records(&h.peek().diagnostics).len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = h.peek();
    let mine = page_records(&s.diagnostics);
    // 2 the two documents spoke, 1 the flat line the driver relayed when the first arrived.
    assert_eq!(mine.len(), 3, "both documents' records are kept: {:?}", s.diagnostics);
    assert_ne!(mine[0].seq, mine[1].seq, "and they do not share an id");
    // What must never happen is a row that repeats or an id a reader cannot compare against. The ids
    // come from the app, so they climb in the order the records landed — including the flat line the
    // driver relayed, which has an id of its own between the two documents' words.
    let mut ids: Vec<i64> = mine.iter().map(|d| d.seq).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "three records, three different ids: {ids:?}");
    let oldest = mine.iter().map(|d| d.seq).min().unwrap();
    let newest = mine.iter().map(|d| d.seq).max().unwrap();
    let newer = s.diagnostics_since(oldest, Some("console"), None, 10);
    assert_eq!(newer.len(), 2, "two records landed after the oldest: {newer:?}");
    assert_eq!(newer[0].seq, newest, "and the read comes back newest first");
    // The move between them is in the log too, written by the hub rather than by the page.
    let moved: Vec<_> = s
        .diagnostics_since(0, Some("navigation"), None, 10)
        .into_iter()
        .filter(|d| d.source == "hub")
        .collect();
    assert_eq!(moved.len(), 2, "the page arrived, then a second document arrived: {:?}", moved);
    // Newest first: that is what "only what happened after" means to a reader.
    assert_eq!(moved[0].url.as_deref(), Some("http://127.0.0.1:9/two"));
    assert_eq!(moved[1].url.as_deref(), Some("http://127.0.0.1:9/one"));
    h.close("test over").await;
}

#[tokio::test]
async fn the_page_in_the_window_reports_the_box_it_was_given_and_a_box_of_none_hides_it() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    // The panel's numbers reach the snapshot as the widget's numbers, because a panel cannot grade
    // its own request: it can only be told what the widget tree made of it.
    h.view_rect(305.0, 86.0, 284.0, 370.0).await.unwrap();
    let s = h.peek();
    assert_eq!((s.placed_x, s.placed_y, s.placed_w, s.placed_h), (305, 86, 284, 370), "the box the page got");
    assert!(s.placed_shown, "a box with a size is a page that paints");
    // A panel that closes reports no box. That is not a bad request to refuse — it is the only way
    // a page gets taken out of the way, and a page that cannot be hidden sits on the conversation.
    h.view_rect(0.0, 0.0, 0.0, 0.0).await.unwrap();
    let gone = h.peek();
    assert_eq!((gone.placed_w, gone.placed_h), (0, 0), "the page was told to go away");
    assert!(!gone.placed_shown, "and it is not shown");
    // A page in another process has no box to fill, and says which kind of browser it is.
    let _ = page;
    h.close("test over").await;
}

#[tokio::test]
async fn a_page_says_what_it_saw_and_the_hub_keeps_it_as_a_record_not_a_line() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    // Sent as commands on purpose: the order is what the test then asserts on, and events racing
    // past each other would make the assertions about which page a record belongs to meaningless.
    page.send(&json!({"cmd": "diag", "diag": {"seq": 1, "t": 1_700_000_000_000i64, "kind": "console", "level": "info", "text": "the page printed a thing"}}).to_string());
    page.send(&json!({"cmd": "diag", "diag": {"seq": 2, "t": 1_700_000_000_001i64, "kind": "error", "level": "error", "text": "null is not an object", "where": "app.js:12:9"}}).to_string());
    page.send(&json!({"cmd": "diag", "diag": {"seq": 3, "t": 1_700_000_000_002i64, "kind": "network", "level": "error", "text": "nope", "url": "http://127.0.0.1:9/api", "status": 503, "method": "POST"}}).to_string());
    page.send(&json!({"cmd": "diag", "diag": {"seq": 4, "t": 1_700_000_000_003i64, "kind": "resource", "level": "error", "text": "img could not load", "url": "http://127.0.0.1:9/gone.png"}}).to_string());
    page.send(&json!({"cmd": "diag", "diag": {"seq": 5, "t": 1_700_000_000_004i64, "kind": "download", "level": "info", "text": "report.pdf saved to the workspace", "url": "http://127.0.0.1:9/report.pdf"}}).to_string());
    for _ in 0..40 {
        if page_records(&h.peek().diagnostics).len() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = h.peek();
    // 5 the page reported through the structured path, plus the line the driver relayed flat when the
    // document arrived — which lands as a record too, so the drawer and the model's tool cannot be
    // shown different things about the same page.
    assert_eq!(page_records(&s.diagnostics).len(), 6, "every kind the page reported is kept: {:?}", s.diagnostics);
    assert!(
        s.diagnostics.iter().any(|d| d.kind == "console" && d.text == "the page spoke up"),
        "a flat `console` event from the driver is a record as well as a panel line: {:?}",
        s.diagnostics
    );
    // 5 structured records, 1 flat console line relayed by the driver, 1 the hub wrote for the page
    // arriving (note_move). Every one of them got an id from the app.
    assert_eq!(s.diag_seq, 7, "the newest record's id is what a reader compares against");
    assert!(
        s.diagnostics.iter().any(|d| d.kind == "navigation" && d.url.as_deref() == Some("http://127.0.0.1:9/page")),
        "the hub also logged the move itself, because a model's first question is where the walk has been"
    );
    let net = s.diagnostics.iter().find(|d| d.kind == "network").expect("the failed request is there");
    assert_eq!(net.status, Some(503), "the status survives, because the status is the useful part");
    assert_eq!(net.method.as_deref(), Some("POST"));
    assert_eq!(net.url.as_deref(), Some("http://127.0.0.1:9/api"));
    assert_eq!(net.nav, s.nav as i64, "a record says which page it belongs to: {} vs {}", net.nav, s.nav);
    assert_eq!(
        s.diagnostics.iter().find(|d| d.kind == "error").unwrap().at.as_deref(),
        Some("app.js:12:9"),
        "and where in the page it happened"
    );
    // The panel's columns are filled from the same records, so the person's drawer and the model's
    // tool cannot be shown different numbers.
    assert!(s.console.iter().any(|l| l.text == "the page printed a thing"), "the console column");
    assert!(s.page_errors.iter().any(|t| t == "null is not an object"), "the errors column");
    assert!(s.requests_failed.iter().any(|t| t.contains("503")), "the failed requests column");
    assert!(s.downloads_blocked.iter().any(|t| t.contains("report.pdf")), "the downloads column");
    h.close("test over").await;
}

#[tokio::test]
async fn a_console_message_is_not_treated_as_a_failure_by_the_health_report() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    for (i, (kind, level)) in [
        ("console", "info"),
        ("console", "error"), // the page chose to shout; that is not an exception
        ("error", "error"),
        ("network", "error"),
        ("navigation", "warning"),
    ]
    .iter()
    .enumerate()
    {
        page.send(&json!({"cmd": "diag", "diag": {"seq": i + 1, "kind": kind, "level": level, "text": format!("a {kind} at {level}")}}).to_string());
    }
    for _ in 0..40 {
        if page_records(&h.peek().diagnostics).len() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = h.peek();
    let said = s.diagnostics_since(0, Some("console"), Some("error"), 10);
    assert_eq!(said.len(), 1, "a console.error stays a console message: {:?}", said);
    assert!(!said[0].is_failure(), "and the hub does not call a shout a failure");
    let report = s.health();
    // 2 the test asked for, plus the page's first line relayed flat by the driver on open.
    assert_eq!(report["counts"]["console"], 3, "the report counts by kind: {report}");
    assert_eq!(report["failed_total"], 3, "three of the five are things that went wrong: {report}");
    assert_eq!(report["nav"], s.nav);
    // The page sent one navigation record; the hub added its own for the document arriving. The two
    // are distinguishable by `source`, which is the point: "the page logged nothing" is not the same
    // answer as "nothing happened".
    assert_eq!(report["counts"]["navigation"], 2, "the page's own and the hub's record of it arriving: {report}");
    assert_eq!(page_records(&s.diagnostics).iter().filter(|d| d.kind == "navigation").count(), 1);
    // A reader who has seen everything but the last record of the page is only shown that one. The
    // ids are the hub's, so read them off the records rather than assuming what numbering began at.
    let mine = page_records(&s.diagnostics);
    let seen = mine[mine.len() - 2].seq;
    let fresh = s.diagnostics_since(seen, None, None, 10);
    assert_eq!(fresh.len(), 1, "one record is newer than {seen}: {:?}", s.diagnostics);
    assert_eq!(fresh[0].seq, mine[mine.len() - 1].seq, "and it is the one they had not seen");
    h.close("test over").await;
}

#[tokio::test]
async fn a_page_that_wont_stopping_logging_does_not_grow_the_app() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    for i in 1..=(DIAG_CAP + 40) {
        page.send(&json!({"cmd": "diag", "diag": {"seq": i, "kind": "console", "level": "info", "text": format!("line {i}")}}).to_string());
    }
    for _ in 0..80 {
        if h.peek().diag_seq >= (DIAG_CAP + 40) as u64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = h.peek();
    assert_eq!(s.diagnostics.len(), DIAG_CAP, "the buffer is bounded, however much the page talks");
    assert_eq!(
        s.diag_seq,
        (DIAG_CAP + 42) as u64,
        "and the id keeps counting (the page's 40, the line the driver relayed flat on open, and the \
         hub's own record of the page arriving)"
    );
    assert_eq!(s.diagnostics.last().unwrap().seq, s.diag_seq as i64, "the newest record is the newest one");
    assert_eq!(s.console.len(), 60, "the panel's column is bounded too");
    h.close("test over").await;
}

#[tokio::test]
async fn a_page_in_the_window_answers_the_same_commands_a_driver_process_would() {
    let (mut h, page) = native_hub(600);
    let out = h.open("http://127.0.0.1:9/page", Who::Person, false).await.expect("the page opens");
    assert_eq!(out["url_after"], "http://127.0.0.1:9/page");
    let s = h.peek();
    assert!(s.alive && s.opened, "a page in the window is a live session like any other");
    assert_eq!(s.opened_by, Who::Person);
    assert_eq!(s.url, "http://127.0.0.1:9/page", "the `nav` line reached the snapshot unprompted");
    assert_eq!((s.viewport_w, s.viewport_h), (400, 620), "the window's own size, not a requested one");
    assert_eq!(s.console.len(), 1, "the page's console line reaches the panel with nobody asking");

    h.act(json!({"do": "click", "target": "Send"}), Who::Model).await.expect("the model drives it");
    let s = h.peek();
    assert_eq!(s.opened_by, Who::Person, "still somebody's window…");
    assert_eq!(s.last_actor, Who::Model, "…and the model is the one who moved it");
    assert_eq!(s.last_act, "clicked \"Send\"");

    // A page already on screen has no picture of itself. The hub must report that as *no frame*,
    // not as a broken browser.
    let f = h.frame(false).await.expect_err("there is no picture of the page you are looking at");
    assert!(f.contains("already on screen"), "{f}");
    assert!(h.frame_now().is_none(), "and nothing is left in the frame slot");
    assert!(h.peek().alive, "while the page stays open");

    assert!(page.cmds().contains(&"open".to_string()) && page.cmds().contains(&"act".to_string()));
    h.close("test over").await;
    assert!(!h.peek().alive);
}

#[tokio::test]
async fn the_address_moving_within_one_page_moves_the_hub_s_answer_without_moving_the_page_count() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    let was = h.peek().nav;
    // A fragment is not a new page. Somebody asking "how many pages has this been?" is asking about
    // documents, and a hash change that answered six would be a lie about a page nobody loaded.
    page.emit(json!({"ev": "url", "url": "http://127.0.0.1:9/page#second", "title": "A page"}));
    for _ in 0..40 {
        if h.peek().url.ends_with("#second") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = h.peek();
    assert!(s.url.ends_with("#second"), "the address the panel shows follows the page: {}", s.url);
    assert_eq!(s.nav, was, "and the page counter says one page is still one page");
    h.close("test over").await;
}

#[tokio::test]
async fn a_page_in_the_window_says_its_size_and_every_other_reader_hears_it() {
    let (mut h, page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    assert_eq!((h.peek().viewport_w, h.peek().viewport_h), (400, 620));
    // The box the panel asked for is not necessarily the box the page got — a window manager, a
    // scrollbar or a scale factor can disagree. The page's number has to reach the snapshot, not only
    // the window that asked, or a phone and the model would be sizing a page neither can see.
    page.emit(json!({"ev": "viewport", "viewport": {"width": 320, "height": 480}}));
    for _ in 0..40 {
        if h.peek().viewport_w == 320 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = h.peek();
    assert_eq!((s.viewport_w, s.viewport_h), (320, 480), "the page's own size, not the requested one");
    h.close("test over").await;
}

#[tokio::test]
async fn a_page_in_the_window_says_it_is_on_screen_instead_of_claiming_a_live_view() {
    let (mut h, _page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    let out = h.watch(true, Some(400)).await.expect("the panel asks to watch");
    assert_eq!(out["live"], false, "asking is not getting: a page on screen streams nothing");
    let s = h.peek();
    assert!(!s.live, "and the panel's live mark stays off");
    assert!(s.live_why.contains("on screen"), "saying why: {}", s.live_why);
    assert!(s.why.is_empty(), "while the live view's absence is not the page's failure: {}", s.why);
    h.close("test over").await;
}

#[tokio::test]
async fn the_idle_clock_closes_a_page_in_the_window_and_says_the_number() {
    let (mut h, page) = native_hub(4);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    // Watching the page is not using it. A person who only looks must not keep it alive forever —
    // that is the same rule as for a driver process, and the same clock enforces it.
    h.watch(true, Some(400)).await.unwrap();
    assert_eq!(h.reap_if_idle().await, None, "the clock has not run out yet");
    tokio::time::sleep(Duration::from_millis(4100)).await;
    let why = h.reap_if_idle().await.expect("the clock ran out");
    assert!(why.contains("4 seconds"), "{why}");
    assert!(!h.peek().alive, "the page is gone from the window");
    assert!(page.cmds().contains(&"close".to_string()), "and the page view was told, not just forgotten");
    assert!(h.peek().why.contains("4 seconds"), "and the panel reads the reason: {}", h.peek().why);}

#[tokio::test]
async fn a_page_view_that_forgets_itself_leaves_no_picture_and_no_promise() {
    let (mut h, _page) = native_hub(600);
    h.open("http://127.0.0.1:9/page", Who::Person, false).await.unwrap();
    // The page view can be taken away under the hub — the window closed, the app quit. The rules
    // for that are the driver-died rules: answer the caller with the reason, and stop claiming a
    // live view over a page that is gone.
    _page.emit(json!({"ev": "closed", "why": "the window closed"}));
    tokio::time::sleep(Duration::from_millis(80)).await;
    let s = h.peek();
    assert!(!s.alive && !s.opened && !s.live);
    assert_eq!(s.why, "the window closed");
    assert_eq!(h.frame_pic(), 0, "no picture id left over for the next page to be judged against");
    assert!(h.frame_now().is_none());
}

