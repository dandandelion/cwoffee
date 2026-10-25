//! Naming a conversation, in the conversation's own voice.
//!
//! The old design borrowed a second model: a python helper loaded a small local
//! title model and named the conversation *after* its first turn, in a spawned
//! task, so the name appeared some seconds after the answer had already finished.
//! That whole path is gone — the script, the weights cache, the thread knobs, the
//! memory warning in Settings, and the "it is opt-in because it costs 900 MB"
//! apology. A conversation is now named by the model that is about to answer it,
//! from the first message, before that answer starts.
//!
//! Three rules hold this together:
//!
//!   * **Separate context.** The title request is one user turn and nothing else:
//!     no system prompt, no tools, no conversation history, and its own request.
//!     Whatever it says never enters the conversation, and the conversation's own
//!     first request is untouched by it. The name is therefore paid for once, at
//!     ~40 tokens, and it cannot disturb the prefix cache of the real turn.
//!   * **Stall on purpose.** The first real request waits for the title. That is
//!     the trade the reader asked for: a name that arrives forty seconds later, on
//!     a page that has already moved on, reads as a glitch rather than as a name.
//!     The stall is bounded by a timeout, and a failure is not fatal — the
//!     conversation keeps a name and the answer proceeds.
//!   * **Parse anyway.** We ask for thinking off, but an endpoint decides what it
//!     can do: a chat template may ignore `enable_thinking`, a reasoning model may
//!     answer inside its thinking block and leave `content` empty, and some
//!     servers wrap the answer in a sentence of their own. So the parser takes the
//!     first line that looks like a name from wherever it appeared.
//!
//! Both cores run this file. The pi bridge depends on the engine crate, so the
//! words, the parser and the request shape cannot drift apart between cores.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::provider::{ChatRequest, Delta, Message, Provider, Reasoning};
use crate::store::{derive_title, UNTITLED};

/// The ask, in the reader's own words. Kept as one line so a mock endpoint can
/// match it and a test can assert it, and so nobody "improves" it into a prompt
/// that also asks for a summary, a category, or an explanation.
pub const TITLE_ASK: &str = "Title this conversation in under ~10 words and say nothing more:";

/// How long the first answer may be delayed for a name. The title request is a
/// short completion on the same endpoint that is about to answer, so this is a
/// guard against an endpoint that accepts the connection and says nothing — the
/// case where "stall briefly" would otherwise become "stall forever".
pub const TITLE_BUDGET_MS: u64 = 8_000;

/// A name is not worth an unbounded completion. ~10 words is the ask; 96 tokens
/// leaves room for a model that thinks a little anyway without truncating the name.
const TITLE_MAX_TOKENS: u32 = 96;

/// The cap on a title we will actually paint. The ask says ~10 words; a model
/// that ignores it and writes a paragraph should not widen the sidebar row.
const TITLE_MAX_CHARS: usize = 72;

/// Build the one-line prompt. The conversation's first message is included
/// verbatim rather than summarised: summarising it here would put our reading of
/// the request inside the request. Whitespace is collapsed, so a first message
/// holding a code fence or a blank line still arrives as one ask rather than as a
/// multi-part prompt the model can answer part of.
pub fn title_prompt(first_user: &str) -> String {
    let body = collapse_spaces(first_user);
    format!("{TITLE_ASK} {body}")
}

/// Has this conversation been named yet — meaning: is its title something *we*
/// made up rather than something the reader typed?
///
/// Three spellings count, because three generations of rows exist on real disks:
/// the current placeholder, the lowercase one the store used before it, and the
/// opening-line cut the store used to derive by itself. A title that matches none
/// of them was chosen by a person or by the titler once already, and is never
/// touched again. `derive_title` is public for exactly this question.
pub fn still_untitled(title: &str, first_user: &str) -> bool {
    let t = title.trim();
    if t.is_empty() || t.eq_ignore_ascii_case(UNTITLED) || t.eq_ignore_ascii_case("new session") {
        return true;
    }
    !first_user.trim().is_empty() && t == derive_title(first_user).as_str()
}

