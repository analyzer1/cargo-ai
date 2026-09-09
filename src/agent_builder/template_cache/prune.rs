use super::key::TemplateCacheKey;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

pub(super) fn prune_stale_template_cache(
    root: &Path,
    active_key: &TemplateCacheKey,
) -> Result<usize, String> {
    if !root.exists() {
        return Ok(0);
    }

    let mut removed_parent_count = 0;
    for hash_entry in fs::read_dir(root).map_err(|error| {
        format!(
            "Failed to read template cache root '{}': {error}",
            root.display()
        )
    })? {
        let hash_entry = hash_entry.map_err(|error| {
            format!(
                "Failed to read template cache entry under '{}': {error}",
                root.display()
            )
        })?;
        let hash_path = hash_entry.path();
        if !hash_path.is_dir() {
            continue;
        }

        if hash_entry.file_name() != OsStr::new(&active_key.binary_sha256) {
            fs::remove_dir_all(&hash_path).map_err(|error| {
                format!(
                    "Failed to remove stale template cache parent '{}': {error}",
                    hash_path.display()
                )
            })?;
            removed_parent_count += 1;
            continue;
        }

        removed_parent_count += prune_stale_rustc_parents(&hash_path, active_key)?;
    }

    Ok(removed_parent_count)
}

fn prune_stale_rustc_parents(
    binary_root: &Path,
    active_key: &TemplateCacheKey,
) -> Result<usize, String> {
    let mut removed_parent_count = 0;
    for rustc_entry in fs::read_dir(binary_root).map_err(|error| {
        format!(
            "Failed to read Cargo AI template cache parent '{}': {error}",
            binary_root.display()
        )
    })? {
        let rustc_entry = rustc_entry.map_err(|error| {
            format!(
                "Failed to read rustc template cache entry under '{}': {error}",
                binary_root.display()
            )
        })?;
        let rustc_path = rustc_entry.path();
        if !rustc_path.is_dir() {
            continue;
        }

        if rustc_entry.file_name() != OsStr::new(&active_key.rustc_version) {
            fs::remove_dir_all(&rustc_path).map_err(|error| {
                format!(
                    "Failed to remove stale rustc template cache parent '{}': {error}",
                    rustc_path.display()
                )
            })?;
            removed_parent_count += 1;
        }
    }

    Ok(removed_parent_count)
}

#[cfg(test)]
mod tests {
    use super::prune_stale_template_cache;
    use crate::agent_builder::build_target::CargoCompileProfile;
    use crate::agent_builder::template_cache::key::TemplateCacheKey;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_test_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let sequence = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cargo-ai-template-cache-test-{nanos}-{sequence}"));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        path
    }

    #[test]
    fn prune_stale_template_cache_keeps_active_parent_and_sibling_targets() {
        let root = temp_test_dir();
        let active_key = TemplateCacheKey {
            binary_sha256: "sha-current".to_string(),
            rustc_version: "rustc-1.91.1".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            compile_profile: CargoCompileProfile::Release,
        };

        let stale_hash_target = root
            .join("sha-old")
            .join("rustc-1.90.0")
            .join("aarch64-apple-darwin");
        let stale_rustc_target = root
            .join("sha-current")
            .join("rustc-1.90.0")
            .join("aarch64-apple-darwin");
        let active_target = root
            .join("sha-current")
            .join("rustc-1.91.1")
            .join("aarch64-apple-darwin");
        let sibling_target = root
            .join("sha-current")
            .join("rustc-1.91.1")
            .join("x86_64-pc-windows-msvc");

        for path in [
            &stale_hash_target,
            &stale_rustc_target,
            &active_target,
            &sibling_target,
        ] {
            fs::create_dir_all(path).expect("template cache fixture should be creatable");
        }

        let active_profile = active_target.join("release");
        let sibling_profile = active_target.join("dev");
        fs::create_dir_all(&active_profile).unwrap();
        fs::create_dir_all(&sibling_profile).unwrap();

        let removed = prune_stale_template_cache(&root, &active_key)
            .expect("pruning stale template cache should succeed");

        assert_eq!(removed, 2);
        assert!(!root.join("sha-old").exists());
        assert!(!root.join("sha-current").join("rustc-1.90.0").exists());
        assert!(active_target.exists());
        assert!(sibling_target.exists());
        assert!(active_profile.exists());
        assert!(sibling_profile.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_stale_template_cache_is_noop_when_root_missing() {
        let missing_root = std::env::temp_dir().join("cargo-ai-template-cache-missing-root");
        let active_key = TemplateCacheKey {
            binary_sha256: "sha-current".to_string(),
            rustc_version: "rustc-1.91.1".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            compile_profile: CargoCompileProfile::Release,
        };

        let removed = prune_stale_template_cache(&missing_root, &active_key)
            .expect("missing cache root should be treated as empty");
        assert_eq!(removed, 0);
    }
}
