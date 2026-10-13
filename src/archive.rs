//! Checksummed backup archives: a `.sha256` sidecar is written beside every
//! archive, and restore verifies a private copy against it before extracting
//! that copy with path-traversal guards.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use plugin_toolkit::prelude::*;
use plugin_toolkit::process::Command;

/// Suffix of the checksum sidecar written next to every archive.
pub const CHECKSUM_SUFFIX: &str = ".sha256";

/// Hashes `archive` and writes `<hex>  <basename>` (so `sha256sum -c` also
/// checks it) via a temp file and rename, so a reader never sees a partial
/// sidecar. Returns the hex digest.
pub fn write_sidecar(archive: &str) -> Result<String> {
    let sha256 = hash_file(Path::new(archive))?;
    let checksum_path = format!("{archive}{CHECKSUM_SUFFIX}");
    let name = Path::new(archive)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| archive.to_string());
    let tmp = format!("{checksum_path}.tmp.{}", std::process::id());
    std::fs::write(&tmp, format!("{sha256}  {name}\n"))
        .with_context(|| format!("failed to write checksum '{tmp}'"))?;
    std::fs::rename(&tmp, &checksum_path)
        .with_context(|| format!("failed to write checksum '{checksum_path}'"))?;
    Ok(sha256)
}

fn hash_file(path: &Path) -> Result<String> {
    plugin_toolkit::hash::sha256_file(path)
        .with_context(|| format!("failed to hash '{}'", path.display()))
}

/// A verified copy of an archive in a fresh 0700 directory, removed on drop.
/// Extracting this copy means the bytes extracted are the bytes verified, even
/// if the original is replaced in between.
pub struct VerifiedArchive {
    pub sha256: String,
    dir: PathBuf,
    file: PathBuf,
}

impl Drop for VerifiedArchive {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            plugin_toolkit::tracing::warn!("failed to remove '{}': {e}", self.dir.display());
        }
    }
}

/// Copies `archive` privately and verifies the copy against its sidecar. A
/// missing or malformed sidecar fails: an unverifiable archive is never
/// restored. The sidecar detects corruption, not tampering — whoever can
/// rewrite the archive can rewrite the sidecar.
pub fn verify(archive: &str) -> Result<VerifiedArchive> {
    let checksum_path = format!("{archive}{CHECKSUM_SUFFIX}");
    let raw = std::fs::read_to_string(&checksum_path).with_context(|| {
        format!("checksum sidecar '{checksum_path}' missing; refusing an unverified archive")
    })?;
    let expected = raw
        .split_whitespace()
        .next()
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .with_context(|| format!("checksum sidecar '{checksum_path}' is malformed"))?
        .to_ascii_lowercase();

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "orca-restore-{}-{}-{}",
        std::process::id(),
        plugin_toolkit::time::now().compact(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("failed to create '{}'", dir.display()))?;
    }
    let mut copy = VerifiedArchive {
        sha256: String::new(),
        file: dir.join("archive.tar.gz"),
        dir,
    };
    std::fs::copy(archive, &copy.file).with_context(|| format!("failed to copy '{archive}'"))?;
    let actual = hash_file(&copy.file)?;
    if actual != expected {
        bail!("checksum mismatch for '{archive}': expected {expected}, got {actual}");
    }
    copy.sha256 = actual;
    Ok(copy)
}

