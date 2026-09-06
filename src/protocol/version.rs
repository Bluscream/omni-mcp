//! Protocol version negotiation.
//!
//! The previous implementation replied with a hardcoded `2024-11-05` no matter
//! what the client asked for. The spec requires echoing the client's version
//! when we support it, and otherwise replying with our own latest so the client
//! can decide whether to continue.

use std::fmt;

/// Versions this server speaks, newest first.
pub const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The version offered when the client proposes something we do not know.
pub const LATEST: &str = SUPPORTED[0];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolVersion(String);

impl ProtocolVersion {
    /// Negotiates against the client's proposal.
    ///
    /// `None` (client omitted the field) selects the oldest supported version,
    /// which is the most compatible choice for a client too old to advertise.
    pub fn negotiate(requested: Option<&str>) -> Self {
        match requested {
            Some(v) if SUPPORTED.contains(&v) => Self(v.to_string()),
            Some(_) => Self(LATEST.to_string()),
            None => Self(SUPPORTED[SUPPORTED.len() - 1].to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_version_is_echoed_back() {
        assert_eq!(ProtocolVersion::negotiate(Some("2024-11-05")).as_str(), "2024-11-05");
        assert_eq!(ProtocolVersion::negotiate(Some("2025-06-18")).as_str(), "2025-06-18");
    }

    #[test]
    fn unknown_version_falls_back_to_our_latest() {
        assert_eq!(ProtocolVersion::negotiate(Some("1999-01-01")).as_str(), LATEST);
    }

    #[test]
    fn missing_version_selects_the_most_compatible() {
        assert_eq!(ProtocolVersion::negotiate(None).as_str(), "2024-11-05");
    }
}
