//! This module contains [`TokenBucket`], which provides ability to limit
//! rate of certain events, while allowing bursts through.
//! [`KeyedRateLimiter`] allows to rate-limit multiple keyed items, such
//! as connections.
use {
    cfg_if::cfg_if,
    solana_svm_type_overrides::sync::atomic::{AtomicU64, Ordering},
    std::time::Instant,
};

/// Enforces a rate limit on the volume of requests per unit time.
///
/// Instances update the amount of tokens upon access, and thus does not need to
/// be constantly polled to refill. Uses atomics internally so should be
/// relatively cheap to access from many threads
pub struct TokenBucket {
    new_tokens_per_us: f64,
    max_tokens: u64,
    /// bucket creation
    base_time: Instant,
    tokens: AtomicU64,
    /// time of last update in us since base_time
    last_update: AtomicU64,
    /// time unused in last token creation round
    credit_time_us: AtomicU64,
}

#[cfg(feature = "shuttle-test")]
static TIME_US: AtomicU64 = AtomicU64::new(0); //used to override Instant::now()

// If changing this impl, make sure to run benches and ensure they do not panic.
// much of the testing is impossible outside of real multithreading in release mode.
impl TokenBucket {
    /// Allocate a new TokenBucket
    pub fn new(initial_tokens: u64, max_tokens: u64, new_tokens_per_second: f64) -> Self {
        assert!(
            new_tokens_per_second > 0.0,
            "Token bucket can not have zero influx rate"
        );
        assert!(
            initial_tokens <= max_tokens,
            "Can not have more initial tokens than max tokens"
        );
        let base_time = Instant::now();
        TokenBucket {
            // recompute into us to avoid FP division on every update
            new_tokens_per_us: new_tokens_per_second / 1e6,
            max_tokens,
            tokens: AtomicU64::new(initial_tokens),
            last_update: AtomicU64::new(0),
            base_time,
            credit_time_us: AtomicU64::new(0),
        }
    }

    /// Return current amount of tokens in the bucket.
    /// This may be somewhat inconsistent across threads
    /// due to Relaxed atomics.
    #[inline]
    pub fn current_tokens(&self) -> u64 {
        let now = self.time_us();
        self.update_state(now);
        self.tokens.load(Ordering::Relaxed)
    }

    /// Attempts to consume tokens from bucket.
    ///
    /// On success, returns Ok(amount of tokens left in the bucket).
    /// On failure, returns Err(amount of tokens missing to fill request).
    #[inline]
    pub fn consume_tokens(&self, request_size: u64) -> Result<u64, u64> {
        let now = self.time_us();
        self.update_state(now);
        match self.tokens.fetch_update(
            Ordering::AcqRel,  // winner publishes new amount
            Ordering::Acquire, // everyone observed correct number
            |tokens| {
                if tokens >= request_size {
                    Some(tokens.saturating_sub(request_size))
                } else {
                    None
                }
            },
        ) {
            Ok(prev) => Ok(prev.saturating_sub(request_size)),
            Err(prev) => Err(request_size.saturating_sub(prev)),
        }
    }

    /// Retrieves monotonic time since bucket creation.
    fn time_us(&self) -> u64 {
        cfg_if! {
            if #[cfg(feature="shuttle-test")] {
                TIME_US.load(Ordering::Relaxed)
            } else {
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(self.base_time);
                elapsed.as_micros() as u64
            }
        }
    }

    /// Updates internal state of the bucket by
    /// depositing new tokens (if appropriate)
    fn update_state(&self, now: u64) {
        // fetch last update time
        let last = self.last_update.load(Ordering::SeqCst);

        // If time has not advanced, nothing to do.
        if now <= last {
            return;
        }

        // Try to claim the interval [last, now].
        // If we can not claim it, someone else will claim [last..some other time] when they
        // touch the bucket.
        // If we can claim interval [last, now], no other thread can credit tokens for it anymore.
        // If [last, now] is too short to mint any tokens, spare time will be preserved in credit_time_us.
        match self.last_update.compare_exchange(
            last,
            now,
            Ordering::AcqRel,  // winner publishes new timestamp
            Ordering::Acquire, // loser observes updates
        ) {
            Ok(_) => {
                // This thread won the race and is responsible for minting tokens
                let elapsed = now.saturating_sub(last);

                // also add leftovers from previous conversion attempts.
                // we do not care about who uses the spare_time_us, so relaxed is ok here.
                let elapsed =
                    elapsed.saturating_add(self.credit_time_us.swap(0, Ordering::Relaxed));

                let new_tokens_f64 = elapsed as f64 * self.new_tokens_per_us;

                // amount of full tokens to be minted
                let new_tokens = new_tokens_f64.floor() as u64;

                let time_to_return = if new_tokens >= 1 {
                    // Credit tokens, saturating at max_tokens
                    let _ = self.tokens.fetch_update(
                        Ordering::AcqRel,  // writer publishes new amount
                        Ordering::Acquire, //we fetch the correct amount
                        |tokens| Some(tokens.saturating_add(new_tokens).min(self.max_tokens)),
                    );
                    // Fractional remainder of elapsed time (not enough to mint a whole token)
                    // that will be credited to other minters
                    (new_tokens_f64.fract() / self.new_tokens_per_us) as u64
                } else {
                    // No whole tokens minted → return whole interval
                    elapsed
                };
                // Save unused elapsed time for other threads
                self.credit_time_us
                    .fetch_add(time_to_return, Ordering::Relaxed);
            }
            Err(_) => {
                // Another thread advanced last_update first → nothing we can do now.
            }
        }
    }
}

