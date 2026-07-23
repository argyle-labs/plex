//! plex.tv **account** layer — authenticates to the account and runs the
//! **synthetic-player presence probe**, and will host the self-heal remediation
//! ladder.
//!
//! The server-local `crate::Client` answers "is this box healthy from the
//! inside." It cannot answer the question that actually matters to a viewer:
//! *can a player out on the internet still see this server?* That signal lives
//! only on plex.tv, in each owned server's `lastSeenAt` announce timestamp.
//!
//! Proven by the 2026-07-22 mimir incident: the server was 100% healthy locally
//! (process up, `/identity` 200, NFS mounted, `signInState=ok`,
//! `mappingState=mapped`, both plex.direct URIs 200, port-forward open) yet was
//! **invisible to players** because its plex.tv announce had gone stale
//! (`lastSeenAt` frozen ~20h). Only a token'd query to plex.tv revealed it — and
//! that same query *revived* the announce (observer effect).
//!
//! That is why the discovery call is a **three-in-one primitive**: discovery +
//! staleness detector + **tier-1 self-heal nudge**. Calling it periodically as a
//! heartbeat re-announces presence and prevents the drift in the first place. It
//! is idempotent, cheap, and mesh-independent — safe to run on a timer. No
//! scheduler is built here; the verb is simply safe to schedule.
//!
//! Split by concern: [`client`] owns transport + wire parsing, [`presence`] owns
//! the verdict + output shape, this module owns orchestration.

pub mod client;
pub mod presence;

pub use presence::{PresenceStatus, ServerPresence};

use crate::PlexError;
use client::PlexTvClient;

/// Default staleness threshold: presence older than this is `Stale`. 600s
/// (10 min) sits inside the normal plex.tv announce window, so it flags a
/// genuinely quiet server without false-positiving on routine keepalive jitter.
pub const DEFAULT_THRESHOLD_SECS: i64 = 600;

/// Probe plex.tv for every **owned** server and return each one's presence with
/// a derived freshness verdict. Also serves as the tier-1 heartbeat nudge — the
/// query itself re-announces presence.
///
/// The token is an account token and must already be resolved from the secret
/// seam by the caller — it is never logged or surfaced here.
pub async fn probe(token: String, threshold_secs: i64) -> Result<Vec<ServerPresence>, PlexError> {
    let raws = PlexTvClient::new(token).resources().await?;
    let now = plugin_toolkit::time::now().unix_seconds();
    Ok(raws
        .into_iter()
        .filter(|r| r.owned)
        .map(|r| {
            let age = r.last_seen_at.map(|ls| now - ls);
            ServerPresence {
                name: r.name,
                client_identifier: r.client_identifier,
                product: r.product,
                owned: r.owned,
                presence: r.presence,
                last_seen_at: r.last_seen_at,
                last_seen_age_secs: age,
                status: PresenceStatus::for_age(age, threshold_secs),
            }
        })
        .collect())
}

// ═══════════════════════════════════════════════════════════════════════════
// REMEDIATION LADDER — attach points (design only; not built this session).
//
//   Tier 1  ▸ nudge         — DONE. `probe()` / `plex.discover`: the plex.tv
//                             query re-announces presence. Run on a timer as the
//                             heartbeat that prevents drift.
//   Tier 2  ▸ restart       — TODO. If a server stays `Stale` for N consecutive
//                             probes, `systemctl restart plexmediaserver` INSIDE
//                             the guest. MUST NOT depend on orca mesh health at
//                             that moment (during the incident orca's lxc adapter
//                             on thor returned zero containers, and
//                             thor/mimir.scottkey.me DNS both resolve to
//                             baldur/caddy 10.10.10.6). Reach the guest directly.
//   Tier 3  ▸ notify        — TODO. Escalate via orca's notify seam when tier 2
//                             fails to restore a fresh announce.
//   Watchdog ▸ in-CT timer  — TODO. Plugin-provisioned systemd timer / onboot
//                             unit inside the guest as the mesh-independent safety
//                             net that runs the tier-1 nudge locally even if orca
//                             can't reach the box.
// ═══════════════════════════════════════════════════════════════════════════
