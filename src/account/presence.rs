//! Presence verdict + output shape — the schema-facing result of the probe.
//!
//! Pure data + classification: knows nothing about HTTP or plex.tv wire format.
//! The orchestration layer (`super`) maps transport rows into these types.

use plugin_toolkit::prelude::*;

/// Health verdict for one server's plex.tv announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    /// Announce is within the freshness threshold — a player can see it.
    Fresh,
    /// Announce is older than the threshold — likely invisible to players.
    Stale,
    /// plex.tv reported no `lastSeenAt` — cannot judge freshness.
    Unknown,
}

impl PresenceStatus {
    /// Classify an announce age (seconds) against the staleness threshold.
    /// `None` age (no timestamp) is `Unknown`.
    pub fn for_age(age_secs: Option<i64>, threshold_secs: i64) -> Self {
        match age_secs {
            Some(a) if a <= threshold_secs => Self::Fresh,
            Some(_) => Self::Stale,
            None => Self::Unknown,
        }
    }
}

/// One owned server's presence as seen from plex.tv, with the derived verdict.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct ServerPresence {
    pub name: Option<String>,
    pub client_identifier: Option<String>,
    pub product: Option<String>,
    pub owned: bool,
    /// plex.tv's own `presence` flag for the resource.
    pub presence: bool,
    /// Raw announce timestamp, epoch seconds (normalized on ingest).
    pub last_seen_at: Option<i64>,
    /// Age of the announce in seconds at probe time — the core staleness signal.
    pub last_seen_age_secs: Option<i64>,
    /// Verdict derived from `last_seen_age_secs` vs the threshold.
    pub status: PresenceStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_fresh_stale_unknown() {
        assert_eq!(
            PresenceStatus::for_age(Some(300), 600),
            PresenceStatus::Fresh
        );
        assert_eq!(
            PresenceStatus::for_age(Some(72_000), 600),
            PresenceStatus::Stale
        );
        assert_eq!(PresenceStatus::for_age(None, 600), PresenceStatus::Unknown);
        // Boundary: exactly at threshold is still Fresh.
        assert_eq!(
            PresenceStatus::for_age(Some(600), 600),
            PresenceStatus::Fresh
        );
    }
}
