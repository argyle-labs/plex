#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use plugin_toolkit::privileged::{self, PrivilegedOps, ORCA_BIN_ENV};
use tokio::process::Command;

use crate::apt_repo::{valid_target, SCRIPT};
use crate::lifecycle::Runtime;
use crate::security::{parse_owner, safe_codec_path, valid_data_dir, GREP_PATTERNS};
use crate::storage::{not_under_clause, repair_sql, DriftRewrite};

const PLUGIN: &str = "plex";
const PLEX_MARKER: &str = "/usr/lib/plexmediaserver/Plex Media Server";
const PLEX_UNIT: &str = "plexmediaserver";
const ORCA_USER: &str = "orca";
const OWNER: &str = "\0owner";
const FIND_CAP_BYTES: &str = "4194304";
const GREP_TIMEOUT_SECS: &str = "60";
const MAX_ROOTS: usize = 64;
const MAX_PATH: usize = 4096;

pub const QUARANTINE_ROOT: &str = "/var/lib/orca-quarantine/plex";

pub const DATA_DIR_OWNER_SH: &str = r#"[ ! -L "$1" ] || { echo "data_dir is a symlink" >&2; exit 3; }
realpath -e / >/dev/null 2>&1 || { echo "needs GNU realpath -e" >&2; exit 5; }
[ "$(realpath -e -- "$1")" = "$1" ] || { echo "data_dir does not resolve to itself" >&2; exit 3; }
stat -L -c %u/%U -- "$1""#;

pub const MKQDIR_SH: &str = r#"set -eu
umask 077
[ "$(id -u)" = 0 ] || { echo "quarantine needs root in the guest" >&2; exit 3; }
for d in /var /var/lib /var/lib/orca-quarantine /var/lib/orca-quarantine/plex "$1"; do
  [ ! -L "$d" ] || { echo "symlink in quarantine path: $d" >&2; exit 3; }
  [ -d "$d" ] || mkdir -m 0700 -- "$d"
done
[ "$(stat -c %u:%a -- "$1")" = 0:700 ] || { echo "quarantine dir not root 0700" >&2; exit 3; }"#;

