//! Demonstrates the circuit breaker protecting a flaky dependency.
//!
//! The service fails 3 times, the circuit opens, rejects calls while open,
//! then transitions to half-open after the timeout. Two successful probes
//! close the circuit and traffic resumes normally.

use sentinel_rs::{CircuitBreaker, CircuitBreakerConfig, CircuitBreakerError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let breaker = CircuitBreaker::new(
        "example-service",
        CircuitBreakerConfig::new()
            .failure_threshold(3)
            .success_threshold(2)
            .timeout(Duration::from_millis(800)),
    );

    // The service will fail for the first 3 calls, then recover.
    let call_count = Arc::new(AtomicUsize::new(0));

    for i in 1..=12 {
        let count = Arc::clone(&call_count);
        let result = breaker
            .call(|| {
                let count = Arc::clone(&count);
                async move { flaky_service(&count).await }
            })
            .await;

        match result {
            Ok(val) => println!("[{i:>2}] success: {val}"),
            Err(CircuitBreakerError::Open) => println!("[{i:>2}] rejected (circuit open)"),
            Err(CircuitBreakerError::Inner(e)) => println!("[{i:>2}] failed: {e}"),
        }

        let snap = breaker.snapshot().await;
        println!(
            "     state={}, failures={}, successes={}",
            snap.state, snap.failures, snap.successes
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn flaky_service(count: &AtomicUsize) -> Result<&'static str, String> {
    let n = count.fetch_add(1, Ordering::SeqCst);
    if n < 3 {
        Err(format!("transient failure #{n}"))
    } else {
        Ok("ok")
    }
}
