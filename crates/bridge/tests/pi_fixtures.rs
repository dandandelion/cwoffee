//! The translator replayed against a **recorded** pi stream.
//!
//! These fixtures come from `scripts/pi-event-capture.mjs`, which runs pi against
//! cwoffee's own scripted endpoint and writes every record verbatim. That matters
//! more than it looks: a translation written against a doc example passes whether
//! or not pi behaves that way, and it keeps passing when pi changes. A fixture off
//! the wire fails when the protocol moves, which is exactly when we want to know.
//!
//! Run the capture again after bumping pi:
//!
//! ```text
//! cd ../cwoffee && node scripts/mock-llm.mjs --port 8190
//! cd ../pi-cwoffee/spike && ./build-shim.sh
//! SCENARIO=read-file-approval node ../scripts/pi-event-capture.mjs
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cwoffee_bridge::translate::Translator;
use cwoffee_engine::event::{EngineEvent, FinishReason};
use cwoffee_engine::provider::Role;

fn fixture(name: &str) -> Vec<serde_json::Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} — regenerate with scripts/pi-event-capture.mjs", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line is not JSON"))
        .collect()
}

/// A clock the test controls: 40 ms per record. The live estimate fields are the
/// only thing that depends on wall time, and asserting on their exact values would
/// make the suite flaky without proving anything about the protocol.
struct Clock(Instant);
impl Clock {
    fn new() -> Self {
        Self(Instant::now())
    }
    fn tick(&mut self) -> Instant {
        self.0 += Duration::from_millis(40);
        self.0
    }
}

#[test]
fn one_prompt_is_one_turn_even_when_pi_runs_two_of_its_own() {
    let recs = fixture("pi-events-read-file-approval.jsonl");
    let mut tr = Translator::default();
    tr.session = 7;
    tr.model = "cwoffee-mock".into();
    let mut clock = Clock::new();

    let mut starts = 0;
    let mut ends = 0;
    let mut settled = 0;
    let mut turn_ids = Vec::new();
    let mut out = Vec::new();
    for r in &recs {
        let o = tr.on_record(r, clock.tick());
        for e in &o.events {
            match e {
                EngineEvent::TurnStart { turn_id, session_id, model } => {
                    starts += 1;
                    turn_ids.push(*turn_id);
                    assert_eq!(*session_id, 7, "the turn did not name the conversation it belongs to");
                    assert_eq!(model, "cwoffee-mock");
                }
                EngineEvent::TurnEnd { turn_id, .. } => {
                    ends += 1;
                    assert_eq!(turn_ids.last(), Some(turn_id), "TurnEnd named a turn that never started");
                }
                _ => {}
            }
        }
        if o.settled {
            settled += 1;
        }
        out.push(o);
    }

    // The fixture holds two pi `turn_start` records and two `turn_end` records —
    // one round trip for the tool call, one for the answer. cwoffee sees one turn.
    let pi_turns = recs.iter().filter(|r| r["type"] == "turn_start").count();
    assert_eq!(pi_turns, 2, "the fixture no longer looks like pi 1.2.0");
    assert_eq!(starts, 1, "one prompt opened {starts} turns; pi's own turn boundaries leaked");
    assert_eq!(ends, 1);
    assert_eq!(settled, 1);
    assert!(!tr.running(), "the translator still thinks a run is open");

    let last = out.last().unwrap();
    let finish = last.events.iter().find_map(|e| match e {
        EngineEvent::TurnEnd { finish, .. } => Some(*finish),
        _ => None,
    });
    assert_eq!(finish, Some(FinishReason::Stop));
}

