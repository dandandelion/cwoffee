//! The engine, end to end, against a scripted endpoint.
//!
//! These tests used to be impossible to write honestly: driving a real model
//! means the assertion depends on what the model felt like doing, and it costs a
//! 200k-token prefill on a single-slot llama.cpp server every time anyone runs
//! the suite. `scripts/mock-llm.mjs` answers with the wire shape of llama.cpp
//! (SSE deltas, `reasoning_content`, fragmented `function.arguments`, a
//! usage+`timings` chunk) straight out of `tests/scenarios/scenarios.json`, so
//! what is under test is our side of the socket: the SSE parser, tool-call
//! accumulation, JSON repair, the approval/question round trips, file stamping,
//! telemetry, and error handling.
//!
//! What these tests therefore do NOT prove: that a real model calls the right
//! tool, that a quantisation mangles arguments at some measurable rate, or that
//! prefill/decode is fast. `scripts/selftest.sh --real-endpoint` is the
//! deliberate, warned way to check that.
//!
//! The suite skips itself (loudly) when node or the scenario script is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cwoffee_engine::config::Config;
use cwoffee_engine::event::EngineEvent;
use cwoffee_engine::host::{HostConfig, HostState, SessionHost};
use cwoffee_engine::store::Store;
use cwoffee_engine::tools::ask::Answer;
use cwoffee_engine::tools::web::SearchBackend;
use cwoffee_engine::tools::{ExecCfg, Registry};
use tokio::sync::mpsc;

const MODEL: &str = "cwoffee-mock";

struct Mock {
    child: tokio::process::Child,
    base: String,
}

