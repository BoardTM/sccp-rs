//! Panic containment for callbacks invoked through Asterisk's C ABI.

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Run one complete foreign callback body and return `fallback` on unwind.
///
/// Operational errors belong in the callback's normal typed `Result` path.
/// This function exists only to prevent an unexpected Rust panic from crossing
/// Asterisk's C ABI.
pub fn contain_panic<R>(fallback: R, operation: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_only_panics_to_the_foreign_fallback() {
        assert_eq!(contain_panic(41, || 42), 42);
        assert_eq!(contain_panic(41, || panic!("foreign callback panic")), 41);
    }
}
