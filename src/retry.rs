//! Retry policy with exponential backoff and jitter.
//!
//! Provides two execution modes:
//!
//! - [`RetryPolicy::call`] — retries when the error implements [`Retryable`]
//!   and `is_retryable()` returns `true`.
//! - [`RetryPolicy::call_when`] — retries based on a caller-supplied predicate,
//!   no trait bound required.
//!
//! Delays grow exponentially up to `max_delay`, with optional random jitter to
//! avoid thundering-herd effects on shared backends.

use std::future::Future;
use std::time::Duration;

use rand::Rng;
use tokio::time::sleep;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Retryable trait
// ---------------------------------------------------------------------------

/// Marker trait for errors that know whether they can be retried.
///
/// Implement this on your error types so [`RetryPolicy::call`] can decide
/// automatically.
///
/// ```
/// use sentinel_rs::Retryable;
///
/// #[derive(Debug)]
/// enum ApiError {
///     Timeout,
///     BadRequest,
/// }
///
/// impl Retryable for ApiError {
///     fn is_retryable(&self) -> bool {
///         matches!(self, Self::Timeout)
///     }
/// }
/// # impl std::fmt::Display for ApiError {
/// #     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
/// #         write!(f, "{:?}", self)
/// #     }
/// # }
/// ```
pub trait Retryable {
    fn is_retryable(&self) -> bool;
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for [`RetryPolicy`].
///
/// ```
/// use sentinel_rs::RetryConfig;
/// use std::time::Duration;
///
/// let cfg = RetryConfig::new()
///     .max_attempts(5)
///     .initial_delay(Duration::from_millis(200))
///     .max_delay(Duration::from_secs(30))
///     .multiplier(2.0)
///     .jitter(0.15);
/// ```
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub(crate) max_attempts: usize,
    pub(crate) initial_delay: Duration,
    pub(crate) max_delay: Duration,
    pub(crate) multiplier: f64,
    pub(crate) jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            multiplier: 2.0,
            jitter: 0.1,
        }
    }
}

impl RetryConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Maximum number of attempts (including the first). Must be >= 1.
    pub fn max_attempts(mut self, n: usize) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Base delay before the first retry.
    pub fn initial_delay(mut self, d: Duration) -> Self {
        self.initial_delay = d;
        self
    }

    /// Upper bound on the computed delay.
    pub fn max_delay(mut self, d: Duration) -> Self {
        self.max_delay = d;
        self
    }

    /// Exponential growth factor applied after each attempt.
    pub fn multiplier(mut self, m: f64) -> Self {
        self.multiplier = m;
        self
    }

    /// Fraction of the delay added as random jitter (0.0–1.0).
    pub fn jitter(mut self, j: f64) -> Self {
        self.jitter = j.clamp(0.0, 1.0);
        self
    }

    /// Compute the delay for a zero-indexed retry attempt.
    fn delay_for(&self, attempt: usize) -> Duration {
        let base = self.initial_delay.as_millis() as f64 * self.multiplier.powi(attempt as i32);
        let capped = base.min(self.max_delay.as_millis() as f64);

        let jitter_range = capped * self.jitter;
        let offset = if jitter_range > 0.0 {
            rand::thread_rng().gen_range(-jitter_range..=jitter_range)
        } else {
            0.0
        };

        Duration::from_millis((capped + offset).max(0.0) as u64)
    }
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Executes an async operation with automatic retries.
///
/// ```rust,no_run
/// use sentinel_rs::{RetryPolicy, RetryConfig, Retryable};
/// use std::time::Duration;
///
/// # #[derive(Debug)]
/// # struct MyError;
/// # impl std::fmt::Display for MyError {
/// #     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "err") }
/// # }
/// # impl Retryable for MyError { fn is_retryable(&self) -> bool { true } }
/// # async fn demo() -> Result<(), MyError> {
/// let policy = RetryPolicy::new(
///     RetryConfig::new()
///         .max_attempts(4)
///         .initial_delay(Duration::from_millis(50)),
/// );
///
/// let value = policy.call(|| async {
///     Ok::<_, MyError>(42)
/// }).await?;
/// # Ok(())
/// # }
/// ```
pub struct RetryPolicy {
    config: RetryConfig,
}

impl RetryPolicy {
    pub fn new(config: RetryConfig) -> Self {
        Self { config }
    }

