//! An adaptive rate limiter.
//!
//! SafeTrade publishes no rate limit. Peatio answers 429 when one is breached,
//! but the thresholds are set per deployment by the operator, so the only way
//! to know the ceiling is to find it and stay under it.
//!
//! This is AIMD, the same rule TCP uses for congestion: creep upward while
//! things are fine, drop hard the moment they are not. The asymmetry is the
//! point. Probing upward slowly costs a little throughput; probing downward
//! slowly costs a stream of rejected requests and, on an exchange, an unhappy
//! counterparty. So the increase is a few percent per stretch of successes and
//! the decrease is a halving on the first refusal.
//!
//! The learned rate is worth persisting: a deploy should not have to rediscover
//! the ceiling, and rediscovering it means bouncing off it again.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Never exceed this, whatever the probing suggests. A market maker that
/// quotes passively on three thin books has no use for more, and being the
/// loudest client on a small exchange is not a good way to stay welcome.
const MAX_RATE: f64 = 4.0;
/// Never fall below this, or a spell of 429s would stall the bot entirely and
/// leave orders resting with nothing watching them.
const MIN_RATE: f64 = 0.3;
/// Successes before nudging the rate up.
const PROBE_AFTER: u32 = 50;
/// How much to nudge up, and how hard to cut back.
const INCREASE: f64 = 1.05;
const DECREASE: f64 = 0.5;
/// Burst allowance, so a loop's handful of calls goes out together rather than
/// being dribbled one per interval.
const BURST: f64 = 4.0;

#[derive(Debug)]
struct State {
    rate: f64,
    allowance: f64,
    last: Instant,
    successes: u32,
    limited_at: Option<Instant>,
    total_limited: u64,
}

pub struct RateLimiter {
    state: Mutex<State>,
}

impl RateLimiter {
    pub fn new(initial: f64) -> Self {
        RateLimiter {
            state: Mutex::new(State {
                rate: initial.clamp(MIN_RATE, MAX_RATE),
                allowance: BURST,
                last: Instant::now(),
                successes: 0,
                limited_at: None,
                total_limited: 0,
            }),
        }
    }

    /// Wait until a request may be sent.
    pub async fn acquire(&self) {
        let wait = {
            let mut s = self.state.lock().await;
            let now = Instant::now();
            s.allowance = (s.allowance + now.duration_since(s.last).as_secs_f64() * s.rate)
                .min(BURST);
            s.last = now;

            if s.allowance >= 1.0 {
                s.allowance -= 1.0;
                Duration::ZERO
            } else {
                // Take the token now and pay for it by sleeping, so concurrent
                // callers queue instead of all deciding the bucket is empty.
                let deficit = 1.0 - s.allowance;
                s.allowance = 0.0;
                Duration::from_secs_f64(deficit / s.rate)
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// A request came back fine.
    pub async fn on_success(&self) {
        let mut s = self.state.lock().await;
        s.successes += 1;
        if s.successes >= PROBE_AFTER {
            s.successes = 0;
            // Do not climb again straight after being refused; let the window
            // the exchange is measuring roll past first.
            let recently_limited = s
                .limited_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(60));
            if !recently_limited {
                s.rate = (s.rate * INCREASE).min(MAX_RATE);
            }
        }
    }

    /// A request was refused for being too frequent.
    pub async fn on_limited(&self) {
        let mut s = self.state.lock().await;
        s.rate = (s.rate * DECREASE).max(MIN_RATE);
        s.successes = 0;
        s.limited_at = Some(Instant::now());
        s.total_limited += 1;
        // Empty the bucket too, so the next call waits rather than spending a
        // token that was granted under the old, wrong estimate.
        s.allowance = 0.0;
    }

    pub async fn set_rate(&self, rate: f64) {
        let mut s = self.state.lock().await;
        s.rate = rate.clamp(MIN_RATE, MAX_RATE);
    }

    pub async fn rate(&self) -> f64 {
        self.state.lock().await.rate
    }

    pub async fn times_limited(&self) -> u64 {
        self.state.lock().await.total_limited
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_refusal_halves_the_rate_immediately() {
        let rl = RateLimiter::new(2.0);
        rl.on_limited().await;
        assert_eq!(rl.rate().await, 1.0);
        rl.on_limited().await;
        assert_eq!(rl.rate().await, 0.5);
    }

    #[tokio::test]
    async fn the_rate_never_leaves_its_bounds() {
        let rl = RateLimiter::new(999.0);
        assert_eq!(rl.rate().await, MAX_RATE, "clamped on the way in");
        for _ in 0..50 {
            rl.on_limited().await;
        }
        assert_eq!(rl.rate().await, MIN_RATE, "never stalls completely");
    }

    #[tokio::test]
    async fn success_creeps_upward_only_after_a_long_run() {
        let rl = RateLimiter::new(1.0);
        for _ in 0..(PROBE_AFTER - 1) {
            rl.on_success().await;
        }
        assert_eq!(rl.rate().await, 1.0, "one short of the threshold, unchanged");
        rl.on_success().await;
        assert!(rl.rate().await > 1.0, "and then a small step up");
        assert!(rl.rate().await < 1.1, "a small one");
    }

    /// Recovery must not begin the instant we are refused, or we climb straight
    /// back into the window the exchange is still measuring.
    #[tokio::test]
    async fn it_does_not_climb_again_right_after_a_refusal() {
        let rl = RateLimiter::new(2.0);
        rl.on_limited().await;
        let after = rl.rate().await;
        for _ in 0..(PROBE_AFTER * 2) {
            rl.on_success().await;
        }
        assert_eq!(rl.rate().await, after, "held back while the refusal is fresh");
    }

    #[tokio::test]
    async fn acquire_spends_the_burst_then_paces() {
        let rl = RateLimiter::new(100.0); // clamped to MAX_RATE
        let start = Instant::now();
        for _ in 0..(BURST as usize) {
            rl.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(50), "burst goes straight out");
        rl.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(100), "then it paces");
    }
}
