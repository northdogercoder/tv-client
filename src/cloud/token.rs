//! Bearer secrets with redacted diagnostics.
//!
//! Tokens are accepted only from an explicit prompt or a 0600 credential
//! file. They are never derived from argv, environment variables or URLs, and
//! their `Debug` representation is always redacted.

use std::fmt;

use thiserror::Error;

/// Lowest accepted token length.
pub const MIN_TOKEN_BYTES: usize = 16;
/// Highest accepted token length.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// Why a token string was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TokenError {
    /// The value was shorter or longer than the accepted range.
    #[error("token length is outside the accepted range")]
    Length,
    /// The value contained whitespace, control or non-ASCII bytes.
    #[error("token contains unsupported characters")]
    Characters,
}

/// Validate a single-line opaque token.
pub fn validate_token(value: &str) -> Result<(), TokenError> {
    if value.len() < MIN_TOKEN_BYTES || value.len() > MAX_TOKEN_BYTES {
        return Err(TokenError::Length);
    }
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(TokenError::Characters);
    }
    Ok(())
}

macro_rules! redacted_token {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap a token.
            pub fn new(value: impl Into<String>) -> Result<Self, TokenError> {
                let value = value.into();
                validate_token(&value)?;
                Ok(Self(value))
            }

            /// Borrow the secret for a request header or credential file.
            ///
            /// Callers must not format the returned value.
            #[must_use]
            pub fn expose(&self) -> &str {
                &self.0
            }

            /// Consume the wrapper and return the secret bytes.
            #[must_use]
            pub fn into_bytes(self) -> Vec<u8> {
                self.0.into_bytes()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(redacted)"))
            }
        }
    };
}

redacted_token!(EnrollmentToken, "One-use enrollment token read from stdin.");
redacted_token!(DeviceToken, "Long-lived device bearer token.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_contains_the_secret() {
        let secret = "enroll-secret-value-1234567890";
        let token = EnrollmentToken::new(secret).expect("valid token");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn whitespace_and_short_values_are_refused() {
        assert_eq!(
            EnrollmentToken::new("short").unwrap_err(),
            TokenError::Length
        );
        assert_eq!(
            DeviceToken::new("contains a space here").unwrap_err(),
            TokenError::Characters
        );
    }
}
