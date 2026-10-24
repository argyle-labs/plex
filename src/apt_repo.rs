#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;

use crate::guest_exec::{exec, Guest, GuestOp};
use crate::lifecycle::Runtime;

pub(crate) const SCRIPT: &str = include_str!("../scripts/apt-repo.sh");

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexAptRepoRepairArgs {
    /// Where the instance runs: `lxc` or `docker`.
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid or docker container name/id.
    #[arg(long)]
    pub target: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, PartialEq)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexAptRepoRepairOutput {
    pub dry_run: bool,
    pub changed: bool,
    pub actions: Vec<String>,
}

pub(crate) fn valid_target(runtime: Runtime, target: &str) -> bool {
    match runtime {
        Runtime::Lxc => !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()),
        Runtime::Docker => {
            target
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
                && target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        }
    }
}

pub(crate) async fn repair(guest: &Guest, dry_run: bool) -> Result<String> {
    exec(guest, GuestOp::AptRepoRepair { dry_run })
        .await?
        .checked("repair plex apt source")
}

fn parse_output(stdout: &str, dry_run: bool) -> PlexAptRepoRepairOutput {
    let actions: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    PlexAptRepoRepairOutput {
        dry_run,
        changed: !dry_run && !actions.is_empty(),
        actions,
    }
}

/// **Repair the Plex apt source** in its guest: removes stale `downloads.plex.tv`
/// sources and the old keyring, installs the v2 key, and writes the canonical
/// `repo.plex.tv` source. Idempotent; `dryRun` lists the actions without applying.
#[orca_tool(
    domain = "plex",
    verb = "apt_repo_repair",
    role = "admin",
    execute_gated = false,
    dry_run_aware = true
)]
async fn plex_apt_repo_repair(
    args: PlexAptRepoRepairArgs,
    ctx: &ToolCtx,
) -> Result<PlexAptRepoRepairOutput> {
    let guest = Guest::parse(args.runtime, &args.target)?;
    let dry_run = ctx.dry_run();
    let stdout = repair(&guest, dry_run).await?;
    Ok(parse_output(&stdout, dry_run))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_reject_option_injection() {
        assert!(valid_target(Runtime::Lxc, "105"));
        assert!(!valid_target(Runtime::Lxc, "-1"));
        assert!(!valid_target(Runtime::Lxc, ""));
        assert!(valid_target(Runtime::Docker, "plex_1.a-b"));
        assert!(!valid_target(Runtime::Docker, "--privileged"));
        assert!(!valid_target(Runtime::Docker, "plex;rm"));
    }

    #[test]
    fn script_installs_new_source_before_removing_legacy() {
        let write = SCRIPT.find("act \"write $LIST\"").unwrap();
        let remove = SCRIPT.find("act \"remove $f\"").unwrap();
        let install = SCRIPT.find("act \"install $KEYRING\"").unwrap();
        assert!(install < write && write < remove);
        assert!(SCRIPT.contains("mv -f \"$dearmored\" \"$KEYRING\""));
        assert!(SCRIPT.contains("mv -f \"$tmp\" \"$LIST\""));
    }

    #[test]
    fn script_targets_v2_repo_and_cleans_legacy() {
        assert!(SCRIPT.contains("https://repo.plex.tv/deb/ public main"));
        assert!(SCRIPT.contains("PlexSign.v2.key"));
        assert!(SCRIPT.contains("/usr/share/keyrings/plexmediaserver.v2.gpg"));
        assert!(SCRIPT.contains("LIST=\"/etc/apt/sources.list.d/plex.list\""));
        assert!(SCRIPT.contains("/etc/apt/sources.list.d/plexmediaserver.list"));
        assert!(SCRIPT.contains("plex-archive-keyring.gpg"));
        assert!(!SCRIPT.contains("downloads.plex.tv/repo"));
    }

    #[test]
    fn converged_guest_reports_no_change() {
        let out = parse_output("", false);
        assert_eq!(
            out,
            PlexAptRepoRepairOutput {
                dry_run: false,
                changed: false,
                actions: vec![],
            }
        );
    }

    #[test]
    fn applied_actions_report_changed() {
        let out = parse_output("remove /etc/apt/sources.list.d/plexmediaserver.list\nwrite /etc/apt/sources.list.d/plex.list\napt-get update\n", false);
        assert!(out.changed);
        assert_eq!(out.actions.len(), 3);
    }

    #[test]
    fn dry_run_lists_actions_without_change() {
        let out = parse_output("install /usr/share/keyrings/plexmediaserver.v2.gpg\n", true);
        assert!(out.dry_run);
        assert!(!out.changed);
        assert_eq!(
            out.actions,
            ["install /usr/share/keyrings/plexmediaserver.v2.gpg"]
        );
    }

    #[tokio::test]
    async fn script_dry_run_on_clean_host_plans_without_writing() {
        let out = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(SCRIPT)
            .env("DRY_RUN", "1")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let plan = parse_output(&String::from_utf8_lossy(&out.stdout), true);
        assert!(plan.actions.iter().all(|a| a.starts_with("remove ")
            || a.starts_with("install ")
            || a.starts_with("write ")
            || a == "apt-get update"));
    }
}
