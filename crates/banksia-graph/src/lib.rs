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
///
/// `.` and `..` are left as they are; [`GraphClient::resolve`] refuses any
/// URL path containing a dot segment, encoded or not.
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

/// True if a URL path segment is `.` or `..`, including percent-encoded
/// forms, which URL parsers also treat as dot segments.
pub fn is_dot_segment(segment: &str) -> bool {
    let d = segment.to_ascii_lowercase().replace("%2e", ".");
    d == "." || d == ".."
}

/// Whether a single path component is safe as a OneDrive or mail folder
/// name: not empty, not a dot segment, and free of separators and
/// characters OneDrive rejects.
pub fn safe_component(c: &str) -> bool {
    !c.is_empty()
        && !is_dot_segment(c)
        && !c.chars().any(|ch| {
            matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || ch.is_control()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_segments() {
        for s in ["..", ".", "%2E%2E", "%2e.", ".%2E", "%2E"] {
            assert!(is_dot_segment(s), "{s}");
        }
        assert!(!is_dot_segment("a.b") && !is_dot_segment("...") && !is_dot_segment(""));
        assert!(!safe_component("..") && !safe_component("") && !safe_component("a/b"));
        assert!(safe_component("Plans 2026"));
    }
}
