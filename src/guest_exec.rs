#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use plugin_toolkit::privileged::{self, PrivilegedOps, ORCA_BIN_ENV};
use tokio::process::Command;

use crate::apt_repo::{valid_target, SCRIPT};
use crate::lifecycle::Runtime;
use crate::security::{safe_codec_path, scrub_tokens, valid_data_dir, GREP_PATTERNS};
use crate::storage::{not_under_clause, repair_sql, DriftRewrite};

const PLUGIN: &str = "plex";
const PLEX_MARKER: &str = "/usr/lib/plexmediaserver/Plex Media Server";
const PLEX_TAG: &str = "plex";
const PLEX_UNIT: &str = "plexmediaserver";
const ORCA_USER: &str = "orca";
const FIND_CAP_BYTES: &str = "4194304";
const GREP_TIMEOUT_SECS: &str = "60";
const MAX_ROOTS: usize = 64;
const MAX_PATH: usize = 4096;
const SYSTEM_PREFIXES: &[&str] = &[
    "/etc",
    "/proc",
    "/sys",
    "/root",
    "/boot",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/dev",
    "/run",
    "/var/lib/orca-quarantine",
];

pub const QUARANTINE_ROOT: &str = "/var/lib/orca-quarantine/plex";

const OWNER_PREAMBLE_SH: &str = r#"set -eu
dd=$1; shift
realpath -e / >/dev/null 2>&1 || { echo "needs GNU realpath -e" >&2; exit 5; }
[ ! -L "$dd" ] || { echo "data_dir is a symlink" >&2; exit 3; }
[ "$(realpath -e -- "$dd")" = "$dd" ] || { echo "data_dir does not resolve to itself" >&2; exit 3; }
uid=$(stat -L -c %u -- "$dd")
owner=$(stat -L -c %U -- "$dd")
[ "$uid" != 0 ] || { echo "data_dir is owned by uid 0; refusing to act as root" >&2; exit 3; }
bad() { echo "data_dir owner is not a usable user name" >&2; exit 3; }
[ -n "$owner" ] && [ "$owner" != UNKNOWN ] || bad
case "$owner" in -*) bad ;; esac
[ -z "$(printf '%s' "$owner" | tr -d 'A-Za-z0-9_-')" ] || bad
"#;

const RUN_AS_OWNER_TAIL: &str = r#"exec runuser -u "$owner" -- "$@""#;
const KILL_TAIL: &str = r#"exec pkill -x -u "$owner" "Plex Transcoder""#;
const PRINT_OWNER_TAIL: &str = r#"printf '%s/%s\n' "$uid" "$owner""#;
const LAYOUT_TAIL: &str = r#"for p in "$dd/Plug-in Support" "$dd/Plug-in Support/Databases"; do
  [ -d "$p" ] && [ ! -L "$p" ] || { echo "not a Plex data dir: $p" >&2; exit 3; }
done"#;

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
set -C
{ true > "$dest"; } 2>/dev/null || { echo "backup destination exists" >&2; exit 3; }
set +C
[ -f "$dest" ] && [ ! -L "$dest" ] || { echo "backup destination is not a regular file" >&2; exit 4; }
cat -- "$db" >> "$dest"
[ "$(stat -c %s -- "$db")" = "$(stat -c %s -- "$dest")" ] || { echo "backup size mismatch" >&2; exit 4; }
[ "$(stat -c %i -- "$db")" != "$(stat -c %i -- "$dest")" ] || { echo "backup is the source inode" >&2; exit 4; }
printf '%s\n' "$dest""#;

fn owner_script(tail: &str) -> String {
    format!("{OWNER_PREAMBLE_SH}{tail}")
}

