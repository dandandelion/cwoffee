//! `ask_user`: the model asks a human to choose, and waits.
//!
//! This is the one tool whose "result" is a person. Everything else in the tool
//! layer either returns data or fails; here the agent loop stops until the front
//! end answers, so the failure modes matter more than the happy path:
//!
//!   * nobody is attached        -> the send fails, the model is told so
//!   * nobody answers in time    -> the model is told to decide for itself and
//!                                  to say that it asked
//!   * the user stops the turn   -> the reply sender is dropped, which wakes the
//!                                  tool immediately instead of leaving it parked
//!                                  until the timeout
//!
//! The question never enters the prompt on its own: what goes back to the model
//! is the answer text, in the tool_result slot, exactly like any other tool.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::{ExecSink, Risk, Tool, ToolCtx, ToolResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    /// One line of trade-off, shown under the label. Optional because most
    /// choices do not need one and padding every option makes the card longer
    /// to read, not clearer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What the model asked and how the front end should render it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: u64,
    pub question: String,
    pub options: Vec<QuestionOption>,
    /// Index into `options`. The front end badges it; it is not enforced,
    /// because the point of a recommendation is that the human may refuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
    /// Whether a free-text answer is offered alongside the options.
    pub allow_custom: bool,
    /// Whether more than one option may be chosen. The model decides, because
    /// only it knows whether the question is "which one" or "which of these".
    #[serde(default)]
    pub multi: bool,
}

/// The answer, as the front end sent it.
///
/// `options` indexes into `Question::options` and may be empty when the user
/// typed something instead. `text` is the resolved answer either way: the label
/// of what was picked, or what was typed. Keeping both matters because the model
/// needs to know whether it got a choice from its own list or words of the
/// user's own - the second usually carries constraints the model did not think of.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Answer {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<usize>,
}

pub type QuestionRx = (Question, oneshot::Sender<Answer>);
pub type QuestionTx = mpsc::UnboundedSender<QuestionRx>;

pub struct AskUserTool {
    next_id: AtomicU64,
    timeout: std::time::Duration,
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            // Shorter than the approval timeout on purpose. An unanswered
            // approval costs nothing; an unanswered question holds a turn that
            // the user may have walked away from.
            timeout: std::time::Duration::from_secs(180),
        }
    }
}

