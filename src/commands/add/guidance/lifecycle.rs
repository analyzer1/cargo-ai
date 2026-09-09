//! Ownership and read-only planning for installed guidance.
use super::{
    transaction, EntrypointStatus, GuidanceBundleReport, GuidanceEntrypointReport, GuidanceStyle,
    GUIDANCE_ARTIFACTS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub(super) const BUNDLE: &str = ".cargo-ai/guidance";
pub(super) const MANIFEST: &str = ".cargo-ai/guidance/manifest.json";
pub(super) const TRANSACTION: &str = ".cargo-ai/guidance-transaction";
pub(super) const LOCK: &str = ".cargo-ai/guidance.lock";
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TREE_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILES: usize = 512;
pub(super) type Tree = BTreeMap<String, Vec<u8>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    format_version: u32,
    producer_version: String,
    bundle_revision: String,
    bundle_digest: String,
    styles: Vec<String>,
    #[serde(deserialize_with = "unique_paths")]
    managed_paths: BTreeMap<String, String>,
}

fn unique_paths<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Paths;
    impl<'de> serde::de::Visitor<'de> for Paths {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of unique managed paths and hashes")
        }
        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut paths = BTreeMap::new();
            while let Some((path, hash)) = map.next_entry::<String, String>()? {
                if paths.insert(path.clone(), hash).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate managed path '{path}'"
                    )));
                }
            }
            Ok(paths)
        }
    }
    deserializer.deserialize_map(Paths)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum State {
    Missing,
    Current,
    UpdateAvailable,
    Modified,
    Incomplete,
    Malformed,
    Legacy,
}
impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Current => "current",
            Self::UpdateAvailable => "update available",
            Self::Modified => "locally modified",
            Self::Incomplete => "incomplete",
            Self::Malformed => "malformed",
            Self::Legacy => "legacy/unmanaged",
        }
    }
}

struct Inspection {
    state: State,
    manifest: Option<Manifest>,
    tree: Option<Tree>,
    details: Vec<String>,
}

pub(super) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn checked(root: &Path, relative: &str) -> Result<PathBuf, String> {
    crate::commands::runtime_data::confined_path(root, Path::new(relative), "Guidance path")
}

fn project_root(root: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|e| format!("Cannot inspect guidance project '{}': {e}", root.display()))?;
    if crate::commands::runtime_data::link_like(&metadata) || !metadata.is_dir() {
        return Err(
            "Guidance project must be a real directory, not a symbolic link or reparse point."
                .into(),
        );
    }
    fs::canonicalize(root)
        .map_err(|e| format!("Cannot resolve guidance project '{}': {e}", root.display()))
}

pub(super) fn read_file(root: &Path, relative: &str) -> Result<Option<Vec<u8>>, String> {
    let path = checked(root, relative)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot inspect guidance file '{relative}': {e}")),
    };
    if !metadata.is_file() || crate::commands::runtime_data::link_like(&metadata) {
        return Err(format!(
            "Guidance file '{relative}' must be a regular file, not a link or reparse point."
        ));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "Guidance file '{relative}' exceeds the 16 MiB safety limit."
        ));
    }
    fs::read(&path)
        .map(Some)
        .map_err(|e| format!("Cannot read guidance file '{relative}': {e}"))
}

pub(super) fn read_tree(root: &Path, relative: &str) -> Result<Option<Tree>, String> {
    Ok(read_tree_inventory(root, relative)?.map(|(tree, _)| tree))
}

fn read_tree_inventory(
    root: &Path,
    relative: &str,
) -> Result<Option<(Tree, BTreeSet<String>)>, String> {
    let path = checked(root, relative)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "Cannot inspect guidance directory '{relative}': {e}"
            ))
        }
    };
    if !metadata.is_dir() || crate::commands::runtime_data::link_like(&metadata) {
        return Err(format!(
            "Guidance directory '{relative}' must be a real directory."
        ));
    }
    let mut tree = Tree::new();
    let mut pending = vec![(path, String::new(), 0usize)];
    let mut bytes = 0usize;
    let mut directories = 0;
    let mut directory_paths = BTreeSet::new();
    while let Some((directory, prefix, depth)) = pending.pop() {
        directories += 1;
        if depth > 16 || directories > MAX_FILES {
            return Err("Guidance directory nesting/count exceeds safety limits.".into());
        }
        if !prefix.is_empty() {
            directory_paths.insert(prefix.clone());
        }
        for entry in
            fs::read_dir(directory).map_err(|e| format!("Cannot enumerate guidance: {e}"))?
        {
            let entry = entry.map_err(|e| format!("Cannot inspect guidance entry: {e}"))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "Guidance paths must be Unicode".to_string())?;
            let name = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let full = format!("{relative}/{name}");
            let path = checked(root, &full)?;
            let metadata =
                fs::symlink_metadata(&path).map_err(|e| format!("Cannot inspect '{full}': {e}"))?;
            if metadata.is_dir() {
                pending.push((path, name, depth + 1));
            } else {
                let content = read_file(root, &full)?
                    .ok_or_else(|| format!("Guidance changed while inspecting '{full}'; retry."))?;
                bytes = bytes.saturating_add(content.len());
                if tree.len() >= MAX_FILES || bytes > MAX_TREE_BYTES {
                    return Err("Guidance bundle exceeds file-count/byte safety limits.".into());
                }
                tree.insert(name, content);
            }
        }
    }
    Ok(Some((tree, directory_paths)))
}

