//! Demonstrates retry with exponential backoff.

use sentinel_rs::{RetryConfig, RetryPolicy, Retryable};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
struct ApiError(&'static str);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ApiError {}

impl Retryable for ApiError {
    fn is_retryable(&self) -> bool {
        // Only retry timeouts, not bad requests.
        self.0.contains("timeout")
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let policy = RetryPolicy::new(
        RetryConfig::new()
            .max_attempts(5)
            .initial_delay(Duration::from_millis(100))
            .multiplier(2.0)
            .jitter(0.15),
    );

    // Succeeds on attempt 3
    let counter = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&counter);
    let result = policy
        .call(move || {
            let c = Arc::clone(&c);
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(ApiError("timeout"))
                } else {
                    Ok(42)
                }
            }
        })
        .await;

    println!(
        "result: {:?} (after {} attempts)",
        result,
        counter.load(Ordering::SeqCst)
    );

    // Non-retryable error stops immediately
    let result = policy
        .call(|| async { Err::<i32, _>(ApiError("bad request")) })
        .await;
    println!("non-retryable: {:?}", result);
}
