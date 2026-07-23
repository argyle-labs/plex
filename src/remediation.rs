//! Self-heal orchestration — the remediation ladder, composed.
//!
//! Ties the two primitives together into one detect→correct action:
//! - detection lives in [`crate::account`] (`plex.discover` / `probe`),
//! - the tier-2 restart action lives in [`crate::lifecycle`].
//!
//! This module owns only the *policy* that sequences them; it holds no
//! transport and no process exec of its own. Keeping it separate leaves
//! `account` pure-detection and `lifecycle` pure-action.
//!
//! `plex.heal` is a single-shot ladder for one server:
//!   1. **Tier-1 nudge** — `probe()` queries plex.tv, which re-announces
//!      presence (observer effect). This alone fixed the 2026-07-22 mimir drift.
//!   2. **Tier-1 verdict** — read the target server's freshness.
//!   3. **Tier-2 restart** — only if still `stale`, restart the service in its
//!      guest (host-local `pct`/`docker`, mesh-independent).
//!
//! Deliberately explicit about the guest (runtime + vmid/container): the
//! endpoint→runtime topology mapping that would let this be inferred is a known
//! gap (see `docs/self-remediation.md`), so the caller supplies it for now.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;

use crate::account::{self, PresenceStatus, ServerPresence};
use crate::lifecycle::{self, PlexRestartArgs, Runtime};

/// What the ladder decided to do for the target server, given its post-nudge
/// freshness. A pure classification — no side effects — so it is unit-testable
/// in isolation from HTTP and process exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealPlan {
    /// Target is `fresh` after the nudge — nothing more to do.
    NudgeSufficient,
    /// Target is `stale` — escalate to a tier-2 restart.
    Restart,
    /// plex.tv gave no `lastSeenAt` — cannot judge; do not blindly restart.
    CannotJudge,
    /// No owned server matched the requested target.
    TargetNotFound,
}

impl HealPlan {
    fn from_status(status: Option<PresenceStatus>) -> Self {
        match status {
            Some(PresenceStatus::Fresh) => Self::NudgeSufficient,
            Some(PresenceStatus::Stale) => Self::Restart,
            Some(PresenceStatus::Unknown) => Self::CannotJudge,
            None => Self::TargetNotFound,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NudgeSufficient => "nudge_sufficient",
            Self::Restart => "restart",
            Self::CannotJudge => "cannot_judge",
            Self::TargetNotFound => "target_not_found",
        }
    }
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexHealArgs {
    /// Registered endpoint whose token is used as the plex.tv account token.
    pub endpoint: String,
    /// The owned server to heal — matched against `clientIdentifier` or `name`.
    pub target: String,
    /// Staleness threshold in seconds; older announces are `stale`. Default 600.
    #[arg(long)]
    #[serde(default)]
    pub threshold_secs: Option<i64>,
    /// Where the target runs: `lxc` or `docker` (tier-2 restart path).
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid (LXC runtime only). Required when `runtime=lxc`.
    #[arg(long)]
    #[serde(default)]
    pub vmid: Option<u32>,
    /// Docker container name (Docker runtime only).
    #[arg(long, default_value = "plex")]
    #[serde(default = "default_container")]
    pub container: String,
}

fn default_container() -> String {
    "plex".to_string()
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexHealReport {
    /// The target identifier that was requested.
    pub target: String,
    /// The matched server's presence after the tier-1 nudge, if found.
    pub matched: Option<ServerPresence>,
    /// What the ladder decided: `nudge_sufficient` / `restart` / `cannot_judge`
    /// / `target_not_found`.
    pub plan: String,
    /// True when a tier-2 restart was executed and succeeded.
    pub restarted: bool,
    /// Effective staleness threshold applied.
    pub threshold_secs: i64,
}

/// Find the owned server matching `target` by `clientIdentifier` or `name`.
fn match_target<'a>(servers: &'a [ServerPresence], target: &str) -> Option<&'a ServerPresence> {
    servers.iter().find(|s| {
        s.client_identifier.as_deref() == Some(target) || s.name.as_deref() == Some(target)
    })
}

/// **Self-heal one Plex server.** Runs the ladder: query plex.tv (tier-1 nudge
/// that re-announces presence), read the target's freshness, and — only if it
/// is still `stale` — restart the service in its guest (tier-2). Returns what it
/// found and what it did.
#[orca_tool(domain = "plex", verb = "heal")]
async fn plex_heal(args: PlexHealArgs, _ctx: &ToolCtx) -> Result<PlexHealReport> {
    let threshold = args
        .threshold_secs
        .unwrap_or(account::DEFAULT_THRESHOLD_SECS);

    // Resolve the token at the boundary; it never leaves this scope.
    let token = crate::tools::endpoint_token(&args.endpoint)?;

    // Tier 1: the probe itself is the re-announce nudge.
    let servers = account::probe(token, threshold).await?;
    let matched = match_target(&servers, &args.target).cloned();
    let plan = HealPlan::from_status(matched.as_ref().map(|s| s.status));

    // Tier 2: restart only on a confirmed stale verdict.
    let restarted = if plan == HealPlan::Restart {
        lifecycle::restart_service(PlexRestartArgs {
            runtime: args.runtime,
            vmid: args.vmid,
            container: args.container,
        })
        .await?;
        true
    } else {
        false
    };

    // Tier 3 (notify on unresolved stale) is a documented next rung; it needs
    // the notify seam wired and belongs after a post-restart re-probe confirms
    // the announce did not recover. Not built this session.

    Ok(PlexHealReport {
        target: args.target,
        matched,
        plan: plan.label().to_string(),
        restarted,
        threshold_secs: threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(id: &str, status: PresenceStatus) -> ServerPresence {
        ServerPresence {
            name: Some(id.to_string()),
            client_identifier: Some(format!("cid-{id}")),
            product: Some("Plex Media Server".to_string()),
            owned: true,
            presence: true,
            last_seen_at: Some(1),
            last_seen_age_secs: Some(0),
            status,
        }
    }

    #[test]
    fn plan_maps_status_to_action() {
        assert_eq!(
            HealPlan::from_status(Some(PresenceStatus::Fresh)),
            HealPlan::NudgeSufficient
        );
        assert_eq!(
            HealPlan::from_status(Some(PresenceStatus::Stale)),
            HealPlan::Restart
        );
        assert_eq!(
            HealPlan::from_status(Some(PresenceStatus::Unknown)),
            HealPlan::CannotJudge
        );
        assert_eq!(HealPlan::from_status(None), HealPlan::TargetNotFound);
    }

    #[test]
    fn match_target_by_name_or_client_identifier() {
        let servers = vec![
            presence("mimir", PresenceStatus::Stale),
            presence("njord", PresenceStatus::Fresh),
        ];
        assert!(match_target(&servers, "mimir").is_some());
        assert!(match_target(&servers, "cid-njord").is_some());
        assert!(match_target(&servers, "loki").is_none());
    }

    #[test]
    fn stale_target_plans_restart() {
        let servers = vec![presence("mimir", PresenceStatus::Stale)];
        let m = match_target(&servers, "mimir");
        assert_eq!(
            HealPlan::from_status(m.map(|s| s.status)),
            HealPlan::Restart
        );
    }
}
