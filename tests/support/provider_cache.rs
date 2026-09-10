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
                    Some(digest(path)?)
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

    #[test]
    fn streaming_inventory_checks_known_hashes_unicode_and_byte_changes() {
        let fixture = Fixture::new();
        let empty = fixture.home.join("empty");
        let abc = fixture.home.join("café-東京");
        fs::write(&empty, b"").unwrap();
        fs::write(&abc, b"abc").unwrap();
        assert_eq!(
            digest(&empty).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(&abc).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        for (length, expected) in [
            (
                65535,
                "09ab7495d3e61a76f0deb12cb0306f0696cbb17ffc12131368c7a939f12f56d3",
            ),
            (
                65536,
                "1f8745f0d2d1387ec1af2211a3cf417b2e9e885e853472649c1d979d0e9370e3",
            ),
            (
                65537,
                "1abe08ebecf1c18cab71f6fe28aaddf20268f85bad78bb9a72f88ca47c874662",
            ),
        ] {
            let path = fixture.home.join(length.to_string());
            let bytes = vec![b'x'; length];
            fs::write(&path, &bytes).unwrap();
            assert_eq!(digest(&path).unwrap(), expected);
        }
        let before = inventory(&fixture.home).unwrap();
        assert_eq!(before.len(), 6);
        let modified = fs::metadata(&abc).unwrap().modified().unwrap();
        fs::write(&abc, b"abd").unwrap();
        fs::File::options()
            .write(true)
            .open(&abc)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_ne!(
            inventory(&fixture.home).unwrap(),
            before,
            "same length and timestamp must not conceal changed bytes"
        );
    }

    #[test]
    #[ignore = "explicit test-owned seed path required for bounded performance diagnosis"]
    fn measures_retained_seed_hash_copy_and_recipient_compilation() {
        let home = PathBuf::from(
            std::env::var_os("CARGO_AI_TEST_SEED_HOME").expect("explicit disposable seed required"),
        );
        let only_dir = |path: &Path| {
            let paths = fs::read_dir(path)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect::<Vec<_>>();
            assert_eq!(paths.len(), 1);
            plain_metadata(&paths[0]).unwrap();
            paths[0].clone()
        };
        let binary = only_dir(&home.join("templates"));
        let rustc = only_dir(&binary);
        let target = only_dir(&rustc);
        let identity = CacheIdentity {
            binary: binary.file_name().unwrap().to_str().unwrap().into(),
            rustc: rustc.file_name().unwrap().to_str().unwrap().into(),
            target: target.file_name().unwrap().to_str().unwrap().into(),
        };
        let started = std::time::Instant::now();
        let cache = SeedCache::capture(&home, identity.clone()).unwrap();
        eprintln!(
            "retained seed: {} files; hash {:.3}s",
            cache
                .entries
                .values()
                .filter(|e| e.digest.is_some())
                .count(),
            started.elapsed().as_secs_f64()
        );
        let recipient = Fixture::new();
        let started = std::time::Instant::now();
        cache.copy_into(&recipient.home, &identity).unwrap();
        eprintln!(
            "retained seed copy + source/destination validation: {:.3}s",
            started.elapsed().as_secs_f64()
        );
        let started = std::time::Instant::now();
        let workspace = recipient
            .home
            .join("templates")
            .join(identity.relative_path());
        let output = Command::new("cargo")
            .args(["build", "--offline", "--release"])
            .current_dir(&workspace)
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        eprintln!(
            "recipient compilation: {:.3}s\n{}",
            started.elapsed().as_secs_f64(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("Compiling serde "),
            "dependency reuse must remain effective"
        );
        let started = std::time::Instant::now();
        cache.unchanged().unwrap();
        eprintln!(
            "retained seed final immutable verification: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }

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
