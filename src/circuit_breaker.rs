//! Three-state circuit breaker for preventing cascading failures.
//!
//! The breaker tracks failures within a time window. Once the failure count
//! reaches `failure_threshold`, the circuit **opens** and subsequent calls fail
//! immediately with [`CircuitBreakerError::Open`]. After `timeout` elapses the
//! circuit moves to **half-open**, allowing a probe request through. If
//! `success_threshold` consecutive probes succeed the circuit **closes** again;
//! any probe failure re-opens it.
//!
//! The failure window is a **tumbling window**: when `window` elapses, the
//! failure counter resets to zero. This is simpler and cheaper than a true
//! sliding window (which would track individual failure timestamps), and is
//! sufficient for the vast majority of production use cases.
//!
//! An optional [`StateListener`] callback fires on every state transition,
//! letting you plug in metrics, logging, or alerting without coupling to a
//! specific observability stack.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Possible states of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CircuitState {
    /// Requests flow through normally. Failures are counted.
    Closed,
    /// Requests are rejected immediately.
    Open,
    /// A limited number of probe requests are allowed through to test recovery.
    HalfOpen,
}

impl fmt::Display for CircuitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("closed"),
            Self::Open => f.write_str("open"),
            Self::HalfOpen => f.write_str("half-open"),
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for a [`CircuitBreaker`].
///
/// Use the builder methods to override defaults:
///
/// ```
/// use sentinel_rs::CircuitBreakerConfig;
/// use std::time::Duration;
///
/// let cfg = CircuitBreakerConfig::new()
///     .failure_threshold(10)
///     .success_threshold(3)
///     .timeout(Duration::from_secs(90))
///     .window(Duration::from_secs(120));
/// ```
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    pub(crate) failure_threshold: usize,
    pub(crate) success_threshold: usize,
    pub(crate) timeout: Duration,
    pub(crate) window: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            timeout: Duration::from_secs(60),
            window: Duration::from_secs(60),
        }
    }
}

impl CircuitBreakerConfig {
    /// Create a new config with sensible defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Failures required within the window before the circuit opens.
    ///
    /// Clamped to a minimum of 1.
    #[must_use]
    pub fn failure_threshold(mut self, n: usize) -> Self {
        self.failure_threshold = n.max(1);
        self
    }

    /// Consecutive successes in half-open state before the circuit closes.
    ///
    /// Clamped to a minimum of 1.
    #[must_use]
    pub fn success_threshold(mut self, n: usize) -> Self {
        self.success_threshold = n.max(1);
        self
    }

    /// How long the circuit stays open before transitioning to half-open.
    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Sliding window duration for counting failures. When the window expires
    /// the failure counter resets (tumbling window semantics).
    #[must_use]
    pub fn window(mut self, d: Duration) -> Self {
        self.window = d;
        self
    }
}

// ---------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------

/// Callback invoked on every state transition.
///
/// Implement this to wire up metrics, alerting, or structured logging.
///
/// ```
/// use sentinel_rs::circuit_breaker::{StateListener, CircuitState};
///
/// struct LogListener;
///
/// impl StateListener for LogListener {
///     fn on_state_change(&self, name: &str, from: CircuitState, to: CircuitState) {
///         println!("[{name}] {from} -> {to}");
///     }
/// }
/// ```
pub trait StateListener: Send + Sync + 'static {
    /// Called whenever the breaker transitions from one state to another.
    fn on_state_change(&self, name: &str, from: CircuitState, to: CircuitState);
}

impl<T: StateListener> StateListener for Arc<T> {
    fn on_state_change(&self, name: &str, from: CircuitState, to: CircuitState) {
        (**self).on_state_change(name, from, to);
    }
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Inner {
    state: CircuitState,
    failures: usize,
    successes: usize,
    last_failure: Option<Instant>,
    opened_at: Option<Instant>,
    window_start: Instant,
}

impl Inner {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            failures: 0,
            successes: 0,
            last_failure: None,
            opened_at: None,
            window_start: Instant::now(),
        }
    }

    fn reset_window(&mut self) {
        self.failures = 0;
        self.successes = 0;
        self.window_start = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// CircuitBreaker
// ---------------------------------------------------------------------------

/// A three-state circuit breaker.
///
/// `CircuitBreaker` is cheaply cloneable — it wraps all mutable state in an
/// [`Arc`] so clones share the same underlying breaker.
pub struct CircuitBreaker {
    name: String,
    config: CircuitBreakerConfig,
    inner: Arc<RwLock<Inner>>,
    probe_gate: Arc<Semaphore>,
    listener: Option<Arc<dyn StateListener>>,
}

impl fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("listener", &self.listener.as_ref().map(|_| "..."))
            .finish_non_exhaustive()
    }
}