/// Turn whatever the model produced into a name, or nothing.
///
/// Ordered by what actually goes wrong against real endpoints:
///   1. a model that thought anyway and put the name in its reasoning block, with
///      `content` empty — so reasoning is a fallback source, not an error;
///   2. a name wearing a preamble ("Title: ...", "This conversation can be
///      titled ...") or quotes or a code fence;
///   3. a name followed by commentary, which we cut at the first line;
///   4. nothing usable at all, which is a miss rather than a failure the reader
///      should be charged for.
pub fn parse_title(text: &str, reasoning: &str) -> Option<String> {
    // `last` decides which line wins. From `content` the first line is the name and
    // anything after it is commentary. From a thinking block it is the other way
    // round: a model that reasons anyway arrives at its title at the end, so the
    // first line of a thinking block is usually "let me consider the request".
    let clean = |src: &str, last: bool| -> Option<String> {
        let lines: Vec<&str> = if last { src.lines().rev().collect() } else { src.lines().collect() };
        for raw in lines {
            let mut line = raw.trim().to_string();
            // A fence marker or a stray thinking tag is chrome, not part of a name.
            for cut in ["```", "<thinking>", "</thinking>", "<think>", "</think>"] {
                line = line.replace(cut, " ");
            }
            while line.starts_with('`') {
                line = line.trim_start_matches('`').trim().to_string();
            }
            line = strip_preamble(&line).to_string();
            line = line
                .trim_matches(|c: char| c == '"' || c == '\'' || c == '\u{201c}' || c == '\u{201d}')
                .to_string();
            line = line
                .trim_end_matches(|c: char| c == '.' || c == ':' || c == ';' || c == ',' || c == '\u{3001}')
                .to_string();
            line = collapse_spaces(&line);
            if line.is_empty() {
                continue;
            }
            return Some(cap_words(&line));
        }
        None
    };
    clean(text, false)
        .or_else(|| clean(reasoning, true))
        .filter(|t| !t.chars().all(|c| c.is_whitespace()))
}

