//! Adaptive polling policy and the due-time queue.
//!
//! The scheduler is the main lever on "fast **and** cheap": with a fixed
//! interval you either burn bandwidth on feeds that never change, or you miss
//! news. So each source learns its own rhythm:
//!
//! | observed behaviour                    | next interval                        |
//! |---------------------------------------|--------------------------------------|
//! | new items arrived                     | reset to the configured base          |
//! | 200 OK but nothing new                | ×1.25, up to `quiet_factor × base`    |
//! | 304 Not Modified (cheap!)             | ×1.5,  up to `quiet_factor × base`    |
//! | transient error                       | ×2^(consecutive errors), up to `max`  |
//! | `Retry-After` header                  | honoured verbatim                     |
//!
//! Two properties keep a large fleet well-behaved:
//!
//! * **Jitter.** Every wake-up is perturbed by `±jitter_percent` derived from
//!   `(source, sequence)`, so 3 000 feeds never line up on the same second and
//!   the request rate stays smooth instead of bursty.
//! * **Staggered start.** `SourceSpec::initial_delay` spreads the *first* poll
//!   across a whole interval, so launching the app does not fire 3 000 requests
//!   in the first second.
//!
//! The queue itself is a `BinaryHeap` of `(due, sequence)`; rescheduling is
//! lazy — a stale heap entry is dropped when popped and its `due` no longer
//! matches the runtime state.

use crate::model::{Health, SourceId};
use crate::text::hash_pair64;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    due: Instant,
    seq: u64,
    id: SourceId,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.due.cmp(&other.due).then(self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Everything the scheduler remembers about one source between polls.
#[derive(Debug, Clone)]
pub struct SourceRuntime {
    pub id: SourceId,
    pub db_id: i64,
    pub base_interval: Duration,
    pub interval: Duration,
    pub next_due: Instant,
    pub consecutive_ok: u32,
    pub consecutive_err: u32,
    pub health: Health,
    pub latency_ema_ms: f32,
    pub etag: Option<Arc<str>>,
    pub last_modified: Option<Arc<str>>,
    pub last_body_bytes: u64,
    pub last_fetch_unix: i64,
    pub last_status: i32,
    pub last_error: Option<Arc<str>>,
    pub new_items_total: u64,
    pub last_new_items: u32,
    pub requests: u64,
    pub not_modified: u64,
    /// Set when the persisted row needs an update.
    pub dirty: bool,
}

impl SourceRuntime {
    pub fn new(id: SourceId, db_id: i64, base_interval: Duration, first_delay: Duration) -> Self {
        Self {
            id,
            db_id,
            base_interval,
            interval: base_interval,
            next_due: Instant::now() + first_delay,
            consecutive_ok: 0,
            consecutive_err: 0,
            health: Health::Idle,
            latency_ema_ms: 0.0,
            etag: None,
            last_modified: None,
            last_body_bytes: 0,
            last_fetch_unix: 0,
            last_status: 0,
            last_error: None,
            new_items_total: 0,
            last_new_items: 0,
            requests: 0,
            not_modified: 0,
            dirty: false,
        }
    }

    /// Effective interval in whole seconds, for display.
    pub fn effective_secs(&self) -> u64 {
        self.interval.as_secs()
    }

    pub fn secs_until_due(&self, now: Instant) -> i64 {
        if self.next_due <= now {
            0
        } else {
            (self.next_due - now).as_secs() as i64
        }
    }
}

/// What a fetch attempt produced.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// 200 with at least one new item.
    New {
        items: u32,
        latency_ms: u32,
        bytes: u64,
        etag: Option<Arc<str>>,
        last_modified: Option<Arc<str>>,
    },
    /// 200, parsed fine, but nothing we had not already seen.
    Quiet { latency_ms: u32, bytes: u64, etag: Option<Arc<str>>, last_modified: Option<Arc<str>> },
    /// 304 — the cheapest possible outcome.
    NotModified { latency_ms: u32 },
    /// Failure. `retry_after` (seconds) comes from the server when present.
    Error { message: Arc<str>, retry_after: Option<u64>, fatal: bool },
}

