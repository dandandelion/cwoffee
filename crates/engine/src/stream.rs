//! M0 streaming harness.
//!
//! This is a scripted event source that emits a realistic `EngineEvent` sequence
//! with real timing. It exists to prove the transport (Tauri `Channel`, or the
//! TUI's stdout) end-to-end **before** any provider code exists, and it stays in
//! the repo permanently as the fixture for UI perf work — you want a way to replay
//! a 4,000-token turn at 50 tok/s without paying a model for it.
//!
//! M1 replaces this with real providers behind the same shape.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::event::{EngineEvent, ExecStream, FinishReason};
use crate::telemetry::TurnTelemetry;

/// Scripted turn shape: reasoning, answer text, one tool call, more text, usage.
#[derive(Debug, Clone)]
pub struct FakeScript {
    pub model: &'static str,
    /// tokens per second to simulate
    pub tok_per_sec: f64,
    pub reasoning_words: usize,
    pub answer_words: usize,
    pub with_tool: bool,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            // matches the measured dev endpoint (PLAN.md §4)
            model: "qwen3.8-flash-next-iq3_s",
            tok_per_sec: 50.0,
            reasoning_words: 24,
            answer_words: 90,
            with_tool: true,
        }
    }
}

/// Emit a scripted turn, sleeping to imitate real token pacing.
///
/// `emit` is called on the current thread; callers that need it off the UI thread
/// spawn it (the Tauri host does exactly that).
pub fn fake_turn<F: FnMut(EngineEvent)>(turn_id: u64, script: &FakeScript, mut emit: F) {
    let start = Instant::now();
    // A scripted turn belongs to no stored conversation.
    emit(EngineEvent::TurnStart {
        turn_id,
        model: script.model.to_string(),
        session_id: 0,
    });

    let gap = Duration::from_secs_f64(1.0 / script.tok_per_sec.max(1.0));

    // ---- reasoning deltas ----
    for w in 0..script.reasoning_words {
        emit(EngineEvent::Reasoning(format!(
            "thinking about step {} ",
            w + 1
        )));
        sleep(gap);
    }

    // ---- a tool call, to exercise the card UI ----
    if script.with_tool {
        let id = format!("call_{turn_id}_1");
        emit(EngineEvent::ToolCallBegin { id: id.clone(), name: "exec".into() });
        // fragmented args, exactly like the real endpoint
        for frag in ["{\"cm", "d\":\", \"~/Pro", "jects/cow", "ry\", \"c", "md\": \"l", "s -la\"}"] {
            emit(EngineEvent::ToolCallArgs(frag.into()));
            sleep(gap);
        }
        emit(EngineEvent::ToolCallReady {
            id: id.clone(),
            args: serde_json::json!({ "cmd": "ls -la", "cwd": "~/Projects/cwoffee" }),
        });
        emit(EngineEvent::ToolExecStart { id: id.clone() });
        for line in ["total 48", "drwxr-xr-x  9 you you 4096 Oct  1 19:00 .", "-rw-r--r-- 1 you you 8123 Oct 1 19:00 PLAN.md"] {
            emit(EngineEvent::ToolExecOutput {
                id: id.clone(),
                stream: ExecStream::Stdout,
                chunk: format!("{line}\n"),
            });
            sleep(gap);
        }
        emit(EngineEvent::ToolExecEnd {
            id,
            ok: true,
            exit_code: Some(0),
            changes: Vec::new(),
        });
    }

    // ---- answer deltas ----
    for w in 0..script.answer_words {
        let word = match w {
            0 => "Here".into(),
            1 => " is".into(),
            2 => " the".into(),
            3 => " M0".into(),
            4 => " streaming".into(),
            5 => " proof.\n\n".into(),
            6 => "```rust\n".into(),
            7..=12 => format!("fn step_{w}() {{ /* delta {w} */ }}\n").replace('\n', "\n"),
            13 => "```\n\n".into(),
            _ => format!(" word_{w}"),
        };
        emit(EngineEvent::Text(word));
        sleep(gap);
    }

    // ---- telemetry, then end ----
    let ttft_ms = 420; // pretend prefill
    emit(EngineEvent::Usage(TurnTelemetry {
        prompt_tokens: 308,
        cached_tokens: 303,
        completion_tokens: (script.reasoning_words + script.answer_words) as u64,
        ttft_ms,
        server_prefill_tps: 68.0,
        server_decode_tps: script.tok_per_sec,
        draft_n: 48,
        draft_accepted: 42,
    }));

    emit(EngineEvent::TurnEnd { turn_id, finish: FinishReason::Stop });

    let _ = start;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn scripted_turn_has_expected_shape() {
        let events = Mutex::new(Vec::<EngineEvent>::new());
        let script = FakeScript {
            tok_per_sec: 100_000.0, // no sleeping in tests
            reasoning_words: 3,
            answer_words: 5,
            ..Default::default()
        };
        fake_turn(1, &script, |e| events.lock().unwrap().push(e));

        let ev = events.lock().unwrap();
        assert!(matches!(ev.first().unwrap(), EngineEvent::TurnStart { .. }));
        assert!(matches!(ev.last().unwrap(), EngineEvent::TurnEnd { finish: FinishReason::Stop, .. }));

        let texts = ev.iter().filter(|e| e.is_answer_delta()).count();
        assert_eq!(texts, 5);
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::ToolCallReady { .. })));
        assert!(ev.iter().any(|e| matches!(e, EngineEvent::Usage(_))));
    }
}