/// Drop the "here is your title" sentences a model puts in front of the title.
fn strip_preamble(line: &str) -> &str {
    let lowered = line.to_lowercase();
    for lead in [
        "this conversation can be titled",
        "this conversation could be titled",
        "the conversation can be titled",
        "a suitable title",
        "suggested title",
        "conversation title",
        "title of this conversation",
        "title for this conversation",
        "title",
    ] {
        if lowered.starts_with(lead) {
            let rest = &line[lead.len()..];
            let rest = rest.trim_start_matches([':', '-', '–', ' ', '"', '\'']);
            return rest.trim();
        }
    }
    line
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keep the ask's promise: about ten words, and never wide enough to reflow the
/// sidebar. Cutting mid-word is avoided; the ellipsis says the name continued.
fn cap_words(s: &str) -> String {
    let words: Vec<&str> = s.split(' ').collect();
    let mut out = String::new();
    let mut n = 0usize;
    for w in words {
        if n >= 10 {
            out.push('\u{2026}');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
        n += 1;
        if out.chars().count() > TITLE_MAX_CHARS {
            // Trim back to whole words that still fit.
            let keep: Vec<&str> = out.split(' ').filter(|x| !x.is_empty()).collect();
            let mut acc = String::new();
            let mut used = 0usize;
            for w in keep {
                let add = w.chars().count() + if acc.is_empty() { 0 } else { 1 };
                if used + add > TITLE_MAX_CHARS {
                    break;
                }
                if !acc.is_empty() {
                    acc.push(' ');
                }
                acc.push_str(w);
                used += add;
            }
            acc.push('\u{2026}');
            return acc;
        }
    }
    out
}

/// Did the title pass produce a name, need the fallback, or should the caller not
/// have asked at all. The distinction matters in the log: "the model named it" and
/// "the model could not, so the opening line did" are different facts about the
/// endpoint, and both are things a person may need to know when a sidebar looks odd.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Named(String),
    /// The ask failed; the caller should fall back to the opening-line cut.
    /// The `String` is why, for the log and for the note the UI may show.
    FellBack(String),
    /// Not eligible — the conversation already has a name a person chose.
    NotEligible,
}

/// Ask the endpoint for a name, streaming what arrives into `on_title` so the UI
/// can type it in rather than swap it.
///
/// `on_title(text, done)` receives the **cumulative** title so far, which makes it
/// idempotent and order-safe: a duplicate or an out-of-order delivery cannot show a
/// shorter name than one already painted, and a reload can settle on the same text.
///
/// The window is bounded by [`TITLE_BUDGET_MS`]. A timeout is a `FellBack`, not an
/// error: the answer to the reader's real question is more important than the name.
pub async fn ask_title<F: FnMut(&str, bool)>(
    provider: &dyn Provider,
    model: &str,
    first_user: &str,
    mut on_title: F,
) -> Outcome {
    if first_user.trim().is_empty() {
        return Outcome::FellBack("the first message is empty".into());
    }
    let mut req = ChatRequest::new(model, vec![Message::user(title_prompt(first_user))]);
    // Thinking off, and no tools: a title needs neither. `Reasoning::Off` is sent
    // in whichever shape this endpoint's chat template reads, by the provider that
    // also answers the real turn — not by a guess made here.
    req.reasoning = Reasoning::Off;
    req.max_tokens = Some(TITLE_MAX_TOKENS);

    let (tx, mut rx) = mpsc::unbounded_channel::<Delta>();
    let stream = provider.stream(&req, &tx);
    tokio::pin!(stream);

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut painted = String::new();
    let mut closed = false;
    let deadline = tokio::time::sleep(Duration::from_millis(TITLE_BUDGET_MS));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            biased;
            _ = &mut deadline, if !closed => {
                // Stop reading; whatever arrived is what we have.
                drop(rx);
                return Outcome::FellBack(format!(
                    "no title inside {} ms",
                    TITLE_BUDGET_MS
                ));
            }
            d = rx.recv(), if !closed => {
                match d {
                    None => { closed = true; }
                    Some(Delta::Text(t)) => {
                        text.push_str(&t);
                        // Paint through the same parser as the settled name, so the
                        // live name never shows a preamble like `Title:` or a second
                        // line of commentary that the settled name will not have.
                        if let Some(cand) = parse_title(&text, "") {
                            if cand != painted {
                                painted = cand;
                                on_title(&painted, false);
                            }
                        }
                    }
                    // A model that thinks anyway is not an error; its thinking is a
                    // second place the name can be. It is not painted live, because
                    // thinking is not a name and typing it into a row would lie.
                    Some(Delta::Reasoning(r)) => reasoning.push_str(&r),
                    Some(Delta::Done(_)) => closed = true,
                    Some(_) => {}
                }
            }
            _ = &mut stream, if !closed => { closed = true; }
        }
        if closed {
            break;
        }
    }
    // Drain anything already buffered before the close marker.
    while let Ok(d) = rx.try_recv() {
        match d {
            Delta::Text(t) => text.push_str(&t),
            Delta::Reasoning(r) => reasoning.push_str(&r),
            _ => {}
        }
    }

    match parse_title(&text, &reasoning) {
        Some(name) => {
            on_title(&name, true);
            Outcome::Named(name)
        }
        None => {
            let why = if text.trim().is_empty() && reasoning.trim().is_empty() {
                "the endpoint returned nothing at all".to_string()
            } else {
                format!(
                    "no usable name in {} chars of answer ({} of thinking)",
                    text.chars().count(),
                    reasoning.chars().count()
                )
            };
            Outcome::FellBack(why)
        }
    }
}

/// One titler at a time, across the process.
///
/// Cheap by design — an `AtomicBool` rather than a slot machine — because the
/// thing being guarded is no longer memory but prompt cost: two conversations
/// started in the same second should not each pay for a title request while the
/// other one is still waiting, and a queue of stalled title requests is a queue of
/// conversations whose first answer has not started.
#[derive(Default)]
pub struct TitleSlot(AtomicBool);

