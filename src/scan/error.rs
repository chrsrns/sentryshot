// SPDX-License-Identifier: GPL-2.0-or-later

use common::DynError;
use thiserror::Error;

/// Camera scan failure taxonomy.
#[derive(Debug, Error)]
pub enum ScanError {
    /// `host` is not a valid IPv4 literal.
    #[error("invalid host: {0}")]
    InvalidHost(String),

    /// A scan is already in flight for a different request.
    #[error("scan already in progress")]
    Busy,

    /// An operation exceeded its bound.
    #[error("timeout")]
    Timeout,

    /// IO error on a socket or stream.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP layer failure.
    #[error("http: {0}")]
    Http(#[from] DynError),

    /// Device answered, but the reply did not parse.
    #[error("malformed reply: {0}")]
    Malformed(String),

    /// Device requires authentication and none (or wrong) was supplied.
    #[error("not authorized")]
    NotAuthorized,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn display_strings() {
        assert_eq!(
            "invalid host: x",
            ScanError::InvalidHost("x".into()).to_string()
        );
        assert_eq!("scan already in progress", ScanError::Busy.to_string());
        assert_eq!("timeout", ScanError::Timeout.to_string());
        assert_eq!("not authorized", ScanError::NotAuthorized.to_string());
    }
}
