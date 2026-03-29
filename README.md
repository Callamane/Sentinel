<p align="center">
  <img src="assets/logo.png" alt="Sentinel" width="400">
</p>

<h1 align="center">Sentinel</h1>

<p align="center">
  Resilience primitives for async Rust — circuit breaker, retry with backoff, validation error collection, and structured error context.
</p>

## Why

Distributed services fail in predictable ways: dependencies go down, requests time out, users submit garbage. Sentinel gives you four small, composable tools to handle all of it without pulling in a framework.

| Module | What it does |
|--------|-------------|
| `CircuitBreaker` | Stops calling a degraded dependency. Three-state (closed → open → half-open) with configurable thresholds and a pluggable state listener for metrics. |
| `RetryPolicy` | Retries transient failures with exponential backoff and jitter. Works with a `Retryable` trait or an ad-hoc predicate. |
| `ErrorCollector` | Accumulates field-level validation errors, then converts to a single `ValidationError`. |
| `ErrorContext` | Wraps any error with a message, key-value metadata, and an automatic trace ID from `tracing`. |

Each module is independent. Use one or all four.

## Install

```toml
[dependencies]
sentinel-rs = "0.1"
```

## Usage

### Circuit Breaker

```rust
use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig};
use std::time::Duration;

let breaker = CircuitBreaker::new(
    "payments-api",
    CircuitBreakerConfig::new()
        .failure_threshold(5)
        .success_threshold(2)
        .timeout(Duration::from_secs(30)),
);

let result = breaker.call(|| async {
    http_client.post("/charge").send().await
}).await;
```

When 5 failures occur within the window, the circuit opens and subsequent calls return `Err(CircuitBreakerError::Open)` immediately — no network call made. After 30 seconds it transitions to half-open and lets a probe through. Two consecutive successes close it again.

#### Metrics hook

```rust
use sentinel_rs::circuit_breaker::{StateListener, CircuitState};

struct PrometheusListener;

impl StateListener for PrometheusListener {
    fn on_state_change(&self, name: &str, from: CircuitState, to: CircuitState) {
        // increment your counter, push to StatsD, etc.
    }
}

let breaker = CircuitBreaker::new("api", config)
    .with_listener(PrometheusListener);
```

### Retry

```rust
use sentinel_rs::{RetryPolicy, RetryConfig, Retryable};
use std::time::Duration;

#[derive(Debug)]
enum ApiError { Timeout, BadRequest }

impl Retryable for ApiError {
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

let policy = RetryPolicy::new(
    RetryConfig::new()
        .max_attempts(4)
        .initial_delay(Duration::from_millis(100))
        .multiplier(2.0)
        .jitter(0.1),
);

let value = policy.call(|| async {
    fetch_price().await
}).await?;
```

Or skip the trait and pass a predicate:

```rust
policy.call_when(
    || async { do_thing().await },
    |err| err.is_temporary(),
).await?;
```

### Validation

```rust
use sentinel_rs::ErrorCollector;

let mut c = ErrorCollector::new();

c.check(age >= 18, "age", "min_value", "must be at least 18");
c.check(!name.is_empty(), "name", "required", "name is required");
c.require("email", &email_opt);

c.finish()?; // Ok(()) or Err(ValidationError)
```

Built-in validators for common checks:

```rust
use sentinel_rs::validation::validators;

validators::min_length(password, 8, &mut c, "password");
validators::email(addr, &mut c, "email");
validators::pattern(code, &regex, &mut c, "code", "invalid format");
```

### Error Context

```rust
use sentinel_rs::WithContext;

let data = std::fs::read_to_string("config.toml")
    .context("failed to read config")?;

// With metadata
let user = db.find(id).await
    .context("user lookup failed")
    .map_err(|e| e.meta("user_id", id))?;
```

Trace IDs from the active `tracing` span are captured automatically.

## Combining modules

Circuit breaker + retry compose naturally:

```rust
let result = breaker.call(|| async {
    policy.call(|| async {
        client.get(url).send().await
    }).await
}).await;
```

The retry policy handles transient blips; the circuit breaker handles sustained outages.

## Examples

```bash
cargo run --example circuit_breaker
cargo run --example retry
cargo run --example validation
```

## License

MIT

