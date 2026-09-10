//! Warmed template cache helpers for hatch builds.

mod key;
mod prepare;
mod prune;

use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};
use key::{resolve_template_cache_key, TemplateCacheKey};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WarmedTemplate {
    key: TemplateCacheKey,
    pub path: PathBuf,
    pub created: bool,
    pub pruned_parent_count: usize,
}

pub(crate) fn ensure_warmed_template_with_prepare_hook<F>(
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
    on_prepare_start: F,
) -> Result<WarmedTemplate, String>
where
    F: FnOnce(),
{
    ensure_warmed_template_with_prepare_hook_and_deps(
        build_target,
        profile,
        on_prepare_start,
        prepare::template_workspace_ready,
        prepare::prepare_warmed_template_workspace,
    )
}

fn ensure_warmed_template_with_prepare_hook_and_deps<F, G, H>(
    build_target: &BuildTarget,
    profile: CargoCompileProfile,
    on_prepare_start: F,
    template_workspace_ready: G,
    prepare_warmed_template_workspace: H,
) -> Result<WarmedTemplate, String>
where
    F: FnOnce(),
    G: FnOnce(&Path, &BuildTarget, CargoCompileProfile) -> bool,
    H: FnOnce(&Path, &BuildTarget, CargoCompileProfile) -> Result<(), String>,
{
    let key = resolve_template_cache_key(build_target, profile)?;
    let path = template_workspace_path(&key);

    if template_workspace_ready(&path, build_target, profile) {
        return Ok(WarmedTemplate {
            pruned_parent_count: prune_stale_parents(&key),
            key,
            path,
            created: false,
        });
    }

    on_prepare_start();
    prepare_warmed_template_workspace(&path, build_target, profile)?;

    Ok(WarmedTemplate {
        pruned_parent_count: prune_stale_parents(&key),
        key,
        path,
        created: true,
    })
}

fn template_workspace_path(key: &TemplateCacheKey) -> PathBuf {
    crate::agent_builder::templates_workspace_root()
        .join(&key.binary_sha256)
        .join(&key.rustc_version)
        .join(&key.target_triple)
        .join(key.compile_profile.name())
}

fn prune_stale_parents(key: &TemplateCacheKey) -> usize {
    prune::prune_stale_template_cache(&crate::agent_builder::templates_workspace_root(), key)
        .unwrap_or_else(|error| {
            eprintln!("⚠️ Failed to prune stale template cache parents: {error}");
            0
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_warmed_template_with_prepare_hook_and_deps, template_workspace_path,
        TemplateCacheKey,
    };
    use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn builds_nested_template_cache_path() {
        let key = TemplateCacheKey {
            binary_sha256: "abc123".to_string(),
            rustc_version: "rustc-1.90.0".to_string(),
            target_triple: "aarch64-apple-darwin".to_string(),
            compile_profile: CargoCompileProfile::Release,
        };

        let path = template_workspace_path(&key);
        let suffix = PathBuf::from("abc123")
            .join("rustc-1.90.0")
            .join("aarch64-apple-darwin")
            .join("release");

        assert!(path.ends_with(suffix));
        let dev_key = TemplateCacheKey {
            compile_profile: CargoCompileProfile::Dev,
            ..key
        };
        assert_ne!(path, template_workspace_path(&dev_key));
        assert!(template_workspace_path(&dev_key).ends_with("dev"));
    }

    #[test]
    fn prepare_hook_runs_only_when_template_is_created() {
        let build_target =
            BuildTarget::from_cli(Some("aarch64-apple-darwin")).expect("target should resolve");
        let hook_called = Arc::new(AtomicBool::new(false));
        let prepare_called = Arc::new(AtomicBool::new(false));

        let created = ensure_warmed_template_with_prepare_hook_and_deps(
            &build_target,
            CargoCompileProfile::Release,
            {
                let hook_called = Arc::clone(&hook_called);
                move || {
                    hook_called.store(true, Ordering::SeqCst);
                }
            },
            |_, _, profile| {
                assert_eq!(profile, CargoCompileProfile::Release);
                false
            },
            {
                let prepare_called = Arc::clone(&prepare_called);
                move |_, _, profile| {
                    assert_eq!(profile, CargoCompileProfile::Release);
                    prepare_called.store(true, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .expect("created template should succeed");

        assert!(created.created);
        assert!(hook_called.load(Ordering::SeqCst));
        assert!(prepare_called.load(Ordering::SeqCst));

        hook_called.store(false, Ordering::SeqCst);
        prepare_called.store(false, Ordering::SeqCst);

        let reused = ensure_warmed_template_with_prepare_hook_and_deps(
            &build_target,
            CargoCompileProfile::Release,
            {
                let hook_called = Arc::clone(&hook_called);
                move || {
                    hook_called.store(true, Ordering::SeqCst);
                }
            },
            |_, _, profile| {
                assert_eq!(profile, CargoCompileProfile::Release);
                true
            },
            {
                let prepare_called = Arc::clone(&prepare_called);
                move |_, _, profile| {
                    assert_eq!(profile, CargoCompileProfile::Release);
                    prepare_called.store(true, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .expect("reused template should succeed");

        assert!(!reused.created);
        assert!(!hook_called.load(Ordering::SeqCst));
        assert!(!prepare_called.load(Ordering::SeqCst));
    }
}
