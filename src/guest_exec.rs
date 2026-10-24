#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use plugin_toolkit::privileged::{self, PrivilegedOps, ORCA_BIN_ENV};
use tokio::process::Command;

use crate::apt_repo::{valid_target, SCRIPT};
use crate::lifecycle::Runtime;
use crate::security::{safe_codec_path, valid_data_dir, valid_user, GREP_PATTERNS};

const PLUGIN: &str = "plex";
const PLEX_SQLITE: &str = "/usr/lib/plexmediaserver/Plex SQLite";
const PLEX_UNIT: &str = "plexmediaserver";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guest {
    Lxc(u32),
    Docker(String),
}

impl Guest {
    pub fn parse(runtime: Runtime, target: &str) -> Result<Self> {
        if !valid_target(runtime, target) {
            bail!("invalid target {target:?}");
        }
        Ok(match runtime {
            Runtime::Lxc => Self::Lxc(target.parse().context("vmid")?),
            Runtime::Docker => Self::Docker(target.to_string()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Stop,
    Start,
    Restart,
}

impl ServiceAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Start => "start",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GuestOp {
    AptRepoRepair {
        dry_run: bool,
    },
    AptUpgradePlex,
    PlexVersion,
    Sql {
        db: String,
        sql: String,
    },
    DbBackup {
        db: String,
        stamp: String,
    },
    Service {
        action: ServiceAction,
    },
    StorageProbe,
    SecurityListCodecs {
        data_dir: String,
    },
    SecurityListLogs {
        data_dir: String,
    },
    SecurityGrepLogs {
        data_dir: String,
        logs: Vec<String>,
    },
    SecurityDataDirOwner {
        data_dir: String,
    },
    SecurityQuarantine {
        data_dir: String,
        user: String,
        src: String,
        dest: String,
    },
    KillTranscoders {
        user: String,
    },
}

pub const OP_NAMES: &[&str] = &[
    "apt_repo_repair",
    "apt_upgrade_plex",
    "plex_version",
    "sql",
    "db_backup",
    "service",
    "storage_probe",
    "security_list_codecs",
    "security_list_logs",
    "security_grep_logs",
    "security_data_dir_owner",
    "security_quarantine",
    "kill_transcoders",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub user: Option<String>,
    pub argv: Vec<String>,
}

fn step(argv: &[&str]) -> Step {
    Step {
        user: None,
        argv: argv.iter().map(|s| s.to_string()).collect(),
    }
}

fn step_as(user: &str, argv: &[&str]) -> Step {
    Step {
        user: Some(user.to_string()),
        ..step(argv)
    }
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn valid_db(db: &str) -> bool {
    valid_data_dir(db) && db.ends_with(".db")
}

fn strip_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_lit = false;
    for c in sql.chars() {
        if c == '\'' {
            in_lit = !in_lit;
            out.push(' ');
        } else if !in_lit {
            out.push(c);
        }
    }
    out
}

pub fn valid_sql(sql: &str) -> bool {
    let code = strip_literals(sql);
    let lower = code.to_ascii_lowercase();
    !sql.trim().is_empty()
        && !code.lines().any(|l| l.trim_start().starts_with('.'))
        && !lower.contains("attach")
        && !lower.contains("load_extension")
        && !lower.contains("writefile")
        && !lower.contains("readfile")
        && !sql.contains('\0')
}

fn valid_stamp(stamp: &str) -> bool {
    !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

fn valid_log(data_dir: &str, log: &str) -> bool {
    let prefix = format!("{}/Logs/", data_dir.trim_end_matches('/'));
    log.strip_prefix(&prefix).is_some_and(|name| {
        name.starts_with("Plex")
            && name.ends_with(".log")
            && !name.contains('/')
            && !has_control(name)
    })
}

fn quarantine_dir(data_dir: &str) -> String {
    format!("{}/Codecs.quarantine", data_dir.trim_end_matches('/'))
}

fn valid_quarantine_dest(data_dir: &str, dest: &str) -> bool {
    dest.strip_prefix(&format!("{}/", quarantine_dir(data_dir)))
        .is_some_and(|name| {
            name.ends_with(".quarantined")
                && !name.contains('/')
                && name != ".."
                && !has_control(name)
        })
}

fn need(ok: bool, what: &str) -> Result<()> {
    if !ok {
        bail!("invalid {what}");
    }
    Ok(())
}

impl GuestOp {
    pub fn name(&self) -> &'static str {
        match self {
            Self::AptRepoRepair { .. } => "apt_repo_repair",
            Self::AptUpgradePlex => "apt_upgrade_plex",
            Self::PlexVersion => "plex_version",
            Self::Sql { .. } => "sql",
            Self::DbBackup { .. } => "db_backup",
            Self::Service { .. } => "service",
            Self::StorageProbe => "storage_probe",
            Self::SecurityListCodecs { .. } => "security_list_codecs",
            Self::SecurityListLogs { .. } => "security_list_logs",
            Self::SecurityGrepLogs { .. } => "security_grep_logs",
            Self::SecurityDataDirOwner { .. } => "security_data_dir_owner",
            Self::SecurityQuarantine { .. } => "security_quarantine",
            Self::KillTranscoders { .. } => "kill_transcoders",
        }
    }

    pub fn steps(&self) -> Result<Vec<Step>> {
        Ok(match self {
            Self::AptRepoRepair { dry_run } => vec![step(&[
                "env",
                if *dry_run { "DRY_RUN=1" } else { "DRY_RUN=0" },
                "bash",
                "-c",
                SCRIPT,
            ])],
            Self::AptUpgradePlex => vec![
                step(&["apt-get", "update"]),
                step(&["apt-get", "install", "-y", "--only-upgrade", PLEX_UNIT]),
                step(&["systemctl", "restart", PLEX_UNIT]),
            ],
            Self::PlexVersion => vec![step(&["dpkg-query", "-W", "-f=${Version}", PLEX_UNIT])],
            Self::Sql { db, sql } => {
                need(valid_db(db), "db path")?;
                need(valid_sql(sql), "sql statement")?;
                vec![step(&[PLEX_SQLITE, db, sql])]
            }
            Self::DbBackup { db, stamp } => {
                need(valid_db(db), "db path")?;
                need(valid_stamp(stamp), "stamp")?;
                let dest = format!("{db}.orca-bak-{stamp}");
                vec![step(&["cp", "-a", "--", db, &dest])]
            }
            Self::Service { action } => vec![step(&["systemctl", action.as_str(), PLEX_UNIT])],
            Self::StorageProbe => vec![step(&["findmnt", "-rno", "TARGET"])],
            Self::SecurityListCodecs { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                let codecs = format!("{data_dir}/Codecs");
                vec![step(&[
                    "find",
                    &codecs,
                    "-mindepth",
                    "1",
                    "(",
                    "-type",
                    "f",
                    "-o",
                    "-type",
                    "l",
                    ")",
                    "-printf",
                    "%T@ %y %p\\0",
                ])]
            }
            Self::SecurityListLogs { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                let logs = format!("{data_dir}/Logs");
                vec![step(&[
                    "find",
                    &logs,
                    "-maxdepth",
                    "1",
                    "-type",
                    "f",
                    "-name",
                    "Plex*.log",
                    "-print0",
                ])]
            }
            Self::SecurityGrepLogs { data_dir, logs } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                need(
                    !logs.is_empty() && logs.iter().all(|l| valid_log(data_dir, l)),
                    "log path",
                )?;
                let mut argv: Vec<&str> = vec!["grep", "-h", "-s", "-i", "-F"];
                for p in GREP_PATTERNS {
                    argv.push("-e");
                    argv.push(p);
                }
                argv.push("--");
                argv.extend(logs.iter().map(String::as_str));
                vec![step(&argv)]
            }
            Self::SecurityDataDirOwner { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                vec![step(&["stat", "-c", "%U", "--", data_dir])]
            }
            Self::SecurityQuarantine {
                data_dir,
                user,
                src,
                dest,
            } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                need(valid_user(user) && user != "root", "user")?;
                need(safe_codec_path(data_dir, src), "source path")?;
                need(valid_quarantine_dest(data_dir, dest), "quarantine path")?;
                let qdir = quarantine_dir(data_dir);
                vec![
                    step_as(user, &["mkdir", "-p", "--", &qdir]),
                    step_as(user, &["mv", "-n", "--", src, dest]),
                    step_as(user, &["test", "!", "-e", src, "-a", "!", "-L", src]),
                    step_as(user, &["test", "-e", dest, "-o", "-L", dest]),
                    step_as(user, &["chmod", "000", "--", dest]),
                ]
            }
            Self::KillTranscoders { user } => {
                need(valid_user(user), "user")?;
                vec![step(&["pkill", "-x", "Plex Transcoder", "-u", user])]
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub status: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl From<std::process::Output> for ExecOutput {
    fn from(o: std::process::Output) -> Self {
        Self {
            success: o.status.success(),
            code: o.status.code(),
            status: o.status.to_string(),
            stdout: o.stdout,
            stderr: o.stderr,
        }
    }
}

impl ExecOutput {
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn checked(self, what: &str) -> Result<String> {
        if !self.success {
            bail!(
                "{what} failed ({}): {}",
                self.status,
                String::from_utf8_lossy(&self.stderr).trim()
            );
        }
        Ok(self.stdout_string())
    }
}

pub fn command_for(guest: &Guest, s: &Step) -> Command {
    let mut cmd = match guest {
        Guest::Lxc(vmid) => {
            let mut c = Command::new("pct");
            c.arg("exec").arg(vmid.to_string()).arg("--");
            if let Some(u) = &s.user {
                c.arg("runuser").arg("-u").arg(u).arg("--");
            }
            c
        }
        Guest::Docker(id) => {
            let mut c = Command::new("docker");
            c.arg("exec");
            if let Some(u) = &s.user {
                c.arg("-u").arg(u);
            }
            c.arg(id);
            c
        }
    };
    cmd.args(&s.argv);
    cmd.kill_on_drop(true);
    cmd
}

async fn run_steps(guest: &Guest, op: &GuestOp) -> Result<ExecOutput> {
    let steps = op.steps()?;
    let mut last = None;
    for s in &steps {
        let out: ExecOutput = command_for(guest, s)
            .output()
            .await
            .context("failed to spawn guest command")?
            .into();
        let ok = out.success;
        last = Some(out);
        if !ok {
            break;
        }
    }
    last.context("op has no steps")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestRequest {
    pub vmid: u32,
    pub op: GuestOp,
}

fn seam_available() -> bool {
    std::env::var(ORCA_BIN_ENV).is_ok_and(|v| !v.is_empty())
}

pub(crate) async fn exec(guest: &Guest, op: GuestOp) -> Result<ExecOutput> {
    op.steps()?;
    match guest {
        Guest::Lxc(vmid) if seam_available() => {
            let name = op.name();
            let req = GuestRequest { vmid: *vmid, op };
            let detail = privileged::apply(PLUGIN, name, Some(&req))
                .await
                .with_context(|| format!("{PLUGIN} {name} through `orca admin plugin-apply`"))?;
            plugin_toolkit::serde_json::from_str(&detail)
                .with_context(|| format!("decode the {name} privileged reply"))
        }
        _ => run_steps(guest, &op).await,
    }
}

async fn run_privileged(name: &'static str, req: GuestRequest) -> Result<String> {
    if privileged::caller_uid().is_none() {
        bail!("caller uid is missing or invalid; refusing {name}");
    }
    if req.op.name() != name {
        bail!("payload op {} does not match {name}", req.op.name());
    }
    let out = run_steps(&Guest::Lxc(req.vmid), &req.op).await?;
    Ok(plugin_toolkit::serde_json::to_string(&out)?)
}

pub fn privileged_ops() -> PrivilegedOps {
    OP_NAMES.iter().fold(PrivilegedOps::new(), |ops, &name| {
        ops.op(name, move |req: GuestRequest| run_privileged(name, req))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DD: &str = "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server";

    fn argv(cmd: &Command) -> Vec<String> {
        let std = cmd.as_std();
        std::iter::once(std.get_program())
            .chain(std.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    fn all_ops() -> Vec<GuestOp> {
        vec![
            GuestOp::AptRepoRepair { dry_run: true },
            GuestOp::AptUpgradePlex,
            GuestOp::PlexVersion,
            GuestOp::Sql {
                db: "/x/a.db".into(),
                sql: "SELECT 1;".into(),
            },
            GuestOp::DbBackup {
                db: "/x/a.db".into(),
                stamp: "20261024-000000".into(),
            },
            GuestOp::Service {
                action: ServiceAction::Restart,
            },
            GuestOp::StorageProbe,
            GuestOp::SecurityListCodecs {
                data_dir: DD.into(),
            },
            GuestOp::SecurityListLogs {
                data_dir: DD.into(),
            },
            GuestOp::SecurityGrepLogs {
                data_dir: DD.into(),
                logs: vec![format!("{DD}/Logs/Plex Media Server.log")],
            },
            GuestOp::SecurityDataDirOwner {
                data_dir: DD.into(),
            },
            GuestOp::SecurityQuarantine {
                data_dir: DD.into(),
                user: "plex".into(),
                src: format!("{DD}/Codecs/a-linux-x86_64/zz.so"),
                dest: format!("{DD}/Codecs.quarantine/a-linux-x86_64__zz.so.1.quarantined"),
            },
            GuestOp::KillTranscoders {
                user: "plex".into(),
            },
        ]
    }

    #[test]
    fn every_op_is_registered_and_builds() {
        let ops = all_ops();
        assert_eq!(ops.len(), OP_NAMES.len());
        for op in ops {
            assert!(OP_NAMES.contains(&op.name()), "{}", op.name());
            assert!(!op.steps().unwrap().is_empty(), "{}", op.name());
        }
    }

    #[test]
    fn guest_parse_validates_target() {
        assert_eq!(Guest::parse(Runtime::Lxc, "112").unwrap(), Guest::Lxc(112));
        assert!(Guest::parse(Runtime::Lxc, "-1").is_err());
        assert!(Guest::parse(Runtime::Lxc, "1;rm").is_err());
        assert!(Guest::parse(Runtime::Docker, "--privileged").is_err());
    }

    #[test]
    fn apt_repo_repair_runs_embedded_script() {
        let s = GuestOp::AptRepoRepair { dry_run: false }.steps().unwrap();
        assert_eq!(s[0].argv[..4], ["env", "DRY_RUN=0", "bash", "-c"]);
        assert_eq!(s[0].argv[4], SCRIPT);
        let c = command_for(&Guest::Lxc(105), &s[0]);
        assert_eq!(
            argv(&c)[..6],
            ["pct", "exec", "105", "--", "env", "DRY_RUN=0"]
        );
    }

    #[test]
    fn apt_upgrade_is_fixed() {
        let s = GuestOp::AptUpgradePlex.steps().unwrap();
        assert_eq!(s[0].argv, ["apt-get", "update"]);
        assert_eq!(
            s[1].argv,
            [
                "apt-get",
                "install",
                "-y",
                "--only-upgrade",
                "plexmediaserver"
            ]
        );
        assert_eq!(s[2].argv, ["systemctl", "restart", "plexmediaserver"]);
    }

    #[test]
    fn sql_rejects_dot_commands_and_bad_db() {
        let ok = |db: &str, sql: &str| {
            GuestOp::Sql {
                db: db.into(),
                sql: sql.into(),
            }
            .steps()
            .is_ok()
        };
        assert!(ok("/x/lib.db", "SELECT 1;"));
        assert!(!ok("/x/lib.db", ".shell id"));
        assert!(!ok("/x/lib.db", "SELECT 1;\n  .system id"));
        assert!(!ok("/x/lib.db", "ATTACH '/etc/x' AS y;"));
        assert!(!ok("/x/lib.db", "SELECT load_extension('x');"));
        assert!(ok(
            "/x/lib.db",
            "SELECT 1 WHERE file LIKE '/media/Attachments/it''s/%';"
        ));
        assert!(!ok("x/lib.db", "SELECT 1;"));
        assert!(!ok("/x/../lib.db", "SELECT 1;"));
        assert!(!ok("/x/lib.txt", "SELECT 1;"));
    }

    #[test]
    fn db_backup_builds_dest_root_side() {
        let s = GuestOp::DbBackup {
            db: "/x/a.db".into(),
            stamp: "20261024-000000".into(),
        }
        .steps()
        .unwrap();
        assert_eq!(
            s[0].argv,
            [
                "cp",
                "-a",
                "--",
                "/x/a.db",
                "/x/a.db.orca-bak-20261024-000000"
            ]
        );
        assert!(GuestOp::DbBackup {
            db: "/x/a.db".into(),
            stamp: "1/../../etc".into()
        }
        .steps()
        .is_err());
    }

    #[test]
    fn grep_logs_only_under_logs_dir() {
        let op = |l: String| GuestOp::SecurityGrepLogs {
            data_dir: DD.into(),
            logs: vec![l],
        };
        assert!(op(format!("{DD}/Logs/Plex Media Server.1.log"))
            .steps()
            .is_ok());
        assert!(op("/etc/shadow".into()).steps().is_err());
        assert!(op(format!("{DD}/Logs/../x/Plex.log")).steps().is_err());
        assert!(op(format!("{DD}/Logs/other.log")).steps().is_err());
        assert!(GuestOp::SecurityGrepLogs {
            data_dir: DD.into(),
            logs: vec![]
        }
        .steps()
        .is_err());
    }

    #[test]
    fn quarantine_validates_and_runs_as_user() {
        let q = |user: &str, src: String, dest: String| GuestOp::SecurityQuarantine {
            data_dir: DD.into(),
            user: user.into(),
            src,
            dest,
        };
        let src = format!("{DD}/Codecs/a/zz.so");
        let dest = format!("{DD}/Codecs.quarantine/a__zz.so.1.quarantined");
        let s = q("plex", src.clone(), dest.clone()).steps().unwrap();
        assert!(s.iter().all(|st| st.user.as_deref() == Some("plex")));
        assert_eq!(s[1].argv, ["mv", "-n", "--", src.as_str(), dest.as_str()]);
        let c = command_for(&Guest::Lxc(7), &s[1]);
        assert_eq!(
            argv(&c)[..8],
            ["pct", "exec", "7", "--", "runuser", "-u", "plex", "--"]
        );
        let d = command_for(&Guest::Docker("plex".into()), &s[1]);
        assert_eq!(argv(&d)[..5], ["docker", "exec", "-u", "plex", "plex"]);
        assert!(q("root", src.clone(), dest.clone()).steps().is_err());
        assert!(q("plex", "/etc/passwd".into(), dest.clone())
            .steps()
            .is_err());
        assert!(q("plex", src.clone(), "/tmp/x.quarantined".into())
            .steps()
            .is_err());
        assert!(q(
            "plex",
            src,
            format!("{DD}/Codecs.quarantine/sub/x.quarantined")
        )
        .steps()
        .is_err());
    }

    #[test]
    fn kill_transcoders_is_exact() {
        let s = GuestOp::KillTranscoders {
            user: "plex".into(),
        }
        .steps()
        .unwrap();
        assert_eq!(s[0].argv, ["pkill", "-x", "Plex Transcoder", "-u", "plex"]);
        assert!(GuestOp::KillTranscoders { user: "-1".into() }
            .steps()
            .is_err());
        assert!(GuestOp::KillTranscoders { user: "a b".into() }
            .steps()
            .is_err());
    }

    #[test]
    fn request_round_trips_with_op_tag() {
        let req = GuestRequest {
            vmid: 112,
            op: GuestOp::Service {
                action: ServiceAction::Stop,
            },
        };
        let json = plugin_toolkit::serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"service\""));
        let back: GuestRequest = plugin_toolkit::serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }
}
