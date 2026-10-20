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

/// Which card a question belongs to. A model that asks three things at once
/// sends three questions, and a front end that shows them as three unrelated
/// cards turns one decision into three interruptions. The set id groups them;
/// `index`/`total` let the shell say "question 2 of 3" without waiting to see
/// whether more are coming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionSet {
    pub id: u64,
    pub index: usize,
    pub total: usize,
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
    /// Absent for a one-question call, which is the shape every existing client
    /// already renders. Making it optional rather than always-present keeps that
    /// wire shape byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<QuestionSet>,
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

    /// The questions this call would ask, in the order they should be asked.
    ///
    /// A host that cannot hold the question open *inside* the tool needs them first.
    /// pi runs the tool inside an extension, so by the time the bridge sees the call it
    /// is too late to ask; cwoffee's answers therefore arrive as an argument to the tool
    /// host rather than as a live channel. Both halves call this, so the card the user
    /// sees and the answers the tool scores are one set, and the ids that
    /// `QuestionExpired` is reported by come from a single allocation.
    pub fn questions_for(&self, args: &Value) -> Result<Vec<Question>, String> {
        let specs = parse_specs(args)?;
        Ok(self.build_questions(&specs))
    }

    /// Id allocation for one call: a set id first — so "question 2 of 3" can be said
    /// without waiting to see whether more questions are coming — then one id per
    /// question. A single question carries `set: None`, which is the wire shape every
    /// existing client already renders.
    fn build_questions(&self, specs: &[Spec]) -> Vec<Question> {
        let total = specs.len();
        // One set id for the whole call, so a shell can group the cards and say
        // "question 2 of 3". A single question keeps `set: None`, which is exactly the
        // wire shape every existing client already renders.
        let set_id = if total > 1 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        specs
            .iter()
            .enumerate()
            .map(|(i, spec)| Question {
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                question: spec.question.clone(),
                options: spec.options.clone(),
                recommended: spec.recommended,
                allow_custom: spec.allow_custom,
                multi: spec.multi,
                set: (total > 1).then(|| QuestionSet { id: set_id, index: i, total }),
            })
            .collect()
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
         the one you recommend, and keep the question to one sentence. When one step \
         needs several decisions, ask them in a single call with `questions` (2-4 of \
         them, same fields each) instead of one call per question: the user answers \
         them as one card and every answer comes back together."
    }

    fn schema(&self) -> Value {
        let one = json!({
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
        });
        // The set form reuses the single-question object instead of inventing a
        // narrower variant: two shapes to learn is two ways to get it wrong, and a
        // quantized local model will get it wrong.
        json!({
            "type": "object",
            "properties": {
                "question": one["properties"]["question"].clone(),
                "options": one["properties"]["options"].clone(),
                "recommended": one["properties"]["recommended"].clone(),
                "allow_custom": one["properties"]["allow_custom"].clone(),
                "multi": one["properties"]["multi"].clone(),
                "questions": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 4,
                    "description": "Ask 2-4 questions as one card instead of one call each. Each entry has the same fields as a single call.",
                    "items": one
                }
            }
        })
    }

    /// Asking is not a side effect, and a second confirmation on top of a
    /// question would be absurd.
    fn risk(&self) -> Risk {
        Risk::Harmless
    }

    /// The one tool whose result is a person, so it is the one tool a remote host has
    /// to ask on its behalf.
    fn host_questions(&self, args: &Value) -> Option<Result<Vec<Question>, String>> {
        Some(self.questions_for(args))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx, _sink: &ExecSink) -> ToolResult {
        let specs = match parse_specs(&args) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(e),
        };

        let Some(tx) = ctx.ask.as_ref() else {
            // Not a stub: this client genuinely has no way to ask, and the
            // model needs to know that rather than sit waiting.
            return ToolResult::err(
                "ask_user: this client cannot show questions to the user. Decide \
                 yourself, state the assumption you made, and continue.",
            );
        };

        // The questions come from one helper, so the card `cwtool questions` shows a
        // remote host and the card this call would have shown are the same set.
        let total = specs.len();
        let qs = self.build_questions(&specs);
        let mut waiting: Vec<(u64, Spec, oneshot::Receiver<Answer>)> = Vec::with_capacity(total);
        for (i, q) in qs.into_iter().enumerate() {
            // Keep the id the question was sent with: an expired question is reported to
            // every window by that id, and a card that closed under a different number
            // than the one the expiry names is a card that never closes.
            let id = q.id;
            let (reply_tx, reply_rx) = oneshot::channel::<Answer>();
            if tx.send((q, reply_tx)).is_err() {
                return ToolResult::err(
                    "ask_user: nobody is connected to answer. Decide yourself, state the \
                     assumption you made, and continue.",
                );
            }
            waiting.push((id, specs[i].clone(), reply_rx));
        }

        // A set is answered as one card, so the wait is one budget rather than N
        // sequential ones. Reading and answering three questions takes longer than
        // answering one, and a per-question timeout would punish the model for
        // asking them together - which is what the description just told it to do.
        // The host's configured budget wins; the tool's own default covers callers
        // that build a ToolCtx without a host.
        let timeout = ctx.question_timeout.unwrap_or(self.timeout);
        let budget = timeout.mul_f64(1.0 + 0.5 * (total - 1) as f64);
        let deadline = tokio::time::Instant::now() + budget;
        let mut got: Vec<Option<Answer>> = Vec::with_capacity(total);
        let mut timed_out = false;
        for (id, _, rx) in waiting.iter_mut() {
            if timed_out {
                // The person walked away from the card, not from one field of it, so
                // the rest of the set stops waiting too.
                got.push(None);
                continue;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, rx).await {
                Ok(Ok(a)) => {
                    got.push(Some(a));
                    // An answer belongs to one window, but the card sits in all of them.
                    // Say so, and every other copy closes instead of offering a choice
                    // nothing will take any more. The expiry below is the same announcement
                    // for the case where nobody answered; a question that is answered and a
                    // question that is ignored must both be visible from outside the window
                    // that acted. The pi core's bridge emits this for its questions because
                    // there the tool runs in another process; here the tool emits it, so
                    // exactly one announcement per question either way.
                    if let Some(tx) = ctx.events.as_ref() {
                        let _ = tx.send(crate::event::EngineEvent::QuestionAnswered { id: *id });
                    }
                }
                // The reply sender is gone: the turn was stopped, or the pending
                // question was cleared.
                Ok(Err(_)) => got.push(None),
                Err(_) => {
                    timed_out = true;
                    got.push(None);
                }
            }
        }

        // Tell every window when a question closes on its own. The reply channel is
        // dead by then, so the shells cannot report it: they are the ones holding the
        // sender, and a sender whose receiver is gone says nothing until someone
        // tries to use it.
        let expire = |ev: &ToolCtx, id: u64| {
            if let Some(tx) = &ev.events {
                let _ = tx.send(crate::event::EngineEvent::QuestionExpired { id });
            }
        };
        let unanswered: Vec<u64> = waiting
            .iter()
            .zip(got.iter())
            .filter(|(_, a)| a.is_none())
            .map(|((id, _, _), _)| *id)
            .collect();
        for id in &unanswered {
            expire(ctx, *id);
        }

        if total == 1 {
            // One question keeps the three outcomes the shells and the tests describe:
            // answered, timed out, cancelled.
            let (_id, spec, _rx) = waiting.into_iter().next().expect("one question per slot");
            match got.first().and_then(|a| a.as_ref()) {
                Some(answer) => {
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
                        .iter()
                        .copied()
                        .filter(|i| *i < spec.options.len())
                        .take(if spec.multi { usize::MAX } else { 1 })
                        .collect();
                    ToolResult::ok(restate(
                        &spec.question,
                        &spec.options,
                        spec.recommended,
                        &picks,
                        text,
                    ))
                }
                None if timed_out => ToolResult::err(format!(
                    "ask_user: the user did not answer within {}s. Pick the option you \
                     recommended (or the most sensible one), say in your reply that you \
                     asked and got no answer, and continue. If the answer needs asking \
                     someone else, question_timeout_secs in config.toml sets how long this \
                     card stays open.",
                    timeout.as_secs()
                )),
                None => ToolResult::err(
                    "ask_user: the question was cancelled before it was answered. Stop and \
                     let the user know you were waiting on it.",
                ),
            }
        } else {
            let answered = got.iter().flatten().filter(|a| !a.text.trim().is_empty()).count();
            if answered == 0 {
                return ToolResult::err(if timed_out {
                    format!(
                        "ask_user: the user answered none of the {total} questions within {}s. \
                         Decide them yourself, say in your reply that you asked and got no \
                         answer, and continue. question_timeout_secs in config.toml sets how \
                         long these cards stay open.",
                        budget.as_secs()
                    )
                } else {
                    format!(
                        "ask_user: the {total} questions were cancelled before any was \
                         answered. Stop and let the user know you were waiting on them."
                    )
                });
            }
            // Partial answers are not an error. Three of four answers are real
            // information, and losing them because the fourth went unanswered would
            // throw away the person's actual decisions.
            let mut out = String::new();
            for (i, ((_id, spec, _rx), answer)) in waiting.iter().zip(got.iter()).enumerate() {
                let block = match answer {
                    Some(a) if !a.text.trim().is_empty() => {
                        let text = a.text.trim();
                        let picks: Vec<usize> = a
                            .options
                            .iter()
                            .copied()
                            .filter(|i| *i < spec.options.len())
                            .take(if spec.multi { usize::MAX } else { 1 })
                            .collect();
                        restate_nth(
                            i + 1,
                            total,
                            &spec.question,
                            &spec.options,
                            spec.recommended,
                            &picks,
                            text,
                        )
                    }
                    Some(_) => format!(
                        "Question {} of {total} you asked: \"{}\"\nThe user skipped this one \
                         without answering.",
                        i + 1,
                        spec.question
                    ),
                    None => format!(
                        "Question {} of {total} you asked: \"{}\"\nThe user did not answer this \
                         one in time.",
                        i + 1,
                        spec.question
                    ),
                };
                out.push_str(&block);
                out.push('\n');
            }
            let skipped = total - answered;
            if skipped > 0 {
                out.push_str(&format!(
                    "\n{skipped} of {total} went unanswered (marked above). Decide those \
                     yourself, state the assumption you made for each, and say in your reply \
                     that you asked."
                ));
            }
            ToolResult::ok(out)
        }
    }
}

