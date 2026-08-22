//! A boxed-error `Result` and a way to say what was being attempted.

use std::error::Error as StdError;

pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;
pub type Result<T> = std::result::Result<T, BoxError>;

/// Attach a context message to an error. The closure runs only on the error
/// path, so the common case pays nothing for a message it will not print.
pub trait Context<T> {
    /// # Errors
    /// The receiver's error, prefixed with `f()`.
    fn ctx(self, f: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E: StdError + Send + Sync + 'static> Context<T> for std::result::Result<T, E> {
    fn ctx(self, f: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| -> BoxError { format!("{}: {e}", f()).into() })
    }
}

impl<T> Context<T> for Option<T> {
    fn ctx(self, f: impl FnOnce() -> String) -> Result<T> {
        self.ok_or_else(|| -> BoxError { f().into() })
    }
}

/// Return early with a formatted error.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return ::core::result::Result::Err(::std::format!($($arg)*).into())
    };
}