impl Clone for CircuitBreaker {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            config: self.config.clone(),
            inner: Arc::clone(&self.inner),
            probe_gate: Arc::clone(&self.probe_gate),
            listener: self.listener.clone(),
        }
    }
}

const _: () = {
    #[allow(dead_code)]
    fn assert_send_sync<T: Send + Sync>() {}
    #[allow(dead_code)]
    fn assert_all() {
        assert_send_sync::<CircuitBreaker>();
    }
};

impl CircuitBreaker {
    /// Create a new breaker with the given name and config.
    #[must_use]
    pub fn new(name: impl Into<String>, config: CircuitBreakerConfig) -> Self {
        Self {
            name: name.into(),
            config,
            inner: Arc::new(RwLock::new(Inner::new())),
            probe_gate: Arc::new(Semaphore::new(1)),
            listener: None,
        }
    }

    /// Attach a [`StateListener`] that fires on transitions.
    #[must_use]
    pub fn with_listener(mut self, listener: impl StateListener) -> Self {
        self.listener = Some(Arc::new(listener));
        self
    }

    /// Current circuit state.
    pub async fn state(&self) -> CircuitState {
        self.inner.read().await.state
    }

    /// Snapshot of internal counters for observability.
    pub async fn snapshot(&self) -> Snapshot {
        let inner = self.inner.read().await;
        Snapshot {
            state: inner.state,
            failures: inner.failures,
            successes: inner.successes,
            last_failure: inner.last_failure,
        }
    }

    /// Execute `op` through the circuit breaker.
    ///
    /// Returns [`CircuitBreakerError::Open`] immediately if the circuit is
    /// open, or [`CircuitBreakerError::Inner`] if the operation itself fails.
    ///
    /// # Errors
    ///
    /// Returns [`CircuitBreakerError::Open`] when the circuit rejects the
    /// operation, or [`CircuitBreakerError::Inner`] when the operation returns
    /// an error.
    pub async fn call<F, Fut, T, E>(&self, op: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let _probe_permit = self.acquire_permit().await?;

        match op().await {
            Ok(val) => {
                self.record_success().await;
                Ok(val)
            }
            Err(e) => {
                self.record_failure().await;
                Err(CircuitBreakerError::Inner(e))
            }
        }
    }

    /// Force the circuit open.
    pub async fn trip(&self) {
        let transition = {
            let mut inner = self.inner.write().await;
            let prev = inner.state;
            if prev == CircuitState::Open {
                None
            } else {
                warn!(breaker = %self.name, "manually tripped");
                inner.state = CircuitState::Open;
                inner.opened_at = Some(Instant::now());
                Some((prev, CircuitState::Open))
            }
        };

        if let Some((from, to)) = transition {
            self.notify(from, to);
        }
    }

    /// Force the circuit closed and reset counters.
    pub async fn reset(&self) {
        let transition = {
            let mut inner = self.inner.write().await;
            let prev = inner.state;
            info!(breaker = %self.name, "manually reset");
            inner.state = CircuitState::Closed;
            inner.failures = 0;
            inner.successes = 0;
            inner.opened_at = None;
            if prev == CircuitState::Closed {
                None
            } else {
                Some((prev, CircuitState::Closed))
            }
        };

        if let Some((from, to)) = transition {
            self.notify(from, to);
        }
    }

    // -- private ------------------------------------------------------------

    /// Check whether a request is allowed through. Handles the open → half-open
    /// transition when the timeout expires.
    async fn acquire_permit<E>(
        &self,
    ) -> Result<Option<OwnedSemaphorePermit>, CircuitBreakerError<E>> {
        let mut inner = self.inner.write().await;

        // Expire the sliding window if needed.
        if inner.window_start.elapsed() > self.config.window {
            debug!(breaker = %self.name, "failure window expired, resetting counters");
            inner.reset_window();
        }

        match inner.state {
            CircuitState::Closed => Ok(None),
            CircuitState::HalfOpen => {
                // Allow only one in-flight probe while half-open.
                match Arc::clone(&self.probe_gate).try_acquire_owned() {
                    Ok(permit) => Ok(Some(permit)),
                    Err(_) => Err(CircuitBreakerError::Open),
                }
            }
            CircuitState::Open => {
                let Some(opened) = inner.opened_at else {
                    inner.opened_at = Some(Instant::now());
                    return Err(CircuitBreakerError::Open);
                };

                if opened.elapsed() >= self.config.timeout {
                    info!(breaker = %self.name, "timeout elapsed, transitioning to half-open");
                    inner.state = CircuitState::HalfOpen;
                    inner.successes = 0;

                    // First caller after timeout becomes the probe.
                    let permit = Arc::clone(&self.probe_gate)
                        .try_acquire_owned()
                        .map_err(|_| CircuitBreakerError::Open)?;
                    drop(inner);

                    self.notify(CircuitState::Open, CircuitState::HalfOpen);
                    Ok(Some(permit))
                } else {
                    Err(CircuitBreakerError::Open)
                }
            }
        }
    }

