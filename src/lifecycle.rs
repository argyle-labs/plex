//! Plex deployment lifecycle tool surface.
//!
//! Net-new over the diagnosis surface: these `#[orca_tool]`s own the full
//! deploy lifecycle of a Plex instance — provision, version bump, and config
//! backup/restore — driving the host's container runtime (`pct` for Proxmox
//! LXC, `docker` for Compose) and `tar` for the `/config` volume through
//! `tokio::process::Command`. There is no parallel shell glue: the bootstrap
//! scripts in `scripts/` + `lxc/` are the curl-bootstrap payload these tools
//! orchestrate, and every capability is reachable as an orca tool.
//!
//! Imports flow through `plugin_toolkit::prelude::*` only — the toolkit is the
//! single gateway. Process exec uses the toolkit's re-exported `tokio`.
#![allow(clippy::disallowed_types)]

use std::path::Path;
use std::process::Output;

use plugin_toolkit::prelude::*;
use plugin_toolkit::tokio::process::Command;

/// Where a Plex instance is deployed — selects which runtime the lifecycle
/// tools drive.
#[derive(
    Debug,
    Clone,
    Copy,
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
    plugin_toolkit::clap::ValueEnum,
    Default,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// Proxmox LXC, driven via `pct`.
    #[default]
    Lxc,
    /// Docker / Compose, driven via `docker`.
    Docker,
}

/// Release channel for `plex.update`. Maps to an image/package version.
#[derive(
    Clone,
    Copy,
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
    plugin_toolkit::clap::ValueEnum,
    Default,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Newest published public Plex release.
    #[default]
    Latest,
    /// Plex Pass beta channel.
    Beta,
    /// Pinned public stable line.
    Stable,
}

impl Channel {
    /// The container image tag this channel resolves to. Plex publishes
    /// `latest`, `beta`, and `public` tags on `plexinc/pms-docker`.
    fn image_tag(self) -> &'static str {
        match self {
            Channel::Latest => "latest",
            Channel::Beta => "beta",
            Channel::Stable => "public",
        }
    }
}

