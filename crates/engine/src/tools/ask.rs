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
}

/// The answer. `text` is what the model reads; `option` records that the user
/// picked one of the offered choices rather than typing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answer {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<usize>,
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
                "allow_custom": { "type": "boolean", "description": "Offer a free-text answer too (default true)" }
            },
            "required": ["question", "options"]
        })
    }

    /// Asking is not a side effect, and a second confirmation on top of a
    /// question would be absurd.
    fn risk(&self) -> Risk {
        Risk::ReadOnly
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

        match tokio::time::timeout(self.timeout, reply_rx).await {
            Err(_) => ToolResult::err(format!(
                "ask_user: the user did not answer within {}s. Pick the option you \
                 recommended (or the most sensible one), say in your reply that you \
                 asked and got no answer, and continue.",
                self.timeout.as_secs()
            )),
            // The turn was cancelled, or the pending question was cleared.
            Ok(Err(_)) => ToolResult::err(
                "ask_user: the question was cancelled before it was answered. Stop and \
                 let the user know you were waiting on it.",
            ),
            Ok(Ok(answer)) => {
                let text = answer.text.trim();
                if text.is_empty() {
                    return ToolResult::err(
                        "ask_user: the user dismissed the question without answering. \
                         Do not invent an answer; ask differently or state that you \
                         need it.",
                    );
                }
                let _ = id;
                ToolResult::ok(match answer.option {
                    Some(i) => format!(
                        "User chose option {} ({}) for \"{question}\": {text}",
                        i + 1,
                        options[i].label
                    ),
                    None => format!("User answered \"{question}\": {text}"),
                })
            }
        }
    }
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

    /// The happy path, and the shape the model sees: an option pick is reported
    /// as a choice, not as a bare string.
    #[tokio::test]
    async fn an_answer_comes_back_as_the_tool_result() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            ask: Some(tx),
        };
        let t = tool();
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            assert_eq!(q.question, "Which format should the report use?");
            assert_eq!(q.options.len(), 2);
            assert_eq!(q.recommended, Some(1));
            assert!(q.allow_custom);
            reply
                .send(Answer { text: "PDF".into(), option: Some(1) })
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
        assert!(r.output.contains("User chose option 2 (PDF)"), "{}", r.output);
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
        };
        let h = tokio::spawn(async move {
            let (_, reply) = rx.recv().await.unwrap();
            reply
                .send(Answer { text: "make it landscape".into(), option: None })
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
        assert!(r.output.contains("User answered"), "{}", r.output);
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
        };
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            assert_eq!(q.recommended, None, "bad index survived");
            reply.send(Answer { text: "a".into(), option: Some(0) }).unwrap();
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