    async fn record_success(&self) {
        let mut transition = None;
        let mut inner = self.inner.write().await;
        inner.successes += 1;

        match inner.state {
            CircuitState::Closed => {
                if inner.failures > 0 {
                    debug!(breaker = %self.name, "success after failures, resetting failure count");
                    inner.failures = 0;
                }
            }
            CircuitState::HalfOpen => {
                if inner.successes >= self.config.success_threshold {
                    info!(
                        breaker = %self.name,
                        successes = inner.successes,
                        "probe succeeded, closing circuit"
                    );
                    inner.state = CircuitState::Closed;
                    inner.failures = 0;
                    inner.successes = 0;
                    inner.opened_at = None;
                    transition = Some((CircuitState::HalfOpen, CircuitState::Closed));
                }
            }
            CircuitState::Open => {} // unreachable after acquire_permit
        }

        drop(inner);
        if let Some((from, to)) = transition {
            self.notify(from, to);
        }
    }

    async fn record_failure(&self) {
        let mut transition = None;
        let mut inner = self.inner.write().await;
        inner.failures += 1;
        inner.last_failure = Some(Instant::now());

        match inner.state {
            CircuitState::Closed => {
                if inner.failures >= self.config.failure_threshold {
                    warn!(
                        breaker = %self.name,
                        failures = inner.failures,
                        "failure threshold reached, opening circuit"
                    );
                    inner.state = CircuitState::Open;
                    inner.opened_at = Some(Instant::now());
                    transition = Some((CircuitState::Closed, CircuitState::Open));
                } else {
                    debug!(
                        breaker = %self.name,
                        failures = inner.failures,
                        threshold = self.config.failure_threshold,
                        "failure recorded"
                    );
                }
            }
            CircuitState::HalfOpen => {
                warn!(breaker = %self.name, "probe failed, re-opening circuit");
                inner.state = CircuitState::Open;
                inner.opened_at = Some(Instant::now());
                inner.successes = 0;
                transition = Some((CircuitState::HalfOpen, CircuitState::Open));
            }
            CircuitState::Open => {} // unreachable after acquire_permit
        }

        drop(inner);
        if let Some((from, to)) = transition {
            self.notify(from, to);
        }
    }