/// One question as the model wrote it, before it becomes a `Question` on the wire.
#[derive(Debug, Clone)]
struct Spec {
    question: String,
    options: Vec<QuestionOption>,
    recommended: Option<usize>,
    allow_custom: bool,
    multi: bool,
}

/// Read either shape: one question, or `questions` holding 2-4 of them.
///
/// The flat form stays the primary one, and the set form is an option on top of it
/// rather than a second tool. Two tools would make the model choose which to call,
/// and a quantized local model chooses by which one it saw more often in training,
/// not by which one fits.
fn parse_specs(args: &Value) -> Result<Vec<Spec>, String> {
    if let Some(list) = args.get("questions") {
        let items = match list {
            Value::Array(items) if items.len() >= 2 => items.clone(),
            Value::Array(items) => {
                return Err(format!(
                    "ask_user: `questions` needs 2-4 entries; got {}. Ask a single question \
                     with `question` and `options` instead.",
                    items.len()
                ))
            }
            _ => {
                return Err(
                    "ask_user: `questions` must be an array of question objects".to_string(),
                )
            }
        };
        if items.len() > 4 {
            return Err(format!(
                "ask_user: {} questions at once is an interrogation, not a question card. \
                 Ask at most 4, or ask the rest in a later turn.",
                items.len()
            ));
        }
        // A mixed call is refused rather than guessed at: a top-level `question` beside
        // `questions` reads as a fifth question to anyone skimming the call, including
        // the model that wrote it.
        if args.get("question").is_some() || args.get("options").is_some() {
            return Err(
                "ask_user: use either `question`/`options` alone or `questions`, not both in \
                 one call."
                    .to_string(),
            );
        }
        return items.iter().map(parse_spec).collect();
    }
    Ok(vec![parse_spec(args)?])
}

