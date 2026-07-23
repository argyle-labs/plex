//! plex.tv transport — hand-written client + raw wire types.
//!
//! Mirrors the server-local `crate::Client`, but targets the plex.tv account
//! API rather than a specific server. `GET /api/v2/resources` returns a JSON
//! array of the account's resources; `owned` + `lastSeenAt` are the fields the
//! presence probe is built on. Nothing here crosses an `#[orca_tool]` boundary
//! — these are serde-only transport types.
#![allow(clippy::disallowed_types)]

use crate::PlexError;
use plugin_toolkit::http::{Client as HttpClient, HttpError, Response};

/// A stable client identifier so plex.tv attributes the announce consistently.
const CLIENT_IDENTIFIER: &str = "orca-plex-plugin";

/// One resource row from `/api/v2/resources`. Serde-only; normalized into the
/// schema-facing `ServerPresence` by the orchestration layer.
#[derive(Debug, Clone, plugin_toolkit::serde::Deserialize)]
#[serde(crate = "plugin_toolkit::serde")]
pub struct RawResource {
    #[serde(rename = "name", default)]
    pub name: Option<String>,
    #[serde(rename = "clientIdentifier", default)]
    pub client_identifier: Option<String>,
    #[serde(rename = "product", default)]
    pub product: Option<String>,
    #[serde(rename = "owned", default)]
    pub owned: bool,
    #[serde(rename = "presence", default)]
    pub presence: bool,
    #[serde(rename = "lastSeenAt", default, deserialize_with = "de_opt_epoch")]
    pub last_seen_at: Option<i64>,
}

#[derive(Clone)]
pub struct PlexTvClient {
    token: String,
    base_url: String,
    http: HttpClient,
}

impl PlexTvClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            base_url: "https://plex.tv".to_string(),
            http: HttpClient::new(),
        }
    }

    /// Override the base URL — test seam for pointing at a mock plex.tv.
    pub fn with_base(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            base_url: base_url.into(),
            http: HttpClient::new(),
        }
    }

    /// Enumerate the account's resources. This *is* the re-announce nudge — the
    /// call itself refreshes presence on plex.tv (tier-1 self-heal, observer
    /// effect from the mimir incident).
    pub async fn resources(&self) -> Result<Vec<RawResource>, PlexError> {
        let resp = self.authed_get("/api/v2/resources?includeHttps=1").await?;
        resp.json::<Vec<RawResource>>().map_err(PlexError::Http)
    }

    async fn authed_get(&self, path: &str) -> std::result::Result<Response, HttpError> {
        self.http
            .get(self.url(path))
            .header("x-plex-token", &self.token)
            .header("x-plex-client-identifier", CLIENT_IDENTIFIER)
            .header("accept", "application/json")
            .send()
            .await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }
}

// ── timestamp normalization ─────────────────────────────────────────────────
// plex.tv has, across API generations, encoded `lastSeenAt` as an epoch integer,
// a numeric string, or an ISO-8601 datetime. Accept all three and normalize to
// epoch seconds so age math is a plain subtraction — and so no date-parsing
// crate is pulled in.

fn de_opt_epoch<'de, D>(de: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: plugin_toolkit::serde::Deserializer<'de>,
{
    use plugin_toolkit::serde::Deserialize;
    let v = Option::<plugin_toolkit::serde_json::Value>::deserialize(de)?;
    Ok(v.and_then(epoch_from_value))
}

fn epoch_from_value(v: plugin_toolkit::serde_json::Value) -> Option<i64> {
    use plugin_toolkit::serde_json::Value;
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        // Numeric string → epoch directly; otherwise treat as RFC3339/ISO-8601
        // and normalize through orca core's datetime seam. No hand-rolled date
        // math, no direct `chrono` — per the plugin-toolkit-only rule.
        Value::String(s) => s.parse::<i64>().ok().or_else(|| {
            plugin_toolkit::time::Timestamp::parse_rfc3339(&s)
                .ok()
                .map(|t| t.unix_seconds())
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_toolkit::serde_json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn epoch_accepts_int_string_and_iso() {
        use serde_json::json;
        assert_eq!(epoch_from_value(json!(1_700_000_000)), Some(1_700_000_000));
        assert_eq!(epoch_from_value(json!("1700000000")), Some(1_700_000_000));
        // 2023-11-14T22:13:20Z == 1_700_000_000
        assert_eq!(
            epoch_from_value(json!("2023-11-14T22:13:20Z")),
            Some(1_700_000_000)
        );
        assert_eq!(epoch_from_value(json!(null)), None);
    }

    #[tokio::test]
    async fn resources_parses_and_is_authed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/resources"))
            .and(header("x-plex-token", "acct-tok"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "name": "mimir", "clientIdentifier": "id-1", "product": "Plex Media Server",
                  "owned": true, "presence": true, "lastSeenAt": 1_700_000_000i64 },
                { "name": "njord", "clientIdentifier": "id-2", "product": "Plex Media Server",
                  "owned": true, "presence": false, "lastSeenAt": "2023-11-14T22:13:20Z" }
            ])))
            .mount(&server)
            .await;
        let raws = PlexTvClient::with_base("acct-tok", server.uri())
            .resources()
            .await
            .unwrap();
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].name.as_deref(), Some("mimir"));
        assert_eq!(raws[0].last_seen_at, Some(1_700_000_000));
        assert!(raws[0].owned);
        // ISO form on the second row normalized to the same epoch.
        assert_eq!(raws[1].last_seen_at, Some(1_700_000_000));
    }
}