impl Drop for Mock {
    fn drop(&mut self) {
        // A leaked mock holds a port and answers the next test run's requests,
        // which is the most confusing possible failure.
        let _ = self.child.start_kill();
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

impl Mock {
    /// Start the scripted endpoint on a free loopback port. `None` means this box
    /// cannot run it, and the caller must say so rather than pass quietly.
    async fn start() -> Option<Mock> {
        let script = repo_root().join("scripts/mock-llm.mjs");
        if !script.exists() {
            eprintln!("SKIP: no scripts/mock-llm.mjs at {}", script.display());
            return None;
        }
        if !std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("SKIP: node is not installed, so the scripted endpoint cannot run");
            return None;
        }
        // Claim a port, release it, hand it to the server. Small race, and a
        // server that fails to bind shows up as the health check timing out.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            l.local_addr().ok()?.port()
        };
        let child = tokio::process::Command::new("node")
            .arg(&script)
            .arg("--port")
            .arg(port.to_string())
            .arg("--speed")
            .arg("0.2") // the delays in the script are for a human-scale UI; tests want the shape, not the pacing
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // No kill_on_exit: this tokio build does not carry it, and `Drop`
            // above already makes sure a panicking test cannot leak a server.
            .spawn()
            .ok()?;
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..80 {
            if reqwest::get(format!("{base}/health")).await.is_ok() {
                return Some(Mock { child, base });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        eprintln!("SKIP: the scripted endpoint never answered /health");
        None
    }

    /// Which sampling keys reached the request, and with what values. The engine's
    /// contract is that an unset key is *absent*, so absence has to be observable.
    async fn sampling_seen(&self) -> Option<serde_json::Value> {
        let v: serde_json::Value = reqwest::get(format!("{}/mock/state", self.base))
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        v.get("sampling_seen").cloned()
    }

    /// How many turns the mock actually served. A test that thinks it ran a turn
    /// while the mock served nothing is testing a memory.
    async fn turns(&self) -> Option<u64> {
        let v: serde_json::Value = reqwest::get(format!("{}/mock/state", self.base))
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        v.get("turns").and_then(|t| t.as_u64())
    }
}

struct Harness {
    host: SessionHost,
    events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
    questions: mpsc::UnboundedReceiver<cwoffee_engine::tools::ask::QuestionRx>,
    store: Arc<Store>,
    ws: PathBuf,
}

impl Harness {
    async fn new(mock: &Mock) -> Harness {
        Self::with_sampling(mock, cwoffee_engine::provider::Sampling::default()).await
    }

    /// The `[sampling]` passthrough, end to end: the only proof that matters is
    /// what the endpoint saw on the wire, and the scripted one reports it.
    async fn with_sampling(mock: &Mock, sampling: cwoffee_engine::provider::Sampling) -> Harness {
        let ws = std::env::temp_dir().join(format!("cwoffee-mock-test-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut cfg = Config::default();
        cfg.provider.base_url = format!("{}/v1", mock.base);
        cfg.provider.model = MODEL.into();
        let provider = cfg.build_provider().expect("the scripted endpoint builds a provider");

        let registry = Registry::with_v1_tools_exec(SearchBackend::DuckDuckGo, ExecCfg::default());
        let store = Arc::new(Store::in_memory().unwrap());
        let mut hcfg = HostConfig::new(MODEL, ws.clone());
        // Auto so the tool loop is the thing under test rather than the approval
        // plumbing, which has its own tests in host.rs and its own UI check.
        hcfg.mode = cwoffee_engine::approval::ApprovalMode::Auto;
        hcfg.reasoning = cwoffee_engine::provider::Reasoning::Off;
        hcfg.sampling = sampling;
        let mut host = SessionHost::spawn(provider, registry, Arc::clone(&store), hcfg);
        let (events, _approvals) = host.take_receivers().expect("event receivers");
        let questions = host.take_questions().expect("question receiver");
        Harness {
            host,
            events,
            questions,
            store,
            ws,
        }
    }

    fn state(&self) -> HostState {
        self.host.state()
    }

    /// Run one turn, answering any question the model asks along the way.
    /// Returns (answer text, reasoning text, events seen, tool results seen).
    async fn turn(
        &mut self,
        prompt: &str,
        answer_with: &mut dyn FnMut(&cwoffee_engine::tools::ask::Question) -> Answer,
    ) -> Turn {
        let before = self.state().turns;
        self.host.submit(prompt);
        let mut t = Turn::default();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(60));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => { t.timed_out = true; break }
                q = self.questions.recv() => {
                    if let Some((question, reply)) = q {
                        let ans = answer_with(&question);
                        t.questions.push(question);
                        let _ = reply.send(ans);
                    }
                }
                ev = self.events.recv() => {
                    match ev {
                        None => break,
                        Some(e) => {
                            match &e {
                                EngineEvent::Text(s) => t.answer.push_str(s),
                                EngineEvent::Reasoning(s) => t.reasoning.push_str(s),
                                EngineEvent::Error { message, .. } => t.errors.push(message.clone()),
                                EngineEvent::Note { message, .. } => t.notes.push(message.clone()),
                                EngineEvent::ToolExecEnd { ok, .. } => t.tool_results.push(*ok),
                                EngineEvent::ToolExecOutput { chunk, .. } => t.tool_out.push_str(&chunk),
                                EngineEvent::TurnFiles { files, .. } => t.files.extend(files.clone()),
                                EngineEvent::Usage(tel) => t.telemetry = Some(tel.clone()),
                                EngineEvent::TurnEnd { finish, .. } => { t.finish = Some(*finish); break }
                                _ => {}
                            }
                            t.events.push(e);
                        }
                    }
                }
            }
        }
        // The turn counter is the honest end-of-turn signal: `busy` can go false
        // before the store row lands.
        for _ in 0..40 {
            if self.state().turns > before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        t.turns_after = self.state().turns;
        t
    }
}

#[derive(Default)]
struct Turn {
    answer: String,
    reasoning: String,
    errors: Vec<String>,
    notes: Vec<String>,
    events: Vec<EngineEvent>,
    tool_results: Vec<bool>,
    tool_out: String,
    files: Vec<cwoffee_engine::tools::OutputFile>,
    questions: Vec<cwoffee_engine::tools::ask::Question>,
    telemetry: Option<cwoffee_engine::telemetry::TurnTelemetry>,
    finish: Option<cwoffee_engine::event::FinishReason>,
    timed_out: bool,
    turns_after: u64,
}

/// The bug this test exists for: a turn that stopped because an output cap bit it
/// used to be reported as a clean stop, so "the answer got cut off mid-sentence"
/// left no trace anywhere. The turn must notice, say so, and pick the answer back
/// up rather than ending on a truncated sentence.
#[tokio::test]
async fn an_answer_cut_off_at_the_output_limit_is_picked_back_up() {
    let Some(mock) = Mock::start().await else {
        eprintln!("SKIP: scripted endpoint unavailable");
        return;
    };
    let mut h = Harness::new(&mock).await;
    let mut h2 = Harness::new(&mock).await;
    let _ = &mut h2;
    let t = h
        .turn("Selftest truncation-check: give me the one-sentence recommendation.", &mut |_| {
            Answer::default()
        })
        .await;

    assert!(
        t.answer.contains("engineering team"),
        "the first half never arrived: {:?}",
        t.answer
    );
    assert!(
        t.answer.contains("hiring gap"),
        "the cut-off answer was never continued: {:?}",
        t.answer
    );
    assert!(
        t.notes.iter().any(|n| n.contains("output limit")),
        "nothing told the user the answer had been cut short: {:?}",
        t.notes
    );
    assert_eq!(
        t.finish,
        Some(cwoffee_engine::event::FinishReason::Stop),
        "the resumed turn should end on the model's own stop, not on the cap"
    );

    // Both halves are history, in order: a continuation is not a rewrite of the
    // partial answer, and the model has to be able to see what it already said.
    let rows = h.store.messages(h.state().session_id).unwrap_or_default();
    let assistant: Vec<String> = rows
        .iter()
        .filter(|r| r.role == "assistant")
        .map(|r| r.content.clone())
        .collect();
    assert!(
        assistant.len() >= 2,
        "the partial answer and its continuation should both be stored, got {assistant:?}"
    );
    assert!(
        assistant.last().unwrap().contains("three risks"),
        "the stored continuation is not what the model said: {assistant:?}"
    );
    assert_eq!(
        mock.turns().await,
        Some(2),
        "the mock should have served exactly two round trips: the cut-off and the continuation"
    );
}

/// The other half of the same fault: with thinking on, the *thinking* can consume
/// the whole output budget, and the turn produces a "Done" chip and no prose. That
/// has to name the mechanism and the knob, not report an empty answer as success.
#[tokio::test]
async fn thinking_that_eats_the_output_budget_says_which_knob_to_turn() {
    let Some(mock) = Mock::start().await else {
        eprintln!("SKIP: scripted endpoint unavailable");
        return;
    };
    let mut h = Harness::new(&mock).await;
    let t = h
        .turn("Selftest thinking-ate-the-budget: what is the plan?", &mut |_| Answer::default())
        .await;

    assert!(t.answer.trim().is_empty(), "there should be no answer: {:?}", t.answer);
    let named = t
        .errors
        .iter()
        .find(|e| e.contains("thinking") && e.contains("output limit"));
    assert!(
        named.is_some(),
        "no error explained that thinking consumed the budget: {:?}",
        t.errors
    );
    assert_eq!(
        t.finish,
        Some(cwoffee_engine::event::FinishReason::Length),
        "the turn must report the cap as the reason it stopped, not a clean stop"
    );
}

fn first_line(prompt: &str) -> String {
    prompt.lines().next().unwrap_or("").to_string()
}

#[tokio::test]
async fn a_scripted_answer_streams_and_reports_what_the_server_claimed() {
    let Some(mock) = Mock::start().await else { return };
    let mut h = Harness::new(&mock).await;
    let t = h.turn("What is 6 times 7? Answer with just the number.", &mut |_| {
        Answer::default()
    })
    .await;
    assert!(!t.timed_out, "the scripted endpoint could not finish a two-token answer");
    assert_eq!(t.answer.trim(), "42", "answer text: {:?}", t.answer);
    assert_eq!(t.turns_after, 1, "the host did not count the turn");
    assert_eq!(t.finish, Some(cwoffee_engine::event::FinishReason::Stop), "finish reason: {:?}", t.finish);
    let tel = t.telemetry.clone().expect("the provider reported telemetry");
    assert!(tel.prompt_tokens > 0, "no prompt tokens: {tel:?}");
    assert!(tel.completion_tokens > 0, "no completion tokens: {tel:?}");
    // The scripted server sends llama.cpp `timings`; the telemetry bar reads them.
    assert!(
        tel.server_decode_tps > 0.0 && tel.server_prefill_tps > 0.0,
        "llama.cpp `timings` did not survive the parser: {tel:?}"
    );
    // A first request has nothing cached; the follow-up in the tool-loop test
    // below is where a cache claim has to appear.
    assert_eq!(mock.turns().await, Some(1), "the mock did not serve the request");
}

#[tokio::test]
async fn a_scripted_tool_loop_writes_the_file_and_stamps_it_on_the_row() {
    let Some(mock) = Mock::start().await else { return };
    let mut h = Harness::new(&mock).await;
    let t = h
        .turn("Use write_file to create selftest-note.md in the workspace containing exactly the line: cwoffee output file. Then tell me the path you wrote.", &mut |_| Answer::default())
        .await;
    assert!(!t.timed_out, "the tool loop never closed");
    assert!(
        t.events.iter().any(|e| matches!(e, EngineEvent::ToolCallBegin { name, .. } if name == "write_file")),
        "no write_file call in the event stream"
    );
    assert_eq!(t.tool_results, vec![true], "the tool did not report success");
    let written = h.ws.join("selftest-note.md");
    assert!(written.exists(), "the tool did not write {}", written.display());

    // `TurnFiles` is the live path; the store row is the path a reopened session
    // reads. Both have to carry it or the attachment appears then disappears.
    assert!(!t.files.is_empty(), "the turn reported no output files");
    // The follow-up request in a tool loop is the one that can claim a prefix hit.
    if let Some(tel) = t.telemetry.as_ref() {
        assert!(tel.cached_tokens > 0, "the second request claimed a cold cache: {tel:?}");
    }
    let rows = h.store.messages(h.state().session_id).unwrap();
    let stamped = rows.iter().any(|m| m.files.iter().any(|f| f.path.contains("selftest-note.md")));
    assert!(stamped, "no stored row carries the file: {:?}", rows.iter().map(|m| (&m.role, m.files.len())).collect::<Vec<_>>());
    assert!(t.answer.contains("selftest-note.md"), "second-step answer: {:?}", t.answer);
}

#[tokio::test]
async fn a_scripted_question_set_arrives_as_one_group_and_answers_as_one() {
    let Some(mock) = Mock::start().await else { return };
    let mut h = Harness::new(&mock).await;
    let mut picks = 0usize;
    let t = h
        .turn("Ask me two questions before writing the report, then write it.", &mut |q| {
            picks += 1;
            Answer {
                text: q.options.first().map(|o| o.label.clone()).unwrap_or_else(|| "yes".into()),
                options: vec![0],
            }
        })
        .await;
    assert!(!t.timed_out, "the ask_user loop never closed");
    assert_eq!(
        t.questions.len(),
        2,
        "the model asked {} questions (errors={:?}, tool results={:?}, tool said={:?}, answer={:?})",
        t.questions.len(),
        t.errors,
        t.tool_results,
        t.tool_out,
        t.answer
    );
    // The wire contract: one shared set id, per-question index, the same total.
    let a = t.questions[0].set.as_ref().expect("a set has a set marker");
    let b = t.questions[1].set.as_ref().expect("a set has a set marker");
    assert_eq!(a.id, b.id, "two questions from one call must share a set id");
    assert_eq!((a.index, a.total), (0, 2), "first question is indexed {a:?}");
    assert_eq!((b.index, b.total), (1, 2), "second question is indexed {b:?}");
    assert_eq!(picks, 2, "the loop answered {} questions", picks);
    assert!(
        t.answer.contains("Written"),
        "the answer after the questions: {:?}",
        t.answer
    );
}

#[tokio::test]
async fn broken_tool_arguments_are_repaired_run_and_admitted() {
    let Some(mock) = Mock::start().await else { return };
    let mut h = Harness::new(&mock).await;
    let t = h
        .turn("Call a tool with arguments that are missing a brace.", &mut |_| Answer::default())
        .await;
    assert!(!t.timed_out, "the broken-arguments turn never closed");
    let said = t.errors.iter().any(|m| m.contains("repaired malformed arguments"));
    assert!(
        said,
        "a braceless call was silently accepted; errors: {:?}",
        t.errors
    );
    // Repair is a rescue, not a pass: the loop has to keep going afterwards.
    assert!(
        t.answer.contains("put back together"),
        "the model was not told what happened: {:?}",
        t.answer
    );
}

#[tokio::test]
async fn an_endpoint_that_dies_mid_stream_is_reported_not_truncated() {
    let Some(mock) = Mock::start().await else { return };
    let mut h = Harness::new(&mock).await;
    let t = h
        .turn("Answer and then stop halfway through the tokens.", &mut |_| Answer::default())
        .await;
    assert!(
        !t.errors.is_empty(),
        "the stream died and the user was told nothing (answer so far: {:?})",
        t.answer
    );
    assert!(
        t.answer.len() < "This answer is cut off in the middle of its final sentence because the mock".len(),
        "the whole answer arrived, so the stream was not actually cut: {:?}",
        t.answer
    );
    assert_eq!(t.turns_after, 1, "the turn never closed: the UI would hang on this");
}

#[tokio::test]
async fn an_unscripted_prompt_is_an_error_rather_than_a_plausible_stub() {
    let Some(mock) = Mock::start().await else { return };
    let mut h = Harness::new(&mock).await;
    let t = h
        .turn("Please describe the weather over the Bay of Biscay in one line.", &mut |_| Answer::default())
        .await;
    assert!(
        t.errors.iter().any(|m| m.contains("no scenario")),
        "an unmatched prompt was answered anyway: errors={:?} answer={:?}",
        t.errors,
        first_line(&t.answer)
    );
    assert!(
        t.answer.trim().is_empty(),
        "the harness invented an answer for a prompt nothing answers: {:?}",
        t.answer
    );
}

#[tokio::test]
async fn the_scripted_endpoint_advertises_itself_the_way_llama_cpp_does() {
    let Some(mock) = Mock::start().await else { return };
    let mut cfg = Config::default();
    cfg.provider.base_url = format!("{}/v1", mock.base);
    cfg.provider.model = MODEL.into();
    let provider = cfg.build_provider().unwrap();
    let list = provider.list_models_info().await.unwrap();
    let m = list
        .iter()
        .find(|m| m.id == MODEL)
        .unwrap_or_else(|| panic!("the scripted endpoint listed {list:?}"));
    // These three are what the model chip and the thinking menu are built from. If
    // the scripted endpoint stopped publishing them, the capability checks in the
    // UI would be testing a server that does not exist.
    assert_eq!(m.context_tokens, Some(262_144), "context window: {m:?}");
    assert_eq!(m.tools, Some(true), "tool support: {m:?}");
    assert!(m.tools_inferred, "tool support must be labelled as inferred");
    assert_eq!(m.reasoning_efforts.len(), 3, "efforts: {:?}", m.reasoning_efforts);
    assert!(m.advertised, "the endpoint published nothing");
    assert_eq!(m.server.as_deref(), Some("cwoffee-mock 1.0"), "build_info: {m:?}");
    // The endpoint's own sampling defaults, read from `default_generation_settings`
    // (the values it actually applies) and reported, never applied. The mock states
    // only temperature, so the rest must stay None rather than becoming our guess.
    // Real llama.cpp nests these under default_generation_settings.params; the
    // mock mirrors that, and states top_p at the root instead, so the per-key
    // fallback is exercised rather than assumed.
    assert_eq!(m.sampling.temperature, Some(0.8), "defaults: {:?}", m.sampling);
    assert_eq!(m.sampling.top_k, Some(40), "defaults: {:?}", m.sampling);
    assert_eq!(m.sampling.top_p, Some(0.95), "root-level fallback: {:?}", m.sampling);
    assert_eq!(m.sampling.min_p, None, "unstated defaults must stay unstated");
    assert_eq!(m.sampling.seed, None, "unstated defaults must stay unstated");
}

/// The `[sampling]` section, proved at the only place it can be proved: the bytes
/// the endpoint receives. Unset keys must be absent, because a default invented
/// here would silently rewrite how someone else's server behaves — and this
/// project's own endpoint runs at temperature 1.0, which is a decision about that
/// model, not a bug for us to paper over.
#[tokio::test]
async fn sampling_keys_reach_the_endpoint_only_when_they_were_written_down() {
    let Some(mock) = Mock::start().await else {
        eprintln!("SKIP: scripted endpoint unavailable (node or scenarios missing)");
        return;
    };
    let prompt = "What is 6 times 7? Answer with just the number.";

    // 1. Nothing configured: the request carries no sampling keys at all.
    let mut bare = Harness::new(&mock).await;
    let t = bare.turn(prompt, &mut |_| Answer::default()).await;
    assert!(!t.timed_out, "the turn never closed: {:?}", t.errors);
    assert_eq!(t.answer.trim(), "42", "answer: {:?}", t.answer);
    let seen = mock
        .sampling_seen()
        .await
        .expect("the scripted endpoint reports what it received");
    let last = &seen[0];
    assert_eq!(
        last["keys"].as_array().map(|a| a.len()),
        Some(0),
        "unset sampling keys must not be invented: {seen}"
    );

    // 2. Configured: those keys, with those values, in the shape llama.cpp takes.
    let mut tuned = Harness::with_sampling(
        &mock,
        cwoffee_engine::provider::Sampling {
            temperature: Some(0.6),
            top_p: Some(0.9),
            top_k: Some(20),
            min_p: Some(0.05),
            seed: Some(7),
        },
    )
    .await;
    let t = tuned.turn(prompt, &mut |_| Answer::default()).await;
    assert!(!t.timed_out, "the turn never closed: {:?}", t.errors);
    let seen = mock.sampling_seen().await.expect("state");
    let last = &seen[0];
    let keys: Vec<&str> = last["keys"]
        .as_array()
        .map(|a| a.iter().filter_map(|k| k.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        keys,
        vec!["temperature", "top_p", "top_k", "min_p", "seed"],
        "every configured key must be on the wire: {seen}"
    );
    let v = &last["values"];
    assert!((v["temperature"].as_f64().unwrap() - 0.6).abs() < 1e-3);
    assert!((v["top_p"].as_f64().unwrap() - 0.9).abs() < 1e-3);
    assert_eq!(v["top_k"].as_u64(), Some(20), "top_k must stay an integer");
    assert!((v["min_p"].as_f64().unwrap() - 0.05).abs() < 1e-3);
    assert_eq!(v["seed"].as_u64(), Some(7), "seed must stay an integer");
}
