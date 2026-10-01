//! Telemetry for the status bar (PLAN.md §7).
//!
//! Two rules that are easy to get wrong and matter:
//!
//! 1. llama.cpp reports **per-request aggregates only** (`timings`), so any
//!    "current" rate must be measured client-side from delta arrival times.
//! 2. Prefill has **two** rates that must never be merged:
//!      - *effective* = prompt_tokens / ttft  → what the user experiences; huge
//!        on cache hits
//!      - *raw*       = timings.prompt_per_second → the true compute rate, which
//!        is the number that warns you before a slow turn
//!    Showing only the effective number is accidentally dishonest, and
//!    local-model users will catch it within a day.

use std::collections::VecDeque;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Per-turn telemetry: server aggregates plus client-measured TTFT.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTelemetry {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,

    /// Client-measured: request sent -> first delta arrived.
    pub ttft_ms: u64,

    /// `timings.prompt_per_second` — raw compute rate.
    pub server_prefill_tps: f64,
    /// `timings.predicted_per_second` — includes speculative-decode gains.
    pub server_decode_tps: f64,

    /// Speculative decoding counters; `accepted / n` is the acceptance rate.
    pub draft_n: u64,
    pub draft_accepted: u64,
}

impl TurnTelemetry {
    /// What the user actually waited for, per prompt token.
    pub fn effective_prefill_tps(&self) -> f64 {
        if self.ttft_ms == 0 || self.prompt_tokens == 0 {
            return 0.0;
        }
        self.prompt_tokens as f64 / (self.ttft_ms as f64 / 1000.0)
    }

    /// Raw compute rate as reported by the server.
    pub fn raw_prefill_tps(&self) -> f64 {
        self.server_prefill_tps
    }

    pub fn decode_tps(&self) -> f64 {
        self.server_decode_tps
    }

    pub fn cache_hit_pct(&self) -> f64 {
        if self.prompt_tokens == 0 {
            return 0.0;
        }
        100.0 * self.cached_tokens as f64 / self.prompt_tokens as f64
    }

    pub fn spec_accept_pct(&self) -> f64 {
        if self.draft_n == 0 {
            return 0.0;
        }
        100.0 * self.draft_accepted as f64 / self.draft_n as f64
    }
}

/// Live rates, measured from delta arrival times over a sliding window.
///
/// There is no such thing as a "live prefill rate" from the server's point of
/// view — prefill is not streamed. We approximate it as the arrival rate of the
/// first deltas after TTFT is known, which is what the user perceives.
#[derive(Debug, Clone)]
pub struct LiveWindow {
    /// (arrival instant) for each token in the window
    marks: VecDeque<Instant>,
    window: std::time::Duration,
}

impl LiveWindow {
    pub fn new(window: std::time::Duration) -> Self {
        Self { marks: VecDeque::new(), window }
    }

    pub fn reset(&mut self) {
        self.marks.clear();
    }

    /// Record one arriving token.
    pub fn mark(&mut self, at: Instant) {
        self.marks.push_back(at);
        self.trim(at);
    }

    fn trim(&mut self, now: Instant) {
        while let Some(front) = self.marks.front() {
            if now.duration_since(*front) > self.window {
                self.marks.pop_front();
            } else {
                break;
            }
        }
    }

    /// Instantaneous decode rate over the window. 0 until we have a span.
    pub fn decode_tps(&self) -> f64 {
        let (Some(front), Some(back)) = (self.marks.front(), self.marks.back()) else {
            return 0.0;
        };
        let span = back.duration_since(*front).as_secs_f64();
        if span <= 0.0 || self.marks.len() < 2 {
            return 0.0;
        }
        // tokens in the window = gaps between marks
        (self.marks.len() - 1) as f64 / span
    }
}

impl Default for LiveWindow {
    fn default() -> Self {
        Self::new(std::time::Duration::from_millis(1000))
    }
}

/// Session rolling averages: mean over the **last N turns**, not lifetime.
///
/// Turn 1 of a session is cold-cache and drags the averages down badly, so it is
/// excluded by default (PLAN.md §7).
#[derive(Debug, Clone)]
pub struct Rolling {
    turns: VecDeque<TurnTelemetry>,
    max: usize,
}