/// Extracts a verified archive into `dest` after rejecting any entry that is
/// absolute, contains `..`, is a symlink/hardlink resolving outside `dest`, or
/// is not a regular file, directory, or link.
pub async fn extract(archive: &VerifiedArchive, dest: &str) -> Result<()> {
    let names = tar_list(&archive.file, "-tzf").await?;
    let lines = tar_list(&archive.file, "-tvzf").await?;
    if names.len() != lines.len() {
        bail!("archive listing is ambiguous; refusing to extract");
    }
    for (name, line) in names.iter().zip(&lines) {
        check_entry(name, line)?;
    }
    std::fs::create_dir_all(dest).with_context(|| format!("failed to create '{dest}'"))?;
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(&archive.file)
        .arg("-C")
        .arg(dest)
        .output()
        .await
        .context("failed to spawn tar")?;
    if !out.status.success {
        bail!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn tar_list(file: &Path, flags: &str) -> Result<Vec<String>> {
    let out = Command::new("tar")
        .arg(flags)
        .arg(file)
        .output()
        .await
        .context("failed to spawn tar")?;
    if !out.status.success {
        bail!(
            "tar list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// `line` is the `tar -tv` row for `name`; its first char is the entry type and
/// a link's target follows `name` (` -> ` for symlinks, ` link to ` for hardlinks).
fn check_entry(name: &str, line: &str) -> Result<()> {
    let refuse = |why: &str| -> Result<()> { bail!("refusing archive entry '{name}': {why}") };
    if !stays_inside(Path::new(""), name) {
        return refuse("escapes the restore directory");
    }
    match line.chars().next() {
        Some('-' | 'd') => Ok(()),
        Some('l') => {
            let Some(target) = line.split_once(&format!(" {name} -> ")).map(|(_, t)| t) else {
                return refuse("unparseable symlink");
            };
            let parent = Path::new(name).parent().unwrap_or(Path::new(""));
            if stays_inside(parent, target) {
                Ok(())
            } else {
                refuse("symlink points outside the restore directory")
            }
        }
        Some('h') => {
            let Some(target) = line
                .split_once(&format!(" {name} link to "))
                .map(|(_, t)| t)
            else {
                return refuse("unparseable hardlink");
            };
            if stays_inside(Path::new(""), target) {
                Ok(())
            } else {
                refuse("hardlink points outside the restore directory")
            }
        }
        _ => refuse("unsupported entry type"),
    }
}

/// Lexically resolves `rel` against `base` (both relative to the restore root)
/// and reports whether it never climbs above the root.
fn stays_inside(base: &Path, rel: &str) -> bool {
    let mut depth: usize = 0;
    for c in base.components().chain(Path::new(rel).components()) {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar(args: &[&str], cwd: &Path) {
        let ok = std::process::Command::new("tar")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap()
            .success();
        assert!(ok, "tar {args:?}");
    }

    fn good_archive(tmp: &Path) -> String {
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/file"), b"data").unwrap();
        std::os::unix::fs::symlink("sub/file", src.join("link")).unwrap();
        let archive = tmp.join("good.tar.gz").to_string_lossy().into_owned();
        tar(&["-czf", &archive, "-C", &src.to_string_lossy(), "."], tmp);
        archive
    }

    #[tokio::test]
    async fn good_archive_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = good_archive(tmp.path());
        let sha = write_sidecar(&archive).unwrap();
        let v = verify(&archive).unwrap();
        assert_eq!(v.sha256, sha);
        let dest = tmp.path().join("dest");
        extract(&v, &dest.to_string_lossy()).await.unwrap();
        assert_eq!(std::fs::read(dest.join("sub/file")).unwrap(), b"data");
        assert_eq!(std::fs::read(dest.join("link")).unwrap(), b"data");
    }

    #[test]
    fn missing_sidecar_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = good_archive(tmp.path());
        let err = verify(&archive).err().unwrap().to_string();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn mismatched_sidecar_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = good_archive(tmp.path());
        write_sidecar(&archive).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&archive)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"x"))
            .unwrap();
        let err = verify(&archive).err().unwrap().to_string();
        assert!(err.contains("mismatch"), "{err}");
    }

    #[tokio::test]
    async fn traversal_entry_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("a/b");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(tmp.path().join("a/evil"), b"x").unwrap();
        let archive = tmp
            .path()
            .join("evil.tar.gz")
            .to_string_lossy()
            .into_owned();
        // -P keeps the `..` prefix in the stored name.
        tar(&["-czPf", &archive, "../evil"], &work);
        write_sidecar(&archive).unwrap();
        let v = verify(&archive).unwrap();
        let dest = tmp.path().join("dest");
        let err = extract(&v, &dest.to_string_lossy())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes"), "{err}");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn escaping_symlink_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::os::unix::fs::symlink("../../etc", src.join("out")).unwrap();
        let archive = tmp.path().join("sym.tar.gz").to_string_lossy().into_owned();
        tar(
            &["-czf", &archive, "-C", &src.to_string_lossy(), "."],
            tmp.path(),
        );
        write_sidecar(&archive).unwrap();
        let v = verify(&archive).unwrap();
        let err = extract(&v, &tmp.path().join("dest").to_string_lossy())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("symlink points outside"), "{err}");
    }

    #[test]
    fn stays_inside_rules() {
        assert!(stays_inside(Path::new(""), "./a/../b"));
        assert!(!stays_inside(Path::new(""), "../x"));
        assert!(!stays_inside(Path::new(""), "/etc/passwd"));
        assert!(stays_inside(Path::new("a"), ".."));
        assert!(!stays_inside(Path::new("a"), "../.."));
    }
}
