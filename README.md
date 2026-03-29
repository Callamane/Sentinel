<p align="center">
  <img src="https://raw.githubusercontent.com/Callamane/sentinel/main/assets/logo.png" alt="Sentinel" width="400">
</p>

<h1 align="center">Sentinel</h1>

<p align="center">
  Four focused resilience primitives for async Rust — nothing more, nothing less.
</p>

<p align="center">
  <a href="https://crates.io/crates/sentinel-rs"><img src="https://img.shields.io/crates/v/sentinel-rs.svg" alt="crates.io"></a>
  <a href="https://docs.rs/sentinel-rs"><img src="https://docs.rs/sentinel-rs/badge.svg" alt="docs.rs"></a>
  <a href="https://github.com/Callamane/sentinel/actions"><img src="https://github.com/Callamane/sentinel/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License"></a>
  <img src="https://img.shields.io/badge/MSRV-1.70-brightgreen.svg" alt="MSRV 1.70">
</p>

---

`sentinel-rs` is a small, dependency-light crate. No framework, no macros, no proc-derive magic. Four modules you can drop into any async Rust service and use independently:

| Module | What it does |
|--------|-------------|
| [`CircuitBreaker`](#circuit-breaker) | Short-circuits calls to a degraded dependency. Three-state (closed → open → half-open), semaphore-gated probe, pluggable state listener. `Clone + Send + Sync`. |
| [`RetryPolicy`](#retry) | Exponential backoff with jitter. Implement `Retryable` on your error type, or pass an ad-hoc predicate. |
| [`ErrorCollector`](#validation) | Collect every field validation failure before returning — not just the first. |
| [`ErrorContext`](#error-context) | Wrap any error with a human message, key-value metadata, and a `tracing` span ID. |

## Install

```toml
[dependencies]
sentinel-rs = "0.1"
```

The only optional dependency is `regex`, used exclusively by `validators::pattern()`. Drop it if you don't need regex validation:

```toml
sentinel-rs = { version = "0.1", default-features = false }
```

## Circuit Breaker

Protects a dependency by stopping calls once a failure threshold is crossed, then probing for recovery after a timeout.

```rust
use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let breaker = CircuitBreaker::new(
        "payments",
        CircuitBreakerConfig::new()
            .failure_threshold(5)   // open after 5 failures
            .success_threshold(2)   // close after 2 consecutive probe successes
            .timeout(Duration::from_secs(30)), // wait 30s before probing
    );

    match breaker.call(|| async { charge_card().await }).await {
        Ok(receipt) => println!("charged: {receipt:?}"),
        Err(CircuitBreakerError::Open) => eprintln!("payments degraded, skipping"),
        Err(CircuitBreakerError::Inner(e)) => eprintln!("charge failed: {e}"),
        Err(_) => {}
    }
}
# async fn charge_card() -> Result<u64, String> { Ok(42) }
```

**Concurrency:** while half-open, exactly one probe is allowed in-flight. Every concurrent caller gets `Open` until the probe completes. This is enforced with a semaphore — not a flag that races.

**Sharing across tasks:** `CircuitBreaker` is `Clone`. Clones share the same underlying state:

```rust
let b1 = CircuitBreaker::new("db", config);
let b2 = b1.clone();

tokio::spawn(async move {
    let _ = b2.call(|| async { query().await }).await;
});
# async fn query() -> Result<(), String> { Ok(()) }
# use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig};
# let config = CircuitBreakerConfig::new();
```

**Metrics hook:** implement `StateListener` to push transitions to Prometheus, StatsD, or your own logger:

```rust
use sentinel_rs::circuit_breaker::{StateListener, CircuitState};

struct MyMetrics;

impl StateListener for MyMetrics {
    fn on_state_change(&self, name: &str, from: CircuitState, to: CircuitState) {
        eprintln!("[{name}] {from} → {to}");
    }
}

# use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig};
let breaker = CircuitBreaker::new("api", CircuitBreakerConfig::new())
    .with_listener(MyMetrics);
```

The callback fires **after** the state lock is released, so calling back into the breaker from inside `on_state_change` is safe.

## Retry

Retries an async operation with exponential backoff + optional jitter. Implement `Retryable` on your error type to control which failures are retried:

```rust
use sentinel_rs::{RetryPolicy, RetryConfig, Retryable};
use std::time::Duration;
use std::fmt;

#[derive(Debug)]
enum ApiError {
    Timeout,
    BadRequest(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "request timed out"),
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
        }
    }
}

impl Retryable for ApiError {
    fn is_retryable(&self) -> bool {
        // Only retry timeouts — bad requests won't get better
        matches!(self, Self::Timeout)
    }
}

# #[tokio::main] async fn main() -> Result<(), ApiError> {
let policy = RetryPolicy::new(
    RetryConfig::new()
        .max_attempts(4)
        .initial_delay(Duration::from_millis(100))
        .multiplier(2.0)  // 100ms → 200ms → 400ms
        .jitter(0.1),     // ±10% to avoid thundering herd
);

let data = policy.call(|| async { fetch().await }).await?;
# Ok(())
# }
# async fn fetch() -> Result<Vec<u8>, ApiError> { Ok(vec![]) }
```

Don't want to implement a trait? Pass a predicate instead:

```rust
# use sentinel_rs::{RetryPolicy, RetryConfig};
# #[tokio::main] async fn main() -> Result<(), String> {
# let policy = RetryPolicy::new(RetryConfig::new());
let result = policy
    .call_when(
        || async { ping().await },
        |err: &String| err.contains("timeout"),
    )
    .await?;
# Ok(())
# }
# async fn ping() -> Result<(), String> { Ok(()) }
```

## Validation

Collect all field errors before returning, so a user sees every problem at once:

```rust
use sentinel_rs::{ErrorCollector, ValidationError};
use sentinel_rs::validation::validators;

fn validate_signup(username: &str, password: &str, email: &str) -> Result<(), ValidationError> {
    let mut c = ErrorCollector::new();

    validators::min_length(username, 3, &mut c, "username");
    validators::min_length(password, 8, &mut c, "password");
    validators::email(email, &mut c, "email");

    // Custom rule
    c.check(
        !username.contains(' '),
        "username",
        "no_spaces",
        "username cannot contain spaces",
    );

    c.finish() // Ok(()) if clean, Err(ValidationError) listing every failure
}

match validate_signup("al", "short", "not-an-email") {
    Ok(()) => println!("valid"),
    Err(e) => {
        // e.errors()      — all failures in insertion order
        // e.field_errors() — grouped by field name
        eprintln!("{e}");
    }
}
```

## Error Context

Attach a message and structured metadata to any error, without losing the original source:

```rust
use sentinel_rs::{ErrorContext, WithContext};

// .context() on any Result<T, E> where E: std::error::Error
let config = std::fs::read_to_string("config.toml")
    .context("failed to read config file")?;

// Build one manually with metadata
fn lookup(id: u64) -> Result<String, ErrorContext> {
    find_user(id).map_err(|e| {
        ErrorContext::wrap(e)
            .message("user lookup failed")
            .meta("user_id", id.to_string())
            .meta("service", "accounts")
    })
}
# fn find_user(_: u64) -> Result<String, std::io::Error> { Ok("alice".into()) }
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let _ = lookup(1)?;
# Ok(())
# }
```

If you're inside an active `tracing` span, the span ID is captured automatically and accessible via `err.trace_id()`.

## Composing modules

The circuit breaker and retry policy are designed to stack. Put retry inside the breaker — the breaker sees the final outcome, not each individual attempt:

```rust
# use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig, RetryPolicy, RetryConfig, Retryable};
# use std::time::Duration;
# #[derive(Debug)] struct E;
# impl std::fmt::Display for E { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, "err") } }
# impl Retryable for E { fn is_retryable(&self) -> bool { true } }
# #[tokio::main] async fn main() {
let breaker = CircuitBreaker::new("payments", CircuitBreakerConfig::new());
let policy  = RetryPolicy::new(RetryConfig::new().max_attempts(3));

let result = breaker.call(|| async {
    policy.call(|| async {
        // individual HTTP call
        Ok::<_, E>(())
    }).await
}).await;
# }
```

Retry handles transient blips within a single call. The breaker steps in when the dependency is consistently broken.

## Running the examples

```bash
git clone https://github.com/Callamane/sentinel
cd sentinel
cargo run --example circuit_breaker
cargo run --example retry
cargo run --example validation
```

## Running the tests

```bash
cargo test
```

The test suite covers unit tests and doc-tests: 40 tests total. Tests run concurrently and finish in under a second.

## Optional features

| Feature | Default | Enables |
|---------|---------|---------|
| `regex` | ✓ | `validators::pattern(value, &regex, &mut collector, "field", "msg")` |

## License

MIT — see [LICENSE](LICENSE).