pub(super) fn extra_directories(
    root: &Path,
    relative: &str,
    expected: &Tree,
) -> Result<Vec<String>, String> {
    let Some((_, directories)) = read_tree_inventory(root, relative)? else {
        return Ok(Vec::new());
    };
    Ok(directories
        .into_iter()
        .filter(|directory| {
            let prefix = format!("{directory}/");
            !expected.keys().any(|path| path.starts_with(&prefix))
        })
        .collect())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn validate_tree_paths(tree: &Tree) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    let mut total = 0usize;
    if tree.len() > MAX_FILES {
        return Err("Guidance inventory exceeds its file limit.".into());
    }
    for (path, bytes) in tree {
        let normalized = crate::commands::runtime_data::portable_relative_path(
            Path::new(path),
            "Guidance inventory",
        )?;
        if path.len() > 1024
            || normalized.to_string_lossy().replace('\\', "/") != *path
            || !seen.insert(path.to_ascii_lowercase())
        {
            return Err(format!("Malformed or duplicate guidance path '{path}'."));
        }
        if path.split('/').count() > 16 || bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("Guidance inventory exceeds path/file limits.".into());
        }
        total = total.saturating_add(bytes.len());
        if total > MAX_TREE_BYTES {
            return Err("Guidance inventory exceeds its byte limit.".into());
        }
    }
    for path in tree.keys() {
        if path
            .split('/')
            .scan(String::new(), |prefix, part| {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                Some(prefix.clone())
            })
            .any(|prefix| {
                prefix != *path && tree.keys().any(|other| other.eq_ignore_ascii_case(&prefix))
            })
        {
            return Err(format!(
                "Guidance file/directory paths overlap at '{path}'."
            ));
        }
    }
    Ok(())
}

fn decode_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    if bytes.len() > 1024 * 1024 {
        return Err("Guidance manifest exceeds 1 MiB.".into());
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|e| format!("Malformed guidance manifest: {e}"))?;
    if manifest.format_version != 1
        || manifest.producer_version.trim().is_empty()
        || !valid_hash(&manifest.bundle_digest)
        || manifest.bundle_revision != manifest.bundle_digest
    {
        return Err("Malformed or unsupported guidance manifest version/digest.".into());
    }
    let mut styles = BTreeSet::new();
    if manifest.styles.is_empty() || manifest.styles.len() > 2 {
        return Err("Malformed guidance manifest styles.".into());
    }
    for style in &manifest.styles {
        GuidanceStyle::from_cli(style)?;
        if !styles.insert(style) {
            return Err("Duplicate guidance style in manifest.".into());
        }
    }
    let mut paths = Tree::new();
    if manifest.managed_paths.is_empty() || manifest.managed_paths.len() > MAX_FILES {
        return Err("Malformed guidance managed inventory.".into());
    }
    for (path, digest) in &manifest.managed_paths {
        let is_root = path == "AGENTS.md" || path == "CLAUDE.md";
        if !valid_hash(digest)
            || path == MANIFEST
            || (!is_root && !path.starts_with(&format!("{BUNDLE}/")))
        {
            return Err(format!("Malformed guidance ownership path/hash '{path}'."));
        }
        if is_root
            && !manifest
                .styles
                .iter()
                .any(|s| GuidanceStyle::from_cli(s).unwrap().root_filename() == path)
        {
            return Err(format!(
                "Guidance entrypoint '{path}' has no declared style."
            ));
        }
        paths.insert(path.clone(), Vec::new());
    }
    validate_tree_paths(&paths)?;
    Ok(manifest)
}

