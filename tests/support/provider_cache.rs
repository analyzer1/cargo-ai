//! Disposable compilation reuse for sequential generated-provider qualification.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    digest: Option<String>,
    mode: u32,
    modified: Option<std::time::SystemTime>,
}

fn plain_metadata(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || reparse(&metadata) {
        return Err(io::Error::other("cache links/reparse points are forbidden"));
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(io::Error::other("unsupported cache entry"));
    }
    Ok(metadata)
}

#[cfg(windows)]
fn reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn reparse(_: &fs::Metadata) -> bool {
    false
}

fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        u32::from(metadata.permissions().readonly())
    }
}

fn digest(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn inventory(root: &Path) -> io::Result<BTreeMap<PathBuf, Entry>> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) -> io::Result<()> {
        let metadata = plain_metadata(path)?;
        entries.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            Entry {
                digest: if metadata.is_file() {
                    Some(String::new())
                } else {
                    None
                },
                mode: mode(&metadata),
                modified: if metadata.is_file() {
                    Some(metadata.modified()?)
                } else {
                    None
                },
            },
        );
        if metadata.is_dir() {
            for child in fs::read_dir(path)? {
                visit(root, &child?.path(), entries)?;
            }
        }
        Ok(())
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries)?;
    // CI already uses Python. Its native hashing avoids unoptimized test-binary
    // hashing dominating the cost of copying a complete compilation cache.
    let output = Command::new("python3")
        .args(["-c", include_str!("cache_digests.py")])
        .arg(root)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("disposable cache inventory failed"));
    }
    let mut hashes: BTreeMap<PathBuf, String> = serde_json::from_slice(&output.stdout)
        .map_err(|_| io::Error::other("invalid cache digest inventory"))?;
    for (path, entry) in &mut entries {
        if entry.digest.is_some() {
            let hash = hashes
                .remove(path)
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| io::Error::other("missing cache digest"))?;
            entry.digest = Some(hash);
        }
    }
    if !hashes.is_empty() {
        return Err(io::Error::other("unexpected cache digest"));
    }
    Ok(entries)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CacheIdentity {
    pub binary: String,
    pub rustc: String,
    pub target: String,
}

impl CacheIdentity {
    pub fn current(cli: &Path) -> Self {
        let output = Command::new("rustc").arg("-vV").output().unwrap();
        assert!(output.status.success(), "rustc identity should resolve");
        let version = String::from_utf8(output.stdout).unwrap();
        let release = version
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let host = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .unwrap();
        let target = std::env::var("CARGO_BUILD_TARGET")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| host.to_string());
        Self {
            binary: digest(cli).unwrap(),
            rustc: format!("rustc-{release}"),
            target,
        }
    }

    fn relative_path(&self) -> PathBuf {
        PathBuf::from(&self.binary)
            .join(&self.rustc)
            .join(&self.target)
            .join("release")
    }
}

pub(super) struct SeedCache {
    root: PathBuf,
    identity: CacheIdentity,
    entries: BTreeMap<PathBuf, Entry>,
}

impl SeedCache {
    pub fn capture(home: &Path, identity: CacheIdentity) -> io::Result<Self> {
        plain_metadata(home)?;
        let root = home.join("templates");
        let entries = inventory(&root)?;
        let relative = identity.relative_path();
        // Only the expected binary/toolchain/target/release workspace may transfer.
        if entries
            .keys()
            .any(|p| !p.starts_with(&relative) && !relative.starts_with(p))
        {
            return Err(io::Error::other("unexpected cache identity or entry"));
        }
        for file in ["Cargo.toml", "build.rs", ".agentcfg", "src/main.rs"] {
            if entries
                .get(&relative.join(file))
                .and_then(|e| e.digest.as_ref())
                .is_none()
            {
                return Err(io::Error::other("incomplete template seed"));
            }
        }
        let executable = if identity.target.contains("windows") {
            "template_seed_agent.exe"
        } else {
            "template_seed_agent"
        };
        let native = relative.join("target/release").join(executable);
        let explicit = relative
            .join("target")
            .join(&identity.target)
            .join("release")
            .join(executable);
        if ![native, explicit]
            .iter()
            .any(|p| entries.get(p).is_some_and(|e| e.digest.is_some()))
        {
            return Err(io::Error::other("compiled release seed missing"));
        }
        Ok(Self {
            root,
            identity,
            entries,
        })
    }

    pub fn unchanged(&self) -> io::Result<()> {
        if inventory(&self.root)? != self.entries {
            return Err(io::Error::other("neutral seed was modified"));
        }
        Ok(())
    }

