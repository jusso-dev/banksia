//! A narrow Microsoft Graph client for unattended backup and restore.
//!
//! - App-only auth (client credentials) with a certificate assertion or secret.
//! - Retries on 429, 503 and 504, honouring `Retry-After`.
//! - Bearer tokens are only ever sent to the configured Graph origin.
//! - Downloads and upload sessions follow pre-authenticated URLs without the token.
//! - Expired delta state surfaces as a distinct error.

pub mod auth;
pub mod client;
pub mod error;
pub mod pem;
pub mod types;

pub use auth::{AppAuth, Credential};
pub use client::{GraphClient, GraphSettings, Page};
pub use error::GraphError;

/// Percent-encode one path segment (for example a UPN or file name).
pub fn seg(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'@');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

/// Percent-encode a slash-separated drive path, keeping the slashes.
pub fn drive_path(path: &str) -> String {
    path.split('/')
        .filter(|p| !p.is_empty())
        .map(seg)
        .collect::<Vec<_>>()
        .join("/")
}

/// Quote a value for an OData `$filter` string literal.
pub fn odata_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
