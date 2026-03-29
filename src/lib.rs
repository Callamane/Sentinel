//! # Sentinel
//!
//! Resilience primitives for async Rust services.
//!
//! Sentinel provides four composable building blocks for writing fault-tolerant
//! systems:
//!
//! - [`CircuitBreaker`] — prevent cascading failures by short-circuiting calls
//!   to degraded dependencies.
//! - [`RetryPolicy`] — retry transient failures with exponential backoff and
//!   jitter.
//! - [`ErrorCollector`] — accumulate multiple validation errors before
//!   returning them as a batch.
//! - [`ErrorContext`] — attach structured metadata and trace IDs to errors
//!   without losing the original source.
//!
//! Each module is independent. Use one, or compose them together.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig};
//! use std::time::Duration;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let breaker = CircuitBreaker::new(
//!     "payments",
//!     CircuitBreakerConfig::new()
//!         .failure_threshold(5)
//!         .timeout(Duration::from_secs(30)),
//! );
//!
//! let result = breaker.call(|| async {
//!     // your fallible operation
//!     Ok::<_, std::io::Error>(42)
//! }).await;
//! # Ok(())
//! # }
//! ```

pub mod circuit_breaker;
pub mod context;
pub mod retry;
pub mod validation;

pub use circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError, CircuitState,
};
pub use context::{ErrorContext, WithContext};
pub use retry::{RetryConfig, RetryPolicy, Retryable};
pub use validation::{ErrorCollector, FieldError, ValidationError};