impl AskUserTool {
    pub fn with_timeout(mut self, d: std::time::Duration) -> Self {
        self.timeout = d;
        self
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their answer. Use it when a decision \
         is theirs to make - which option, which format, which risk to accept - not \
         to confirm things you can determine yourself. Give 2-4 short options, mark \
         the one you recommend, and keep the question to one sentence."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "One sentence" },
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 6,
                    "items": {
                        "type": "object",
                        "properties": {
                            "label":  { "type": "string", "description": "Few words" },
                            "detail": { "type": "string", "description": "Optional one-line trade-off" }
                        },
                        "required": ["label"]
                    }
                },
                "recommended": { "type": "integer", "description": "Index into options of the choice you recommend" },
                "allow_custom": { "type": "boolean", "description": "Offer a free-text answer too (default true)" },
                "multi": { "type": "boolean", "description": "Let the user pick more than one option. Only set this when the answers genuinely combine." }
            },
            "required": ["question", "options"]
        })
    }

    /// Asking is not a side effect, and a second confirmation on top of a
    /// question would be absurd.
    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let question = match args.get("question").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => return ToolResult::err("ask_user: \"question\" is required and must not be empty"),
        };

        let raw_opts = match args.get("options") {
            Some(Value::Array(items)) if !items.is_empty() => items.clone(),
            _ => {
                return ToolResult::err(
                    "ask_user: \"options\" must be a non-empty array of {label, detail?}",
                )
            }
        };
        if raw_opts.len() > 6 {
            return ToolResult::err(format!(
                "ask_user: {} options is too many to choose from. Offer at most 6.",
                raw_opts.len()
            ));
        }
        let mut options: Vec<QuestionOption> = Vec::with_capacity(raw_opts.len());
        for item in raw_opts {
            let label = item.get("label").and_then(|v| v.as_str()).unwrap_or("").trim();
            if label.is_empty() {
                return ToolResult::err("ask_user: every option needs a non-empty \"label\"");
            }
            if options.iter().any(|o| o.label.eq_ignore_ascii_case(label)) {
                return ToolResult::err(format!(
                    "ask_user: two options have the same label {label:?}. They must be \
                     distinguishable at a glance."
                ));
            }
            options.push(QuestionOption {
                label: label.to_string(),
                detail: item
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty()),
            });
        }

        let recommended = args
            .get("recommended")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .filter(|i| *i < options.len());
        let allow_custom = args
            .get("allow_custom")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let multi = args.get("multi").and_then(|v| v.as_bool()).unwrap_or(false);

        let Some(tx) = ctx.ask.as_ref() else {
            // Not a stub: this client genuinely has no way to ask, and the
            // model needs to know that rather than sit waiting.
            return ToolResult::err(
                "ask_user: this client cannot show questions to the user. Decide \
                 yourself, state the assumption you made, and continue.",
            );
        };

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel::<Answer>();
        if tx
            .send((
                Question {
                    id,
                    question: question.clone(),
                    options: options.clone(),
                    recommended,
                    allow_custom,
                    multi,
                },
                reply_tx,
            ))
            .is_err()
        {
            return ToolResult::err(
                "ask_user: nobody is connected to answer. Decide yourself, state the \
                 assumption you made, and continue.",
            );
        }

        // Tell every window when the question closes on its own. The reply
        // channel is dead by then, so the shells cannot report it: they are the
        // ones holding the sender, and a sender whose receiver is gone says
        // nothing until someone tries to use it.
        let expire = |ev: &super::ToolCtx, id: u64| {
            if let Some(tx) = &ev.events {
                let _ = tx.send(crate::event::EngineEvent::QuestionExpired { id });
            }
        };

        match tokio::time::timeout(self.timeout, reply_rx).await {
            Err(_) => {
                expire(ctx, id);
                ToolResult::err(format!(
                    "ask_user: the user did not answer within {}s. Pick the option you \
                     recommended (or the most sensible one), say in your reply that you \
                     asked and got no answer, and continue.",
                    self.timeout.as_secs()
                ))
            }
            // The turn was cancelled, or the pending question was cleared.
            Ok(Err(_)) => {
                expire(ctx, id);
                ToolResult::err(
                    "ask_user: the question was cancelled before it was answered. Stop and \
                     let the user know you were waiting on it.",
                )
            }
            Ok(Ok(answer)) => {
                let text = answer.text.trim();
                if text.is_empty() {
                    return ToolResult::err(
                        "ask_user: the user dismissed the question without answering. \
                         Do not invent an answer; ask differently or state that you \
                         need it.",
                    );
                }
                // A single-select answer that arrived with several picks is a
                // client bug; keep the first rather than failing a turn over it.
                let picks: Vec<usize> = answer
                    .options
                    .into_iter()
                    .filter(|i| *i < options.len())
                    .take(if multi { usize::MAX } else { 1 })
                    .collect();
                ToolResult::ok(restate(&question, &options, recommended, &picks, text))
            }
        }
    }
}