fn parse_spec(v: &Value) -> Result<Spec, String> {
    let question = match v.get("question").and_then(|q| q.as_str()) {
        Some(q) if !q.trim().is_empty() => q.trim().to_string(),
        _ => return Err("ask_user: \"question\" is required and must not be empty".to_string()),
    };

    let raw_opts = match v.get("options") {
        Some(Value::Array(items)) if !items.is_empty() => items.clone(),
        _ => {
            return Err(
                "ask_user: \"options\" must be a non-empty array of {label, detail?}".to_string(),
            )
        }
    };
    if raw_opts.len() > 6 {
        return Err(format!(
            "ask_user: {} options is too many to choose from. Offer at most 6.",
            raw_opts.len()
        ));
    }
    let mut options: Vec<QuestionOption> = Vec::with_capacity(raw_opts.len());
    for item in raw_opts {
        let label = item.get("label").and_then(|l| l.as_str()).unwrap_or("").trim();
        if label.is_empty() {
            return Err("ask_user: every option needs a non-empty \"label\"".to_string());
        }
        if options.iter().any(|o| o.label.eq_ignore_ascii_case(label)) {
            return Err(format!(
                "ask_user: two options have the same label {label:?}. They must be \
                 distinguishable at a glance."
            ));
        }
        options.push(QuestionOption {
            label: label.to_string(),
            detail: item
                .get("detail")
                .and_then(|d| d.as_str())
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty()),
        });
    }

    let recommended = v
        .get("recommended")
        .and_then(|n| n.as_u64())
        .map(|n| n as usize)
        .filter(|i| *i < options.len());
    let allow_custom = v.get("allow_custom").and_then(|b| b.as_bool()).unwrap_or(true);
    let multi = v.get("multi").and_then(|b| b.as_bool()).unwrap_or(false);
    Ok(Spec { question, options, recommended, allow_custom, multi })
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
    restate_nth(0, 0, question, options, recommended, picks, text)
}

