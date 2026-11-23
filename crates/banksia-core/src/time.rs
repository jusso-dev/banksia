//! Small time helpers. All timestamps are UTC RFC 3339.

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

pub fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339)
        .unwrap_or_else(|_| t.unix_timestamp().to_string())
}

pub fn now_rfc3339() -> String {
    rfc3339(now())
}

pub fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// A sortable, unique id: `20261122T103000Z-<8 hex>`.
pub fn sortable_id() -> String {
    let t = now();
    let mut r = [0u8; 4];
    crate::crypto::fill_random(&mut r);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z-{}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        hex::encode(r)
    )
}