impl TitleSlot {
    pub fn claim(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }
    pub fn release(&self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{DeltaSink, ProviderError};

    #[test]
    fn the_ask_is_one_line_and_names_its_own_budget() {
        let p = title_prompt("  why is my cashflow sheet off by 300 dollars  ");
        assert_eq!(
            p,
            "Title this conversation in under ~10 words and say nothing more: why is my cashflow sheet off by 300 dollars"
        );
        // One line, whatever the message contains: a first message with newlines in
        // it must not turn the ask into a multi-paragraph prompt.
        let multi = title_prompt("first line\n\nsecond   line");
        assert_eq!(multi.lines().count(), 1, "{multi}");
        assert_eq!(multi, "Title this conversation in under ~10 words and say nothing more: first line second line");
    }

    #[test]
    fn a_name_we_made_up_is_eligible_and_a_name_a_person_typed_is_not() {
        let first = "hey can you look at my quarterly cashflow spreadsheet please";
        let cut = derive_title(first);
        assert!(still_untitled(UNTITLED, first));
        assert!(still_untitled("new session", first), "the legacy lowercase placeholder");
        assert!(still_untitled("NEW SESSION", first), "spelling is not a person's choice");
        assert!(still_untitled("", first), "an empty title is unnamed");
        assert!(still_untitled(&cut, first), "the opening-line cut is ours, not theirs");
        assert!(!still_untitled("Quarterly cashflow gap", first), "a chosen name is never touched");
        // The placeholder is still "ours" with nothing to name it from; the empty
        // first message is turned away by `ask_title`, not by this question.
        assert!(still_untitled("New Session", ""));
    }

    #[test]
    fn a_plain_answer_becomes_a_title() {
        assert_eq!(parse_title("Quarterly cashflow discrepancy", "").as_deref(), Some("Quarterly cashflow discrepancy"));
    }

    #[test]
    fn preambles_quotes_and_fences_are_stripped() {
        assert_eq!(parse_title("Title: Cashflow gap of $300", "").as_deref(), Some("Cashflow gap of $300"));
        assert_eq!(
            parse_title("This conversation can be titled: \"Cashflow sheet mismatch\"", "").as_deref(),
            Some("Cashflow sheet mismatch")
        );
        assert_eq!(parse_title("```\nCashflow sheet mismatch\n```", "").as_deref(), Some("Cashflow sheet mismatch"));
    }

    #[test]
    fn commentary_after_the_first_line_is_cut() {
        assert_eq!(
            parse_title("Cashflow sheet mismatch\n\nI titled it that because the numbers differ.", "").as_deref(),
            Some("Cashflow sheet mismatch")
        );
    }

    #[test]
    fn a_name_only_found_in_thinking_still_titles_the_conversation() {
        // An endpoint that ignores enable_thinking=false, answers in its reasoning
        // block, and leaves content empty: the ask still worked.
        assert_eq!(
            parse_title("", "Let me consider the request.\nCashflow sheet mismatch\n").as_deref(),
            Some("Cashflow sheet mismatch")
        );
    }

    #[test]
    fn an_overlong_answer_is_cut_at_the_asks_own_limit() {
        let long = "the quarterly cashflow spreadsheet disagrees with the bank export again and I cannot find the reason";
        let t = parse_title(long, "").unwrap();
        assert!(t.chars().count() <= TITLE_MAX_CHARS + 1, "{t}");
        assert!(t.ends_with('\u{2026}'), "{t}");
        assert!(t.split(' ').count() <= 10, "{t}");
    }

    #[test]
    fn nothing_usable_is_a_miss_rather_than_a_title() {
        assert_eq!(parse_title("", ""), None);
        assert_eq!(parse_title("   \n\n  ", "   "), None);
        assert_eq!(parse_title("... ", "  ...  "), None);
    }

    /// The endpoint the ask reaches, and what it was asked with.
    struct Asker {
        got: std::sync::Mutex<Vec<(usize, usize, Reasoning, String)>>,
        answer: &'static str,
        in_thinking: bool,
    }

    #[async_trait::async_trait]
    impl Provider for Asker {
        fn id(&self) -> &str { "asker" }
        fn label(&self) -> &str { "asker" }
        async fn stream(&self, req: &ChatRequest, sink: &DeltaSink) -> Result<(), ProviderError> {
            self.got.lock().unwrap().push((
                req.messages.len(),
                req.tools.len(),
                req.reasoning,
                req.messages.first().map(|m| m.content.clone()).unwrap_or_default(),
            ));
            if self.in_thinking {
                sink.send(Delta::Reasoning("Let me consider the request.\n".into())).ok();
                sink.send(Delta::Reasoning(self.answer.into())).ok();
            } else {
                for w in self.answer.split(' ') {
                    // With the trailing space, because that is how a chat
                    // completion streams: the space belongs to a token, and a
                    // joiner invented here would be a second parser.
                    sink.send(Delta::Text(format!("{w} "))).ok();
                }
            }
            sink.send(Delta::Done(crate::event::FinishReason::Stop)).ok();
            Ok(())
        }
    }

    /// "Separate context" is a claim about the request, so it is tested as one.
    ///
    /// One user message, no system prompt, no tools, thinking off: the name is paid
    /// for once, and it must not disturb the prefix cache of the turn that answers
    /// the actual question.
    #[tokio::test]
    async fn the_ask_is_one_user_turn_with_no_system_prompt_and_thinking_off() {
        let a = Asker {
            got: std::sync::Mutex::new(vec![]),
            answer: "Cashflow sheet mismatch",
            in_thinking: false,
        };
        let mut painted: Vec<(String, bool)> = vec![];
        let out = ask_title(
            &a,
            "some-model",
            "why is my cashflow sheet off by 300 dollars",
            |t, d| painted.push((t.to_string(), d)),
        )
        .await;
        assert_eq!(out, Outcome::Named("Cashflow sheet mismatch".into()));
        {
            let got = a.got.lock().unwrap();
            assert_eq!(got.len(), 1, "exactly one request for one name");
            let (msgs, tools, reasoning, prompt) = &got[0];
            assert_eq!(*msgs, 1, "the title request carries one message and nothing else");
            assert_eq!(*tools, 0, "a title needs no tools");
            assert_eq!(*reasoning, Reasoning::Off, "thinking off, in the shape this endpoint reads");
            assert!(prompt.starts_with(TITLE_ASK), "{prompt}");
        }
        assert!(painted.iter().any(|(_, d)| *d), "the name must arrive marked final");
        assert_eq!(painted.last().unwrap().0, "Cashflow sheet mismatch");
        // Cumulative, never shorter than what came before.
        let mut prev = 0usize;
        for (t, _) in &painted {
            assert!(t.len() >= prev, "title paint went backwards: {t}");
            prev = t.len();
        }
    }

    /// An endpoint that ignores `enable_thinking=false` and answers inside its
    /// thinking block: the ask still worked, which is the whole reason the parser
    /// reads a second source.
    #[tokio::test]
    async fn a_model_that_thinks_anyway_still_names_the_conversation() {
        let a = Asker {
            got: std::sync::Mutex::new(vec![]),
            answer: "Cashflow sheet mismatch",
            in_thinking: true,
        };
        let out = ask_title(&a, "some-model", "why is my cashflow sheet off", |_t, _d| {}).await;
        assert_eq!(out, Outcome::Named("Cashflow sheet mismatch".into()), "{out:?}");
    }

    #[test]
    fn one_title_request_at_a_time_across_the_process() {
        let slot = TitleSlot::default();
        assert!(slot.claim());
        assert!(!slot.claim(), "a second ask in the same second is turned away, not queued");
        slot.release();
        assert!(slot.claim());
    }
}