/// `n`/`total` number the restatement when a call asked several questions. Without
/// the number, two restated answers in one tool result read as one answer to both,
/// which is the exact ambiguity the restatement exists to remove.
fn restate_nth(
    n: usize,
    total: usize,
    question: &str,
    options: &[QuestionOption],
    recommended: Option<usize>,
    picks: &[usize],
    text: &str,
) -> String {
    let mut out = String::new();
    let lead = if n > 1 || total > 1 {
        format!("Question {n} of {total} you asked")
    } else {
        "Question you asked".to_string()
    };
    out.push_str(&format!("{lead}: \"{question}\"\n"));
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
            question_timeout: None,
            ask: Some(tx),
            events: Some(etx),
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
    /// The host's budget is the wait, not the tool's own default, and the expiry has
    /// to name the key. Session 1306 lost a question the user needed to take to a
    /// second person, and nothing in the transcript said a knob existed.
    #[tokio::test]
    async fn the_hosts_budget_is_the_wait_and_names_the_knob_when_it_bites() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: Some(std::time::Duration::from_millis(150)),
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        tokio::spawn(async move {
            let (_q, _reply) = rx.recv().await.unwrap();
            // Held open past the budget on purpose: dropping it early would take the
            // cancelled arm instead of the timed-out one.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        // AskUserTool::default() would wait 180s. If the context value were ignored
        // this test would sit for three minutes instead of a sixth of one.
        let r = AskUserTool::default()
            .run(
                json!({"question": "Ask the treasurer which month?", "options": [{"label": "January"}]}),
                &ctx,
                &sink(),
            )
            .await;
        let said = format!("{}{}", r.output, r.error.clone().unwrap_or_default());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited on the tool default, not the host's budget: {:?}",
            started.elapsed()
        );
        assert!(said.contains("did not answer within"), "{said}");
        assert!(said.contains("question_timeout_secs"), "the expiry must name the knob: {said}");
    }

    #[tokio::test]
    async fn an_answered_question_is_not_reported_as_expired() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let (etx, mut erx) = mpsc::unbounded_channel::<crate::event::EngineEvent>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: Some(etx),
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        let (idx, iqx) = oneshot::channel::<u64>();
        tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            let _ = idx.send(q.id);
            reply.send(Answer { text: "PDF".into(), options: vec![0] }).unwrap();
        });
        let r = tool()
            .run(json!({"question": "Which format?", "options": [{"label": "PDF"}]}), &ctx, &sink())
            .await;
        assert!(r.ok, "{}", r.output);
        let asked = iqx.await.unwrap();
        // An answer is not an expiry — but it is an announcement. Every other window showing
        // this question has to learn the card was settled, which is why the core that runs the
        // tool in-process emits it too: a card that stays live after being answered is a
        // control wired to a question somebody already settled.
        let ev = erx.recv().await.expect("an answered question announces the answer");
        match ev {
            crate::event::EngineEvent::QuestionAnswered { id } => assert_eq!(id, asked),
            other => panic!("an answer announced {other:?}, not QuestionAnswered"),
        }
        assert!(erx.try_recv().is_err(), "one answer announces once, and nothing expires");
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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
    /// A set has to arrive as one card, not as three unrelated interruptions. The
    /// `set` marker is what lets a shell render "question 2 of 3" and keep the
    /// answers together.
    #[tokio::test]
    async fn a_set_of_questions_arrives_marked_as_one_card() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        let h = tokio::spawn(async move {
            let (q1, r1) = rx.recv().await.unwrap();
            let (q2, r2) = rx.recv().await.unwrap();
            let s1 = q1.set.clone().expect("a set question must say which set");
            let s2 = q2.set.clone().expect("the second too");
            assert_eq!((s1.id, s1.index, s1.total), (s2.id, 0, 2), "set markers disagree");
            assert_eq!((s2.index, s2.total), (1, 2), "the second question must know it is second");
            assert_ne!(q1.id, q2.id, "each question still has its own id to answer");
            r1.send(Answer { text: "PDF".into(), options: vec![1] }).unwrap();
            r2.send(Answer { text: "landscape".into(), options: vec![] }).unwrap();
            (q1.question, q2.question)
        });
        let r = tool()
            .run(
                json!({"questions": [
                    {"question": "Which format?", "options": [{"label": "MD"}, {"label": "PDF"}]},
                    {"question": "Which orientation?", "options": [{"label": "portrait"}, {"label": "landscape"}]}
                ]}),
                &ctx,
                &sink(),
            )
            .await;
        let (q1, q2) = h.await.unwrap();
        assert!(r.ok, "{:?}", r.error);
        let out = r.text();
        assert!(out.contains(&format!("Question 1 of 2 you asked: \"{q1}\"")), "{out}");
        assert!(out.contains(&format!("Question 2 of 2 you asked: \"{q2}\"")), "{out}");
        assert!(out.contains("User chose: 2: PDF"), "{out}");
        assert!(out.contains("User typed their own answer"), "{out}");
    }

    /// Answering one question of a set and skipping another is a normal answer. The
    /// choices the person did make are information; dropping the whole result because
    /// one field was skipped throws them away.
    #[tokio::test]
    async fn a_partial_set_keeps_the_answers_that_arrived() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        let h = tokio::spawn(async move {
            let (_, r1) = rx.recv().await.unwrap();
            let (_, r2) = rx.recv().await.unwrap();
            r1.send(Answer { text: "MD".into(), options: vec![0] }).unwrap();
            // Skipped: an empty answer, which is what a "no answer" submit sends.
            r2.send(Answer::default()).unwrap();
        });
        let r = tool()
            .run(
                json!({"questions": [
                    {"question": "Format?", "options": [{"label": "MD"}, {"label": "PDF"}]},
                    {"question": "Orientation?", "options": [{"label": "portrait"}]}
                ]}),
                &ctx,
                &sink(),
            )
            .await;
        h.await.unwrap();
        assert!(r.ok, "{:?}", r.error);
        let out = r.text();
        assert!(out.contains("User chose: 1: MD"), "{out}");
        assert!(out.contains("The user skipped this one"), "{out}");
        assert!(out.contains("1 of 2 went unanswered"), "{out}");
        assert!(out.contains("Decide those yourself"), "{out}");
    }

    /// A set nobody answered is an error, and the wording has to match what actually
    /// happened: two questions, not "the question".
    #[tokio::test]
    async fn an_unanswered_set_says_so_about_all_of_them() {
        let (tx, _rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        let r = tool()
            .run(
                json!({"questions": [
                    {"question": "a?", "options": [{"label": "x"}]},
                    {"question": "b?", "options": [{"label": "y"}]}
                ]}),
                &ctx,
                &sink(),
            )
            .await;
        assert!(!r.ok);
        let said = r.text();
        assert!(said.contains("answered none of the 2 questions"), "{said}");
    }

    /// One budget for a set, but every question that went unanswered still has to
    /// close in every window. A card that still offers an answer to a tool call that
    /// has already returned is a dead button with a live affordance.
    #[tokio::test]
    async fn a_set_that_ran_out_of_time_closes_the_ones_left_open() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let (etx, mut erx) = mpsc::unbounded_channel::<crate::event::EngineEvent>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: Some(etx),
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        let (idx, iqx) = oneshot::channel::<(u64, u64)>();
        let h = tokio::spawn(async move {
            let (q1, r1) = rx.recv().await.unwrap();
            let (q2, r2) = rx.recv().await.unwrap();
            r1.send(Answer { text: "MD".into(), options: vec![0] }).unwrap();
            // The second stays unanswered: the watcher holds its sender past the
            // budget, which is the situation the timeout has to survive.
            let _ = idx.send((q1.id, q2.id));
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(r2);
        });
        let r = tool()
            .run(
                json!({"questions": [
                    {"question": "Format?", "options": [{"label": "MD"}]},
                    {"question": "Orientation?", "options": [{"label": "portrait"}]}
                ]}),
                &ctx,
                &sink(),
            )
            .await;
        let (answered, lost) = iqx.await.unwrap();
        h.abort();
        assert!(r.ok, "{:?}", r.error);
        assert!(r.text().contains("did not answer this one in time"), "{}", r.text());
        // Two questions, two announcements, each the kind that fits what happened to it:
        // the answered one closes as an answer and the other as an expiry. A client that
        // sees only the expiry would render the settled question as if nobody had replied.
        let first = erx.recv().await.expect("the answered question must announce its answer");
        match first {
            crate::event::EngineEvent::QuestionAnswered { id } => assert_eq!(id, answered),
            other => panic!("the answered question announced {other:?}, not QuestionAnswered"),
        }
        let ev = erx.recv().await.expect("the unanswered question must be closed");
        match ev {
            crate::event::EngineEvent::QuestionExpired { id } => assert_eq!(id, lost),
            other => panic!("expected QuestionExpired, got {other:?}"),
        }
        assert!(erx.try_recv().is_err(), "one announcement per question, and only the unanswered one expires");
    }

    /// A single question keeps the wire shape existing clients render: no `set`, no
    /// ordinal in the restatement. Changing that would be a protocol break dressed up
    /// as a feature.
    #[tokio::test]
    async fn a_single_question_keeps_the_shape_clients_already_handle() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
        };
        let h = tokio::spawn(async move {
            let (q, reply) = rx.recv().await.unwrap();
            assert!(q.set.is_none(), "a lone question must not claim to be part of a set");
            reply.send(Answer { text: "PDF".into(), options: vec![0] }).unwrap();
        });
        let r = tool()
            .run(json!({"question": "Format?", "options": [{"label": "PDF"}]}), &ctx, &sink())
            .await;
        h.await.unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(r.text().starts_with("Question you asked:"), "{}", r.text());
    }

    /// Malformed sets are refused with the reason, not quietly truncated: five
    /// questions in one card is a wall, and a call that mixes both shapes is a call
    /// whose meaning depends on which field the reader noticed.
    #[tokio::test]
    async fn malformed_sets_are_refused_with_a_reason() {
        let ctx = ToolCtx::default();
        let t = tool();
        let one = json!({"question": "a?", "options": [{"label": "x"}]});
        let r = t
            .run(json!({"questions": [one.clone()]}), &ctx, &sink())
            .await;
        assert!(!r.ok && r.text().contains("2-4"), "{}", r.text());

        let many: Vec<Value> = (0..5).map(|i| json!({"question": format!("q{i}?"), "options": [{"label": "x"}]})).collect();
        let r = t.run(json!({"questions": many}), &ctx, &sink()).await;
        assert!(!r.ok && r.text().contains("at most 4"), "{}", r.text());

        let r = t
            .run(json!({"question": "outer?", "options": [{"label": "x"}], "questions": [one.clone(), one.clone()]}), &ctx, &sink())
            .await;
        assert!(!r.ok && r.text().contains("not both"), "{}", r.text());

        // A question inside the set is validated like a lone one: no options, no good.
        let r = t
            .run(json!({"questions": [{"question": "a?"}, {"question": "b?", "options": [{"label": "y"}]}]}), &ctx, &sink())
            .await;
        assert!(!r.ok && r.text().contains("options"), "{}", r.text());
    }

    #[tokio::test]
    async fn an_out_of_range_recommendation_is_ignored() {
        let (tx, mut rx) = mpsc::unbounded_channel::<QuestionRx>();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            max_output_bytes: 4096,
            question_timeout: None,
            ask: Some(tx),
            events: None,
            permits: Vec::new(),
            store: None,
            session: None,
            search_backend: Default::default(),
            exec_cfg: Default::default(),
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

    /// A remote tool host has to know what to ask **before** it runs the call, because
    /// it cannot hold the question open inside a process that is about to exit. It gets
    /// the questions from the tool's own parse, so the card the user sees is the card
    /// this call would have shown — and asking twice must produce the same set, since
    /// the extension asks from one call and the answers come back against the other.
    #[test]
    fn a_remote_host_can_ask_the_questions_the_tool_would_have_asked() {
        let args = json!({
            "questions": [
                { "question": "Which file?", "options": [{ "label": "January" }, { "label": "February" }], "recommended": 0 },
                { "question": "Totals or rows?", "options": [{ "label": "Totals" }], "multi": true, "allow_custom": false }
            ]
        });
        let qs = tool().questions_for(&args).expect("the set form parses");
        assert_eq!(qs.len(), 2);
        let s0 = qs[0].set.as_ref().expect("a set for more than one");
        let s1 = qs[1].set.as_ref().expect("the second question belongs to the same set");
        assert_eq!((s0.index, s0.total), (0, 2));
        assert_eq!((s1.index, s1.total), (1, 2));
        assert_eq!(s0.id, s1.id, "one set id for the whole call, or the card splits");
        assert_eq!(qs[1].options.len(), 1);
        assert!(qs[1].multi, "multi is the model's call, not the host's guess");
        assert!(!qs[1].allow_custom);
        assert_eq!(qs[0].recommended, Some(0));
        // Same call, same shape: the extension asks from one `cwtool questions` run and
        // the answers arrive against a second parse in `cwtool run`.
        let again = tool().questions_for(&args).expect("parsing is repeatable");
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].question, qs[0].question);
        assert_eq!(again[0].set.as_ref().unwrap().total, 2);
        // One question is not a set: that is the wire shape every existing client renders.
        let one = tool().questions_for(&json!({ "question": "Which format?", "options": [{ "label": "PDF" }] })).expect("single form");
        assert_eq!(one.len(), 1);
        assert!(one[0].set.is_none(), "a single question must not grow a set");
        // And a malformed call says so in the tool's own words rather than asking nothing.
        let bad = tool().questions_for(&json!({ "questions": [{ "question": "only one", "options": [{ "label": "x" }] }] }));
        assert!(bad.is_err(), "the set form needs 2-4 entries; ask_user says so itself");
    }
}