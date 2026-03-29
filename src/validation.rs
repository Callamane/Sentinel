//! Accumulate multiple validation errors before returning them as a batch.
//!
//! Instead of returning on the first field error, collect every violation and
//! hand them all back at once — better UX for forms and API payloads.
//!
//! ```
//! use sentinel_rs::{ErrorCollector, FieldError};
//!
//! let mut collector = ErrorCollector::new();
//!
//! collector.require("name", &Some("Alice".into()));
//! collector.require("email", &None::<String>);
//!
//! assert!(collector.has_errors());
//! let err = collector.finish().unwrap_err();
//! assert_eq!(err.errors().len(), 1); // only email failed
//! ```

use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// FieldError
// ---------------------------------------------------------------------------

/// A single validation failure tied to a named field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// Name of the field that failed validation.
    pub field: String,
    /// Machine-readable error code (e.g. `"required"`, `"min_length"`).
    pub code: String,
    /// Human-readable description of the failure.
    pub message: String,
}

impl FieldError {
    /// Create a new field error.
    #[must_use]
    pub fn new(
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// ErrorCollector
// ---------------------------------------------------------------------------

/// Collects [`FieldError`]s during validation, then converts to a
/// [`ValidationError`] if any exist.
#[derive(Debug, Clone, Default)]
pub struct ErrorCollector {
    errors: Vec<FieldError>,
    by_field: HashMap<String, Vec<String>>,
}

impl ErrorCollector {
    /// Create a new empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a validation failure.
    pub fn add(
        &mut self,
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        let field = field.into();
        let message = message.into();
        let code = code.into();

        self.by_field
            .entry(field.clone())
            .or_default()
            .push(message.clone());

        self.errors.push(FieldError {
            field,
            code,
            message,
        });
    }

    /// Record a failure only when `valid` is `false`.
    ///
    /// ```
    /// # use sentinel_rs::ErrorCollector;
    /// let mut c = ErrorCollector::new();
    /// let age = 15;
    /// c.check(age >= 18, "age", "min_value", "must be at least 18");
    /// assert!(c.has_errors());
    /// ```
    pub fn check(
        &mut self,
        valid: bool,
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) {
        if !valid {
            self.add(field, code, message);
        }
    }

    /// Record a `"required"` error if the value is `None` or an empty string.
    pub fn require(&mut self, field: &str, value: &Option<String>) {
        let missing = match value {
            None => true,
            Some(s) => s.is_empty(),
        };
        if missing {
            self.add(field, "required", format!("{field} is required"));
        }
    }

    /// `true` when at least one error has been recorded.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Number of errors recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// `true` when no errors have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// All errors in insertion order.
    #[must_use]
    pub fn errors(&self) -> &[FieldError] {
        &self.errors
    }

    /// Errors for a single field, if any.
    #[must_use]
    pub fn field_errors(&self, field: &str) -> Option<&Vec<String>> {
        self.by_field.get(field)
    }

    /// Every field → messages mapping.
    #[must_use]
    pub fn all_field_errors(&self) -> &HashMap<String, Vec<String>> {
        &self.by_field
    }

    /// Absorb all errors from `other` into this collector.
    pub fn merge(&mut self, other: ErrorCollector) {
        for err in other.errors {
            self.by_field
                .entry(err.field.clone())
                .or_default()
                .push(err.message.clone());
            self.errors.push(err);
        }
    }

    /// Convert into `Ok(())` when clean, or `Err(ValidationError)` when dirty.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when at least one field error has been
    /// collected.
    pub fn finish(self) -> Result<(), ValidationError> {
        if self.has_errors() {
            Err(ValidationError::from_collector(self))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// ValidationError
// ---------------------------------------------------------------------------

/// One or more validation failures.
///
/// Implements [`std::error::Error`] so it can be returned from handlers or
/// propagated with `?`.
#[derive(Debug, Clone)]
pub struct ValidationError {
    collector: ErrorCollector,
}

impl ValidationError {
    /// Wrap an already-populated collector.
    #[must_use]
    pub fn from_collector(collector: ErrorCollector) -> Self {
        Self { collector }
    }

    /// Convenience: create an error with a single field failure.
    #[must_use]
    pub fn single(field: impl Into<String>, message: impl Into<String>) -> Self {
        let mut collector = ErrorCollector::new();
        collector.add(field, "validation_error", message);
        Self { collector }
    }

    /// All errors in insertion order.
    #[must_use]
    pub fn errors(&self) -> &[FieldError] {
        self.collector.errors()
    }

    /// Every field → messages mapping.
    #[must_use]
    pub fn field_errors(&self) -> &HashMap<String, Vec<String>> {
        self.collector.all_field_errors()
    }

    /// Consume the error to get the underlying collector back.
    #[must_use]
    pub fn into_collector(self) -> ErrorCollector {
        self.collector
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.errors().len();
        write!(f, "validation failed ({n} error")?;
        if n != 1 {
            f.write_str("s")?;
        }
        f.write_str(")")?;

        for err in self.errors() {
            write!(f, "\n  {}: {}", err.field, err.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

// ---------------------------------------------------------------------------
// Built-in validators
// ---------------------------------------------------------------------------

/// Reusable validation functions that record failures on an [`ErrorCollector`].
pub mod validators {
    use super::ErrorCollector;

    /// Fails when the value is `None` or an empty string.
    pub fn required(value: &Option<String>, collector: &mut ErrorCollector, field: &str) {
        let missing = match value {
            None => true,
            Some(s) => s.is_empty(),
        };
        if missing {
            collector.add(field, "required", format!("{field} is required"));
        }
    }

    /// Fails when the string is shorter than `min` bytes.
    pub fn min_length(value: &str, min: usize, collector: &mut ErrorCollector, field: &str) {
        if value.len() < min {
            collector.add(
                field,
                "min_length",
                format!("{field} must be at least {min} characters"),
            );
        }
    }

    /// Fails when the string is longer than `max` bytes.
    pub fn max_length(value: &str, max: usize, collector: &mut ErrorCollector, field: &str) {
        if value.len() > max {
            collector.add(
                field,
                "max_length",
                format!("{field} must be at most {max} characters"),
            );
        }
    }

    /// Rudimentary email check (contains `@` and `.`).
    pub fn email(value: &str, collector: &mut ErrorCollector, field: &str) {
        if !value.contains('@') || !value.contains('.') {
            collector.add(field, "email", format!("{field} must be a valid email"));
        }
    }

    /// Fails when the value does not match the regex.
    ///
    /// Requires the `regex` feature (enabled by default).
    #[cfg(feature = "regex")]
    pub fn pattern(
        value: &str,
        pat: &regex::Regex,
        collector: &mut ErrorCollector,
        field: &str,
        message: &str,
    ) {
        if !pat.is_match(value) {
            collector.add(field, "pattern", message);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_multiple_errors() {
        let mut c = ErrorCollector::new();
        c.add("username", "required", "username is required");
        c.add("password", "min_length", "password too short");

        assert_eq!(c.len(), 2);
        assert!(c.has_errors());
    }

    #[test]
    fn finish_ok_when_clean() {
        let c = ErrorCollector::new();
        assert!(c.finish().is_ok());
    }

    #[test]
    fn finish_err_when_dirty() {
        let mut c = ErrorCollector::new();
        c.add("x", "bad", "nope");
        let err = c.finish().unwrap_err();
        assert_eq!(err.errors().len(), 1);
    }

    #[test]
    fn check_records_on_false() {
        let mut c = ErrorCollector::new();
        c.check(true, "ok_field", "code", "msg");
        c.check(false, "bad_field", "code", "msg");
        assert_eq!(c.len(), 1);
        assert_eq!(c.errors()[0].field, "bad_field");
    }

    #[test]
    fn require_catches_none_and_empty() {
        let mut c = ErrorCollector::new();
        c.require("a", &None);
        c.require("b", &Some(String::new()));
        c.require("c", &Some("ok".into()));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn field_errors_lookup() {
        let mut c = ErrorCollector::new();
        c.add("email", "email", "bad format");
        c.add("email", "length", "too long");

        let msgs = c.field_errors("email").unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(c.field_errors("name").is_none());
    }

    #[test]
    fn merge_combines_collectors() {
        let mut a = ErrorCollector::new();
        a.add("f1", "e1", "m1");

        let mut b = ErrorCollector::new();
        b.add("f2", "e2", "m2");

        a.merge(b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn validation_error_display() {
        let err = ValidationError::single("token", "invalid token");
        let msg = err.to_string();
        assert!(msg.contains("1 error"));
        assert!(msg.contains("token"));
        assert!(msg.contains("invalid token"));
    }
}