pub fn data_dir_owner_script() -> String {
    owner_script(PRINT_OWNER_TAIL)
}

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
                "SELECT root_path FROM section_locations WHERE substr(root_path,1,1) = '/';"
                    .to_string()
            }
            Self::OffendingParts { roots } => {
                need(valid_roots(roots), "roots")?;
                format!(
                    "SELECT DISTINCT file FROM media_parts WHERE substr(file,1,1) = '/' AND {} LIMIT 100;",
                    not_under_clause("file", roots, "")
                )
            }
            Self::OffendingStreams { roots } => {
                need(valid_roots(roots), "roots")?;
                format!(
                    "SELECT DISTINCT url FROM media_streams WHERE substr(url,1,8) = 'file:///' AND {} LIMIT 100;",
                    not_under_clause("url", roots, "file://")
                )
            }
            Self::RewriteRoot { from_root, to_root } => {
                need(
                    valid_rewrite_root(from_root)
                        && valid_rewrite_root(to_root)
                        && from_root != to_root,
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
    Owner(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub run_as: RunAs,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
}

fn root(argv: &[&str]) -> Step {
    Step {
        run_as: RunAs::Root,
        argv: argv.iter().map(|s| s.to_string()).collect(),
    }
}

fn root_sh(script: &str, args: &[&str]) -> Step {
    Step {
        run_as: RunAs::Root,
        argv: sh(script, args),
    }
}

fn owner_sh(data_dir: &str, script: &str, args: &[&str]) -> Step {
    Step {
        run_as: RunAs::Owner(data_dir.to_string()),
        argv: sh(script, args),
    }
}

fn plan(steps: Vec<Step>) -> Plan {
    Plan { steps }
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn segments(p: &str) -> Vec<&str> {
    p.split('/').filter(|s| !s.is_empty()).collect()
}

pub fn under(path: &str, root: &str) -> bool {
    let (p, r) = (segments(path), segments(root));
    r.len() <= p.len() && p[..r.len()] == r[..]
}

fn valid_root(r: &str) -> bool {
    r.starts_with('/') && r.len() <= MAX_PATH && !has_control(r) && !segments(r).contains(&"..")
}

fn valid_rewrite_root(r: &str) -> bool {
    valid_root(r) && !segments(r).is_empty() && !SYSTEM_PREFIXES.iter().any(|p| under(r, p))
}

fn valid_roots(roots: &[String]) -> bool {
    !roots.is_empty() && roots.len() <= MAX_ROOTS && roots.iter().all(|r| valid_root(r))
}

pub fn check_rewrite(to_root: &str, library_roots: &[String], mounts: &[String]) -> Result<()> {
    if !library_roots.iter().any(|r| under(r, to_root)) {
        bail!("to_root {to_root:?} is not a parent of any current library root");
    }
    if !mounts
        .iter()
        .any(|m| !segments(m).is_empty() && under(to_root, m))
    {
        bail!("to_root {to_root:?} is not under a live mount");
    }
    Ok(())
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
                && !has_control(name)
        })
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelAudit {
    pub guests: Vec<LabelRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelRow {
    pub kind: String,
    pub vmid: u64,
    pub tags: Vec<String>,
}

pub fn check_plex_tag(audit: &LabelAudit, vmid: u32) -> Result<()> {
    let rows: Vec<&LabelRow> = audit
        .guests
        .iter()
        .filter(|g| g.kind.eq_ignore_ascii_case("lxc") && g.vmid == u64::from(vmid))
        .collect();
    if rows.is_empty() {
        bail!("Proxmox reports no LXC {vmid}");
    }
    if !rows
        .iter()
        .all(|g| g.tags.iter().any(|t| t.eq_ignore_ascii_case(PLEX_TAG)))
    {
        bail!("LXC {vmid} is not tagged `{PLEX_TAG}` in Proxmox");
    }
    Ok(())
}

async fn require_plex_tag(vmid: u32) -> Result<()> {
    let audit = tokio::task::spawn_blocking(|| {
        plugin_toolkit::tool_call::call_json(
            "proxmox.guest.label.audit",
            plugin_toolkit::serde_json::json!({ "all": true }),
        )
    })
    .await
    .context("proxmox tag lookup task")?
    .context("proxmox.guest.label.audit")?;
    let audit: LabelAudit =
        plugin_toolkit::serde_json::from_value(audit).context("decode proxmox label audit")?;
    check_plex_tag(&audit, vmid)
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
                plan(vec![owner_sh(
                    data_dir,
                    &script,
                    &[&db.path(data_dir), mode, &stmt],
                )])
            }
            Self::DbBackup { data_dir, db } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                let path = db.path(data_dir);
                let dest = format!("{path}.orca-bak-{}", backup_suffix());
                let script = format!("{DB_PREAMBLE_SH}{DB_BACKUP_SH}");
                plan(vec![owner_sh(data_dir, &script, &[&path, &dest])])
            }
            Self::SecurityDataDirOwner { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                plan(vec![root_sh(&data_dir_owner_script(), &[data_dir])])
            }
            Self::SecurityListCodecs { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                plan(vec![owner_sh(
                    data_dir,
                    LIST_CODECS_SH,
                    &[data_dir, FIND_CAP_BYTES],
                )])
            }
            Self::SecurityListLogs { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                plan(vec![owner_sh(
                    data_dir,
                    LIST_LOGS_SH,
                    &[data_dir, FIND_CAP_BYTES],
                )])
            }
            Self::SecurityGrepLogs { data_dir, logs } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                need(
                    !logs.is_empty() && logs.iter().all(|l| valid_log(data_dir, l)),
                    "log path",
                )?;
                let mut args: Vec<&str> = vec![data_dir.trim_end_matches('/')];
                args.extend(logs.iter().map(String::as_str));
                plan(vec![owner_sh(data_dir, &grep_script(), &args)])
            }
            Self::SecurityMkQuarantineDir { stamp } => {
                need(valid_stamp(stamp), "stamp")?;
                plan(vec![root_sh(MKQDIR_SH, &[&quarantine_dir(stamp)])])
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
                plan(vec![
                    root_sh(&owner_script(LAYOUT_TAIL), &[data_dir]),
                    root_sh(QUARANTINE_SH, &[src, dest, &codecs]),
                ])
            }
            Self::KillTranscoders { data_dir } => {
                need(valid_data_dir(data_dir), "data_dir")?;
                plan(vec![root_sh(&owner_script(KILL_TAIL), &[data_dir])])
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

fn scrub_bytes(b: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(b)
        .split_inclusive('\n')
        .map(scrub_tokens)
        .collect::<String>()
        .into_bytes()
}

pub fn scrub_reply(op: &GuestOp, mut out: ExecOutput) -> ExecOutput {
    if matches!(op, GuestOp::SecurityGrepLogs { .. }) {
        out.stdout = scrub_bytes(&out.stdout);
    }
    out.stderr = scrub_bytes(&out.stderr);
    out
}

pub fn command_for(guest: &Guest, step: &Step) -> Command {
    let mut cmd = match guest {
        Guest::Lxc(vmid) => {
            let mut c = Command::new("pct");
            c.arg("exec").arg(vmid.to_string()).arg("--");
            c
        }
        Guest::Docker(id) => {
            let mut c = Command::new("docker");
            c.arg("exec").arg(id);
            c
        }
    };
    if let RunAs::Owner(dd) = &step.run_as {
        cmd.args(sh(&owner_script(RUN_AS_OWNER_TAIL), &[dd]));
    }
    cmd.args(&step.argv);
    cmd.kill_on_drop(true);
    cmd
}

async fn spawn(guest: &Guest, step: &Step) -> Result<ExecOutput> {
    Ok(command_for(guest, step)
        .output()
        .await
        .context("failed to spawn guest command")?
        .into())
}

async fn run_steps(guest: &Guest, steps: &[Step]) -> Result<ExecOutput> {
    let mut last = None;
    for s in steps {
        let out = spawn(guest, s).await?;
        let ok = out.success;
        last = Some(out);
        if !ok {
            break;
        }
    }
    last.context("op has no steps")
}

fn lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

async fn preflight_rewrite(guest: &Guest, data_dir: &str, db: PlexDb, to_root: &str) -> Result<()> {
    let roots_op = GuestOp::Sql {
        data_dir: data_dir.to_string(),
        db,
        query: SqlQuery::LibraryRoots,
    };
    let roots = lines(
        &run_steps(guest, &roots_op.plan()?.steps)
            .await?
            .checked("read library roots")?,
    );
    let mounts = lines(
        &run_steps(guest, &GuestOp::StorageProbe.plan()?.steps)
            .await?
            .checked("findmnt")?,
    );
    check_rewrite(to_root, &roots, &mounts)?;
    let is_dir = Step {
        run_as: RunAs::Owner(data_dir.to_string()),
        argv: vec!["test".into(), "-d".into(), to_root.to_string()],
    };
    if !spawn(guest, &is_dir).await?.success {
        bail!("to_root {to_root:?} is not an existing directory");
    }
    Ok(())
}

async fn run_plan(guest: &Guest, op: &GuestOp) -> Result<ExecOutput> {
    let plan = op.plan()?;
    if let GuestOp::Sql {
        data_dir,
        db,
        query: SqlQuery::RewriteRoot { to_root, .. },
    } = op
    {
        preflight_rewrite(guest, data_dir, *db, to_root).await?;
    }
    Ok(scrub_reply(op, run_steps(guest, &plan.steps).await?))
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
            require_plex_tag(*vmid).await?;
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

async fn has_plex_marker(vmid: u32) -> Result<()> {
    if !spawn(&Guest::Lxc(vmid), &root(&["test", "-x", PLEX_MARKER]))
        .await?
        .success
    {
        bail!("guest {vmid} has no {PLEX_MARKER}");
    }
    Ok(())
}

async fn run_privileged(name: &'static str, req: GuestRequest) -> Result<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    if !caller_allowed(privileged::caller_uid(), &passwd) {
        bail!("caller is neither root nor the {ORCA_USER} service user; refusing {name}");
    }
    if req.op.name() != name {
        bail!("payload op {} does not match {name}", req.op.name());
    }
    has_plex_marker(req.vmid)
        .await
        .with_context(|| format!("refusing {name}"))?;
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
    fn plex_tree_reads_and_writes_never_run_as_root() {
        for op in all_ops() {
            if matches!(
                op,
                GuestOp::Sql { .. }
                    | GuestOp::DbBackup { .. }
                    | GuestOp::SecurityListCodecs { .. }
                    | GuestOp::SecurityListLogs { .. }
                    | GuestOp::SecurityGrepLogs { .. }
            ) {
                for s in op.plan().unwrap().steps {
                    assert_eq!(s.run_as, RunAs::Owner(DD.into()), "{}", op.name());
                }
            }
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
        let s = &p.steps[0];
        assert_eq!(s.run_as, RunAs::Owner(DD.into()));
        assert!(s.argv[2].contains("-safe -readonly"));
        assert_eq!(
            s.argv[4],
            format!("{DD}/Plug-in Support/Databases/com.plexapp.plugins.library.db")
        );
        assert_eq!(s.argv[5], "ro");
        assert!(!s.argv[6].contains("LIKE"));
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
        assert!(!w.steps[0].argv[6].contains("LIKE"));
    }

    #[test]
    fn offending_queries_use_literal_prefixes() {
        let s = SqlQuery::OffendingParts {
            roots: vec!["/m/100%_x".into()],
        }
        .statement()
        .unwrap();
        assert!(!s.contains("LIKE"));
        assert!(s.contains("substr(file,1,length('/m/100%_x/')) <> '/m/100%_x/'"));
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
        for (from, to) in [
            ("/a", "/b\0"),
            ("/", "/mnt"),
            ("/data", "/"),
            ("/data", "/etc/x"),
            ("/proc/1", "/mnt"),
            ("/data", "/var/lib/orca-quarantine/plex"),
            ("/data", "/data"),
            ("/data", "/mnt/../etc"),
        ] {
            assert!(
                bad(SqlQuery::RewriteRoot {
                    from_root: from.into(),
                    to_root: to.into()
                }),
                "{from} -> {to}"
            );
        }
        assert!(GuestOp::Sql {
            data_dir: "/x/../y".into(),
            db: PlexDb::Blobs,
            query: SqlQuery::LibraryRoots
        }
        .plan()
        .is_err());
    }

    #[test]
    fn rewrite_target_must_back_a_library_root_on_a_live_mount() {
        let roots = vec!["/mnt/data/media/movies".to_string()];
        let mounts = vec!["/".to_string(), "/mnt/data".to_string()];
        assert!(check_rewrite("/mnt/data/media", &roots, &mounts).is_ok());
        assert!(check_rewrite("/srv/other", &roots, &mounts).is_err());
        assert!(check_rewrite("/mnt/data/media", &roots, &["/".to_string()]).is_err());
        assert!(check_rewrite("/mnt/data/med", &roots, &mounts).is_err());
    }

    #[test]
    fn db_paths_are_fixed_basenames() {
        assert!(PlexDb::Blobs
            .path("/d/")
            .ends_with("/d/Plug-in Support/Databases/com.plexapp.plugins.library.blobs.db"));
    }

    #[test]
    fn db_backup_exclusive_create_and_verify() {
        let p = GuestOp::DbBackup {
            data_dir: DD.into(),
            db: PlexDb::Library,
        }
        .plan()
        .unwrap();
        let s = &p.steps[0];
        assert_eq!(s.run_as, RunAs::Owner(DD.into()));
        assert!(s.argv[2].contains("set -C"));
        assert!(s.argv[2].contains("stat -c %s"));
        assert!(s.argv[2].contains("stat -c %i"));
        assert!(!s.argv[2].contains("cp "));
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
    fn grep_reply_is_scrubbed_before_serialising() {
        let op = GuestOp::SecurityGrepLogs {
            data_dir: DD.into(),
            logs: vec![format!("{DD}/Logs/Plex Media Server.log")],
        };
        let out = ExecOutput {
            success: true,
            code: Some(0),
            status: "exit status: 0".into(),
            stdout: b"GET /x?X-Plex-Token=SECRET123&a=1\nGET /leak/ABCD\n".to_vec(),
            stderr: b"token=SECRET456\n".to_vec(),
        };
        let json = plugin_toolkit::serde_json::to_string(&scrub_reply(&op, out)).unwrap();
        let back: ExecOutput = plugin_toolkit::serde_json::from_str(&json).unwrap();
        let text = String::from_utf8(back.stdout).unwrap();
        assert!(!text.contains("SECRET123") && !text.contains("ABCD"));
        assert_eq!(text.lines().count(), 2);
        assert!(!String::from_utf8(back.stderr)
            .unwrap()
            .contains("SECRET456"));
    }

    #[test]
    fn listings_no_follow_with_cap() {
        for op in [
            GuestOp::SecurityListCodecs {
                data_dir: DD.into(),
            },
            GuestOp::SecurityListLogs {
                data_dir: DD.into(),
            },
        ] {
            let s = &op.plan().unwrap().steps[0];
            assert!(s.argv[2].contains("find -P"));
            assert!(s.argv[2].contains("-xdev"));
            assert!(s.argv[2].contains("head -c"));
        }
    }

    #[test]
    fn quarantine_checks_layout_then_runs_root_script() {
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
        assert_eq!(p.steps.len(), 2);
        assert!(p.steps.iter().all(|s| s.run_as == RunAs::Root));
        assert!(p.steps[0].argv[2].contains("uid 0; refusing"));
        assert!(p.steps[0].argv[2].contains("Plug-in Support/Databases"));
        assert_eq!(p.steps[1].argv[2], QUARANTINE_SH);
        assert_eq!(
            p.steps[1].argv[4..],
            [src.clone(), dest.clone(), format!("{DD}/Codecs")]
        );
        let c = command_for(&Guest::Lxc(7), &p.steps[1]);
        assert!(!argv(&c).iter().any(|a| a.contains("runuser")));
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
    fn kill_transcoders_resolves_owner_in_the_same_script() {
        let p = GuestOp::KillTranscoders {
            data_dir: DD.into(),
        }
        .plan()
        .unwrap();
        let s = &p.steps[0];
        assert_eq!(s.run_as, RunAs::Root);
        assert!(s.argv[2].contains("refusing to act as root"));
        assert!(s.argv[2].ends_with(r#"exec pkill -x -u "$owner" "Plex Transcoder""#));
        assert_eq!(s.argv[4], DD);
    }

    #[test]
    fn owner_steps_resolve_and_runuser_in_one_script() {
        let s = Step {
            run_as: RunAs::Owner(DD.into()),
            argv: vec!["true".to_string()],
        };
        for (guest, prefix) in [
            (Guest::Lxc(7), vec!["pct", "exec", "7", "--"]),
            (Guest::Docker("plex".into()), vec!["docker", "exec", "plex"]),
        ] {
            let a = argv(&command_for(&guest, &s));
            let n = prefix.len();
            assert_eq!(a[..n], prefix[..]);
            assert_eq!(a[n..n + 2], ["sh", "-c"]);
            assert!(a[n + 2].contains("uid 0; refusing"));
            assert!(a[n + 2].ends_with(r#"exec runuser -u "$owner" -- "$@""#));
            assert_eq!(a[n + 3..], ["plex", DD, "true"]);
        }
    }

    #[test]
    fn plex_tag_comes_from_proxmox_label_audit() {
        let audit: LabelAudit =
            plugin_toolkit::serde_json::from_value(plugin_toolkit::serde_json::json!({ "guests": [
                { "kind": "lxc", "vmid": 110, "tags": ["media", "plex"], "node": "thor" },
                { "kind": "lxc", "vmid": 115, "tags": ["media"] },
                { "kind": "qemu", "vmid": 120, "tags": ["plex"] },
                { "kind": "lxc", "vmid": 130, "tags": ["plex"] },
                { "kind": "lxc", "vmid": 130, "tags": ["other"] }
            ]}))
            .unwrap();
        assert!(check_plex_tag(&audit, 110).is_ok());
        assert!(check_plex_tag(&audit, 115).is_err());
        assert!(check_plex_tag(&audit, 120).is_err());
        assert!(check_plex_tag(&audit, 130).is_err());
        assert!(check_plex_tag(&audit, 999).is_err());
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
    fn db_backup_script_refuses_existing_dest_and_copies_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let db = root.join("a.db");
        std::fs::write(&db, b"sqlite-bytes").unwrap();
        let script = format!("{DB_PREAMBLE_SH}{DB_BACKUP_SH}");
        let run = |dest: &std::path::Path| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .arg("t")
                .arg(&db)
                .arg(dest)
                .output()
                .unwrap()
        };
        let dest = root.join("a.db.bak");
        assert!(run(&dest).status.success());
        assert_eq!(std::fs::read(&dest).unwrap(), b"sqlite-bytes");
        assert!(!run(&dest).status.success());
        let link = root.join("link.bak");
        std::os::unix::fs::symlink(root.join("victim"), &link).unwrap();
        assert!(!run(&link).status.success());
        assert!(!root.join("victim").exists());
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