/// Put the question back in the answer.
///
/// The model asked this question one tool-call ago, and a bare "PDF" is enough
/// for a human reading the log. It is not enough for a quantized local model, and
/// this is not hypothetical: the first version of this tool replied with the
/// answer alone and the model came back confused about which question it was
/// answering, because a chat template that renders tool results without the
/// matching call leaves the answer floating. Restating costs a couple of dozen
/// tokens once per question and removes the ambiguity.
fn restate(
    question: &str,
    options: &[QuestionOption],
    recommended: Option<usize>,
    picks: &[usize],
    text: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Question you asked: \"{question}\"\n"));
    let offered: Vec<String> = options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let detail = o.detail.as_deref().map(|d| format!(" - {d}")).unwrap_or_default();
            let rec = if recommended == Some(i) { " (you recommended)" } else { "" };
            format!("{}: {}{detail}{rec}", i + 1, o.label)
        })
        .collect();
    out.push_str(&format!("Options you offered: {}\n", offered.join(" | ")));
    if picks.is_empty() {
        out.push_str(&format!("User typed their own answer, not one of the options: \"{text}\""));
    } else {
        let chosen: Vec<String> = picks
            .iter()
            .map(|i| format!("{}: {}", i + 1, options[*i].label))
            .collect();
        out.push_str(&format!("User chose: {}", chosen.join(" + ")));
        // Typed text alongside a pick is the interesting case: the user accepted
        // an option and added a constraint the model has to honour.
        let typed = text.trim();
        let labels: String = picks
            .iter()
            .map(|i| options[*i].label.trim())
            .collect::<Vec<_>>()
            .join(" ");
        if !typed.is_empty() && typed != labels {
            out.push_str(&format!("\nUser also said: \"{typed}\""));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn sink() -> ExecSink {
        mpsc::unbounded_channel().0
    }

    fn tool() -> AskUserTool {
        AskUserTool::default().with_timeout(std::time::Duration::from_millis(120))
    }

    /// A question that times out has to close in *every* window. The reply
    /// channel is dead by then, so the shells holding the sender cannot report
    /// it, and the card would keep offering an answer to a tool call that has
    /// already returned.
    #[tokio::test]
    async fn an_unanswered_question_tells_the_front_ends_it_closed() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let (etx, mut erx) = mpsc::unbounded_channel::<crate::event::EngineEvent>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: Some(etx),
            permits: Vec::new(),
        };
        let (idx, iqx) = oneshot::channel::<u64>();
        // Deliberately not awaited: the watcher holds the reply sender open past
        // the tool's timeout, which is the whole point. Dropping the sender would
        // put the tool on the *cancelled* arm and this would measure something
        // else. The runtime reclaims it when the test ends.
        tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            let _ = idx.send(q.id);
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(reply);
        });
        let r = tool()
            .run(json!({"question": "Which format?", "options": [{"label": "PDF"}]}), &ctx, &sink())
            .await;
        let qid = iqx.await.unwrap();
        let said = format!("{}{}", r.output, r.error.clone().unwrap_or_default());
        assert!(said.contains("did not answer within"), "{said}");
        let ev = erx.recv().await.expect("the expiry must be announced");
        match ev {
            crate::event::EngineEvent::QuestionExpired { id } => assert_eq!(id, qid),
            other => panic!("expected QuestionExpired, got {other:?}"),
        }
        assert!(erx.try_recv().is_err(), "exactly one expiry per question");
    }

    /// An answered question gets no expiry: the front end already settled the
    /// card, and a second "closed" signal would read as a contradiction.
    #[tokio::test]
    async fn an_answered_question_is_not_reported_as_expired() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let (etx, mut erx) = mpsc::unbounded_channel::<crate::event::EngineEvent>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: Some(etx),
            permits: Vec::new(),
        };
        tokio::spawn(async move {
            let (_q, reply) = rx.recv().await.unwrap();
            reply.send(Answer { text: "PDF".into(), options: vec![0] }).unwrap();
        });
        let r = tool()
            .run(json!({"question": "Which format?", "options": [{"label": "PDF"}]}), &ctx, &sink())
            .await;
        assert!(r.ok, "{}", r.output);
        assert!(erx.try_recv().is_err(), "an answer is not an expiry");
    }

    /// The answer has to name the question it answers. A quantized local model
    /// that receives a bare "PDF" two tool calls later does not know which
    /// question it was.
    #[tokio::test]
    async fn the_answer_carries_the_question_and_the_offered_options() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            reply
                .send(Answer { text: "Markdown".into(), options: vec![0] })
                .unwrap();
            q.question
        });
        let r = tool()
            .run(
                json!({
                    "question": "Which output format?",
                    "options": [
                        {"label": "Markdown", "detail": "editable"},
                        {"label": "PDF", "detail": "prints well"}
                    ],
                    "recommended": 1
                }),
                &ctx,
                &sink(),
            )
            .await;
        assert!(r.ok, "{}", r.text());
        let q = h.await.unwrap();
        let out = r.text();
        assert!(out.contains(&format!("Question you asked: \"{q}\"")), "{out}");
        assert!(out.contains("1: Markdown - editable"), "{out}");
        assert!(out.contains("(you recommended)"), "{out}");
        assert!(out.contains("User chose: 1: Markdown"), "{out}");
    }

    #[tokio::test]
    async fn a_multi_answer_reports_every_choice() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            assert!(q.multi, "the multi flag did not reach the front end");
            reply
                .send(Answer {
                    text: "Charts, Summary".into(),
                    options: vec![2, 0],
                })
                .unwrap();
        });
        let r = tool()
            .run(
                json!({
                    "question": "Which sections?",
                    "multi": true,
                    "options": [{"label": "Summary"}, {"label": "Notes"}, {"label": "Charts"}]
                }),
                &ctx,
                &sink(),
            )
            .await;
        assert!(r.ok, "{}", r.text());
        h.await.unwrap();
        let out = r.text();
        assert!(out.contains("3: Charts + 1: Summary"), "{out}");
    }

    /// A typed answer is not a choice from the model's list, and saying so is how
    /// the model learns it missed a constraint.
    #[tokio::test]
    async fn a_typed_answer_is_labelled_as_typed() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let h = tokio::spawn(async move {
            let (_q, reply) = rx.recv().await.unwrap();
            reply
                .send(Answer { text: "make it landscape".into(), options: vec![] })
                .unwrap();
        });
        let r = tool()
            .run(
                json!({"question": "Paper size?", "options": [{"label": "A4"}, {"label": "Letter"}]}),
                &ctx,
                &sink(),
            )
            .await;
        assert!(r.ok, "{}", r.text());
        h.await.unwrap();
        assert!(r.text().contains("User typed their own answer"), "{}", r.text());
    }

    /// Picking an option and adding a note is the common real case: honour both.
    #[tokio::test]
    async fn a_pick_with_a_note_keeps_both() {
        let out = restate(
            "Which format?",
            &[QuestionOption { label: "PDF".into(), detail: None }],
            None,
            &[0],
            "PDF, but landscape",
        );
        assert!(out.contains("User chose: 1: PDF"), "{out}");
        assert!(out.contains("User also said: \"PDF, but landscape\""), "{out}");
    }

    /// A single-select client that sends several picks is a client bug; take the
    /// first rather than failing the turn.
    #[test]
    fn single_select_collapses_extra_picks() {
        let out = restate(
            "Which?",
            &[
                QuestionOption { label: "A".into(), detail: None },
                QuestionOption { label: "B".into(), detail: None },
            ],
            None,
            &[1],
            "B",
        );
        assert!(out.contains("User chose: 2: B"), "{out}");
    }

    /// The happy path, and the shape the model sees: an option pick is reported
    /// as a choice, not as a bare string.
    #[tokio::test]
    async fn an_answer_comes_back_as_the_tool_result() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let t = tool();
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            assert_eq!(q.question, "Which format should the report use?");
            assert_eq!(q.options.len(), 2);
            assert_eq!(q.recommended, Some(1));
            assert!(q.allow_custom);
            reply
                .send(Answer { text: "PDF".into(), options: vec![1] })
                .unwrap();
        });
        let r = t
            .run(
                json!({
                    "question": "Which format should the report use?",
                    "options": [{ "label": "CSV" }, { "label": "PDF", "detail": "prints well" }],
                    "recommended": 1
                }),
                &ctx,
                &sink(),
            )
            .await;
        h.await.unwrap();
        assert!(r.ok, "{:?}", r.error);
        // The new wording, asserted here too so both tests describe the same
        // contract: choice, label, and the question the choice answers.
        assert!(r.output.contains("User chose: 2: PDF"), "{}", r.output);
        assert!(r.output.contains("Question you asked:"), "{}", r.output);
        assert!(r.output.contains("PDF"), "{}", r.output);
    }

    /// A free-text answer must not be dressed up as one of the options.
    #[tokio::test]
    async fn a_typed_answer_is_reported_as_typed() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let h = tokio::spawn(async move {
            let (_, reply) = rx.recv().await.unwrap();
            reply
                .send(Answer { text: "make it landscape".into(), options: vec![] })
                .unwrap();
        });
        let r = tool()
            .run(
                json!({"question": "Orientation?", "options": [{"label": "portrait"}]}),
                &ctx,
                &sink(),
            )
            .await;
        h.await.unwrap();
        assert!(r.ok);
        assert!(r.output.contains("User typed their own answer"), "{}", r.output);
        assert!(r.output.contains("make it landscape"), "{}", r.output);
    }

    #[tokio::test]
    async fn a_client_that_cannot_ask_says_so_instead_of_hanging() {
        let ctx = ToolCtx::default();
        assert!(ctx.ask.is_none(), "default ctx must have no question channel");
        let r = tool()
            .run(json!({"question": "Which?", "options": [{"label": "a"}]}), &ctx, &sink())
            .await;
        assert!(!r.ok);
        assert!(r.text().contains("cannot show questions"), "{}", r.text());
    }

    #[tokio::test]
    async fn nobody_listening_is_reported_not_hung() {
        let (tx, rx) = mpsc::unbounded_channel::<QuestionRx>();
        drop(rx);
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let r = tool()
            .run(json!({"question": "Which?", "options": [{"label": "a"}]}), &ctx, &sink())
            .await;
        assert!(!r.ok);
        assert!(r.text().contains("nobody is connected"), "{}", r.text());
    }

    /// Silence is not an answer. The model is told to decide and to admit it
    /// decided alone - which keeps the transcript honest about who chose.
    #[tokio::test]
    async fn silence_ends_the_wait_with_an_instruction() {
        let (tx, _rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let r = tool()
            .run(json!({"question": "Which?", "options": [{"label": "a"}]}), &ctx, &sink())
            .await;
        assert!(!r.ok);
        assert!(r.text().contains("did not answer"), "{}", r.text());
    }

    /// Stop must not leave the turn parked for the length of the timeout.
    #[tokio::test]
    async fn dropping_the_reply_wakes_the_tool_immediately() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let t = AskUserTool::default().with_timeout(std::time::Duration::from_secs(60));
        let h = tokio::spawn(async move {
            let (_, reply) = rx.recv().await.unwrap();
            drop(reply);
        });
        let started = std::time::Instant::now();
        let r = t
            .run(json!({"question": "Which?", "options": [{"label": "a"}]}), &ctx, &sink())
            .await;
        h.await.unwrap();
        assert!(!r.ok);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "waited for the timeout");
        assert!(r.text().contains("cancelled"), "{}", r.text());
    }

    #[tokio::test]
    async fn malformed_questions_are_refused_with_actionable_text() {
        let ctx = ToolCtx::default();
        let t = tool();
        for args in [
            json!({"options": [{"label": "a"}]}),
            json!({"question": "  ", "options": [{"label": "a"}]}),
            json!({"question": "x"}),
            json!({"question": "x", "options": []}),
            json!({"question": "x", "options": [{"label": "  "}]}),
            json!({"question": "x", "options": [{"label": "a"}, {"label": "A"}]}),
        ] {
            let r = t.run(args.clone(), &ctx, &sink()).await;
            assert!(!r.ok, "accepted {args}");
            assert!(!r.text().is_empty(), "no explanation for {args}");
        }
        let many: Vec<Value> = (0..7).map(|i| json!({"label": format!("o{i}")})).collect();
        let r = t
            .run(json!({"question": "x", "options": many}), &ctx, &sink())
            .await;
        assert!(!r.ok && r.text().contains("at most 6"), "{}", r.text());
    }

    /// A recommended index outside the option list is dropped, not wrapped:
    /// pointing at "option 9 of 3" would render a badge on nothing.
    #[tokio::test]
    async fn an_out_of_range_recommendation_is_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
        };
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            assert_eq!(q.recommended, None, "bad index survived");
            reply.send(Answer { text: "a".into(), options: vec![0] }).unwrap();
        });
        let r = tool()
            .run(
                json!({"question": "x", "options": [{"label": "a"}], "recommended": 9}),
                &ctx,
                &sink(),
            )
            .await;
        h.await.unwrap();
        assert!(r.ok, "{:?}", r.error);
    }
}
