//! Plex tool surface.
//!
//! Endpoint registry: `plex.{list, detail, create, update, delete}` —
//! generated wholesale by `endpoint_resource!`. The macro emits the row
//! struct, db helpers, schema fragment, args/output types, and the five
//! `#[orca_tool]`-annotated functions in one shot. See
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]].
//!
//! Server diagnosis: `plex.server_info`, `plex.libraries`, and the core
//! `plex.transcode_health` are hand-written `#[orca_tool]`s that call out over
//! HTTP through the typed `Client` rather than over the local registry table.
//!
//! Endpoint resolution: every diagnosis tool accepts the endpoint *name* and
//! loads `(base_url, token)` from the toolkit-generated `endpoint_db` at call
//! time. Per [[project-colocated-api-clients]] + model B (any creds-holder may
//! execute), the row syncs to every paired peer so any of them can call
//! `plex.*` against a registered endpoint.
//!
//! Imports flow through `plugin_toolkit::prelude::*` only — the plugin treats
//! the toolkit as the single gateway to the orca system.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;

use crate::diag::SessionTranscodeHealth;
use crate::{Client, Config, LibrarySection, ServerInfo};

// ═══════════════════════════════════════════════════════════════════════════
// plex.{list,detail,create,update,delete} — endpoint registry CRUD.
// One declaration → five tools, three transports each, schema fragment, db
// helpers, row struct, args/output types. Power scales with the macro.
// ═══════════════════════════════════════════════════════════════════════════

#[endpoint_resource(plugin = "plex")]
pub struct PlexEndpoint {
    pub name: String,
    pub base_url: String,
    #[secret]
    pub token: String,
    pub enabled: bool,
}

// ── HTTP client helper ──────────────────────────────────────────────────────

fn make_client(name: &str) -> Result<Client> {
    let row = endpoint_db::get(name)?
        .with_context(|| format!("plex endpoint '{name}' not registered"))?;
    if !row.enabled {
        bail!("plex endpoint '{name}' is disabled");
    }
    Ok(Client::new(Config::new(row.base_url, row.token)))
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.server_info — server name / version / platform
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexServerInfoArgs {
    /// Registered endpoint name.
    pub endpoint: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexServerInfoOutput {
    pub friendly_name: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub platform_version: Option<String>,
    pub machine_identifier: Option<String>,
}

impl From<ServerInfo> for PlexServerInfoOutput {
    fn from(s: ServerInfo) -> Self {
        Self {
            friendly_name: s.friendly_name,
            version: s.version,
            platform: s.platform,
            platform_version: s.platform_version,
            machine_identifier: s.machine_identifier,
        }
    }
}

/// Server name, version, and platform from the Plex root endpoint.
#[orca_tool(domain = "plex", verb = "server_info")]
async fn plex_server_info(
    args: PlexServerInfoArgs,
    _ctx: &ToolCtx,
) -> Result<PlexServerInfoOutput> {
    Ok(make_client(&args.endpoint)?.server_info().await?.into())
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.libraries — configured library sections
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexLibrariesArgs {
    /// Registered endpoint name.
    pub endpoint: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexLibrariesOutput {
    /// Configured library sections from `/library/sections/all`.
    pub libraries: Vec<PlexLibrary>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexLibrary {
    pub key: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub locations: Vec<String>,
}

impl From<LibrarySection> for PlexLibrary {
    fn from(s: LibrarySection) -> Self {
        Self {
            key: s.key,
            title: s.title,
            kind: s.kind,
            locations: s.locations,
        }
    }
}

/// Configured library sections on a registered Plex server.
#[orca_tool(domain = "plex", verb = "libraries")]
async fn plex_libraries(args: PlexLibrariesArgs, _ctx: &ToolCtx) -> Result<PlexLibrariesOutput> {
    let libraries = make_client(&args.endpoint)?
        .libraries()
        .await?
        .into_iter()
        .map(PlexLibrary::from)
        .collect();
    Ok(PlexLibrariesOutput { libraries })
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.transcode_health — CORE DIAGNOSIS
//
// `GET /status/sessions` → per-session transcode state. A session whose
// `videoDecision` is `transcode` but whose `transcodeHwFullPipeline` is false
// is running a SOFTWARE (CPU) video transcode — the condition operators chase.
// The summary surfaces whether *any* session is software-fallback so a caller
// can branch without re-walking the list.
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexTranscodeHealthArgs {
    /// Registered endpoint name.
    pub endpoint: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexTranscodeHealthOutput {
    /// Total active sessions reported by `/status/sessions`.
    pub session_count: usize,
    /// Sessions actively transcoding (have a `TranscodeSession`).
    pub transcoding_count: usize,
    /// Sessions transcoding video on the CPU instead of the HW pipeline.
    pub software_fallback_count: usize,
    /// True when at least one session is a software fallback — the single
    /// flag a caller branches on to alert "HW accel is not engaging".
    pub any_software_fallback: bool,
    /// Per-session detail.
    pub sessions: Vec<SessionTranscodeHealth>,
}

/// **Core diagnosis.** Classify every active Plex session as direct-play,
/// hardware transcode, or software (CPU) fallback, and flag whether hardware
/// acceleration is failing to engage.
#[orca_tool(domain = "plex", verb = "transcode_health")]
async fn plex_transcode_health(
    args: PlexTranscodeHealthArgs,
    _ctx: &ToolCtx,
) -> Result<PlexTranscodeHealthOutput> {
    let sessions = make_client(&args.endpoint)?.transcode_health().await?;
    let session_count = sessions.len();
    let transcoding_count = sessions.iter().filter(|s| s.is_transcoding).count();
    let software_fallback_count = sessions.iter().filter(|s| s.software_fallback).count();
    Ok(PlexTranscodeHealthOutput {
        session_count,
        transcoding_count,
        software_fallback_count,
        any_software_fallback: software_fallback_count > 0,
        sessions,
    })
}