pub const QUARANTINE_SH: &str = r#"set -eu
src=$1 dest=$2 codecs=$3
dir=${src%/*} base=${src##*/}
realpath -e / >/dev/null 2>&1 || { echo "needs GNU realpath -e" >&2; exit 5; }
mv --version >/dev/null 2>&1 || { echo "needs GNU mv (-T)" >&2; exit 5; }
[ "$(realpath -e -- "$codecs")" = "$codecs" ] || { echo "Codecs dir does not resolve to itself" >&2; exit 3; }
c=$dir
while :; do
  [ ! -L "$c" ] || { echo "symlinked parent: $c" >&2; exit 3; }
  [ "$c" != "$codecs" ] || break
  case "$c" in "$codecs"/*) c=${c%/*} ;; *) echo "parent escapes Codecs" >&2; exit 3 ;; esac
done
cd -P -- "$dir"
case "$(pwd -P)/" in "$codecs"/*) ;; *) echo "realpath escapes Codecs" >&2; exit 3 ;; esac
[ "$(stat -c %d -- .)" = "$(stat -c %d -- "${dest%/*}")" ] || { echo "cross-filesystem quarantine refused" >&2; exit 3; }
[ ! -e "$dest" ] && [ ! -L "$dest" ] || { echo "quarantine destination exists (name hash collision)" >&2; exit 3; }
mv -n -T -- "./$base" "$dest"
[ ! -e "./$base" ] && [ ! -L "./$base" ] || { echo "source still present" >&2; exit 4; }
[ -e "$dest" ] || [ -L "$dest" ] || { echo "destination missing" >&2; exit 4; }
if [ ! -L "$dest" ] && [ -f "$dest" ] && [ "$(stat -c %h -- "$dest")" = 1 ]; then chmod 000 -- "$dest"; fi"#;

const LIST_CODECS_SH: &str = r#"set -eu
[ "$(realpath -e -- "$1")" = "$1" ] || { echo "data_dir does not resolve to itself" >&2; exit 3; }
[ ! -L "$1/Codecs" ] || { echo "Codecs is a symlink" >&2; exit 3; }
find -P "$1/Codecs" -xdev -mindepth 1 \( -type f -o -type l \) -printf '%T@ %y %p\0' | head -c "$2""#;

const LIST_LOGS_SH: &str = r#"set -eu
[ "$(realpath -e -- "$1")" = "$1" ] || { echo "data_dir does not resolve to itself" >&2; exit 3; }
[ ! -L "$1/Logs" ] || { echo "Logs is a symlink" >&2; exit 3; }
find -P "$1/Logs" -xdev -maxdepth 1 -type f -name 'Plex*.log' -print0 | head -c "$2""#;

const DB_PREAMBLE_SH: &str = r#"set -eu
db=$1
[ -f "$db" ] && [ ! -L "$db" ] || { echo "db missing, not regular, or a symlink" >&2; exit 3; }
[ "$(realpath -e -- "$db")" = "$db" ] || { echo "db path does not resolve to itself" >&2; exit 3; }
"#;

const SQL_SH: &str = r#"mode=$2 sql=$3
if [ "$mode" = ro ]; then exec "/usr/lib/plexmediaserver/Plex SQLite" -safe -readonly "$db" "$sql"; fi
exec "/usr/lib/plexmediaserver/Plex SQLite" -safe "$db" "$sql""#;

const DB_BACKUP_SH: &str = r#"dest=$2
umask 077
[ ! -e "$dest" ] && [ ! -L "$dest" ] || { echo "backup destination exists" >&2; exit 3; }
cp -P --preserve=all -n -T -- "$db" "$dest"
[ -f "$dest" ] && [ ! -L "$dest" ] || { echo "backup not created" >&2; exit 4; }
printf '%s\n' "$dest""#;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlexDb {
    Library,
    Blobs,
}

impl PlexDb {
    fn basename(self) -> &'static str {
        match self {
            Self::Library => "com.plexapp.plugins.library.db",
            Self::Blobs => "com.plexapp.plugins.library.blobs.db",
        }
    }

    pub fn path(self, data_dir: &str) -> String {
        format!(
            "{}/Plug-in Support/Databases/{}",
            data_dir.trim_end_matches('/'),
            self.basename()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum SqlQuery {
    LibraryRoots,
    OffendingParts { roots: Vec<String> },
    OffendingStreams { roots: Vec<String> },
    RewriteRoot { from_root: String, to_root: String },
}

impl SqlQuery {
    fn read_only(&self) -> bool {
        !matches!(self, Self::RewriteRoot { .. })
    }

    pub fn statement(&self) -> Result<String> {
        Ok(match self {
            Self::LibraryRoots => {
                "SELECT root_path FROM section_locations WHERE root_path LIKE '/%';".to_string()
            }
            Self::OffendingParts { roots } => {
                need(valid_roots(roots), "roots")?;
                format!(
                    "SELECT DISTINCT file FROM media_parts WHERE file LIKE '/%' AND {} LIMIT 100;",
                    not_under_clause("file", roots, "")
                )
            }
            Self::OffendingStreams { roots } => {
                need(valid_roots(roots), "roots")?;
                format!(
                    "SELECT DISTINCT url FROM media_streams WHERE url LIKE 'file:///%' AND {} LIMIT 100;",
                    not_under_clause("url", roots, "file://")
                )
            }
            Self::RewriteRoot { from_root, to_root } => {
                need(
                    valid_root(from_root) && valid_root(to_root),
                    "rewrite roots",
                )?;
                repair_sql(&DriftRewrite {
                    from_root: from_root.clone(),
                    to_root: to_root.clone(),
                    occurrences: 0,
                })
            }
        })
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
    Service {
        action: ServiceAction,
    },
    StorageProbe,
    Sql {
        data_dir: String,
        db: PlexDb,
        query: SqlQuery,
    },
    DbBackup {
        data_dir: String,
        db: PlexDb,
    },
    SecurityDataDirOwner {
        data_dir: String,
    },
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
    SecurityMkQuarantineDir {
        stamp: String,
    },
    SecurityQuarantine {
        data_dir: String,
        stamp: String,
        src: String,
        dest: String,
    },
    KillTranscoders {
        data_dir: String,
    },
}

pub const OP_NAMES: &[&str] = &[
    "apt_repo_repair",
    "apt_upgrade_plex",
    "plex_version",
    "service",
    "storage_probe",
    "sql",
    "db_backup",
    "security_data_dir_owner",
    "security_list_codecs",
    "security_list_logs",
    "security_grep_logs",
    "security_mk_quarantine_dir",
    "security_quarantine",
    "kill_transcoders",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAs {
    Root,
    Owner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub run_as: RunAs,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub owner_of: Option<String>,
    pub steps: Vec<Step>,
}

fn root(argv: &[&str]) -> Step {
    Step {
        run_as: RunAs::Root,
        argv: argv.iter().map(|s| s.to_string()).collect(),
    }
}

fn plan(steps: Vec<Step>) -> Plan {
    Plan {
        owner_of: None,
        steps,
    }
}

fn owned(data_dir: &str, steps: Vec<Step>) -> Plan {
    Plan {
        owner_of: Some(data_dir.to_string()),
        steps,
    }
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn valid_root(r: &str) -> bool {
    r.starts_with('/') && r.len() <= MAX_PATH && !has_control(r)
}

fn valid_roots(roots: &[String]) -> bool {
    !roots.is_empty() && roots.len() <= MAX_ROOTS && roots.iter().all(|r| valid_root(r))
}

pub fn valid_stamp(stamp: &str) -> bool {
    !stamp.is_empty() && stamp.len() <= 32 && stamp.bytes().all(|b| b.is_ascii_digit() || b == b'-')
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

pub fn quarantine_dir(stamp: &str) -> String {
    format!("{QUARANTINE_ROOT}/{stamp}")
}

pub fn valid_quarantine_dest(stamp: &str, dest: &str) -> bool {
    dest.strip_prefix(&format!("{}/", quarantine_dir(stamp)))
        .is_some_and(|name| {
            name.ends_with(".quarantined")
                && name.len() > ".quarantined".len()
                && !name.contains('/')
                && !name.split('/').any(|s| s == "..")
                && !has_control(name)
        })
}

fn need(ok: bool, what: &str) -> Result<()> {
    if !ok {
        bail!("invalid {what}");
    }
    Ok(())
}

fn sh(script: &str, args: &[&str]) -> Vec<String> {
    let mut v = vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "plex".to_string(),
    ];
    v.extend(args.iter().map(|s| s.to_string()));
    v
}

pub fn grep_script() -> String {
    let patterns: String = GREP_PATTERNS.iter().map(|p| format!(" -e '{p}'")).collect();
    format!(
        r#"d=$1; shift
skipped=0
for f do
  shift
  case "$f" in "$d"/Logs/Plex*.log) ;; *) skipped=1; continue ;; esac
  if [ -f "$f" ] && [ ! -L "$f" ]; then set -- "$@" "$f"; else skipped=1; fi
done
[ $# -gt 0 ] || exit 2
rc=0
timeout {GREP_TIMEOUT_SECS} grep -h -s -i -F -D skip{patterns} -- "$@" || rc=$?
if [ "$rc" -le 1 ] && [ "$skipped" = 1 ]; then exit 2; fi
exit "$rc""#
    )
}

fn backup_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{:08x}",
        plugin_toolkit::time::Timestamp::now().compact(),
        nanos ^ std::process::id().rotate_left(16)
    )
}

impl GuestOp {
    pub fn name(&self) -> &'static str {
        match self {
            Self::AptRepoRepair { .. } => "apt_repo_repair",
            Self::AptUpgradePlex => "apt_upgrade_plex",
            Self::PlexVersion => "plex_version",
            Self::Service { .. } => "service",
            Self::StorageProbe => "storage_probe",
            Self::Sql { .. } => "sql",
            Self::DbBackup { .. } => "db_backup",
            Self::SecurityDataDirOwner { .. } => "security_data_dir_owner",
            Self::SecurityListCodecs { .. } => "security_list_codecs",
            Self::SecurityListLogs { .. } => "security_list_logs",
            Self::SecurityGrepLogs { .. } => "security_grep_logs",
            Self::SecurityMkQuarantineDir { .. } => "security_mk_quarantine_dir",
            Self::SecurityQuarantine { .. } => "security_quarantine",
            Self::KillTranscoders { .. } => "kill_transcoders",
        }
    }

    pub fn plan(&self) -> Result<Plan> {
        Ok(match self {
            Self::AptRepoRepair { dry_run } => plan(vec![root(&[
                "env",
                if *dry_run { "DRY_RUN=1" } else { "DRY_RUN=0" },
                "bash",
                "-c",
                SCRIPT,
            ])]),
            Self::AptUpgradePlex => plan(vec![
                root(&["apt-get", "update"]),
                root(&["apt-get", "install", "-y", "--only-upgrade", PLEX_UNIT]),
                root(&["systemctl", "restart", PLEX_UNIT]),
            ]),
            Self::PlexVersion => plan(vec![root(&[
                "dpkg-query",
                "-W",
                "-f=${Version}",
                PLEX_UNIT,
            ])]),
            Self::Service { action } => {
                plan(vec![root(&["systemctl", action.as_str(), PLEX_UNIT])])
            }
            Self::StorageProbe => plan(vec![root(&["findmnt", "-rno", "TARGET"])]),
            Self::Sql {
                data_dir,
                db,
                query,
            } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                let stmt = query.statement()?;
                let script = format!("{DB_PREAMBLE_SH}{SQL_SH}");
                let mode = if query.read_only() { "ro" } else { "rw" };
                let argv = sh(&script, &[&db.path(data_dir), mode, &stmt]);
                owned(
                    data_dir,
                    vec![Step {
                        run_as: RunAs::Owner,
                        argv,
                    }],
                )
            }
            Self::DbBackup { data_dir, db } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                let path = db.path(data_dir);
                let dest = format!("{path}.orca-bak-{}", backup_suffix());
                let script = format!("{DB_PREAMBLE_SH}{DB_BACKUP_SH}");
                owned(
                    data_dir,
                    vec![Step {
                        run_as: RunAs::Owner,
                        argv: sh(&script, &[&path, &dest]),
                    }],
                )
            }
            Self::SecurityDataDirOwner { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                plan(vec![Step {
                    run_as: RunAs::Root,
                    argv: sh(DATA_DIR_OWNER_SH, &[data_dir]),
                }])
            }
            Self::SecurityListCodecs { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                owned(
                    data_dir,
                    vec![Step {
                        run_as: RunAs::Owner,
                        argv: sh(LIST_CODECS_SH, &[data_dir, FIND_CAP_BYTES]),
                    }],
                )
            }
            Self::SecurityListLogs { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                owned(
                    data_dir,
                    vec![Step {
                        run_as: RunAs::Owner,
                        argv: sh(LIST_LOGS_SH, &[data_dir, FIND_CAP_BYTES]),
                    }],
                )
            }
            Self::SecurityGrepLogs { data_dir, logs } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                need(
                    !logs.is_empty() && logs.iter().all(|l| valid_log(data_dir, l)),
                    "log path",
                )?;
                let mut args: Vec<&str> = vec![data_dir.trim_end_matches('/')];
                args.extend(logs.iter().map(String::as_str));
                owned(
                    data_dir,
                    vec![Step {
                        run_as: RunAs::Owner,
                        argv: sh(&grep_script(), &args),
                    }],
                )
            }
            Self::SecurityMkQuarantineDir { stamp } => {
                need(valid_stamp(stamp), "stamp")?;
                plan(vec![Step {
                    run_as: RunAs::Root,
                    argv: sh(MKQDIR_SH, &[&quarantine_dir(stamp)]),
                }])
            }
            Self::SecurityQuarantine {
                data_dir,
                stamp,
                src,
                dest,
            } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                need(valid_stamp(stamp), "stamp")?;
                need(safe_codec_path(data_dir, src), "source path")?;
                need(valid_quarantine_dest(stamp, dest), "quarantine path")?;
                let codecs = format!("{}/Codecs", data_dir.trim_end_matches('/'));
                plan(vec![Step {
                    run_as: RunAs::Root,
                    argv: sh(QUARANTINE_SH, &[src, dest, &codecs]),
                }])
            }
            Self::KillTranscoders { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                owned(
                    data_dir,
                    vec![root(&["pkill", "-x", "-u", OWNER, "Plex Transcoder"])],
                )
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

pub fn command_for(guest: &Guest, user: Option<&str>, argv: &[String]) -> Command {
    let mut cmd = match guest {
        Guest::Lxc(vmid) => {
            let mut c = Command::new("pct");
            c.arg("exec").arg(vmid.to_string()).arg("--");
            if let Some(u) = user {
                c.arg("runuser").arg("-u").arg(u).arg("--");
            }
            c
        }
        Guest::Docker(id) => {
            let mut c = Command::new("docker");
            c.arg("exec");
            if let Some(u) = user {
                c.arg("-u").arg(u);
            }
            c.arg(id);
            c
        }
    };
    cmd.args(argv);
    cmd.kill_on_drop(true);
    cmd
}

async fn spawn(guest: &Guest, user: Option<&str>, argv: &[String]) -> Result<ExecOutput> {
    Ok(command_for(guest, user, argv)
        .output()
        .await
        .context("failed to spawn guest command")?
        .into())
}

pub fn resolve_step(step: &Step, owner: Option<&str>) -> Result<(Option<String>, Vec<String>)> {
    let needs_owner = step.run_as == RunAs::Owner || step.argv.iter().any(|a| a == OWNER);
    let owner = match (needs_owner, owner) {
        (false, _) => None,
        (true, Some(o)) => Some(o.to_string()),
        (true, None) => bail!("step needs the data-dir owner"),
    };
    let argv = step
        .argv
        .iter()
        .map(|a| match (a == OWNER, &owner) {
            (true, Some(o)) => o.clone(),
            _ => a.clone(),
        })
        .collect();
    let user = match step.run_as {
        RunAs::Owner => owner,
        RunAs::Root => None,
    };
    Ok((user, argv))
}

async fn run_plan(guest: &Guest, op: &GuestOp) -> Result<ExecOutput> {
    let plan = op.plan()?;
    let owner = match &plan.owner_of {
        Some(dd) => {
            let out = spawn(guest, None, &sh(DATA_DIR_OWNER_SH, &[dd]))
                .await?
                .checked("resolve data_dir owner")?;
            Some(parse_owner(&out)?)
        }
        None => None,
    };
    let mut last = None;
    for s in &plan.steps {
        let (user, argv) = resolve_step(s, owner.as_deref())?;
        let out = spawn(guest, user.as_deref(), &argv).await?;
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
    op.plan()?;
    match guest {
        Guest::Lxc(vmid) => {
            if !seam_available() {
                bail!("LXC guest ops need the orca privileged seam ({ORCA_BIN_ENV} unset)");
            }
            let name = op.name();
            let req = GuestRequest { vmid: *vmid, op };
            let detail = privileged::apply(PLUGIN, name, Some(&req))
                .await
                .with_context(|| format!("{PLUGIN} {name} through `orca admin plugin-apply`"))?;
            plugin_toolkit::serde_json::from_str(&detail)
                .with_context(|| format!("decode the {name} privileged reply"))
        }
        Guest::Docker(_) => run_plan(guest, &op).await,
    }
}

pub fn caller_allowed(uid: Option<u32>, passwd: &str) -> bool {
    match uid {
        Some(0) => true,
        Some(uid) => passwd.lines().any(|l| {
            let f: Vec<&str> = l.split(':').collect();
            f.len() >= 3 && f[0] == ORCA_USER && f[2].parse() == Ok(uid)
        }),
        None => false,
    }
}

async fn is_plex_guest(vmid: u32) -> Result<bool> {
    let out = spawn(
        &Guest::Lxc(vmid),
        None,
        &[
            "test".to_string(),
            "-x".to_string(),
            PLEX_MARKER.to_string(),
        ],
    )
    .await?;
    Ok(out.success)
}

async fn run_privileged(name: &'static str, req: GuestRequest) -> Result<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    if !caller_allowed(privileged::caller_uid(), &passwd) {
        bail!("caller is neither root nor the {ORCA_USER} service user; refusing {name}");
    }
    if req.op.name() != name {
        bail!("payload op {} does not match {name}", req.op.name());
    }
    if !is_plex_guest(req.vmid).await? {
        bail!("guest {} has no {PLEX_MARKER}; refusing {name}", req.vmid);
    }
    let out = run_plan(&Guest::Lxc(req.vmid), &req.op).await?;
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
            GuestOp::Service {
                action: ServiceAction::Restart,
            },
            GuestOp::StorageProbe,
            GuestOp::Sql {
                data_dir: DD.into(),
                db: PlexDb::Library,
                query: SqlQuery::LibraryRoots,
            },
            GuestOp::DbBackup {
                data_dir: DD.into(),
                db: PlexDb::Library,
            },
            GuestOp::SecurityDataDirOwner {
                data_dir: DD.into(),
            },
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
            GuestOp::SecurityMkQuarantineDir {
                stamp: "20261024-000000".into(),
            },
            GuestOp::SecurityQuarantine {
                data_dir: DD.into(),
                stamp: "20261024-000000".into(),
                src: format!("{DD}/Codecs/a-linux-x86_64/zz.so"),
                dest: format!(
                    "{QUARANTINE_ROOT}/20261024-000000/a-linux-x86_64__zz.so.1.quarantined"
                ),
            },
            GuestOp::KillTranscoders {
                data_dir: DD.into(),
            },
        ]
    }

    #[test]
    fn every_op_is_registered_and_plans() {
        let ops = all_ops();
        assert_eq!(ops.len(), OP_NAMES.len());
        for op in ops {
            assert!(OP_NAMES.contains(&op.name()), "{}", op.name());
            assert!(!op.plan().unwrap().steps.is_empty(), "{}", op.name());
        }
    }

    #[test]
    fn root_only_for_system_ops() {
        for op in all_ops() {
            let p = op.plan().unwrap();
            let any_root_shell_over_tree = p.steps.iter().any(|s| {
                s.run_as == RunAs::Root
                    && matches!(
                        op,
                        GuestOp::Sql { .. }
                            | GuestOp::DbBackup { .. }
                            | GuestOp::SecurityListCodecs { .. }
                            | GuestOp::SecurityListLogs { .. }
                            | GuestOp::SecurityGrepLogs { .. }
                    )
            });
            assert!(!any_root_shell_over_tree, "{}", op.name());
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
    fn apt_ops_are_fixed() {
        let p = GuestOp::AptRepoRepair { dry_run: false }.plan().unwrap();
        assert_eq!(p.steps[0].argv[..4], ["env", "DRY_RUN=0", "bash", "-c"]);
        assert_eq!(p.steps[0].argv[4], SCRIPT);
        let u = GuestOp::AptUpgradePlex.plan().unwrap();
        assert_eq!(u.steps[0].argv, ["apt-get", "update"]);
        assert_eq!(
            u.steps[1].argv,
            [
                "apt-get",
                "install",
                "-y",
                "--only-upgrade",
                "plexmediaserver"
            ]
        );
    }

    #[test]
    fn sql_is_closed_set_run_as_owner_safe_mode() {
        let p = GuestOp::Sql {
            data_dir: DD.into(),
            db: PlexDb::Library,
            query: SqlQuery::LibraryRoots,
        }
        .plan()
        .unwrap();
        assert_eq!(p.owner_of.as_deref(), Some(DD));
        let s = &p.steps[0];
        assert_eq!(s.run_as, RunAs::Owner);
        assert!(s.argv[2].contains("-safe -readonly"));
        assert_eq!(
            s.argv[4],
            format!("{DD}/Plug-in Support/Databases/com.plexapp.plugins.library.db")
        );
        assert_eq!(s.argv[5], "ro");
        let w = GuestOp::Sql {
            data_dir: DD.into(),
            db: PlexDb::Library,
            query: SqlQuery::RewriteRoot {
                from_root: "/data".into(),
                to_root: "/mnt/it's".into(),
            },
        }
        .plan()
        .unwrap();
        assert_eq!(w.steps[0].argv[5], "rw");
        assert!(w.steps[0].argv[6].contains("'/mnt/it''s/'"));
    }

    #[test]
    fn sql_rejects_bad_params() {
        let bad = |q: SqlQuery| {
            GuestOp::Sql {
                data_dir: DD.into(),
                db: PlexDb::Library,
                query: q,
            }
            .plan()
            .is_err()
        };
        assert!(bad(SqlQuery::OffendingParts { roots: vec![] }));
        assert!(bad(SqlQuery::OffendingParts {
            roots: vec!["relative".into()]
        }));
        assert!(bad(SqlQuery::OffendingStreams {
            roots: vec!["/a\n.shell id".into()]
        }));
        assert!(bad(SqlQuery::RewriteRoot {
            from_root: "/a".into(),
            to_root: "/b\0".into()
        }));
        assert!(GuestOp::Sql {
            data_dir: "/x/../y".into(),
            db: PlexDb::Blobs,
            query: SqlQuery::LibraryRoots
        }
        .plan()
        .is_err());
    }

    #[test]
    fn db_paths_are_fixed_basenames() {
        assert!(PlexDb::Blobs
            .path("/d/")
            .ends_with("/d/Plug-in Support/Databases/com.plexapp.plugins.library.blobs.db"));
    }

    #[test]
    fn db_backup_runs_as_owner_with_exclusive_dest() {
        let p = GuestOp::DbBackup {
            data_dir: DD.into(),
            db: PlexDb::Library,
        }
        .plan()
        .unwrap();
        let s = &p.steps[0];
        assert_eq!(s.run_as, RunAs::Owner);
        assert!(s.argv[2].contains("cp -P --preserve=all -n -T"));
        assert!(s.argv[2].contains("[ ! -e \"$dest\" ] && [ ! -L \"$dest\" ]"));
        assert!(s.argv[5].starts_with(&format!("{}.orca-bak-", s.argv[4])));
    }

    #[test]
    fn grep_logs_validation_and_script() {
        let op = |l: String| GuestOp::SecurityGrepLogs {
            data_dir: DD.into(),
            logs: vec![l],
        };
        assert!(op(format!("{DD}/Logs/Plex Media Server.1.log"))
            .plan()
            .is_ok());
        assert!(op("/etc/shadow".into()).plan().is_err());
        assert!(op(format!("{DD}/Logs/../x/Plex.log")).plan().is_err());
        assert!(op(format!("{DD}/Logs/other.log")).plan().is_err());
        let s = grep_script();
        assert!(s.contains("[ -f \"$f\" ] && [ ! -L \"$f\" ]"));
        assert!(s.contains("timeout 60 grep"));
        assert!(s.contains("-D skip"));
        assert!(GREP_PATTERNS.iter().all(|p| !p.contains('\'')));
    }

    #[test]
    fn listings_run_as_owner_no_follow_with_cap() {
        for op in [
            GuestOp::SecurityListCodecs {
                data_dir: DD.into(),
            },
            GuestOp::SecurityListLogs {
                data_dir: DD.into(),
            },
        ] {
            let s = &op.plan().unwrap().steps[0];
            assert_eq!(s.run_as, RunAs::Owner);
            assert!(s.argv[2].contains("find -P"));
            assert!(s.argv[2].contains("-xdev"));
            assert!(s.argv[2].contains("head -c"));
        }
    }

    #[test]
    fn quarantine_runs_root_scripts_without_runuser() {
        let stamp = "20261024-000000";
        let src = format!("{DD}/Codecs/a/zz.so");
        let dest = format!("{QUARANTINE_ROOT}/{stamp}/a__zz.so.1.quarantined");
        let q = |src: String, dest: String| GuestOp::SecurityQuarantine {
            data_dir: DD.into(),
            stamp: stamp.into(),
            src,
            dest,
        };
        let p = q(src.clone(), dest.clone()).plan().unwrap();
        assert!(p.owner_of.is_none());
        let s = &p.steps[0];
        assert_eq!(s.run_as, RunAs::Root);
        assert_eq!(s.argv[2], QUARANTINE_SH);
        assert_eq!(
            s.argv[4..],
            [src.clone(), dest.clone(), format!("{DD}/Codecs")]
        );
        let (user, a) = resolve_step(s, None).unwrap();
        assert!(user.is_none());
        let c = command_for(&Guest::Lxc(7), None, &a);
        assert!(!argv(&c).contains(&"runuser".to_string()));
        assert!(q("/etc/passwd".into(), dest.clone()).plan().is_err());
        assert!(q(src.clone(), "/tmp/x.quarantined".into()).plan().is_err());
        assert!(q(
            src.clone(),
            format!("{QUARANTINE_ROOT}/{stamp}/sub/x.quarantined")
        )
        .plan()
        .is_err());
        assert!(q(src, format!("{QUARANTINE_ROOT}/other/x.quarantined"))
            .plan()
            .is_err());
        let mk = GuestOp::SecurityMkQuarantineDir {
            stamp: stamp.into(),
        }
        .plan()
        .unwrap();
        assert_eq!(mk.steps[0].argv[4], format!("{QUARANTINE_ROOT}/{stamp}"));
        assert!(GuestOp::SecurityMkQuarantineDir {
            stamp: "../../etc".into()
        }
        .plan()
        .is_err());
    }

    #[test]
    fn quarantine_script_guards() {
        assert!(QUARANTINE_SH.contains(r#"if [ ! -L "$dest" ] && [ -f "$dest" ]"#));
        assert!(QUARANTINE_SH.contains(r#"mv -n -T -- "./$base" "$dest""#));
        assert!(QUARANTINE_SH.contains(r#"[ ! -L "$c" ]"#));
        assert!(QUARANTINE_SH.contains("cross-filesystem"));
        assert_eq!(QUARANTINE_SH.matches("chmod").count(), 1);
    }

    #[test]
    fn kill_transcoders_uses_resolved_owner() {
        let p = GuestOp::KillTranscoders {
            data_dir: DD.into(),
        }
        .plan()
        .unwrap();
        assert_eq!(p.owner_of.as_deref(), Some(DD));
        let (user, a) = resolve_step(&p.steps[0], Some("plex")).unwrap();
        assert!(user.is_none());
        assert_eq!(a, ["pkill", "-x", "-u", "plex", "Plex Transcoder"]);
        assert!(resolve_step(&p.steps[0], None).is_err());
    }

    #[test]
    fn owner_steps_wrap_in_runuser_or_docker_user() {
        let s = Step {
            run_as: RunAs::Owner,
            argv: vec!["true".to_string()],
        };
        let (user, a) = resolve_step(&s, Some("plex")).unwrap();
        let c = command_for(&Guest::Lxc(7), user.as_deref(), &a);
        assert_eq!(
            argv(&c),
            ["pct", "exec", "7", "--", "runuser", "-u", "plex", "--", "true"]
        );
        let d = command_for(&Guest::Docker("plex".into()), user.as_deref(), &a);
        assert_eq!(argv(&d), ["docker", "exec", "-u", "plex", "plex", "true"]);
    }

    #[test]
    fn caller_authorization() {
        let passwd = "root:x:0:0::/root:/bin/sh\norca:x:996:996::/var/lib/orca:/usr/sbin/nologin\nbob:x:1000:1000::/home/bob:/bin/sh\n";
        assert!(caller_allowed(Some(0), passwd));
        assert!(caller_allowed(Some(996), passwd));
        assert!(!caller_allowed(Some(1000), passwd));
        assert!(!caller_allowed(None, passwd));
    }

    #[tokio::test]
    async fn lxc_without_seam_fails_closed() {
        if seam_available() {
            return;
        }
        let err = exec(&Guest::Lxc(112), GuestOp::PlexVersion)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("privileged seam"));
    }

    #[test]
    fn request_round_trips_with_op_tag() {
        let req = GuestRequest {
            vmid: 112,
            op: GuestOp::Sql {
                data_dir: DD.into(),
                db: PlexDb::Library,
                query: SqlQuery::OffendingParts {
                    roots: vec!["/m".into()],
                },
            },
        };
        let json = plugin_toolkit::serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"sql\""));
        let back: GuestRequest = plugin_toolkit::serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlink_suspect_moved_as_link_target_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let codecs = root.join("Codecs");
        let q = root.join("q");
        std::fs::create_dir_all(codecs.join("a")).unwrap();
        std::fs::create_dir(&q).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "x").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = codecs.join("a/evil.so");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let dest = q.join("evil");
        let st = std::process::Command::new("sh")
            .arg("-c")
            .arg(QUARANTINE_SH)
            .arg("t")
            .arg(&link)
            .arg(&dest)
            .arg(&codecs)
            .status()
            .unwrap();
        assert!(st.success());
        assert!(std::fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(&link).is_err());
        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlinked_parent_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let codecs = root.join("Codecs");
        let outside = root.join("outside");
        std::fs::create_dir_all(&codecs).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("f.so"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, codecs.join("a")).unwrap();
        let st = std::process::Command::new("sh")
            .arg("-c")
            .arg(QUARANTINE_SH)
            .arg("t")
            .arg(codecs.join("a/f.so"))
            .arg(root.join("dest"))
            .arg(&codecs)
            .status()
            .unwrap();
        assert!(!st.success());
        assert!(outside.join("f.so").exists());
    }
}
