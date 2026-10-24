#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use plugin_toolkit::privileged::{self, PrivilegedOps, ORCA_BIN_ENV};
use tokio::process::Command;

const PLUGIN: &str = "plex";
pub const OP: &str = "guest_exec";

const ALLOWED: &[&str] = &[
    "/usr/lib/plexmediaserver/Plex SQLite",
    "systemctl",
    "findmnt",
    "cp",
    "env",
    "bash",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuestExec {
    pub vmid: u32,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub success: bool,
    pub status: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl From<std::process::Output> for ExecOutput {
    fn from(o: std::process::Output) -> Self {
        Self {
            success: o.status.success(),
            status: o.status.to_string(),
            stdout: o.stdout,
            stderr: o.stderr,
        }
    }
}

pub fn pct_exec_request(cmd: &Command) -> Option<GuestExec> {
    let std = cmd.as_std();
    if std.get_program() != "pct" {
        return None;
    }
    let args: Vec<String> = std
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match args.as_slice() {
        [exec, vmid, sep, argv @ ..] if exec == "exec" && sep == "--" && !argv.is_empty() => {
            Some(GuestExec {
                vmid: vmid.parse().ok()?,
                argv: argv.to_vec(),
            })
        }
        _ => None,
    }
}

pub fn validate(req: &GuestExec) -> Result<()> {
    let Some(prog) = req.argv.first() else {
        bail!("empty argv");
    };
    if !ALLOWED.contains(&prog.as_str()) {
        bail!("{prog:?} is not on the plex guest_exec allowlist");
    }
    Ok(())
}

fn seam_available() -> bool {
    std::env::var(ORCA_BIN_ENV).is_ok_and(|v| !v.is_empty())
}

pub(crate) async fn output(mut cmd: Command) -> Result<ExecOutput> {
    if seam_available() {
        if let Some(req) = pct_exec_request(&cmd) {
            validate(&req)?;
            let detail = privileged::apply(PLUGIN, OP, Some(&req))
                .await
                .with_context(|| format!("{PLUGIN} {OP} through `orca admin plugin-apply`"))?;
            return plugin_toolkit::serde_json::from_str(&detail)
                .context("decode the guest_exec privileged reply");
        }
    }
    Ok(cmd
        .output()
        .await
        .context("failed to spawn command")?
        .into())
}

async fn run_privileged(req: GuestExec) -> Result<String> {
    if privileged::caller_uid().is_none() {
        bail!("caller uid is missing or invalid; refusing guest_exec");
    }
    validate(&req)?;
    let out: ExecOutput = Command::new("pct")
        .arg("exec")
        .arg(req.vmid.to_string())
        .arg("--")
        .args(&req.argv)
        .kill_on_drop(true)
        .output()
        .await
        .context("spawn pct exec")?
        .into();
    Ok(plugin_toolkit::serde_json::to_string(&out)?)
}

pub fn privileged_ops() -> PrivilegedOps {
    PrivilegedOps::new().op(OP, run_privileged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_pct_exec_request() {
        let mut c = Command::new("pct");
        c.arg("exec")
            .arg("112")
            .arg("--")
            .arg("systemctl")
            .arg("restart")
            .arg("plexmediaserver");
        assert_eq!(
            pct_exec_request(&c),
            Some(GuestExec {
                vmid: 112,
                argv: vec![
                    "systemctl".into(),
                    "restart".into(),
                    "plexmediaserver".into()
                ],
            })
        );
    }

    #[test]
    fn ignores_non_pct_and_malformed() {
        let mut d = Command::new("docker");
        d.arg("exec").arg("plex").arg("systemctl");
        assert_eq!(pct_exec_request(&d), None);
        let mut bad = Command::new("pct");
        bad.arg("exec").arg("abc").arg("--").arg("true");
        assert_eq!(pct_exec_request(&bad), None);
        let mut nosep = Command::new("pct");
        nosep.arg("start").arg("112");
        assert_eq!(pct_exec_request(&nosep), None);
    }

    #[test]
    fn allowlist() {
        let ok = GuestExec {
            vmid: 1,
            argv: vec!["/usr/lib/plexmediaserver/Plex SQLite".into(), "db".into()],
        };
        assert!(validate(&ok).is_ok());
        let bad = GuestExec {
            vmid: 1,
            argv: vec!["rm".into(), "-rf".into(), "/".into()],
        };
        assert!(validate(&bad).is_err());
        assert!(validate(&GuestExec {
            vmid: 1,
            argv: vec![]
        })
        .is_err());
    }
}
