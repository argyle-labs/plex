//! Plex storage drift detection + remediation.
//!
//! The failure this module owns: Plex keeps `section_locations.root_path`
//! current when a library's mount path changes, but the per-item paths in
//! `media_parts.file` and the external-subtitle URLs in `media_streams.url`
//! can be left pointing at a **retired root** (e.g. `/data/movies` after the
//! media mount moved to `/mnt/data/media`). When playback needs a drifted
//! sidecar the fetch 404s and Plex aborts with *"the transcoder failed to
//! start up"* — invisible until someone plays a subtitled title.
//!
//! Two tools:
//!   - `plex.storage_check` — **detect.** Reads the live mounts + the DB's
//!     authoritative roots (`section_locations`) and samples the rows that do
//!     NOT live under any authoritative root. Emits typed [`StorageIssue`]s,
//!     each carrying a suggested [`DriftRewrite`] (`from_root` → `to_root`,
//!     inferred by aligning trailing path segments against the authoritative
//!     roots — the same reasoning a human does by eye).
//!   - `plex.storage_repair` — **remediate.** Dry-run by default. With
//!     `apply`, stops Plex, backs up the library DB, applies each planned
//!     rewrite via `replace()` across the three path-bearing columns, and
//!     restarts.
//!
//! The classifier ([`plan_rewrites`], [`classify`]) is pure and unit-tested;
//! the exec wrappers drive `pct exec` / `docker exec` and the bundled
//! `Plex SQLite` binary and hold no logic worth a container to test.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use tokio::process::Command;

use crate::lifecycle::Runtime;

/// Default library DB inside a stock Plex container.
const DEFAULT_DB: &str =
    "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db";
/// The `sqlite3`-compatible binary Plex ships; avoids needing sqlite installed.
const PLEX_SQLITE: &str = "/usr/lib/plexmediaserver/Plex SQLite";
/// Systemd unit name inside the container.
const PLEX_UNIT: &str = "plexmediaserver";

// ═══════════════════════════════════════════════════════════════════════════
// Pure classifier
// ═══════════════════════════════════════════════════════════════════════════

/// A planned prefix rewrite: every stored path under `from_root` should move
/// to `to_root`. `occurrences` is the number of sampled offending paths that
/// resolved to this mapping (a lower bound on affected rows, for reporting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct DriftRewrite {
    /// Retired root currently baked into stored paths, e.g. `/data`.
    pub from_root: String,
    /// Live root the authoritative library location resolves under, e.g.
    /// `/mnt/data/media`.
    pub to_root: String,
    /// How many sampled offending paths mapped to this rewrite.
    pub occurrences: u64,
}

/// Severity of a storage issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Playback-breaking (drifted paths / missing media mount).
    Critical,
    /// Degraded but not blocking.
    Warning,
}

/// One detected storage problem plus the action that would fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct StorageIssue {
    /// Stable machine kind, e.g. `path_drift` / `mount_missing`.
    pub kind: String,
    /// Severity for triage.
    pub severity: Severity,
    /// Human-readable explanation.
    pub detail: String,
    /// The prefix rewrite that resolves a `path_drift` issue, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<DriftRewrite>,
}

/// Full detect report for one instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct StorageReport {
    /// True when no issues were found.
    pub healthy: bool,
    /// Authoritative library roots as Plex records them.
    pub library_roots: Vec<String>,
    /// Live mount targets observed inside the container.
    pub mount_targets: Vec<String>,
    /// Everything wrong, each with its remediation.
    pub issues: Vec<StorageIssue>,
}

/// Split an absolute path into its non-empty segments.
fn segments(path: &str) -> Vec<&str> {
    path.trim_start_matches("file://")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect()
}

/// True when `path` is `root` itself or lives beneath it (segment-aligned, so
/// `/mnt/data` does not match `/mnt/database`).
fn is_under(path: &str, root: &str) -> bool {
    let (p, r) = (segments(path), segments(root));
    r.len() <= p.len() && p[..r.len()] == r[..]
}