    pub fn copy_into(&self, home: &Path, identity: &CacheIdentity) -> io::Result<()> {
        if identity != &self.identity {
            return Err(io::Error::other("incompatible compilation seed"));
        }
        plain_metadata(home)?;
        if fs::read_dir(home)?.next().is_some() {
            return Err(io::Error::other("recipient home must be fresh"));
        }
        self.unchanged()?;
        let destination = home.join("templates");
        // The inventory is frozen before any provider case; copies never flow back.
        for (relative, entry) in &self.entries {
            let source = self.root.join(relative);
            let target = destination.join(relative);
            if entry.digest.is_some() {
                let mut input = fs::File::open(&source)?;
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                io::copy(&mut input, &mut output)?;
                // Cargo fingerprints depend on timestamp relationships as well
                // as bytes. Resetting times invalidates otherwise reusable work.
                output.set_times(fs::FileTimes::new().set_modified(entry.modified.unwrap()))?;
            } else {
                fs::create_dir(&target)?;
            }
            fs::set_permissions(&target, plain_metadata(&source)?.permissions())?;
        }
        if inventory(&destination)? != self.entries {
            return Err(io::Error::other(
                "copied seed differs from frozen inventory",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fixture;

    fn seed(fixture: &Fixture) -> (CacheIdentity, PathBuf) {
        let identity = CacheIdentity {
            binary: "test-digest".into(),
            rustc: "rustc-test".into(),
            target: "test-target".into(),
        };
        let root = fixture
            .home
            .join("templates")
            .join(identity.relative_path());
        for name in [
            "Cargo.toml",
            "build.rs",
            ".agentcfg",
            "src/main.rs",
            "target/release/template_seed_agent",
        ] {
            let file = root.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "neutral").unwrap();
        }
        (identity, root)
    }

    #[test]
    fn copies_only_neutral_cache_and_preserves_independent_state() {
        let source = Fixture::new();
        let (identity, root) = seed(&source);
        fs::write(source.home.join("credentials"), "source-secret-sentinel").unwrap();
        fs::write(source.root.join("case-output"), "source-output-sentinel").unwrap();
        let executable = root.join("target/release/template_seed_agent");
        let recorded_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&executable)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(recorded_time))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cache = SeedCache::capture(&source.home, identity.clone()).unwrap();
        let first = Fixture::new();
        let second = Fixture::new();
        cache.copy_into(&first.home, &identity).unwrap();
        fs::write(first.home.join("case-state"), "case-specific").unwrap();
        fs::write(
            first
                .home
                .join("templates")
                .join(identity.relative_path())
                .join(".agentcfg"),
            "case-definition-sentinel",
        )
        .unwrap();
        cache.copy_into(&second.home, &identity).unwrap();
        assert!(!second.home.join("case-state").exists());
        assert!(!second.home.join("credentials").exists());
        assert!(!second.root.join("case-output").exists());
        assert_eq!(
            fs::read_to_string(
                second
                    .home
                    .join("templates")
                    .join(identity.relative_path())
                    .join(".agentcfg")
            )
            .unwrap(),
            "neutral"
        );
        assert_eq!(
            fs::read_to_string(source.home.join("credentials")).unwrap(),
            "source-secret-sentinel"
        );
        cache.unchanged().unwrap();
    }

    #[test]
    fn rejects_incomplete_mismatched_changed_or_occupied_cache() {
        let source = Fixture::new();
        let (identity, root) = seed(&source);
        let cache = SeedCache::capture(&source.home, identity.clone()).unwrap();
        let target = Fixture::new();
        let mut wrong = identity.clone();
        wrong.target = "another-target".into();
        assert!(cache.copy_into(&target.home, &wrong).is_err());
        fs::write(target.home.join("existing"), "preserve").unwrap();
        assert!(cache.copy_into(&target.home, &identity).is_err());
        assert_eq!(
            fs::read_to_string(target.home.join("existing")).unwrap(),
            "preserve"
        );
        fs::write(root.join("extra"), "unexpected").unwrap();
        assert!(cache.unchanged().is_err());
        fs::remove_file(root.join(".agentcfg")).unwrap();
        assert!(SeedCache::capture(&source.home, identity).is_err());
    }

    #[test]
    fn rejects_linked_cache_without_touching_target() {
        let source = Fixture::new();
        let target = Fixture::new();
        let (identity, root) = seed(&source);
        fs::write(target.root.join("preserve"), "sentinel").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target.root, root.join("linked")).unwrap();
        #[cfg(windows)]
        {
            let output = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("linked").to_string_lossy().replace('/', "\\"))
                .arg(target.root.to_string_lossy().replace('/', "\\"))
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        assert!(SeedCache::capture(&source.home, identity).is_err());
        assert_eq!(
            fs::read_to_string(target.root.join("preserve")).unwrap(),
            "sentinel"
        );
        // Remove the owned link before disposable recursive cleanup.
        #[cfg(unix)]
        fs::remove_file(root.join("linked")).unwrap();
        #[cfg(windows)]
        fs::remove_dir(root.join("linked")).unwrap();
    }
}