impl Outcome {
    pub fn latency_ms(&self) -> u32 {
        match self {
            Outcome::New { latency_ms, .. }
            | Outcome::Quiet { latency_ms, .. }
            | Outcome::NotModified { latency_ms } => *latency_ms,
            Outcome::Error { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub min_interval: Duration,
    pub max_interval: Duration,
    pub quiet_factor: u32,
    pub jitter_percent: u8,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(300),
            max_interval: Duration::from_secs(86_400),
            quiet_factor: 4,
            jitter_percent: 10,
        }
    }
}

pub struct Scheduler {
    heap: BinaryHeap<Reverse<Entry>>,
    seq: u64,
    policy: Policy,
}

impl Scheduler {
    pub fn new(policy: Policy) -> Self {
        Self { heap: BinaryHeap::with_capacity(1024), seq: 0, policy }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    pub fn set_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Queue a wake-up for `state`. Callers keep `state.next_due` authoritative.
    pub fn schedule(&mut self, state: &mut SourceRuntime, delay: Duration) {
        let delay = self.clamp(delay);
        self.push(state, delay);
    }

    /// Queue a wake-up **without** applying the interval floor. The floor
    /// exists to stop feeds being polled too often; the startup stagger and
    /// "refresh now" both intentionally poll sooner than that.
    pub fn schedule_unclamped(&mut self, state: &mut SourceRuntime, delay: Duration) {
        self.push(state, delay);
    }

    /// Push a wake-up without applying the interval floor (used by
    /// `schedule_now`, where the floor would otherwise add a 5-minute delay).
    fn push(&mut self, state: &mut SourceRuntime, delay: Duration) {
        state.next_due = Instant::now() + delay;
        self.seq += 1;
        self.heap.push(Reverse(Entry { due: state.next_due, seq: self.seq, id: state.id }));
    }

    /// Pop the next due source, skipping stale entries left behind by
    /// rescheduling.
    pub fn pop_due(&mut self, now: Instant, states: &[SourceRuntime]) -> Option<SourceId> {
        while let Some(Reverse(entry)) = self.heap.peek() {
            if entry.due > now {
                return None;
            }
            let entry = self.heap.pop().expect("peeked").0;
            let Some(state) = states.get(entry.id as usize) else {
                continue;
            };
            if state.next_due == entry.due {
                return Some(entry.id);
            }
            // Stale: the source was rescheduled or removed after this was pushed.
        }
        None
    }

    /// The earliest due time in the queue. Stale entries (the source was
    /// rescheduled after this one was pushed) are discarded on the way, which
    /// makes this amortised `O(log n)` instead of a full heap scan.
    pub fn next_deadline(&mut self, states: &[SourceRuntime]) -> Option<Instant> {
        while let Some(Reverse(entry)) = self.heap.peek() {
            if states.get(entry.id as usize).is_some_and(|s| s.next_due == entry.due) {
                return Some(entry.due);
            }
            self.heap.pop();
        }
        None
    }

    /// Number of sources whose next poll is due. Recomputed lazily and only
    /// when a snapshot is built (a few times per second at most).
    pub fn due_count(&self, states: &[SourceRuntime], now: Instant) -> usize {
        self.heap
            .iter()
            .filter(|Reverse(e)| {
                e.due <= now && states.get(e.id as usize).is_some_and(|s| s.next_due == e.due)
            })
            .count()
    }

    /// Drop everything queued for `id` (used when a source is removed or
    /// disabled). The heap is rebuilt only when the source was actually dirty.
    pub fn forget(&mut self, id: SourceId) {
        if self.heap.iter().any(|Reverse(e)| e.id == id) {
            let kept: Vec<Reverse<Entry>> =
                std::mem::take(&mut self.heap).into_iter().filter(|Reverse(e)| e.id != id).collect();
            self.heap = kept.into_iter().collect();
        }
    }

    /// Apply the observed outcome, mutate `state`, and schedule the next poll.
    /// Returns the delay until the next attempt.
    pub fn reschedule(&mut self, state: &mut SourceRuntime, outcome: Outcome) -> Duration {
        let quiet_cap = scale(state.base_interval, self.policy.quiet_factor.max(1) as f64);
        let _delay = match outcome {
            Outcome::New { items, latency_ms, bytes, etag, last_modified } => {
                state.health = Health::Ok;
                state.consecutive_ok = state.consecutive_ok.saturating_add(1);
                state.consecutive_err = 0;
                state.last_error = None;
                state.last_status = 200;
                state.last_new_items = items;
                state.new_items_total += items as u64;
                state.last_body_bytes = bytes;
                state.etag = etag;
                state.last_modified = last_modified;
                state.latency_ema_ms = ema(state.latency_ema_ms, latency_ms as f32);
                state.interval = state.base_interval;
                state.base_interval
            }
            Outcome::Quiet { latency_ms, bytes, etag, last_modified } => {
                state.health = Health::Ok;
                state.consecutive_ok = state.consecutive_ok.saturating_add(1);
                state.consecutive_err = 0;
                state.last_error = None;
                state.last_status = 200;
                state.last_new_items = 0;
                state.last_body_bytes = bytes;
                state.etag = etag;
                state.last_modified = last_modified;
                state.latency_ema_ms = ema(state.latency_ema_ms, latency_ms as f32);
                // Nothing new in a feed that did change: creep towards the cap.
                state.interval = scale(state.interval, 1.25).min(quiet_cap);
                state.interval
            }
            Outcome::NotModified { latency_ms } => {
                state.health = Health::NotModified;
                state.consecutive_ok = state.consecutive_ok.saturating_add(1);
                state.consecutive_err = 0;
                state.last_error = None;
                state.last_status = 304;
                state.not_modified += 1;
                state.last_new_items = 0;
                state.latency_ema_ms = ema(state.latency_ema_ms, latency_ms as f32);
                state.interval = scale(state.interval, 1.5).min(quiet_cap);
                state.interval
            }
            Outcome::Error { message, retry_after, fatal } => {
                state.consecutive_err = state.consecutive_err.saturating_add(1);
                state.consecutive_ok = 0;
                state.last_error = Some(message);
                state.health = if fatal && state.consecutive_err >= 6 { Health::Error } else { Health::Backoff };
                state.last_status = 0;
                // Capped so the exponent cannot overflow, but high enough that
                // the configured ceiling is actually reachable.
                let err = state.consecutive_err.min(10);

                // Two ladders, because the two failure modes want opposite
                // things. A dead feed (404, unparseable) should stop being
                // polled hard and fast: `base × 2^n`. A *transient* failure —
                // a timeout, a dropped connection, a 503 — is usually the
                // network or the host having a bad minute, and backing off to
                // two hours on the first blip leaves a whole fleet unread for
                // the rest of the day. Those climb gently from the interval
                // floor: 15 m, 30 m, 1 h, 2 h, then `base × 2^n`.
                let backoff = if fatal {
                    scale(state.base_interval, (1u32 << err) as f64)
                } else if err <= 4 {
                    scale(self.policy.min_interval, (1u32 << (err - 1)) as f64)
                } else {
                    scale(state.base_interval, (1u32 << (err - 4)) as f64)
                };

                match retry_after {
                    Some(secs) => {
                        let hinted = Duration::from_secs(secs.max(5));
                        // Honour the server, but never beyond our own ceiling.
                        state.interval = hinted.max(backoff.min(self.policy.max_interval));
                    }
                    None => state.interval = backoff,
                }
                state.interval
            }
        };
        // The configured ceiling/floor applies to the *learned* interval as
        // well as to the scheduled delay, so a hostile `Retry-After` or a long
        // error streak cannot push a source past `max_interval_secs`.
        state.interval = state.interval.clamp(self.policy.min_interval, self.policy.max_interval);
        state.last_fetch_unix = crate::config::now_unix();
        state.dirty = true;
        let scheduled = self.clamp(self.jitter(state.id, state.interval));
        self.push(state, scheduled);
        scheduled
    }

    /// Force the next poll to happen almost immediately (used by "refresh now"
    /// and by the boot sequence for sources already past their due time).
    pub fn schedule_now(&mut self, state: &mut SourceRuntime) {
        state.health = Health::Fetching;
        self.push(state, Duration::from_millis(1));
    }

    /// Apply the configured ceiling/floor.
    pub fn clamp(&self, delay: Duration) -> Duration {
        delay.clamp(self.policy.min_interval, self.policy.max_interval)
    }

    fn jitter(&self, id: SourceId, delay: Duration) -> Duration {
        let pct = self.policy.jitter_percent as u64;
        if pct == 0 || delay.is_zero() {
            return delay;
        }
        let h = hash_pair64(id as u64, self.seq);
        // Map to [-pct, +pct] in 1/1000 steps.
        let span = 2 * pct * 10;
        let offset = (h % span) as i64 - (pct * 10) as i64;
        let factor = 1000 + offset;
        let scaled = delay.as_millis() as i128 * factor as i128 / 1000;
        Duration::from_millis(scaled.clamp(1, u32::MAX as i128) as u64)
    }
}

#[inline]
fn scale(d: Duration, factor: f64) -> Duration {
    let ms = (d.as_millis() as f64 * factor).min((7 * 24 * 3600 * 1000) as f64);
    Duration::from_millis(ms.max(0.0) as u64)
}

#[inline]
fn ema(prev: f32, sample: f32) -> f32 {
    if prev == 0.0 { sample } else { prev * 0.8 + sample * 0.2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(base_secs: u64) -> SourceRuntime {
        SourceRuntime::new(0, 1, Duration::from_secs(base_secs), Duration::ZERO)
    }

    fn sched() -> Scheduler {
        Scheduler::new(Policy {
            // The floor is exercised separately; tests here drive the learning
            // rules directly.
            min_interval: Duration::ZERO,
            max_interval: Duration::from_secs(86_400),
            quiet_factor: 4,
            jitter_percent: 0,
        })
    }

    #[test]
    fn new_items_reset_to_base() {
        let mut s = sched();
        let mut st = state(600);
        st.interval = Duration::from_secs(1800);
        s.reschedule(
            &mut st,
            Outcome::New { items: 3, latency_ms: 120, bytes: 4096, etag: None, last_modified: None },
        );
        assert_eq!(st.interval, Duration::from_secs(600));
        assert_eq!(st.health, Health::Ok);
        assert_eq!(st.new_items_total, 3);
    }

    #[test]
    fn not_modified_stretches_but_is_capped() {
        let mut s = sched();
        let mut st = state(600);
        for _ in 0..20 {
            s.reschedule(&mut st, Outcome::NotModified { latency_ms: 30 });
        }
        assert_eq!(st.interval, Duration::from_secs(2400), "quiet_factor × base");
    }

    #[test]
    fn errors_back_off_exponentially_and_respect_ceiling() {
        let mut s = sched();
        let mut st = state(600);
        s.reschedule(&mut st, Outcome::Error { message: "boom".into(), retry_after: None, fatal: true });
        assert_eq!(st.interval, Duration::from_secs(1200));
        s.reschedule(&mut st, Outcome::Error { message: "boom".into(), retry_after: None, fatal: true });
        assert_eq!(st.interval, Duration::from_secs(2400));
        for _ in 0..10 {
            s.reschedule(&mut st, Outcome::Error { message: "boom".into(), retry_after: None, fatal: true });
        }
        assert_eq!(st.interval, Duration::from_secs(86_400));
    }

    #[test]
    fn transient_errors_back_off_gently_so_a_fleet_self_heals() {
        let mut s = Scheduler::new(Policy {
            min_interval: Duration::from_secs(300),
            max_interval: Duration::from_secs(86_400),
            quiet_factor: 4,
            jitter_percent: 0,
        });
        let mut st = state(3600);
        let mut seen = Vec::new();
        for _ in 0..9 {
            s.reschedule(
                &mut st,
                Outcome::Error { message: "timed out".into(), retry_after: None, fatal: false },
            );
            seen.push(st.interval.as_secs());
        }
        assert_eq!(
            seen,
            vec![300, 600, 1200, 2400, 7200, 14400, 28800, 57600, 86400],
            "first retry must be the floor, not two hours"
        );
    }

    #[test]
    fn fatal_errors_back_off_hard() {
        let mut s = sched();
        let mut st = state(3600);
        s.reschedule(&mut st, Outcome::Error { message: "404".into(), retry_after: None, fatal: true });
        assert_eq!(st.interval.as_secs(), 7200, "a dead feed should not be retried soon");
    }

    #[test]
    fn retry_after_is_honoured_but_bounded() {
        let mut s = sched();
        let mut st = state(600);
        s.reschedule(
            &mut st,
            Outcome::Error { message: "429".into(), retry_after: Some(3600), fatal: false },
        );
        assert_eq!(st.interval, Duration::from_secs(3600));
        let mut st2 = state(600);
        s.reschedule(
            &mut st2,
            Outcome::Error { message: "429".into(), retry_after: Some(999_999), fatal: false },
        );
        assert_eq!(st2.interval, Duration::from_secs(86_400));
    }

    #[test]
    fn heap_yields_in_due_order_and_skips_stale() {
        let mut s = sched();
        let mut a = state(600);
        let mut b = state(600);
        b.id = 1;
        s.schedule(&mut a, Duration::ZERO);
        s.schedule(&mut b, Duration::ZERO);
        // reschedule a later; its first heap entry becomes stale
        s.schedule(&mut a, Duration::from_secs(3000));
        let now = Instant::now();
        let states = [a, b];
        assert_eq!(s.pop_due(now, &states), Some(1));
        assert_eq!(s.pop_due(now, &states), None, "a was pushed into the future");
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let mut s = Scheduler::new(Policy { jitter_percent: 10, ..Policy::default() });
        let mut st = state(600);
        let mut d = Duration::ZERO;
        for _ in 0..500 {
            d = s.reschedule(&mut st, Outcome::NotModified { latency_ms: 10 });
        }
        // Saturated at quiet_factor × base = 2400 s, ±10 % jitter.
        let secs = d.as_secs_f64();
        assert!((2159.0..=2641.0).contains(&secs), "jittered delay {secs} out of range");
    }
}
