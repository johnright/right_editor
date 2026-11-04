//! `--latency`: input -> nvim flush -> presented frame.
//!
//! Only the first input of a burst is timed; nothing new starts until its
//! frame is out. Whatever flush comes next is assumed to be the answer, which
//! is usually right. "Presented" = handed to the compositor, so add up to one
//! refresh before it's actually on screen.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct FrameTimings {
    pub start: Instant,
    /// Waiting for the swapchain image, i.e. where vsync shows up.
    pub acquire: Duration,
    /// instances, upload, submit, present
    pub render: Duration,
    pub presented: Instant,
}

#[derive(Debug, Clone)]
struct Pending {
    kind: &'static str,
    /// e.g. `<C-x>`, `wheel down`. Useful for tracking down spikes.
    detail: String,
    input: Instant,
    /// (RPC thread got it, window thread got it)
    flushed: Option<(Instant, Instant)>,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    kind: &'static str,
    nvim: f64,
    total: f64,
}

const SUMMARY_EVERY: usize = 20;

#[derive(Default)]
pub struct Latency {
    pending: Option<Pending>,
    samples: Vec<Sample>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Latency {
    pub fn input(&mut self, kind: &'static str, detail: &str) {
        if self.pending.is_none() {
            self.pending = Some(Pending {
                kind,
                detail: detail.to_string(),
                input: Instant::now(),
                flushed: None,
            });
        }
    }

    /// `flushed` is when the RPC thread got it, not us.
    pub fn flushed(&mut self, flushed: Instant) {
        if let Some(p) = &mut self.pending
            && p.flushed.is_none()
        {
            p.flushed = Some((flushed, Instant::now()));
        }
    }

    pub fn presented(&mut self, frame: FrameTimings) {
        let Some((flushed, received)) = self.pending.as_ref().and_then(|p| p.flushed) else {
            return;
        };
        let Some(p) = self.pending.take() else { return };

        let nvim = ms(flushed.saturating_duration_since(p.input));
        let queue = ms(received.saturating_duration_since(flushed));
        let wait = ms(frame.start.saturating_duration_since(received));
        let total = ms(frame.presented.saturating_duration_since(p.input));
        log::info!(
            "latency {} {:?}: {total:.1} ms = nvim {nvim:.1} + queue {queue:.2} + wait {wait:.2} + vsync {:.2} + render {:.2}",
            p.kind,
            p.detail,
            ms(frame.acquire),
            ms(frame.render),
        );
        self.samples.push(Sample {
            kind: p.kind,
            nvim,
            total,
        });
        if self.samples.len().is_multiple_of(SUMMARY_EVERY) {
            self.summary();
        }
    }

    pub fn summary(&self) {
        let mut kinds: Vec<&str> = self.samples.iter().map(|s| s.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        for kind in kinds {
            let pick = |f: fn(&Sample) -> f64| -> Vec<f64> {
                let mut v: Vec<f64> = self
                    .samples
                    .iter()
                    .filter(|s| s.kind == kind)
                    .map(f)
                    .collect();
                v.sort_by(f64::total_cmp);
                v
            };
            let total = pick(|s| s.total);
            let nvim = pick(|s| s.nvim);
            log::info!(
                "latency {kind} ({} samples): total p50 {:.1} p90 {:.1} p99 {:.1} max {:.1} ms | of which nvim p50 {:.1} p90 {:.1} ms",
                total.len(),
                percentile(&total, 0.50),
                percentile(&total, 0.90),
                percentile(&total, 0.99),
                total.last().copied().unwrap_or(0.0),
                percentile(&nvim, 0.50),
                percentile(&nvim, 0.90),
            );
        }
    }
}

/// Nearest-rank, `sorted` must be sorted.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.5), 50.0);
        assert_eq!(percentile(&v, 0.99), 99.0);
        assert_eq!(percentile(&[3.0], 0.9), 3.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }

    #[test]
    fn only_first_input_of_a_burst_is_measured() {
        let mut l = Latency::default();
        l.input("key", "a");
        let first = l.pending.as_ref().unwrap().input;
        l.input("key", "b");
        assert_eq!(l.pending.as_ref().unwrap().input, first);
        assert_eq!(l.pending.as_ref().unwrap().detail, "a");
        let now = Instant::now();
        l.flushed(now);
        l.flushed(now + Duration::from_millis(5));
        assert_eq!(l.pending.as_ref().unwrap().flushed.unwrap().0, now);
        l.presented(FrameTimings {
            start: now,
            acquire: Duration::ZERO,
            render: Duration::ZERO,
            presented: now,
        });
        assert!(l.pending.is_none());
        assert_eq!(l.samples.len(), 1);
    }
}