impl Clone for TokenBucket {
    /// Clones the TokenBucket with approximate state
    /// of the original. While this will never return an object in an
    /// invalid state, using this in a contended environment is not recommended.
    fn clone(&self) -> Self {
        Self {
            new_tokens_per_us: self.new_tokens_per_us,
            max_tokens: self.max_tokens,
            base_time: self.base_time,
            tokens: AtomicU64::new(self.tokens.load(Ordering::Relaxed)),
            last_update: AtomicU64::new(self.last_update.load(Ordering::Relaxed)),
            credit_time_us: AtomicU64::new(self.credit_time_us.load(Ordering::Relaxed)),
        }
    }
}

#[cfg(feature = "keyed_rate_limiter")]
pub mod keyed_rate_limiter;
#[cfg(feature = "keyed_rate_limiter")]
pub use keyed_rate_limiter::KeyedRateLimiter;

#[cfg(test)]
pub mod test {
    use {super::*, solana_svm_type_overrides::thread, std::time::Duration};

    #[test]
    fn test_token_bucket() {
        let tb = TokenBucket::new(100, 100, 1000.0);
        assert_eq!(tb.current_tokens(), 100);
        tb.consume_tokens(50).expect("Bucket is initially full");
        tb.consume_tokens(50)
            .expect("We should still have >50 tokens left");
        tb.consume_tokens(50)
            .expect_err("There should not be enough tokens now");
        thread::sleep(Duration::from_millis(50));
        assert!(
            tb.current_tokens() > 40,
            "We should be refilling at ~1 token per millisecond"
        );
        assert!(
            tb.current_tokens() < 70,
            "We should be refilling at ~1 token per millisecond"
        );
        tb.consume_tokens(40)
            .expect("Bucket should have enough for another request now");
        thread::sleep(Duration::from_millis(120));
        assert_eq!(tb.current_tokens(), 100, "Bucket should not overfill");
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_test_token_bucket_race() {
        use shuttle::sync::atomic::AtomicBool;
        shuttle::check_random(
            || {
                TIME_US.store(0, Ordering::SeqCst);
                let test_duration_us = 2500;
                let run: &AtomicBool = Box::leak(Box::new(AtomicBool::new(true)));
                let tb: &TokenBucket = Box::leak(Box::new(TokenBucket::new(10, 20, 5000.0)));

                // time advancement thread
                let time_advancer = thread::spawn(move || {
                    let mut current_time = 0;
                    while current_time < test_duration_us && run.load(Ordering::SeqCst) {
                        let increment = 100; // microseconds
                        current_time += increment;
                        TIME_US.store(current_time, Ordering::SeqCst);
                        shuttle::thread::yield_now();
                    }
                    run.store(false, Ordering::SeqCst);
                });

                let threads: Vec<_> = (0..2)
                    .map(|_| {
                        thread::spawn(move || {
                            let mut total = 0;
                            while run.load(Ordering::SeqCst) {
                                if tb.consume_tokens(5).is_ok() {
                                    total += 1;
                                }
                                shuttle::thread::yield_now();
                            }
                            total
                        })
                    })
                    .collect();

                time_advancer.join().unwrap();
                let received = threads.into_iter().map(|t| t.join().unwrap()).sum();

                // Initial tokens: 10, refill rate: 5000 tokens/sec (5 tokens/ms)
                // In 2ms: 10 + (5 * 2) = 20 tokens total
                // Each consumption: 5 tokens → 4 total consumptions expected
                assert_eq!(4, received);
            },
            100,
        );
    }
}