/// Infer the `from_root → to_root` rewrite for a single offending path.
///
/// An authoritative library root ends at its library folder
/// (`/mnt/data/media/movies`); an offending item path continues *past* that
/// folder (`/data/movies/Film/f.mkv`). So the shared token is the
/// authoritative root's **last segment** (`movies`), which appears mid-path in
/// the offending path. Split the offending path there: everything before is the
/// retired `from_root` (`/data`); the authoritative root minus its last segment
/// is the live `to_root` (`/mnt/data/media`).
///
/// When several authoritative roots match, prefer the shallowest split (the
/// most-general retired root). Returns `None` when nothing aligns, the split
/// would leave an empty `from_root` (offending starts at the library folder —
/// too broad to map), or the roots already match.
fn infer_rewrite(offending: &str, authoritative: &[String]) -> Option<(String, String)> {
    let off = segments(offending);
    let mut best: Option<(usize, String)> = None; // (split index, to_root)
    for a in authoritative {
        let a_segs = segments(a);
        let Some((last, prefix)) = a_segs.split_last() else {
            continue;
        };
        if prefix.is_empty() {
            continue; // authoritative root has no parent dir to map onto
        }
        let Some(i) = off.iter().position(|s| s == last) else {
            continue;
        };
        if i == 0 {
            continue; // offending starts at the library folder ⇒ from_root "/"
        }
        if best.as_ref().is_none_or(|(bi, _)| i < *bi) {
            best = Some((i, format!("/{}", prefix.join("/"))));
        }
    }
    let (i, to_root) = best?;
    let from_root = format!("/{}", off[..i].join("/"));
    if from_root == to_root {
        return None;
    }
    Some((from_root, to_root))
}

/// Plan the set of distinct prefix rewrites for a batch of offending paths.
/// Deduplicated by `(from_root, to_root)`, summing occurrences, sorted for a
/// stable report/repair order (longest `from_root` first so a shorter,
/// more-general rewrite can never shadow a specific one).
pub fn plan_rewrites(offending: &[String], authoritative: &[String]) -> Vec<DriftRewrite> {
    let mut acc: Vec<DriftRewrite> = Vec::new();
    for path in offending {
        let Some((from_root, to_root)) = infer_rewrite(path, authoritative) else {
            continue;
        };
        if let Some(existing) = acc
            .iter_mut()
            .find(|r| r.from_root == from_root && r.to_root == to_root)
        {
            existing.occurrences += 1;
        } else {
            acc.push(DriftRewrite {
                from_root,
                to_root,
                occurrences: 1,
            });
        }
    }
    acc.sort_by(|a, b| {
        b.from_root
            .len()
            .cmp(&a.from_root.len())
            .then_with(|| a.from_root.cmp(&b.from_root))
    });
    acc
}