impl Rolling {
    pub fn new(max: usize) -> Self {
        Self { turns: VecDeque::new(), max: max.max(1) }
    }

    pub fn push(&mut self, t: TurnTelemetry) {
        self.turns.push_back(t);
        while self.turns.len() > self.max {
            self.turns.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    fn mean(&self, f: impl Fn(&TurnTelemetry) -> f64) -> f64 {
        if self.turns.is_empty() {
            return 0.0;
        }
        self.turns.iter().map(&f).sum::<f64>() / self.turns.len() as f64
    }

    pub fn avg_decode_tps(&self) -> f64 {
        self.mean(|t| t.decode_tps())
    }

    pub fn avg_raw_prefill_tps(&self) -> f64 {
        self.mean(|t| t.raw_prefill_tps())
    }

    pub fn avg_effective_prefill_tps(&self) -> f64 {
        self.mean(|t| t.effective_prefill_tps())
    }

    pub fn avg_cache_hit_pct(&self) -> f64 {
        self.mean(|t| t.cache_hit_pct())
    }

    pub fn avg_spec_accept_pct(&self) -> f64 {
        self.mean(|t| t.spec_accept_pct())
    }
}

impl Default for Rolling {
    fn default() -> Self {
        Self::new(10)
    }
}

/// Snapshot for the status bar. Cheap to clone, sent as one event per UI frame
/// at most (never per token).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySnapshot {
    pub cache_hit_pct: f64,
    pub cached_tokens: u64,
    pub prompt_tokens: u64,

    pub current_prefill_tps: f64,
    pub avg_prefill_tps: f64,

    pub current_decode_tps: f64,
    pub avg_decode_tps: f64,

    pub spec_accept_pct: f64,
    pub context_used: u64,
    pub context_max: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(prompt: u64, cached: u64, ttft_ms: u64, prefill: f64, decode: f64) -> TurnTelemetry {
        TurnTelemetry {
            prompt_tokens: prompt,
            cached_tokens: cached,
            completion_tokens: 100,
            ttft_ms,
            server_prefill_tps: prefill,
            server_decode_tps: decode,
            draft_n: 50,
            draft_accepted: 40,
        }
    }

    #[test]
    fn effective_prefill_uses_ttft() {
        // 1000 prompt tokens in 500ms -> 2000 t/s effective
        let x = t(1000, 0, 500, 40.0, 50.0);
        assert!((x.effective_prefill_tps() - 2000.0).abs() < 1e-6);
        // raw stays at the server's number
        assert!((x.raw_prefill_tps() - 40.0).abs() < 1e-6);
    }

    #[test]
    fn cache_hit_percentage() {
        let x = t(308, 303, 10, 40.0, 50.0);
        assert!((x.cache_hit_pct() - (303.0 / 308.0 * 100.0)).abs() < 1e-6);
    }

    #[test]
    fn zero_guards_do_not_divide_by_zero() {
        let z = TurnTelemetry::default();
        assert_eq!(z.effective_prefill_tps(), 0.0);
        assert_eq!(z.cache_hit_pct(), 0.0);
        assert_eq!(z.spec_accept_pct(), 0.0);
    }

    #[test]
    fn rolling_window_evicts_oldest() {
        let mut r = Rolling::new(3);
        for i in 0..5 {
            r.push(t(100, 0, 100, 40.0, 50.0 + i as f64));
        }
        assert_eq!(r.len(), 3);
        // last three decodes: 52, 53, 54 -> mean 53
        assert!((r.avg_decode_tps() - 53.0).abs() < 1e-6);
    }

    #[test]
    fn live_window_counts_gaps() {
        let mut w = LiveWindow::new(std::time::Duration::from_millis(1000));
        assert_eq!(w.decode_tps(), 0.0);
        let now = Instant::now();
        // 11 marks spaced 100ms apart -> 10 gaps over ~1s -> ~10 t/s
        for i in 0..11 {
            w.mark(now + std::time::Duration::from_millis(i * 100));
        }
        let rate = w.decode_tps();
        assert!(rate > 9.0 && rate < 11.0, "rate was {rate}");
    }
}