pub(super) fn validate_snapshot(
    tree: &Option<Tree>,
    files: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<(), String> {
    let Some(tree) = tree else {
        return Ok(());
    };
    let manifest = decode_manifest(
        tree.get("manifest.json")
            .ok_or("Recovery bundle lacks its ownership manifest")?,
    )?;
    let payload = tree
        .iter()
        .filter(|(p, _)| p.as_str() != "manifest.json")
        .map(|(p, b)| (p.clone(), b.clone()))
        .collect::<Tree>();
    if bundle_digest(&payload) != manifest.bundle_digest {
        return Err("Recovery bundle does not match its manifest digest.".into());
    }
    for (path, digest) in &manifest.managed_paths {
        let bytes = if let Some(path) = path.strip_prefix(&format!("{BUNDLE}/")) {
            tree.get(path)
        } else {
            files.get(path).and_then(Option::as_ref)
        };
        if !bytes.is_some_and(|bytes| hash(bytes) == *digest) {
            return Err(format!("Recovery ownership hash does not match '{path}'."));
        }
    }
    if payload.keys().any(|p| {
        !manifest
            .managed_paths
            .contains_key(&format!("{BUNDLE}/{p}"))
    }) {
        return Err("Recovery bundle includes unmanaged content.".into());
    }
    Ok(())
}

fn bundle_digest(tree: &Tree) -> String {
    let mut digest = Sha256::new();
    for (path, bytes) in tree {
        digest.update((path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

fn installed_tree() -> Tree {
    GUIDANCE_ARTIFACTS
        .iter()
        .map(|artifact| {
            (
                artifact
                    .relative_path
                    .strip_prefix(&format!("{BUNDLE}/"))
                    .unwrap()
                    .to_string(),
                artifact.contents.as_bytes().to_vec(),
            )
        })
        .collect()
}

fn inspect(root: &Path) -> Result<Inspection, String> {
    if fs::symlink_metadata(checked(root, TRANSACTION)?).is_ok() {
        return Ok(Inspection { state: State::Incomplete, manifest: None, tree: None, details: vec![format!("Interrupted guidance transaction at '{TRANSACTION}'; explicit `cargo ai guidance update` may recover verified state.")] });
    }
    let tree = read_tree(root, BUNDLE)?;
    let Some(bytes) = tree.as_ref().and_then(|t| t.get("manifest.json")) else {
        let state = if tree.is_some() {
            State::Legacy
        } else {
            State::Missing
        };
        let details = tree.as_ref().map(|t| vec![format!("{} has no ownership manifest; preserve or relocate this legacy/unmanaged bundle before a fresh add. Existing paths: {}", BUNDLE, t.keys().map(|s| format!("{BUNDLE}/{s}")).collect::<Vec<_>>().join(", "))]).unwrap_or_default();
        return Ok(Inspection {
            state,
            manifest: None,
            tree,
            details,
        });
    };
    let manifest = match decode_manifest(bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Ok(Inspection {
                state: State::Malformed,
                manifest: None,
                tree,
                details: vec![error],
            })
        }
    };
    let mut state = State::Current;
    let mut details = Vec::new();
    for (path, expected) in &manifest.managed_paths {
        match read_file(root, path)? {
            None => {
                state = State::Incomplete;
                details.push(format!(
                    "Missing managed file '{path}'; restore the original file before update."
                ));
            }
            Some(bytes) if hash(&bytes) != *expected => {
                if state != State::Incomplete {
                    state = State::Modified;
                }
                details.push(format!("Locally modified '{path}'; preserve your edits and restore its generated bytes before update."));
                if let Some(style) = [GuidanceStyle::Codex, GuidanceStyle::Claude]
                    .into_iter()
                    .find(|style| style.root_filename() == path)
                {
                    details.push(format!(
                        "Keep your {path} instructions. Loader snippet:\n{}",
                        style.merge_snippet(super::BUNDLE_ENTRY_PATH)
                    ));
                }
            }
            Some(_) => {}
        }
    }
    for name in tree
        .as_ref()
        .unwrap()
        .keys()
        .filter(|name| name.as_str() != "manifest.json")
    {
        if !manifest
            .managed_paths
            .contains_key(&format!("{BUNDLE}/{name}"))
        {
            state = State::Legacy;
            details.push(format!(
                "Unmanaged guidance file '{BUNDLE}/{name}'; preserve or relocate it before update."
            ));
        }
    }
    for directory in extra_directories(root, BUNDLE, tree.as_ref().unwrap())? {
        state = State::Legacy;
        details.push(format!(
            "Unmanaged guidance directory '{BUNDLE}/{directory}'; preserve or relocate it before update."
        ));
    }
    let owned_bundle = tree
        .as_ref()
        .unwrap()
        .iter()
        .filter(|(name, _)| name.as_str() != "manifest.json")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<Tree>();
    if state == State::Current && bundle_digest(&owned_bundle) != manifest.bundle_digest {
        state = State::Malformed;
        details.push("The manifest bundle digest does not match its managed inventory.".into());
    }
    if state == State::Current && manifest.bundle_digest != bundle_digest(&installed_tree()) {
        state = State::UpdateAvailable;
    }
    for style in &manifest.styles {
        let style = GuidanceStyle::from_cli(style)?;
        read_file(root, style.root_filename())?;
        if !manifest.managed_paths.contains_key(style.root_filename()) {
            details.push(format!(
                "User-owned {} is preserved. Loader snippet:\n{}",
                style.root_filename(),
                style.merge_snippet(super::BUNDLE_ENTRY_PATH)
            ));
        }
    }
    Ok(Inspection {
        state,
        manifest: Some(manifest),
        tree,
        details,
    })
}

fn generated_paths(inspection: &Inspection) -> Vec<String> {
    let mut paths = inspection
        .manifest
        .as_ref()
        .map(|m| m.managed_paths.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_else(|| {
            GUIDANCE_ARTIFACTS
                .iter()
                .map(|a| a.relative_path.to_string())
                .collect()
        });
    paths.push(MANIFEST.into());
    paths.sort();
    paths.dedup();
    paths
}

pub(super) fn print_status(root: &Path) -> Result<(), String> {
    let root = project_root(root)?;
    let _guard = transaction::read_lock(&root)?;
    let inspected = inspect(&root)?;
    println!("Guidance status: {}", inspected.state.label());
    if let Some(manifest) = &inspected.manifest {
        println!(
            "Produced by Cargo AI {}\nInstalled Cargo AI {}\nBundle: {}",
            manifest.producer_version,
            env!("CARGO_PKG_VERSION"),
            manifest.bundle_digest
        );
    }
    for detail in &inspected.details {
        println!("{detail}");
    }
    let (ignore, notices) =
        crate::commands::scaffold::guidance_vcs_plan(&root, &generated_paths(&inspected))?;
    for notice in notices {
        println!("{notice}");
    }
    if ignore.is_some() {
        println!("Cargo AI ignore entries need repair; only explicit add/update may repair them.");
    }
    if inspected.state == State::Missing {
        println!("Use `cargo ai add guidance --style codex` or `--style claude`.");
    }
    Ok(())
}

fn allowed(inspection: &Inspection, adding: bool) -> Result<(), String> {
    if inspection.state == State::Current
        || (!adding && inspection.state == State::UpdateAvailable)
        || (adding && inspection.state == State::Missing)
    {
        return Ok(());
    }
    Err(format!(
        "Guidance {} blocks {}. {} {}",
        inspection.state.label(),
        if adding { "add" } else { "update" },
        inspection.details.join(" "),
        if inspection.state == State::UpdateAvailable {
            "Run `cargo ai guidance update` before adding another style."
        } else if inspection.state == State::Missing {
            "Run `cargo ai add guidance --style codex` or `--style claude`."
        } else {
            "No generated files were replaced."
        }
    ))
}

pub(super) fn add(root: &Path, styles: &[GuidanceStyle]) -> Result<GuidanceBundleReport, String> {
    if styles.is_empty() {
        return Err("Missing guidance style. Use `--style codex` or `--style claude`.".into());
    }
    mutate(root, Some(styles))
}

pub(super) fn update(root: &Path) -> Result<(), String> {
    let root = project_root(root)?;
    if fs::symlink_metadata(checked(&root, TRANSACTION)?).is_ok() {
        let _guard = transaction::write_lock(&root)?;
        transaction::recover(&root)?;
        println!("Recovered the verified interrupted guidance transaction.");
        // A failed first add has no prior bundle; recovery itself is the requested repair.
        if inspect(&root)?.state == State::Missing {
            return Ok(());
        }
    }
    let report = mutate(&root, None)?;
    for entry in &report.entrypoints {
        if entry.status == EntrypointStatus::Preserved {
            println!(
                "Preserved {}. Loader snippet:\n{}",
                entry.style.root_filename(),
                entry.style.merge_snippet(super::BUNDLE_ENTRY_PATH)
            );
        }
    }
    println!(
        "Guidance is current with installed Cargo AI {} ({} files written).",
        env!("CARGO_PKG_VERSION"),
        report.written_paths.len()
    );
    Ok(())
}

fn mutate(
    root: &Path,
    requested: Option<&[GuidanceStyle]>,
) -> Result<GuidanceBundleReport, String> {
    let root = project_root(root)?;
    let first = inspect(&root)?;
    allowed(&first, requested.is_some())?;
    // Preflight every selected instruction and ignore participant before lock-file creation.
    for style in [GuidanceStyle::Codex, GuidanceStyle::Claude] {
        read_file(&root, style.root_filename())?;
    }
    read_file(&root, ".gitignore")?;
    crate::commands::scaffold::guidance_vcs_plan(&root, &generated_paths(&first))?;
    let _guard = transaction::write_lock(&root)?;
    let inspection = inspect(&root)?;
    allowed(&inspection, requested.is_some())?;
    let mut styles = inspection
        .manifest
        .as_ref()
        .map(|m| m.styles.clone())
        .unwrap_or_default();
    if let Some(requested) = requested {
        for style in requested {
            let name = match style {
                GuidanceStyle::Codex => "codex",
                GuidanceStyle::Claude => "claude",
            };
            if !styles.iter().any(|s| s == name) {
                styles.push(name.into());
            }
        }
    }
    let mut next = installed_tree();
    let mut manifest = Manifest {
        format_version: 1,
        producer_version: env!("CARGO_PKG_VERSION").into(),
        bundle_revision: bundle_digest(&next),
        bundle_digest: bundle_digest(&next),
        styles,
        managed_paths: next
            .iter()
            .map(|(p, b)| (format!("{BUNDLE}/{p}"), hash(b)))
            .collect(),
    };
    let mut before_files = BTreeMap::new();
    let mut after_files = BTreeMap::new();
    let mut entrypoints = Vec::new();
    for name in &manifest.styles {
        let style = GuidanceStyle::from_cli(name)?;
        let path = style.root_filename();
        let before = read_file(&root, path)?;
        let owned = inspection
            .manifest
            .as_ref()
            .is_some_and(|m| m.managed_paths.contains_key(path));
        let status = if before.is_none() || owned {
            let content = style.root_artifact().contents.as_bytes().to_vec();
            manifest.managed_paths.insert(path.into(), hash(&content));
            before_files.insert(path.to_string(), before.clone());
            after_files.insert(path.to_string(), Some(content.clone()));
            if before.as_ref() == Some(&content) {
                EntrypointStatus::Reused
            } else {
                EntrypointStatus::Written
            }
        } else {
            EntrypointStatus::Preserved
        };
        if requested.is_none() || requested.unwrap().contains(&style) {
            entrypoints.push(GuidanceEntrypointReport {
                style,
                root_output_path: root.join(path),
                status,
            });
        }
    }
    // An unchanged inventory need not rewrite producer metadata merely because the binary version changed.
    if let Some(old) = &inspection.manifest {
        if old.bundle_digest == manifest.bundle_digest
            && old.styles == manifest.styles
            && old.managed_paths == manifest.managed_paths
        {
            manifest = old.clone();
        }
    }
    next.insert(
        "manifest.json".into(),
        serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
    );
    let paths = manifest
        .managed_paths
        .keys()
        .cloned()
        .chain([MANIFEST.to_string()])
        .collect::<Vec<_>>();
    let (ignore, notices) = crate::commands::scaffold::guidance_vcs_plan(&root, &paths)?;
    for notice in notices {
        println!("{notice}");
    }
    if let Some(ignore) = ignore {
        before_files.insert(".gitignore".into(), read_file(&root, ".gitignore")?);
        after_files.insert(".gitignore".into(), Some(ignore));
    }
    let mut written_paths = Vec::new();
    let mut reused_paths = Vec::new();
    for (name, bytes) in &next {
        if inspection.tree.as_ref().and_then(|t| t.get(name)) == Some(bytes) {
            reused_paths.push(root.join(BUNDLE).join(name));
        } else {
            written_paths.push(root.join(BUNDLE).join(name));
        }
    }
    for entry in &entrypoints {
        match entry.status {
            EntrypointStatus::Written => written_paths.push(entry.root_output_path.clone()),
            EntrypointStatus::Reused => reused_paths.push(entry.root_output_path.clone()),
            EntrypointStatus::Preserved => {}
        }
    }
    let plan = transaction::Plan {
        before_bundle: inspection.tree,
        after_bundle: Some(next),
        before_files,
        after_files,
    };
    if plan.before_bundle != plan.after_bundle || plan.before_files != plan.after_files {
        transaction::apply(&root, &plan)?;
    }
    Ok(GuidanceBundleReport {
        entrypoints,
        guidance_entry_path: root.join(super::BUNDLE_ENTRY_PATH),
        guidance_root: root.join(BUNDLE),
        written_paths,
        reused_paths,
    })
}

pub(super) fn is_managed_entrypoint(path: &Path) -> Result<bool, String> {
    let Some(name) = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n == "AGENTS.md" || *n == "CLAUDE.md")
    else {
        return Ok(false);
    };
    let root = project_root(path.parent().ok_or("Entrypoint has no parent")?)?;
    let Some(bytes) = read_file(&root, MANIFEST)? else {
        return Ok(false);
    };
    let manifest = decode_manifest(&bytes)?;
    let Some(expected) = manifest.managed_paths.get(name) else {
        return Ok(false);
    };
    Ok(read_file(&root, name)?.is_some_and(|bytes| hash(&bytes) == *expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "cargo-ai-guidance-lifecycle-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fn install(&self) {
            add(&self.0, &[GuidanceStyle::Codex, GuidanceStyle::Claude]).unwrap();
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            transaction::failures(&[]);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, result);
                } else {
                    result.insert(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut result = BTreeMap::new();
        visit(root, root, &mut result);
        result
    }

    fn write_prior_release(project: &Project) {
        project.install();
        let mut manifest = inspect(&project.0).unwrap().manifest.unwrap();
        let mut payload = installed_tree();
        payload.remove("action-rules.md");
        payload.insert("retired.md".into(), b"previous release artifact".to_vec());
        payload.insert("cargo-ai.md".into(), b"previous release guidance".to_vec());
        manifest.producer_version = "0.0.1".into();
        manifest.bundle_digest = bundle_digest(&payload);
        manifest.bundle_revision = manifest.bundle_digest.clone();
        manifest.managed_paths = payload
            .iter()
            .map(|(path, bytes)| (format!("{BUNDLE}/{path}"), hash(bytes)))
            .collect();
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let bytes = format!("previous generated {name} loader").into_bytes();
            manifest.managed_paths.insert(name.into(), hash(&bytes));
            project.write(name, &bytes);
        }
        fs::remove_dir_all(project.0.join(BUNDLE)).unwrap();
        for (path, bytes) in payload {
            project.write(&format!("{BUNDLE}/{path}"), &bytes);
        }
        project.write(MANIFEST, &serde_json::to_vec(&manifest).unwrap());
        assert_eq!(inspect(&project.0).unwrap().state, State::UpdateAvailable);
    }

    fn three_participant_plan(project: &Project) -> transaction::Plan {
        write_prior_release(project);
        project.write(".gitignore", b"user ignore bytes\r\n");
        let current = Project::new();
        current.install();
        current.write(".gitignore", b"user ignore bytes\r\n# generated entries\n");
        let paths = ["AGENTS.md", "CLAUDE.md", ".gitignore"];
        transaction::Plan {
            before_bundle: read_tree(&project.0, BUNDLE).unwrap(),
            after_bundle: read_tree(&current.0, BUNDLE).unwrap(),
            before_files: paths
                .iter()
                .map(|path| (path.to_string(), read_file(&project.0, path).unwrap()))
                .collect(),
            after_files: paths
                .iter()
                .map(|path| (path.to_string(), read_file(&current.0, path).unwrap()))
                .collect(),
        }
    }

    #[test]
    fn status_never_creates_control_files_or_changes_existing_bytes() {
        let project = Project::new();
        project.write("AGENTS.md", b"user\0instructions\r\n");
        let before = files(&project.0);
        print_status(&project.0).unwrap();
        assert_eq!(files(&project.0), before);
        assert!(!project.0.join(".cargo-ai").exists());
        project.install();
        let before = files(&project.0);
        print_status(&project.0).unwrap();
        assert_eq!(files(&project.0), before);
    }

    #[test]
    fn update_reconciles_added_and_retired_owned_files_and_is_idempotent() {
        let project = Project::new();
        write_prior_release(&project);
        project.write("unrelated.txt", b"must remain");
        update(&project.0).unwrap();
        assert_eq!(inspect(&project.0).unwrap().state, State::Current);
        assert!(!project.0.join(BUNDLE).join("retired.md").exists());
        assert!(project.0.join(BUNDLE).join("action-rules.md").exists());
        assert_eq!(
            fs::read(project.0.join("unrelated.txt")).unwrap(),
            b"must remain"
        );
        let before = files(&project.0);
        update(&project.0).unwrap();
        assert_eq!(files(&project.0), before);
        assert!(!project.0.join(TRANSACTION).exists());
    }

    #[test]
    fn user_owned_instruction_bytes_are_never_adopted_even_when_identical() {
        let project = Project::new();
        let bytes = GuidanceStyle::Codex.root_artifact().contents.as_bytes();
        project.write("AGENTS.md", bytes);
        project.write("CLAUDE.md", b"my instructions\0\r\n");
        project.install();
        let manifest = inspect(&project.0).unwrap().manifest.unwrap();
        assert!(!manifest.managed_paths.contains_key("AGENTS.md"));
        assert!(!manifest.managed_paths.contains_key("CLAUDE.md"));
        assert!(!is_managed_entrypoint(&project.0.join("AGENTS.md")).unwrap());
        let before = files(&project.0);
        update(&project.0).unwrap();
        assert_eq!(files(&project.0), before);
        assert!(inspect(&project.0)
            .unwrap()
            .details
            .iter()
            .all(|s| s.contains("Loader snippet")));
    }

    #[test]
    fn modified_missing_malformed_and_unmanaged_states_block_without_writes() {
        for case in [
            "modified",
            "missing",
            "malformed",
            "legacy",
            "extra",
            "root",
        ] {
            let project = Project::new();
            project.install();
            let expected = match case {
                "modified" => {
                    project.write(&format!("{BUNDLE}/cargo-ai.md"), b"my edits");
                    State::Modified
                }
                "missing" => {
                    fs::remove_file(project.0.join(BUNDLE).join("cargo-ai.md")).unwrap();
                    State::Incomplete
                }
                "malformed" => {
                    project.write(MANIFEST, b"{}");
                    State::Malformed
                }
                "legacy" => {
                    fs::remove_file(project.0.join(MANIFEST)).unwrap();
                    State::Legacy
                }
                "extra" => {
                    project.write(&format!("{BUNDLE}/user-note.md"), b"my note");
                    State::Legacy
                }
                "root" => {
                    project.write("AGENTS.md", b"user changed the generated loader");
                    State::Modified
                }
                _ => unreachable!(),
            };
            let before = files(&project.0);
            let inspected = inspect(&project.0).unwrap();
            assert_eq!(inspected.state, expected, "{case}");
            assert!(add(&project.0, &[GuidanceStyle::Codex]).is_err(), "{case}");
            let error = update(&project.0).unwrap_err();
            if case == "root" {
                assert!(error.contains("Loader snippet"), "{error}");
            }
            print_status(&project.0).unwrap();
            assert_eq!(files(&project.0), before, "{case}");
        }
    }

    #[test]
    fn malformed_ownership_is_rejected_without_guessing() {
        let project = Project::new();
        project.install();
        let manifest = fs::read(project.0.join(MANIFEST)).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        let mut cases = Vec::new();
        for (key, replacement) in [
            ("format_version", serde_json::json!(2)),
            ("bundle_digest", serde_json::json!("invalid")),
            ("styles", serde_json::json!(["codex", "codex"])),
            ("styles", serde_json::json!(["unknown"])),
            ("unexpected", serde_json::json!(true)),
        ] {
            let mut malformed = value.clone();
            malformed[key] = replacement;
            cases.push(serde_json::to_vec(&malformed).unwrap());
        }
        for path in [
            "../outside",
            "C:/outside",
            ".cargo-ai/guidance/../escape",
            MANIFEST,
        ] {
            let mut malformed = value.clone();
            malformed["managed_paths"][path] = serde_json::json!(hash(b"x"));
            cases.push(serde_json::to_vec(&malformed).unwrap());
        }
        let hash = hash(b"x");
        let duplicate = String::from_utf8(manifest.clone()).unwrap().replace(
            "\"managed_paths\": {",
            &format!("\"managed_paths\": {{\"AGENTS.md\":\"{hash}\",\"AGENTS.md\":\"{hash}\","),
        );
        cases.push(duplicate.into_bytes());
        for bytes in cases {
            assert!(
                decode_manifest(&bytes).is_err(),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            project.write(MANIFEST, &bytes);
            let before = files(&project.0);
            assert_eq!(inspect(&project.0).unwrap().state, State::Malformed);
            assert!(update(&project.0).is_err());
            assert_eq!(files(&project.0), before);
        }
    }

    #[test]
    fn preparation_and_promotion_failures_restore_the_complete_prior_set() {
        for point in [
            "journal",
            "prepared",
            "file-backed-up",
            "file-promoted",
            "bundle-backed-up",
            "bundle-promoted",
            "before-commit",
        ] {
            let project = Project::new();
            write_prior_release(&project);
            let before = files(&project.0);
            transaction::failures(&[(point, false)]);
            assert!(update(&project.0).is_err(), "{point}");
            assert_eq!(files(&project.0), before, "{point}");
            assert!(!project.0.join(TRANSACTION).exists(), "{point}");
            assert_eq!(inspect(&project.0).unwrap().state, State::UpdateAvailable);
        }
    }

    #[test]
    fn ignore_repair_and_both_loaders_share_the_same_recovery_boundary() {
        for point in [
            "file-backed-up-0",
            "file-backed-up-1",
            "file-backed-up-2",
            "file-promoted-0",
            "file-promoted-1",
            "file-promoted-2",
            "bundle-backed-up",
            "bundle-promoted",
        ] {
            let project = Project::new();
            let plan = three_participant_plan(&project);
            let before = files(&project.0);
            transaction::failures(&[(point, false)]);
            assert!(transaction::apply(&project.0, &plan).is_err());
            assert_eq!(files(&project.0), before, "{point}");
            transaction::apply(&project.0, &plan).unwrap();
            assert_eq!(read_tree(&project.0, BUNDLE).unwrap(), plan.after_bundle);
            for (path, bytes) in &plan.after_files {
                assert_eq!(read_file(&project.0, path).unwrap(), *bytes);
            }
        }
    }

    #[test]
    fn every_sidecar_promotion_interruption_and_rollback_move_can_resume() {
        for point in [
            "file-backed-up-0",
            "file-backed-up-1",
            "file-backed-up-2",
            "file-promoted-0",
            "file-promoted-1",
            "file-promoted-2",
        ] {
            let project = Project::new();
            let plan = three_participant_plan(&project);
            let before = files(&project.0);
            transaction::failures(&[(point, true)]);
            assert!(transaction::apply(&project.0, &plan).is_err(), "{point}");
            transaction::recover(&project.0).unwrap();
            assert_eq!(files(&project.0), before, "{point}");
        }
        for point in [
            "bundle-displaced",
            "bundle-restored",
            "file-displaced-0",
            "file-displaced-1",
            "file-displaced-2",
            "file-restored-0",
            "file-restored-1",
            "file-restored-2",
        ] {
            let project = Project::new();
            let plan = three_participant_plan(&project);
            let before = files(&project.0);
            transaction::failures(&[("bundle-promoted", false), (point, true)]);
            assert!(transaction::apply(&project.0, &plan).is_err(), "{point}");
            assert!(project.0.join(TRANSACTION).exists(), "{point}");
            transaction::recover(&project.0).unwrap();
            assert_eq!(files(&project.0), before, "{point}");
        }
    }

    #[test]
    fn missing_original_backups_block_recovery_before_any_rollback_write() {
        for path in ["old-bundle", "old-files/0", "old-files/1", "old-files/2"] {
            let project = Project::new();
            let plan = three_participant_plan(&project);
            transaction::failures(&[("bundle-promoted", true)]);
            assert!(transaction::apply(&project.0, &plan).is_err());
            let missing = project.0.join(TRANSACTION).join(path);
            if missing.is_dir() {
                fs::remove_dir_all(missing).unwrap();
            } else {
                fs::remove_file(missing).unwrap();
            }
            let before = files(&project.0);
            let error = transaction::recover(&project.0).unwrap_err();
            assert!(error.contains("backup"), "{error}");
            assert_eq!(files(&project.0), before, "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rollback_restores_original_sidecar_permission_modes() {
        use std::os::unix::fs::PermissionsExt;
        let project = Project::new();
        let plan = three_participant_plan(&project);
        for (index, path) in [".gitignore", "AGENTS.md", "CLAUDE.md"]
            .into_iter()
            .enumerate()
        {
            fs::set_permissions(
                project.0.join(path),
                fs::Permissions::from_mode(0o600 | index as u32),
            )
            .unwrap();
        }
        transaction::failures(&[("bundle-promoted", false)]);
        assert!(transaction::apply(&project.0, &plan).is_err());
        for (index, path) in [".gitignore", "AGENTS.md", "CLAUDE.md"]
            .into_iter()
            .enumerate()
        {
            let mode = fs::metadata(project.0.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600 | index as u32);
        }
    }

    #[test]
    fn interrupted_promotion_is_read_only_until_explicit_verified_recovery() {
        for point in [
            "journal",
            "prepared",
            "file-backed-up",
            "file-promoted",
            "bundle-backed-up",
            "bundle-promoted",
            "before-commit",
        ] {
            let project = Project::new();
            write_prior_release(&project);
            let before = files(&project.0);
            transaction::failures(&[(point, true)]);
            assert!(update(&project.0).is_err(), "{point}");
            assert_eq!(inspect(&project.0).unwrap().state, State::Incomplete);
            let interrupted = files(&project.0);
            print_status(&project.0).unwrap();
            assert_eq!(files(&project.0), interrupted, "{point}");
            {
                let _guard = transaction::write_lock(&project.0).unwrap();
                transaction::recover(&project.0).unwrap();
            }
            assert_eq!(files(&project.0), before, "{point}");
            update(&project.0).unwrap();
            assert_eq!(inspect(&project.0).unwrap().state, State::Current);
        }
    }

    #[test]
    fn rollback_failure_is_recoverable_and_keeps_the_prior_bytes() {
        for rollback_point in ["rollback", "file-restored"] {
            let project = Project::new();
            write_prior_release(&project);
            let before = files(&project.0);
            transaction::failures(&[("bundle-promoted", false), (rollback_point, false)]);
            assert!(update(&project.0).is_err());
            assert!(project.0.join(TRANSACTION).exists());
            transaction::recover(&project.0).unwrap();
            assert_eq!(files(&project.0), before, "{rollback_point}");
        }
    }

    #[test]
    fn committed_interruption_keeps_the_complete_new_set_and_retries_cleanup() {
        for point in ["committed", "cleanup"] {
            let project = Project::new();
            write_prior_release(&project);
            transaction::failures(&[(point, true)]);
            assert!(update(&project.0).is_err());
            assert_eq!(inspect(&project.0).unwrap().state, State::Incomplete);
            let bundle = read_tree(&project.0, BUNDLE).unwrap();
            transaction::recover(&project.0).unwrap();
            assert_eq!(read_tree(&project.0, BUNDLE).unwrap(), bundle);
            assert_eq!(inspect(&project.0).unwrap().state, State::Current);
            assert!(!project.0.join(TRANSACTION).exists());
        }
    }

    #[test]
    fn committed_cleanup_failure_finishes_the_verified_new_set() {
        for point in ["committed", "cleanup"] {
            let project = Project::new();
            write_prior_release(&project);
            transaction::failures(&[(point, false)]);
            update(&project.0).unwrap();
            assert_eq!(inspect(&project.0).unwrap().state, State::Current);
            assert!(!project.0.join(TRANSACTION).exists());
        }
    }

    #[test]
    fn failed_first_add_recovers_to_absent_without_adopting_user_instructions() {
        let project = Project::new();
        project.write("AGENTS.md", b"user instructions\r\n");
        transaction::failures(&[("bundle-promoted", true)]);
        assert!(add(&project.0, &[GuidanceStyle::Codex, GuidanceStyle::Claude]).is_err());
        update(&project.0).unwrap();
        assert_eq!(
            read_file(&project.0, "AGENTS.md").unwrap().unwrap(),
            b"user instructions\r\n"
        );
        assert!(read_file(&project.0, "CLAUDE.md").unwrap().is_none());
        assert_eq!(inspect(&project.0).unwrap().state, State::Missing);
        assert!(!project.0.join(TRANSACTION).exists());
    }

    #[test]
    fn changed_recovery_or_live_bytes_are_preserved_without_partial_rollback() {
        for target in [
            "AGENTS.md",
            ".cargo-ai/guidance/cargo-ai.md",
            ".cargo-ai/guidance-transaction/new-bundle/cargo-ai.md",
            ".cargo-ai/guidance-transaction/journal.json",
        ] {
            let project = Project::new();
            write_prior_release(&project);
            transaction::failures(&[("file-promoted", true)]);
            assert!(update(&project.0).is_err());
            project.write(target, b"user edits after interruption");
            let before = files(&project.0);
            assert!(update(&project.0).is_err(), "{target}");
            assert_eq!(files(&project.0), before, "{target}");
        }
    }

    #[test]
    fn live_empty_directories_block_staging_and_recovery_without_mutation() {
        for interruption in [None, Some("bundle-promoted"), Some("committed")] {
            let project = Project::new();
            let plan = three_participant_plan(&project);
            if let Some(point) = interruption {
                transaction::failures(&[(point, true)]);
                assert!(transaction::apply(&project.0, &plan).is_err());
            }
            fs::create_dir(project.0.join(BUNDLE).join("user-empty-directory")).unwrap();
            let before = files(&project.0);
            let directories = read_tree_inventory(&project.0, ".cargo-ai").unwrap();
            let result = if interruption.is_some() {
                transaction::recover(&project.0)
            } else {
                transaction::apply(&project.0, &plan)
            };
            assert!(result.is_err(), "{interruption:?}");
            assert_eq!(files(&project.0), before);
            assert_eq!(
                read_tree_inventory(&project.0, ".cargo-ai").unwrap(),
                directories
            );
        }
    }

    #[test]
    fn noncanonical_backup_names_are_not_owned() {
        let project = Project::new();
        let plan = three_participant_plan(&project);
        transaction::failures(&[("bundle-promoted", true)]);
        assert!(transaction::apply(&project.0, &plan).is_err());
        fs::copy(
            project.0.join(TRANSACTION).join("old-files/0"),
            project.0.join(TRANSACTION).join("old-files/00"),
        )
        .unwrap();
        let before = files(&project.0);
        assert!(transaction::recover(&project.0)
            .unwrap_err()
            .contains("unrecognized"));
        assert_eq!(files(&project.0), before);
    }

    #[test]
    fn cleanup_resumes_after_every_file_and_directory_deletion() {
        for committed in [false, true] {
            // Capture the actual cleanup inventory for this plan, including empty
            // prepared directories; probe every possible directory index below.
            let probe = Project::new();
            let plan = three_participant_plan(&probe);
            let initial = if committed {
                "committed"
            } else {
                "bundle-promoted"
            };
            transaction::failures(&[(initial, true)]);
            assert!(transaction::apply(&probe.0, &plan).is_err());
            transaction::failures(&[("cleanup-journal", true)]);
            assert!(transaction::recover(&probe.0).is_err());
            let count = read_tree(&probe.0, TRANSACTION).unwrap().unwrap().len() - 1;
            let mut points = vec![
                "cleanup-journal".to_string(),
                "cleanup-payload-removed".into(),
                "cleanup-journal-removed".into(),
            ];
            points.extend((0..count).map(|index| format!("cleanup-file-{index}")));
            points.extend((0..16).map(|index| format!("cleanup-directory-{index}")));
            for point in points {
                let project = Project::new();
                let plan = three_participant_plan(&project);
                transaction::failures(&[(initial, true)]);
                assert!(transaction::apply(&project.0, &plan).is_err());
                transaction::failures(&[(&point, true)]);
                let interrupted = transaction::recover(&project.0);
                transaction::failures(&[]);
                if point.starts_with("cleanup-directory-") && interrupted.is_ok() {
                    continue; // This index names an absent prepared directory.
                }
                assert!(interrupted.is_err(), "{committed}/{point}");
                assert!(project.0.join(TRANSACTION).exists());
                let before = files(&project.0)
                    .into_iter()
                    .filter(|(path, _)| !path.starts_with(TRANSACTION))
                    .collect::<Tree>();
                transaction::recover(&project.0)
                    .unwrap_or_else(|error| panic!("{committed}/{point}: {error}"));
                let after = files(&project.0);
                assert_eq!(before, after, "{committed}/{point}");
                assert_eq!(
                    read_tree(&project.0, BUNDLE).unwrap(),
                    if committed {
                        plan.after_bundle
                    } else {
                        plan.before_bundle
                    }
                );
                assert!(!project.0.join(TRANSACTION).exists());
            }
        }
    }

    #[test]
    fn cleanup_preserves_unrecognized_entries_after_partial_deletion() {
        for target in ["user-empty-directory", "user-note.txt"] {
            let project = Project::new();
            let plan = three_participant_plan(&project);
            transaction::failures(&[("cleanup-file-0", true)]);
            assert!(transaction::apply(&project.0, &plan).is_err());
            let path = project.0.join(TRANSACTION).join(target);
            if target.ends_with(".txt") {
                fs::write(path, "user work").unwrap();
            } else {
                fs::create_dir(path).unwrap();
            }
            let before = read_tree_inventory(&project.0, ".cargo-ai").unwrap();
            assert!(transaction::recover(&project.0).is_err());
            assert_eq!(
                read_tree_inventory(&project.0, ".cargo-ai").unwrap(),
                before
            );
        }
    }

    #[test]
    fn readers_do_not_create_locks_and_competing_writers_fail_closed() {
        let project = Project::new();
        assert!(transaction::read_lock(&project.0).unwrap().is_none());
        assert!(!project.0.join(".cargo-ai").exists());
        let writer = transaction::write_lock(&project.0).unwrap();
        assert!(transaction::write_lock(&project.0).is_err());
        assert!(transaction::read_lock(&project.0).is_err());
        drop(writer);
        let reader = transaction::read_lock(&project.0).unwrap().unwrap();
        assert!(transaction::write_lock(&project.0).is_err());
        drop(reader);
        transaction::write_lock(&project.0).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn linked_boundaries_are_rejected_before_writes() {
        for target in [
            ".cargo-ai",
            BUNDLE,
            MANIFEST,
            LOCK,
            TRANSACTION,
            "AGENTS.md",
            "CLAUDE.md",
            ".gitignore",
        ] {
            let project = Project::new();
            let outside = Project::new();
            outside.write("sentinel", b"outside must remain");
            let path = project.0.join(target);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside.0, &path).unwrap();
            #[cfg(windows)]
            {
                let output = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(&path)
                    .arg(&outside.0)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let before = files(&outside.0);
            assert!(
                add(&project.0, &[GuidanceStyle::Codex]).is_err(),
                "{target}"
            );
            assert_eq!(files(&outside.0), before, "{target}");
            assert!(!project.0.join("CLAUDE.md").is_file());
            #[cfg(windows)]
            fs::remove_dir(&path).unwrap();
            #[cfg(unix)]
            fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn portable_inventory_rejects_aliases_traversal_and_file_directory_overlap() {
        for paths in [
            vec!["../escape"],
            vec!["/absolute"],
            vec!["C:/escape"],
            vec!["a\\b"],
            vec!["a", "A"],
            vec!["a", "a/b"],
            vec!["a/../b"],
        ] {
            let tree = paths
                .into_iter()
                .map(|path| (path.into(), Vec::new()))
                .collect();
            assert!(validate_tree_paths(&tree).is_err());
        }
    }

    #[test]
    fn unowned_empty_directories_are_preserved_in_bundles_and_recovery_state() {
        let project = Project::new();
        project.install();
        let extra = project.0.join(BUNDLE).join("user-empty-directory");
        fs::create_dir(&extra).unwrap();
        assert_eq!(inspect(&project.0).unwrap().state, State::Legacy);
        let before = files(&project.0);
        assert!(update(&project.0).is_err());
        assert_eq!(files(&project.0), before);
        assert!(extra.is_dir());
        fs::remove_dir(extra).unwrap();
        write_prior_release(&project);
        transaction::failures(&[("file-promoted", true)]);
        assert!(update(&project.0).is_err());
        let extra = project.0.join(TRANSACTION).join("user-empty-directory");
        fs::create_dir(&extra).unwrap();
        let before = files(&project.0);
        assert!(update(&project.0).is_err());
        assert_eq!(files(&project.0), before);
        assert!(extra.is_dir());
    }
}