    /// Execute `op`, retrying automatically when the error is
    /// [`Retryable::is_retryable`].
    pub async fn call<F, Fut, T, E>(&self, mut op: F) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Retryable + std::fmt::Display,
    {
        let mut attempt = 0;

        loop {
            match op().await {
                Ok(val) => {
                    if attempt > 0 {
                        debug!(retries = attempt, "operation succeeded after retries");
                    }
                    return Ok(val);
                }
                Err(err) => {
                    attempt += 1;

                    if !err.is_retryable() {
                        debug!(error = %err, "error is not retryable");
                        return Err(err);
                    }

                    if attempt >= self.config.max_attempts {
                        warn!(
                            attempts = self.config.max_attempts,
                            error = %err,
                            "all attempts exhausted"
                        );
                        return Err(err);
                    }

                    let delay = self.config.delay_for(attempt - 1);
                    debug!(
                        attempt,
                        max = self.config.max_attempts,
                        error = %err,
                        delay_ms = delay.as_millis() as u64,
                        "retrying"
                    );
                    sleep(delay).await;
                }
            }
        }
    }

    /// Execute `op`, retrying when `should_retry` returns `true`.
    ///
    /// Use this when you can't (or don't want to) implement [`Retryable`] on
    /// the error type.
    pub async fn call_when<F, Fut, T, E, P>(&self, mut op: F, mut should_retry: P) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
        P: FnMut(&E) -> bool,
    {
        let mut attempt = 0;

        loop {
            match op().await {
                Ok(val) => {
                    if attempt > 0 {
                        debug!(retries = attempt, "operation succeeded after retries");
                    }
                    return Ok(val);
                }
                Err(err) => {
                    attempt += 1;

                    if !should_retry(&err) {
                        debug!(error = %err, "retry predicate returned false");
                        return Err(err);
                    }

                    if attempt >= self.config.max_attempts {
                        warn!(
                            attempts = self.config.max_attempts,
                            error = %err,
                            "all attempts exhausted"
                        );
                        return Err(err);
                    }

                    let delay = self.config.delay_for(attempt - 1);
                    debug!(
                        attempt,
                        max = self.config.max_attempts,
                        error = %err,
                        delay_ms = delay.as_millis() as u64,
                        "retrying"
                    );
                    sleep(delay).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug)]
    struct TestError {
        retryable: bool,
    }

    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test error (retryable={})", self.retryable)
        }
    }

    impl std::error::Error for TestError {}

    impl Retryable for TestError {
        fn is_retryable(&self) -> bool {
            self.retryable
        }
    }

    fn fast_policy(attempts: usize) -> RetryPolicy {
        RetryPolicy::new(
            RetryConfig::new()
                .max_attempts(attempts)
                .initial_delay(Duration::from_millis(5))
                .jitter(0.0),
        )
    }

    #[tokio::test]
    async fn succeeds_immediately() {
        let policy = fast_policy(3);
        let result = policy.call(|| async { Ok::<_, TestError>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let counter = Arc::new(AtomicUsize::new(0));
        let policy = fast_policy(5);

        let c = Arc::clone(&counter);
        let result = policy
            .call(move || {
                let c = Arc::clone(&c);
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(TestError { retryable: true })
                    } else {
                        Ok(99)
                    }
                }
            })
            .await;

        assert_eq!(result.unwrap(), 99);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn stops_on_non_retryable() {
        let counter = Arc::new(AtomicUsize::new(0));
        let policy = fast_policy(5);

        let c = Arc::clone(&counter);
        let result = policy
            .call(move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<i32, _>(TestError { retryable: false })
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_attempts() {
        let counter = Arc::new(AtomicUsize::new(0));
        let policy = fast_policy(3);

        let c = Arc::clone(&counter);
        let result = policy
            .call(move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err::<i32, _>(TestError { retryable: true })
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn call_when_uses_predicate() {
        let counter = Arc::new(AtomicUsize::new(0));
        let policy = fast_policy(5);

        let c = Arc::clone(&counter);
        let result = policy
            .call_when(
                move || {
                    let c = Arc::clone(&c);
                    async move {
                        let n = c.fetch_add(1, Ordering::SeqCst);
                        Err::<i32, String>(format!("error #{n}"))
                    }
                },
                |err| err.contains("#0") || err.contains("#1"),
            )
            .await;

        assert!(result.is_err());
        // Attempt 0: err #0, retryable → retry
        // Attempt 1: err #1, retryable → retry
        // Attempt 2: err #2, NOT retryable → stop
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn delay_grows_exponentially() {
        let cfg = RetryConfig::new()
            .initial_delay(Duration::from_millis(100))
            .multiplier(2.0)
            .max_delay(Duration::from_secs(10))
            .jitter(0.0);

        assert_eq!(cfg.delay_for(0), Duration::from_millis(100));
        assert_eq!(cfg.delay_for(1), Duration::from_millis(200));
        assert_eq!(cfg.delay_for(2), Duration::from_millis(400));
    }

    #[test]
    fn delay_caps_at_max() {
        let cfg = RetryConfig::new()
            .initial_delay(Duration::from_millis(100))
            .multiplier(10.0)
            .max_delay(Duration::from_millis(500))
            .jitter(0.0);

        assert_eq!(cfg.delay_for(5), Duration::from_millis(500));
    }
}