#[test]
fn streamed_text_and_reasoning_arrive_once_and_in_order() {
    let recs = fixture("pi-events-read-file-approval.jsonl");
    let mut tr = Translator::default();
    let mut clock = Clock::new();
    let mut deltas = String::new();
    let mut thinking = String::new();
    let mut ends_text: Vec<String> = Vec::new();
    let mut ends_thinking: Vec<String> = Vec::new();
    for r in &recs {
        let o = tr.on_record(r, clock.tick());
        for e in o.events {
            match e {
                EngineEvent::Text(t) => deltas.push_str(&t),
                EngineEvent::Reasoning(t) => thinking.push_str(&t),
                _ => {}
            }
        }
        if r["type"] == "message_update" {
            let ame = &r["assistantMessageEvent"];
            match ame["type"].as_str() {
                Some("text_end") => ends_text.push(ame["content"].as_str().unwrap_or_default().to_string()),
                Some("thinking_end") => {
                    ends_thinking.push(ame["content"].as_str().unwrap_or_default().to_string())
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        deltas,
        ends_text.concat(),
        "the answer the front end assembles is not the answer pi ended the block with"
    );
    assert_eq!(thinking, ends_thinking.concat());
    assert!(!deltas.is_empty() && !thinking.is_empty(), "the fixture streamed nothing");
}

#[test]
fn pi_s_own_scaffolding_never_reaches_the_transcript() {
    let recs = fixture("pi-events-read-file-approval.jsonl");
    let mut tr = Translator::default();
    let mut clock = Clock::new();
    let mut rows = Vec::new();
    let mut events = Vec::new();
    for r in &recs {
        let o = tr.on_record(r, clock.tick());
        rows.extend(o.rows);
        events.extend(o.events);
    }
    // The first message pi streams is its system prompt, complete with the tool
    // list and the injection guardrails. It is what the model was told, not a turn.
    assert!(!rows.iter().any(|m| m.role == Role::System), "a system message was stored as transcript");
    for e in &events {
        if let EngineEvent::Text(t) = e {
            assert!(
                !t.contains("coding assistant") && !t.contains("<tools>"),
                "pi's own prompt leaked into the answer stream: {t}"
            );
        }
    }
    let roles: Vec<_> = rows.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant],
        "the transcript rows this turn stores are not what the UI renders"
    );
    let assistant_with_calls = rows.iter().find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty());
    assert_eq!(assistant_with_calls.map(|m| m.tool_calls.len()), Some(1));
    let call = &assistant_with_calls.unwrap().tool_calls[0];
    assert_eq!(call.name, "read_file");
    assert_eq!(call.id, "mock-read-1");
    assert_eq!(call.arguments, r#"{"path":"/etc/hostname"}"#, "stored args must be the model's JSON");
}

#[test]
fn the_tool_call_and_its_execution_are_named_to_the_same_call() {
    let recs = fixture("pi-events-read-file-approval.jsonl");
    let mut tr = Translator::default();
    let mut clock = Clock::new();
    let mut begin = None;
    let mut ready = None;
    let mut exec_start = None;
    let mut exec_end = None;
    let mut ui = Vec::new();
    for r in &recs {
        let o = tr.on_record(r, clock.tick());
        for e in o.events {
            match e {
                EngineEvent::ToolCallBegin { id, name } => begin = Some((id, name)),
                EngineEvent::ToolCallReady { id, args } => ready = Some((id, args)),
                EngineEvent::ToolExecStart { id } => exec_start = Some(id),
                EngineEvent::ToolExecEnd { id, ok, exit_code, changes } => {
                    exec_end = Some((id, ok, exit_code, changes.len()))
                }
                _ => {}
            }
        }
        ui.extend(o.ui);
    }
    assert_eq!(begin.as_ref().map(|(_, n)| n.as_str()), Some("read_file"));
    assert_eq!(ready.as_ref().map(|(_, a)| a.to_string()), Some(r#"{"path":"/etc/hostname"}"#.into()));
    let id = begin.as_ref().unwrap().0.clone();
    assert_eq!(ready.as_ref().unwrap().0, id);
    assert_eq!(exec_start.as_ref().unwrap(), &id);
    let (eid, ok, exit, changes) = exec_end.unwrap();
    assert_eq!(eid, id, "the execution card does not match the call card");
    assert!(ok);
    assert_eq!(exit, None, "pi reports no exit code for a tool result; inventing one would lie");
    assert_eq!(changes, 0, "this tool changed no files");

    // The approval request in the fixture is pi's `confirm` dialog: title, message,
    // and a timeout in ms that the bridge must pass through to the UI card.
    assert_eq!(ui.len(), 1, "expected exactly one dialog in this fixture");
    match &ui[0] {
        cwoffee_bridge::translate::UiRequest::Confirm { pi_id, title, message, timeout_ms } => {
            assert!(!pi_id.is_empty());
            assert_eq!(title, "Allow read_file?");
            assert_eq!(message, "/etc/hostname");
            assert_eq!(*timeout_ms, 15000);
        }
        other => panic!("expected a confirm dialog, got {other:?}"),
    }
}

#[test]
fn usage_is_summed_over_the_round_trips_and_the_cache_is_visible() {
    let recs = fixture("pi-events-read-file-approval.jsonl");
    let mut tr = Translator::default();
    let mut clock = Clock::new();
    let mut usage = None;
    let mut context = None;
    for r in &recs {
        let o = tr.on_record(r, clock.tick());
        for e in o.events {
            if let EngineEvent::Usage(u) = e {
                usage = Some(u);
            }
        }
        if let Some(ct) = o.context_tokens {
            context = Some(ct);
        }
    }
    let u = usage.expect("no Usage event; the turn would have no telemetry");
    // pi reports per-message usage: 1147 input + 21 output on the tool round trip,
    // then 95 input + 1081 cache read + 8 output on the answer. `input` EXCLUDES
    // cached tokens, so the window holds input + cacheRead + cacheWrite.
    assert_eq!(u.prompt_tokens, 1147 + 95 + 1081, "prompt tokens must add the cache terms back in");
    assert_eq!(u.cached_tokens, 1081);
    assert_eq!(u.completion_tokens, 21 + 8);
    // pi reports no server timings and no speculative counters; 0 means "not
    // reported", and a plausible-looking invented number is worse than that.
    assert_eq!(u.server_prefill_tps, 0.0);
    assert_eq!(u.draft_n, 0);
    assert!(u.ttft_ms > 0, "time to first delta is ours to measure, and it was not measured");
    // The context meter must move with the real prompt size, cached included,
    // or the "how full is my window" bar reads a third of the truth.
    assert_eq!(context, Some(95 + 1081), "the context meter should track the largest prompt in the run");
    assert!(tr.unknown.is_empty(), "pi sent records the translator does not know: {:?}", tr.unknown);
}

#[test]
fn a_process_that_dies_mid_turn_still_closes_the_turn() {
    let recs = fixture("pi-events-read-file-approval.jsonl");
    let mut tr = Translator::default();
    let mut clock = Clock::new();
    // Stop feeding records halfway through, the way a SIGKILL does.
    for r in recs.iter().take(recs.len() / 2) {
        tr.on_record(r, clock.tick());
    }
    assert!(tr.running(), "the fixture's first half should leave a run open");
    let out = tr.abort_run("the model process ended during the turn");
    assert!(out.settled, "a closed run must settle so busy can clear");
    assert!(!tr.running());
    let mut saw_error = false;
    let mut saw_end = false;
    for e in &out.events {
        match e {
            EngineEvent::Error { fatal: true, .. } => saw_error = true,
            EngineEvent::TurnEnd { finish, .. } => {
                saw_end = true;
                assert_eq!(*finish, FinishReason::Error);
            }
            _ => {}
        }
    }
    assert!(saw_error && saw_end, "a front end that never sees TurnEnd leaves the composer dimmed forever");
    assert!(!out.rows.is_empty(), "the half-finished answer should still be stored");
}

/// The rate bar measures generation, not a person deciding. This run streams for
/// one second and spends four seconds parked on an approval card: the reported
/// rate must be 20 tokens over the one second, not over five. The first bridge
/// run reported 2 tok/s for exactly this shape, which would have made a dual-core
/// A/B read a human\'s thinking time as pi being slow.
#[test]
fn a_turn_parked_on_a_human_reports_the_generation_rate_not_the_wall_clock() {
    let now = Instant::now();
    let mut t = |ms: u64| now + Duration::from_millis(ms);
    let mut tr = Translator::default();
    let usage_records = vec![
        serde_json::json!({"type": "agent_start"}),
        serde_json::json!({"type": "message_start", "message": {"role": "assistant", "content": []}}),
        serde_json::json!({"type": "message_update", "assistantMessageEvent": {"type": "text_start", "contentIndex": 0}}),
        serde_json::json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "twenty "}}),
        serde_json::json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "tokens"}}),
        serde_json::json!({"type": "message_update", "assistantMessageEvent": {"type": "text_end", "contentIndex": 0, "content": "twenty tokens"}}),
        // The model asks, and a person takes four whole seconds to answer.
        serde_json::json!({"type": "extension_ui_request", "id": "u1", "method": "confirm",
                           "title": "Allow exec?", "message": "rm -f note.txt", "timeout": 15000}),
        serde_json::json!({"type": "message_end", "message": {"role": "assistant",
            "content": [{"type": "text", "text": "twenty tokens"}],
            "stopReason": "toolUse",
            "usage": {"input": 100, "cacheRead": 0, "cacheWrite": 0, "output": 20}}}),
        serde_json::json!({"type": "agent_settled", "aborted": false}),
    ];
    // Half a second of streaming (first delta at 500 ms, the card opens at
    // 1000 ms), then four seconds of nothing but a card on screen.
    let mut at = 0u64;
    let mut usage = None;
    let mut stats = Vec::new();
    for r in &usage_records {
        if r["type"] == "message_update" {
            at += 250;
        } else if r["type"] == "extension_ui_request" {
            at = 1000;
        } else if r["type"] == "agent_settled" {
            at = 5000;
        }
        let o = tr.on_record(r, t(at));
        if !o.ui.is_empty() {
            // Still open when the turn ends: pi denied on its own timeout, and
            // nobody answered. The wait is over because the run is.
        }
        for e in o.events {
            match e {
                EngineEvent::Usage(u) => usage = Some(u),
                EngineEvent::TurnStats(s) => stats.push(s),
                _ => {}
            }
        }
    }
    let u = usage.expect("no Usage event");
    assert_eq!(u.completion_tokens, 20);
    // 20 tokens over the one second of actual generation, not the five seconds of
    // wall clock: 19-21 tok/s. Over the wall clock it would read 4.
    // 20 tokens over the 500 ms the model was actually generating, not over the
    // 4500 ms since the first delta: 40 tok/s, where the wall clock says 4.4.
    assert!(
        (39.0..=41.0).contains(&u.server_decode_tps),
        "decode rate should be ~40 tok/s of generation, got {:.1} (wall clock would say 4.4)",
        u.server_decode_tps
    );
    assert!(
        stats.iter().all(|s| s.streaming_ms <= 500),
        "the live bar counted waiting as streaming: {:?}",
        stats.iter().map(|s| s.streaming_ms).collect::<Vec<_>>()
    );
    assert!(!stats.is_empty(), "a streamed turn should have produced live stats");
}