/// Build the full report from gathered facts. Pure — the exec layer feeds it
/// the observed mounts, authoritative roots, and a sample of offending paths.
pub fn classify(
    library_roots: Vec<String>,
    mount_targets: Vec<String>,
    offending_sample: &[String],
) -> StorageReport {
    let mut issues = Vec::new();

    // A library root that isn't backed by any live mount = the media mount is
    // missing / not mounted where Plex expects it.
    for root in &library_roots {
        if !mount_targets.iter().any(|m| is_under(root, m)) {
            issues.push(StorageIssue {
                kind: "mount_missing".to_string(),
                severity: Severity::Critical,
                detail: format!(
                    "library root '{root}' is not backed by any live mount ({})",
                    if mount_targets.is_empty() {
                        "no mounts observed".to_string()
                    } else {
                        mount_targets.join(", ")
                    }
                ),
                rewrite: None,
            });
        }
    }

    // Rows stored under a retired root ⇒ path drift, one issue per rewrite.
    for rw in plan_rewrites(offending_sample, &library_roots) {
        issues.push(StorageIssue {
            kind: "path_drift".to_string(),
            severity: Severity::Critical,
            detail: format!(
                "stored paths under '{}' should be '{}' ({}+ sampled)",
                rw.from_root, rw.to_root, rw.occurrences
            ),
            rewrite: Some(rw),
        });
    }

    StorageReport {
        healthy: issues.is_empty(),
        library_roots,
        mount_targets,
        issues,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Exec layer
// ═══════════════════════════════════════════════════════════════════════════

/// Base `pct exec <id> --` / `docker exec <id>` command the guest steps append
/// their argv to. No shell: args are passed literally, so paths with spaces
/// (the `Plex SQLite` binary, the DB) need no quoting.
fn guest(runtime: Runtime, id: &str) -> Command {
    let mut cmd = match runtime {
        Runtime::Lxc => {
            let mut c = Command::new("pct");
            c.arg("exec").arg(id).arg("--");
            c
        }
        Runtime::Docker => {
            let mut c = Command::new("docker");
            c.arg("exec").arg(id);
            c
        }
    };
    cmd.kill_on_drop(true);
    cmd
}

/// Run a guest command, returning stdout; a non-zero exit carries stderr.
async fn capture(mut cmd: Command) -> Result<String> {
    let out = cmd
        .output()
        .await
        .context("failed to spawn guest command")?;
    if !out.status.success() {
        bail!(
            "guest command failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run one SQL statement through the container's `Plex SQLite`, newline-joining
/// result columns with `|` (sqlite default). `db` is passed as a literal arg.
async fn plex_sql(runtime: Runtime, id: &str, db: &str, sql: &str) -> Result<String> {
    let mut cmd = guest(runtime, id);
    cmd.arg(PLEX_SQLITE).arg(db).arg(sql);
    capture(cmd).await
}

/// Non-empty, de-duplicated lines from a guest command's stdout.
fn lines(out: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for l in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !seen.iter().any(|s| s == l) {
            seen.push(l.to_string());
        }
    }
    seen
}

/// Build a `NOT LIKE` guard excluding rows under any authoritative root.
/// `col` is the column; `prefix` is prepended to each root (empty for
/// `media_parts.file`, `file://` for the scheme-carrying `media_streams.url`).
fn not_under_clause(col: &str, roots: &[String], prefix: &str) -> String {
    roots
        .iter()
        .map(|r| format!("{col} NOT LIKE '{prefix}{}/%'", sql_escape(r)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Escape single quotes for a SQL string literal.
fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Gather the live mounts + authoritative roots + a bounded sample of
/// offending paths, then classify.
async fn check(runtime: Runtime, id: &str, db: &str) -> Result<StorageReport> {
    // Live mount targets inside the guest.
    let mut mount_cmd = guest(runtime, id);
    mount_cmd.arg("findmnt").arg("-rno").arg("TARGET");
    let mount_targets = lines(&capture(mount_cmd).await.unwrap_or_default());

    // Authoritative library roots.
    let roots = lines(
        &plex_sql(
            runtime,
            id,
            db,
            "SELECT root_path FROM section_locations WHERE root_path LIKE '/%';",
        )
        .await?,
    );

    // Sample offending rows from both path-bearing columns.
    let mut offending = Vec::new();
    if !roots.is_empty() {
        let parts_sql = format!(
            "SELECT DISTINCT file FROM media_parts WHERE file LIKE '/%' AND {} LIMIT 100;",
            not_under_clause("file", &roots, "")
        );
        offending.extend(lines(&plex_sql(runtime, id, db, &parts_sql).await?));

        let streams_sql = format!(
            "SELECT DISTINCT url FROM media_streams WHERE url LIKE 'file:///%' AND {} LIMIT 100;",
            not_under_clause("url", &roots, "file://")
        );
        offending.extend(lines(&plex_sql(runtime, id, db, &streams_sql).await?));
    }

    Ok(classify(roots, mount_targets, &offending))
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.storage_check — DETECT
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexStorageCheckArgs {
    /// Where the instance runs: `lxc` or `docker`.
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid or docker container name/id.
    #[arg(long)]
    pub target: String,
    /// Override the in-container library DB path.
    #[arg(long, default_value_t = default_db())]
    #[serde(default = "default_db")]
    pub db_path: String,
}

fn default_db() -> String {
    DEFAULT_DB.to_string()
}

/// **Detect media-storage problems** on a Plex instance: a library root not
/// backed by a live mount, and per-item / subtitle paths stranded under a
/// retired root. Read-only; each issue carries the rewrite that would fix it.
#[orca_tool(domain = "plex", verb = "storage_check")]
async fn plex_storage_check(args: PlexStorageCheckArgs, _ctx: &ToolCtx) -> Result<StorageReport> {
    check(args.runtime, &args.target, &args.db_path).await
}

// ═══════════════════════════════════════════════════════════════════════════
// plex.storage_repair — REMEDIATE
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexStorageRepairArgs {
    /// Where the instance runs: `lxc` or `docker`.
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid or docker container name/id.
    #[arg(long)]
    pub target: String,
    /// Override the in-container library DB path.
    #[arg(long, default_value_t = default_db())]
    #[serde(default = "default_db")]
    pub db_path: String,
    /// Apply the fixes. Without this the tool only reports the plan (dry-run).
    #[arg(long)]
    #[serde(default)]
    pub apply: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexStorageRepairOutput {
    /// True when the rewrites were applied; false for a dry-run.
    pub applied: bool,
    /// The rewrites that were (or would be) applied.
    pub rewrites: Vec<DriftRewrite>,
    /// Backup DB path written before applying (apply only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// **Remediate storage drift.** Recomputes the plan via `storage_check`, then
/// (with `apply`) stops Plex, backs up the library DB, rewrites the three
/// path-bearing columns (`media_parts.file`, `media_streams.url`,
/// `section_locations.root_path`) for each drifted root, and restarts. Dry-run
/// by default.
#[orca_tool(domain = "plex", verb = "storage_repair")]
async fn plex_storage_repair(
    args: PlexStorageRepairArgs,
    _ctx: &ToolCtx,
) -> Result<PlexStorageRepairOutput> {
    let report = check(args.runtime, &args.target, &args.db_path).await?;
    let rewrites: Vec<DriftRewrite> = report
        .issues
        .into_iter()
        .filter_map(|i| i.rewrite)
        .collect();

    if !args.apply || rewrites.is_empty() {
        return Ok(PlexStorageRepairOutput {
            applied: false,
            rewrites,
            backup: None,
        });
    }

    let (rt, id, db) = (args.runtime, args.target.as_str(), args.db_path.as_str());

    // Stop Plex so the DB is quiescent before we rewrite it.
    let mut stop = guest(rt, id);
    stop.arg("systemctl").arg("stop").arg(PLEX_UNIT);
    capture(stop).await.context("stop plexmediaserver")?;

    // Back the DB up next to itself.
    let backup = format!("{db}.orca-bak-{}", now_stamp());
    let mut cp = guest(rt, id);
    cp.arg("cp").arg("-a").arg(db).arg(&backup);
    capture(cp).await.context("back up library DB")?;

    // Apply each rewrite, anchored so a row is only touched once.
    for rw in &rewrites {
        let sql = repair_sql(rw);
        plex_sql(rt, id, db, &sql)
            .await
            .with_context(|| format!("apply rewrite {} -> {}", rw.from_root, rw.to_root))?;
    }

    // Restart.
    let mut start = guest(rt, id);
    start.arg("systemctl").arg("start").arg(PLEX_UNIT);
    capture(start).await.context("start plexmediaserver")?;

    Ok(PlexStorageRepairOutput {
        applied: true,
        rewrites,
        backup: Some(backup),
    })
}

/// The `UPDATE ... replace()` batch for one rewrite, across all three
/// path-bearing columns. Rows are guarded by a leading `LIKE` so `to_root`
/// (which never shares `from_root`'s leading string) can't be re-matched.
fn repair_sql(rw: &DriftRewrite) -> String {
    let (from, to) = (sql_escape(&rw.from_root), sql_escape(&rw.to_root));
    format!(
        "UPDATE media_parts SET file=replace(file,'{from}/','{to}/') WHERE file LIKE '{from}/%';\n\
         UPDATE media_streams SET url=replace(url,'file://{from}/','file://{to}/') WHERE url LIKE 'file://{from}/%';\n\
         UPDATE section_locations SET root_path=replace(root_path,'{from}/','{to}/') WHERE root_path LIKE '{from}/%';"
    )
}

/// UTC `YYYYMMDD-HHMMSS` stamp for backup names, via the orca core datetime
/// seam — the plugin bundles no datetime library of its own.
fn now_stamp() -> String {
    plugin_toolkit::time::Timestamp::now().compact()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn is_under_is_segment_aligned() {
        assert!(is_under("/mnt/data/media/movies", "/mnt/data"));
        assert!(is_under("/mnt/data", "/mnt/data"));
        assert!(!is_under("/mnt/database/x", "/mnt/data"));
        assert!(!is_under("/data/movies", "/mnt/data"));
    }

    #[test]
    fn infers_legacy_data_root() {
        // The mimir case: stored under /data, authoritative at /mnt/data/media.
        let auth = s(&["/mnt/data/media/movies", "/mnt/data/media/tv"]);
        let got = infer_rewrite("/data/movies/Film/f.mkv", &auth);
        assert_eq!(got, Some(("/data".into(), "/mnt/data/media".into())));
    }

    #[test]
    fn infers_from_subtitle_url_with_scheme() {
        let auth = s(&["/mnt/data/media/tv"]);
        let got = infer_rewrite("file:///data/tv/Show/S01/e.en.srt", &auth);
        assert_eq!(got, Some(("/data".into(), "/mnt/data/media".into())));
    }

    #[test]
    fn infers_doubled_media_root() {
        // The njord case: stored under /mnt/media/media, authoritative moved to
        // /mnt/data/media.
        let auth = s(&["/mnt/data/media/movies"]);
        let got = infer_rewrite("/mnt/media/media/movies/F/f.mkv", &auth);
        assert_eq!(
            got,
            Some(("/mnt/media/media".into(), "/mnt/data/media".into()))
        );
    }

    #[test]
    fn no_rewrite_when_already_authoritative() {
        let auth = s(&["/mnt/data/media/movies"]);
        assert_eq!(infer_rewrite("/mnt/data/media/movies/F/f.mkv", &auth), None);
    }

    #[test]
    fn no_rewrite_when_offending_starts_at_library_folder() {
        // Offending path begins with the library folder itself, so the retired
        // root would be empty ("/") — too broad to map. Refuse.
        let auth = s(&["/mnt/data/media/movies"]);
        assert_eq!(infer_rewrite("/movies/Film/f.mkv", &auth), None);
    }

    #[test]
    fn no_rewrite_when_no_library_folder_matches() {
        let auth = s(&["/mnt/data/media/movies", "/mnt/data/media/tv"]);
        assert_eq!(infer_rewrite("/somewhere/else/x.mkv", &auth), None);
    }

    #[test]
    fn plan_dedupes_and_counts() {
        let auth = s(&["/mnt/data/media/movies", "/mnt/data/media/tv"]);
        let offending = s(&[
            "/data/movies/A/a.mkv",
            "/data/tv/B/b.mkv",
            "file:///data/tv/B/b.en.srt",
        ]);
        let plan = plan_rewrites(&offending, &auth);
        assert_eq!(plan.len(), 1, "all map to one /data -> /mnt/data/media");
        assert_eq!(plan[0].from_root, "/data");
        assert_eq!(plan[0].to_root, "/mnt/data/media");
        assert_eq!(plan[0].occurrences, 3);
    }

    #[test]
    fn classify_flags_drift_and_missing_mount() {
        let roots = s(&["/mnt/data/media/movies"]);
        let mounts = s(&["/mnt/data"]);
        let offending = s(&["/data/movies/A/a.mkv"]);
        let report = classify(roots, mounts, &offending);
        assert!(!report.healthy);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, "path_drift");
        assert_eq!(
            report.issues[0].rewrite.as_ref().unwrap().from_root,
            "/data"
        );
    }

    #[test]
    fn classify_flags_missing_mount_when_root_unbacked() {
        // Library root present but nothing mounted under it.
        let report = classify(s(&["/mnt/data/media/movies"]), s(&["/mnt/backups"]), &[]);
        assert!(!report.healthy);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, "mount_missing");
    }

    #[test]
    fn classify_healthy_when_aligned() {
        let report = classify(s(&["/mnt/data/media/movies"]), s(&["/mnt/data"]), &[]);
        assert!(report.healthy);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn repair_sql_anchors_and_covers_three_columns() {
        let rw = DriftRewrite {
            from_root: "/data".into(),
            to_root: "/mnt/data/media".into(),
            occurrences: 1,
        };
        let sql = repair_sql(&rw);
        assert!(
            sql.contains("UPDATE media_parts SET file=replace(file,'/data/','/mnt/data/media/')")
        );
        assert!(sql.contains("url=replace(url,'file:///data/','file:///mnt/data/media/')"));
        assert!(sql.contains("UPDATE section_locations"));
        assert!(sql.contains("WHERE file LIKE '/data/%'"));
    }

    #[test]
    fn lines_trims_and_dedupes() {
        assert_eq!(lines("  /a \n\n/a\n/b\n"), s(&["/a", "/b"]));
    }
}