    fn notify(&self, from: CircuitState, to: CircuitState) {
        if let Some(ref listener) = self.listener {
            listener.on_state_change(&self.name, from, to);
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Point-in-time view of circuit breaker internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Current state of the circuit.
    pub state: CircuitState,
    /// Number of failures recorded in the current window.
    pub failures: usize,
    /// Number of successes recorded (relevant in half-open state).
    pub successes: usize,
    /// When the last failure occurred, if any.
    pub last_failure: Option<Instant>,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Error returned by [`CircuitBreaker::call`].
#[derive(Debug)]
#[non_exhaustive]
pub enum CircuitBreakerError<E> {
    /// The circuit is open — the operation was never attempted.
    Open,
    /// The operation ran and returned an error.
    Inner(E),
}

impl<E: fmt::Display> fmt::Display for CircuitBreakerError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("circuit breaker is open"),
            Self::Inner(e) => write!(f, "{e}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for CircuitBreakerError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open => None,
            Self::Inner(e) => Some(e),
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
    use tokio::sync::Barrier;

    fn fast_config(failures: usize, timeout_ms: u64) -> CircuitBreakerConfig {
        CircuitBreakerConfig::new()
            .failure_threshold(failures)
            .success_threshold(2)
            .timeout(Duration::from_millis(timeout_ms))
            .window(Duration::from_secs(60))
    }

    async fn fail() -> Result<(), &'static str> {
        Err("boom")
    }

    async fn succeed() -> Result<i32, &'static str> {
        Ok(1)
    }

    #[tokio::test]
    async fn stays_closed_on_success() {
        let cb = CircuitBreaker::new("test", fast_config(3, 50));
        let _ = cb.call(succeed).await;
        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn opens_after_threshold() {
        let cb = CircuitBreaker::new("test", fast_config(3, 50));

        for _ in 0..3 {
            let _ = cb.call(fail).await;
        }

        assert_eq!(cb.state().await, CircuitState::Open);
    }

    #[tokio::test]
    async fn rejects_while_open() {
        let cb = CircuitBreaker::new("test", fast_config(2, 5000));

        for _ in 0..2 {
            let _ = cb.call(fail).await;
        }

        let res = cb.call(succeed).await;
        assert!(matches!(res, Err(CircuitBreakerError::Open)));
    }

    #[tokio::test]
    async fn transitions_to_half_open_after_timeout() {
        let cb = CircuitBreaker::new("test", fast_config(2, 30));

        for _ in 0..2 {
            let _ = cb.call(fail).await;
        }
        assert_eq!(cb.state().await, CircuitState::Open);

        tokio::time::sleep(Duration::from_millis(40)).await;

        // The next call triggers the half-open transition.
        let _ = cb.call(succeed).await;
        let state = cb.state().await;
        // Either still half-open (1 success, threshold is 2) or closed.
        assert!(state == CircuitState::HalfOpen || state == CircuitState::Closed);
    }

    #[tokio::test]
    async fn closes_after_enough_probes() {
        let cb = CircuitBreaker::new("test", fast_config(2, 30));

        for _ in 0..2 {
            let _ = cb.call(fail).await;
        }

        tokio::time::sleep(Duration::from_millis(40)).await;

        // Two successes in half-open should close it.
        let _ = cb.call(succeed).await;
        let _ = cb.call(succeed).await;

        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn probe_failure_reopens() {
        let cb = CircuitBreaker::new("test", fast_config(2, 30));

        for _ in 0..2 {
            let _ = cb.call(fail).await;
        }

        tokio::time::sleep(Duration::from_millis(40)).await;

        let _ = cb.call(fail).await;
        assert_eq!(cb.state().await, CircuitState::Open);
    }

    #[tokio::test]
    async fn manual_trip_and_reset() {
        let cb = CircuitBreaker::new("test", fast_config(100, 5000));

        cb.trip().await;
        assert_eq!(cb.state().await, CircuitState::Open);

        cb.reset().await;
        assert_eq!(cb.state().await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn listener_fires_on_transitions() {
        struct Counter(AtomicUsize);
        impl StateListener for Counter {
            fn on_state_change(&self, _name: &str, _from: CircuitState, _to: CircuitState) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let cb =
            CircuitBreaker::new("test", fast_config(2, 30)).with_listener(Arc::clone(&counter));

        // 2 failures → open (1 transition)
        for _ in 0..2 {
            let _ = cb.call(fail).await;
        }
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);

        // Wait, then probe → half-open (1 transition)
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = cb.call(succeed).await;
        assert!(counter.0.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn snapshot_reflects_state() {
        let cb = CircuitBreaker::new("test", fast_config(3, 50));

        let _ = cb.call(fail).await;
        let snap = cb.snapshot().await;

        assert_eq!(snap.state, CircuitState::Closed);
        assert_eq!(snap.failures, 1);
        assert!(snap.last_failure.is_some());
    }

    #[tokio::test]
    async fn half_open_allows_only_one_in_flight_probe() {
        let cb = Arc::new(CircuitBreaker::new("test", fast_config(1, 20)));

        // One failure opens the circuit.
        let _ = cb.call(fail).await;
        assert_eq!(cb.state().await, CircuitState::Open);

        tokio::time::sleep(Duration::from_millis(30)).await;

        let barrier = Arc::new(Barrier::new(3));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let cb = Arc::clone(&cb);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                cb.call(|| async {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    Ok::<_, &'static str>(())
                })
                .await
            }));
        }

        barrier.wait().await;

        let r1 = handles.remove(0).await.expect("task finished");
        let r2 = handles.remove(0).await.expect("task finished");

        let ok_count = usize::from(r1.is_ok()) + usize::from(r2.is_ok());
        let open_count = usize::from(matches!(r1, Err(CircuitBreakerError::Open)))
            + usize::from(matches!(r2, Err(CircuitBreakerError::Open)));

        assert_eq!(ok_count, 1, "exactly one probe should be in-flight");
        assert_eq!(open_count, 1, "the concurrent probe should be rejected");
    }
}