/// Run a command, capturing output, and map a non-zero exit to an error that
/// carries stderr — the lifecycle tools surface the runtime's own message
/// rather than a bare exit code.
async fn run(cmd: &mut Command) -> Result<Output> {
    let output = cmd
        .output()
        .await
        .with_context(|| "failed to spawn command".to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed ({}): {}", output.status, stderr.trim());
    }
    Ok(output)
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.install — provision an LXC or Compose deployment
// ═══════════════════════════════════════════════════════════════════════════

#[derive(
    plugin_toolkit::clap::Args,
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexInstallArgs {
    /// Where to deploy: `lxc` (Proxmox) or `docker` (Compose).
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid (LXC runtime only). Required when `runtime=lxc`.
    #[arg(long)]
    #[serde(default)]
    pub vmid: Option<u32>,
    /// Host path for the persistent `/config` volume.
    #[arg(long, default_value = "/opt/plex/config")]
    #[serde(default = "default_config_path")]
    pub config_path: String,
    /// Host path to the media library, mounted read-only.
    #[arg(long)]
    #[serde(default)]
    pub media_path: Option<String>,
    /// Skip GPU passthrough (software-transcode-only minimal deploy).
    #[arg(long)]
    #[serde(default)]
    pub no_gpu: bool,
    /// Path to the bootstrap `provision.sh` (LXC) or `compose.yml` (Docker).
    /// Defaults to the repo-relative asset; override for a non-standard layout.
    #[arg(long)]
    #[serde(default)]
    pub bootstrap_path: Option<String>,
}

fn default_config_path() -> String {
    "/opt/plex/config".to_string()
}

#[derive(
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
#[derive(Debug)]
pub struct PlexInstallOutput {
    /// True when the provisioning command completed successfully.
    pub provisioned: bool,
    /// The runtime the deployment targeted.
    pub runtime: Runtime,
    /// Combined stdout from the provisioning step.
    pub log: String,
}

/// **Provision a Plex deployment.** On `lxc`, runs the Proxmox `provision.sh`
/// bootstrap (create CT, GPU passthrough, install, start). On `docker`, brings
/// up the Compose stack. GPU passthrough is wired by default; pass `no_gpu`
/// for a software-only minimal install.
#[orca_tool(domain = "plex", verb = "install")]
async fn plex_install(args: PlexInstallArgs, _ctx: &ToolCtx) -> Result<PlexInstallOutput> {
    let output = match args.runtime {
        Runtime::Lxc => {
            let vmid = args.vmid.context("`vmid` is required when runtime=lxc")?;
            let script = args
                .bootstrap_path
                .clone()
                .unwrap_or_else(|| "lxc/provision.sh".to_string());
            let mut cmd = Command::new("bash");
            cmd.arg(&script).arg(vmid.to_string());
            cmd.arg("--config").arg(&args.config_path);
            if let Some(media) = &args.media_path {
                cmd.arg("--media").arg(media);
            }
            if args.no_gpu {
                cmd.arg("--no-gpu");
            }
            run(&mut cmd).await?
        }
        Runtime::Docker => {
            let compose = args
                .bootstrap_path
                .clone()
                .unwrap_or_else(|| "compose.yml".to_string());
            let mut cmd = Command::new("docker");
            cmd.arg("compose")
                .arg("-f")
                .arg(&compose)
                .arg("up")
                .arg("-d");
            run(&mut cmd).await?
        }
    };
    Ok(PlexInstallOutput {
        provisioned: true,
        runtime: args.runtime,
        log: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.update — channel-aware image/version bump
// ═══════════════════════════════════════════════════════════════════════════

#[derive(
    plugin_toolkit::clap::Args,
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexUpdateArgs {
    /// Where the instance runs: `lxc` or `docker`.
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// Release channel to move to. `latest` / `beta` / `stable`.
    #[arg(long, value_enum, default_value_t = Channel::Latest)]
    #[serde(default)]
    pub channel: Channel,
    /// LXC vmid (LXC runtime only).
    #[arg(long)]
    #[serde(default)]
    pub vmid: Option<u32>,
    /// Compose file (Docker runtime only).
    #[arg(long, default_value = "compose.yml")]
    #[serde(default = "default_compose")]
    pub compose_file: String,
}

fn default_compose() -> String {
    "compose.yml".to_string()
}

#[derive(
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
#[derive(Debug)]
pub struct PlexUpdateOutput {
    /// True when the update command completed.
    pub updated: bool,
    /// Image tag the channel resolved to.
    pub image_tag: String,
    /// Combined stdout from the update step.
    pub log: String,
}

/// **Update a Plex deployment** to the head of a release channel. On `docker`,
/// re-pulls the channel image tag and recreates the container. On `lxc`, runs
/// the in-CT package upgrade.
#[orca_tool(domain = "plex", verb = "update")]
async fn plex_update(args: PlexUpdateArgs, _ctx: &ToolCtx) -> Result<PlexUpdateOutput> {
    let tag = args.channel.image_tag();
    let output = match args.runtime {
        Runtime::Docker => {
            let image = format!("plexinc/pms-docker:{tag}");
            run(Command::new("docker").arg("pull").arg(&image)).await?;
            run(Command::new("docker")
                .arg("compose")
                .arg("-f")
                .arg(&args.compose_file)
                .arg("up")
                .arg("-d"))
            .await?
        }
        Runtime::Lxc => {
            let vmid = args.vmid.context("`vmid` is required when runtime=lxc")?;
            run(Command::new("pct").arg("exec").arg(vmid.to_string()).arg("--").arg("bash").arg(
                "-c",
            ).arg(
                "apt-get update && apt-get install -y --only-upgrade plexmediaserver && systemctl restart plexmediaserver",
            ))
            .await?
        }
    };
    Ok(PlexUpdateOutput {
        updated: true,
        image_tag: tag.to_string(),
        log: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.backup — tar the /config volume to a destination
// ═══════════════════════════════════════════════════════════════════════════

#[derive(
    plugin_toolkit::clap::Args,
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexBackupArgs {
    /// Host path of the Plex `/config` volume to archive.
    #[arg(long, default_value = "/opt/plex/config")]
    #[serde(default = "default_config_path")]
    pub config_path: String,
    /// Directory to write the `.tar.gz` into. Created if missing.
    #[arg(long)]
    pub destination: String,
}

#[derive(
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
#[derive(Debug)]
pub struct PlexBackupOutput {
    /// Absolute path of the archive written.
    pub archive: String,
}

/// **Back up the Plex `/config` volume** to a `.tar.gz` in the destination
/// directory. The Plex-regenerated `Cache/`, `Crash Reports/`, and `Logs/`
/// trees are excluded — only durable Preferences/metadata/databases is
/// archived.
#[orca_tool(domain = "plex", verb = "backup")]
async fn plex_backup(args: PlexBackupArgs, _ctx: &ToolCtx) -> Result<PlexBackupOutput> {
    backup_config(&args).await
}

/// Archive logic, independent of the tool context so it is directly testable.
async fn backup_config(args: &PlexBackupArgs) -> Result<PlexBackupOutput> {
    let config = Path::new(&args.config_path);
    if !config.is_dir() {
        bail!("config path '{}' is not a directory", args.config_path);
    }
    run(Command::new("mkdir").arg("-p").arg(&args.destination)).await?;

    let stamp = now_stamp();
    let archive = format!(
        "{}/plex-config-{}.tar.gz",
        args.destination.trim_end_matches('/'),
        stamp
    );

    run(Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("--exclude=./Cache")
        .arg("--exclude=./Crash Reports")
        .arg("--exclude=./Logs")
        .arg("-C")
        .arg(&args.config_path)
        .arg("."))
    .await?;

    Ok(PlexBackupOutput { archive })
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.restore — restore the /config volume from a tarball
// ═══════════════════════════════════════════════════════════════════════════

#[derive(
    plugin_toolkit::clap::Args,
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexRestoreArgs {
    /// The backup tarball to restore from.
    #[arg(long = "from")]
    pub from: String,
    /// Host path of the `/config` volume to restore into. Created if missing.
    #[arg(long, default_value = "/opt/plex/config")]
    #[serde(default = "default_config_path")]
    pub config_path: String,
}

#[derive(
    plugin_toolkit::serde::Serialize,
    plugin_toolkit::serde::Deserialize,
    plugin_toolkit::schemars::JsonSchema,
)]
#[serde(crate = "plugin_toolkit::serde")]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
#[derive(Debug)]
pub struct PlexRestoreOutput {
    /// True when extraction completed.
    pub restored: bool,
    /// Where the config was restored to.
    pub config_path: String,
}

/// **Restore the Plex `/config` volume** from a `.tar.gz` produced by
/// `plex.backup`. The service should be stopped before restoring; this tool
/// only extracts the archive over the config directory.
#[orca_tool(domain = "plex", verb = "restore")]
async fn plex_restore(args: PlexRestoreArgs, _ctx: &ToolCtx) -> Result<PlexRestoreOutput> {
    restore_config(args).await
}

/// Extraction logic, independent of the tool context so it is directly testable.
async fn restore_config(args: PlexRestoreArgs) -> Result<PlexRestoreOutput> {
    if !Path::new(&args.from).is_file() {
        bail!("backup tarball '{}' not found", args.from);
    }
    run(Command::new("mkdir").arg("-p").arg(&args.config_path)).await?;
    run(Command::new("tar")
        .arg("-xzf")
        .arg(&args.from)
        .arg("-C")
        .arg(&args.config_path))
    .await?;
    Ok(PlexRestoreOutput {
        restored: true,
        config_path: args.config_path,
    })
}

/// UTC timestamp `YYYYMMDD-HHMMSS` for archive names. Uses chrono (already a
/// plugin dep via progenitor's date-time formats).
fn now_stamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_maps_to_image_tag() {
        assert_eq!(Channel::Latest.image_tag(), "latest");
        assert_eq!(Channel::Beta.image_tag(), "beta");
        assert_eq!(Channel::Stable.image_tag(), "public");
    }

    #[tokio::test]
    async fn backup_rejects_missing_config_dir() {
        let args = PlexBackupArgs {
            config_path: "/nonexistent/plex/config/path".to_string(),
            destination: "/tmp/plex-test-dest".to_string(),
        };
        let err = backup_config(&args).await.unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[tokio::test]
    async fn restore_rejects_missing_tarball() {
        let args = PlexRestoreArgs {
            from: "/nonexistent/backup.tar.gz".to_string(),
            config_path: "/tmp/plex-test-restore".to_string(),
        };
        let err = restore_config(args).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn backup_then_restore_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir_all(config.join("Databases")).unwrap();
        std::fs::write(config.join("Preferences.xml"), b"<Preferences/>").unwrap();
        std::fs::write(
            config
                .join("Databases")
                .join("com.plexapp.plugins.library.db"),
            b"sqlite",
        )
        .unwrap();
        // Cache must be excluded
        std::fs::create_dir_all(config.join("Cache")).unwrap();
        std::fs::write(config.join("Cache").join("junk"), b"x").unwrap();

        let dest = tmp.path().join("backups");
        let out = backup_config(&PlexBackupArgs {
            config_path: config.to_string_lossy().into_owned(),
            destination: dest.to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
        assert!(Path::new(&out.archive).is_file());

        let restore_target = tmp.path().join("restored");
        restore_config(PlexRestoreArgs {
            from: out.archive.clone(),
            config_path: restore_target.to_string_lossy().into_owned(),
        })
        .await
        .unwrap();

        assert!(restore_target.join("Preferences.xml").is_file());
        assert!(restore_target
            .join("Databases")
            .join("com.plexapp.plugins.library.db")
            .is_file());
        // Cache was excluded from the archive
        assert!(!restore_target.join("Cache").exists());
    }
}
