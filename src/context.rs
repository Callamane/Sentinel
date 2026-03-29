//! Structured error context with metadata and optional trace IDs.
//!
//! Wrap any error with a human-readable message, key-value metadata, and an
//! automatic trace ID pulled from the current `tracing` span (when present).
//! The original error is preserved as the `source()` in the standard
//! [`Error`](std::error::Error) chain.
//!
//! ```
//! use sentinel_rs::WithContext;
//!
//! fn parse_config(raw: &str) -> Result<u64, sentinel_rs::ErrorContext> {
//!     raw.parse::<u64>()
//!         .context("failed to parse config value")
//! }
//! ```

use std::fmt;
use tracing::Span;

// ---------------------------------------------------------------------------
// WithContext trait
// ---------------------------------------------------------------------------

/// Extension trait that adds `.context()` and `.with_context()` to any
/// `Result<T, E>` where `E: Error + Send + Sync + 'static`.
pub trait WithContext<T> {
    /// Attach a static message.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorContext`] when the original result is `Err`.
    fn context(self, msg: impl Into<String>) -> Result<T, ErrorContext>;

    /// Attach a lazily-computed message (evaluated only on error).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorContext`] when the original result is `Err`.
    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T, ErrorContext>;
}

impl<T, E: std::error::Error + Send + Sync + 'static> WithContext<T> for Result<T, E> {
    fn context(self, msg: impl Into<String>) -> Result<T, ErrorContext> {
        self.map_err(|e| ErrorContext::wrap(e).message(msg))
    }

    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T, ErrorContext> {
        self.map_err(|e| ErrorContext::wrap(e).message(f()))
    }
}

// ---------------------------------------------------------------------------
// ErrorContext
// ---------------------------------------------------------------------------

/// An error enriched with a message, metadata, and an optional trace ID.
#[derive(Debug)]
pub struct ErrorContext {
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    msg: Option<String>,
    trace_id: Option<String>,
    metadata: Vec<(String, String)>,
}

impl ErrorContext {
    /// Wrap an existing error, capturing the current span's trace ID.
    #[must_use]
    pub fn wrap<E: std::error::Error + Send + Sync + 'static>(error: E) -> Self {
        Self {
            source: Some(Box::new(error)),
            msg: None,
            trace_id: current_trace_id(),
            metadata: Vec::new(),
        }
    }

    /// Create a context from a message alone (no underlying source error).
    #[must_use]
    pub fn from_message(msg: impl Into<String>) -> Self {
        Self {
            source: None,
            msg: Some(msg.into()),
            trace_id: current_trace_id(),
            metadata: Vec::new(),
        }
    }

    /// Set or replace the human-readable message.
    #[must_use]
    pub fn message(mut self, msg: impl Into<String>) -> Self {
        self.msg = Some(msg.into());
        self
    }

    /// Attach an arbitrary key-value pair for structured logging.
    #[must_use]
    pub fn meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.push((key.into(), value.into()));
        self
    }

    /// The trace ID captured at construction time, if any.
    #[must_use]
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    /// All attached metadata pairs.
    #[must_use]
    pub fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }
}

fn current_trace_id() -> Option<String> {
    Span::current().id().map(|id| format!("{id:?}"))
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.msg, &self.source) {
            (Some(m), Some(s)) => write!(f, "{m}: {s}")?,
            (Some(m), None) => write!(f, "{m}")?,
            (None, Some(s)) => write!(f, "{s}")?,
            (None, None) => write!(f, "unknown error")?,
        }

        if !self.metadata.is_empty() {
            write!(f, " [")?;
            for (i, (k, v)) in self.metadata.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{k}={v}")?;
            }
            write!(f, "]")?;
        }

        Ok(())
    }
}

impl std::error::Error for ErrorContext {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn wraps_source_with_message() {
        let err = io::Error::new(io::ErrorKind::NotFound, "gone");
        let ctx = ErrorContext::wrap(err).message("lookup failed");

        assert_eq!(ctx.to_string(), "lookup failed: gone");
        assert!(ctx.source.is_some());
    }

    #[test]
    fn from_message_only() {
        let ctx = ErrorContext::from_message("something broke");
        assert_eq!(ctx.to_string(), "something broke");
        assert!(ctx.source.is_none());
    }

    #[test]
    fn metadata_renders_in_display() {
        let ctx = ErrorContext::from_message("bad")
            .meta("user", "42")
            .meta("op", "insert");

        let s = ctx.to_string();
        assert!(s.contains("user=42"));
        assert!(s.contains("op=insert"));
    }

    #[test]
    fn context_trait_on_result() {
        let res: Result<(), io::Error> = Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));

        let ctx_err = res.context("request failed").unwrap_err();
        assert!(ctx_err.to_string().starts_with("request failed"));
    }

    #[test]
    fn with_context_is_lazy() {
        let ok: Result<i32, io::Error> = Ok(1);
        let mut called = false;
        let _ = ok.with_context(|| {
            called = true;
            "should not run".into()
        });
        assert!(!called);
    }

    #[test]
    fn error_trait_source_chain() {
        let inner = io::Error::new(io::ErrorKind::Other, "root cause");
        let ctx = ErrorContext::wrap(inner).message("outer");

        let source = std::error::Error::source(&ctx).unwrap();
        assert_eq!(source.to_string(), "root cause");
    }
}
