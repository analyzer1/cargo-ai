//! Recoverable directory replacement and byte-preserving sidecar changes.
use super::lifecycle::{
    checked, extra_directories, hash, read_file, read_tree, validate_tree_paths, Tree, BUNDLE,
    LOCK, TRANSACTION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub before_bundle: Option<Tree>,
    pub after_bundle: Option<Tree>,
    pub before_files: BTreeMap<String, Option<Vec<u8>>>,
    pub after_files: BTreeMap<String, Option<Vec<u8>>>,
}

#[derive(Debug)]
pub(super) struct LockGuard(File);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock_file(root: &Path, write: bool) -> Result<Option<LockGuard>, String> {
    let path = checked(root, LOCK)?;
    if write {
        let parent = checked(root, ".cargo-ai")?;
        match fs::create_dir(&parent) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("Cannot create guidance control directory: {e}")),
        }
        checked(root, LOCK)?;
    }
    let exists = read_file(root, LOCK)?.is_some();
    if !write && !exists {
        return Ok(None);
    }
    let file = if exists {
        OpenOptions::new().read(true).write(write).open(&path)
    } else {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                read_file(root, LOCK)?;
                OpenOptions::new().read(true).write(true).open(&path)
            }
            result => result,
        }
    }
    .map_err(|e| format!("Cannot open guidance lock: {e}"))?;
    checked(root, LOCK)?;
    let result = if write {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match result {
        Ok(()) => Ok(Some(LockGuard(file))),
        Err(TryLockError::WouldBlock) => Err(
            "Guidance is being changed by another Cargo AI process; retry when it finishes.".into(),
        ),
        Err(TryLockError::Error(e)) => Err(format!("Cannot lock guidance: {e}")),
    }
}
pub(super) fn read_lock(root: &Path) -> Result<Option<LockGuard>, String> {
    lock_file(root, false)
}
pub(super) fn write_lock(root: &Path) -> Result<LockGuard, String> {
    lock_file(root, true)?.ok_or_else(|| "Guidance writer lock is unavailable".into())
}

fn validate_plan(plan: &Plan) -> Result<(), String> {
    if plan.after_bundle.is_none()
        || plan.before_files.keys().ne(plan.after_files.keys())
        || plan.after_files.len() > 3
    {
        return Err("Malformed guidance transaction participant set.".into());
    }
    for tree in [&plan.before_bundle, &plan.after_bundle]
        .into_iter()
        .flatten()
    {
        validate_tree_paths(tree)?;
    }
    for (name, bytes) in &plan.after_files {
        if !["AGENTS.md", "CLAUDE.md", ".gitignore"].contains(&name.as_str()) || bytes.is_none() {
            return Err(format!(
                "Unauthorized guidance transaction participant '{name}'."
            ));
        }
    }
    super::lifecycle::validate_snapshot(&plan.before_bundle, &plan.before_files)?;
    super::lifecycle::validate_snapshot(&plan.after_bundle, &plan.after_files)?;
    Ok(())
}

fn write_new(root: &Path, relative: &str, bytes: &[u8]) -> Result<(), String> {
    let path = checked(root, relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create staged guidance parent: {e}"))?;
    }
    checked(root, relative)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("Cannot create staged guidance '{relative}': {e}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Cannot persist staged guidance '{relative}': {e}"))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn write_tree(root: &Path, relative: &str, tree: &Tree) -> Result<(), String> {
    let path = checked(root, relative)?;
    fs::create_dir(&path)
        .map_err(|e| format!("Cannot create staged guidance bundle '{relative}': {e}"))?;
    for (name, bytes) in tree {
        write_new(root, &format!("{relative}/{name}"), bytes)?;
    }
    sync_directory(&path)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path).and_then(|f| f.sync_all()).map_err(|e| {
        format!(
            "Cannot persist guidance directory '{}': {e}",
            path.display()
        )
    })?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn rename(root: &Path, from: &str, to: &str) -> Result<(), String> {
    let from_path = checked(root, from)?;
    let to_path = checked(root, to)?;
    if fs::symlink_metadata(&to_path).is_ok() {
        return Err(format!(
            "Guidance destination '{to}' already exists; no overwrite was attempted."
        ));
    }
    if let Some(parent) = to_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create guidance transaction parent: {e}"))?;
    }
    checked(root, from)?;
    checked(root, to)?;
    fs::rename(&from_path, &to_path)
        .map_err(|e| format!("Cannot move guidance '{from}' to '{to}': {e}"))?;
    if let Some(parent) = from_path.parent() {
        sync_directory(parent)?;
    }
    if let Some(parent) = to_path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn matches_bundle(root: &Path, bundle: &Option<Tree>) -> Result<bool, String> {
    if read_tree(root, BUNDLE)? != *bundle {
        return Ok(false);
    }
    match bundle {
        Some(tree) => Ok(extra_directories(root, BUNDLE, tree)?.is_empty()),
        None => Ok(true),
    }
}

fn matches_snapshot(
    root: &Path,
    bundle: &Option<Tree>,
    files: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<bool, String> {
    if !matches_bundle(root, bundle)? {
        return Ok(false);
    }
    for (name, bytes) in files {
        if read_file(root, name)? != *bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The journal stores immutable before/after bytes, including generated ownership
/// manifests. Recovery never trusts a pathname without revalidating its contents.
pub(super) fn apply(root: &Path, plan: &Plan) -> Result<(), String> {
    validate_plan(plan)?;
    if !matches_snapshot(root, &plan.before_bundle, &plan.before_files)? {
        return Err("Guidance changed after preflight; retry without overwriting it.".into());
    }
    let journal = serde_json::to_vec(plan).map_err(|e| e.to_string())?;
    if journal.len() > 16 * 1024 * 1024 {
        return Err("Guidance transaction journal exceeds its 16 MiB safety limit.".into());
    }
    fs::create_dir(checked(root, TRANSACTION)?)
        .map_err(|e| format!("Cannot reserve guidance transaction '{TRANSACTION}': {e}"))?;
    if let Err(error) = write_new(root, &format!("{TRANSACTION}/journal.json"), &journal) {
        // No live path has changed; retain an unreadable partial journal for inspection.
        return Err(format!(
            "{error}. No live guidance changed; inspect '{TRANSACTION}' before retrying."
        ));
    }
    let result = (|| {
        sync_directory(&checked(root, TRANSACTION)?)?;
        sync_directory(&checked(root, ".cargo-ai")?)?;
        maybe_fail("journal")?;
        if let Some(tree) = &plan.after_bundle {
            write_tree(root, &format!("{TRANSACTION}/new-bundle"), tree)?;
        }
        for (index, (name, bytes)) in plan.after_files.iter().enumerate() {
            if bytes == &plan.before_files[name] {
                continue;
            }
            let stage = format!("{TRANSACTION}/new-files/{index}");
            write_new(root, &stage, bytes.as_deref().unwrap())?;
            if plan.before_files[name].is_some() {
                let permissions = fs::metadata(checked(root, name)?)
                    .map_err(|e| e.to_string())?
                    .permissions();
                fs::set_permissions(checked(root, &stage)?, permissions)
                    .map_err(|e| format!("Cannot preserve guidance file permissions: {e}"))?;
            }
        }
        let changed_files = plan
            .after_files
            .iter()
            .filter(|(name, bytes)| *bytes != &plan.before_files[*name])
            .collect::<Vec<_>>();
        for (group, needed) in [
            ("displaced-files", !changed_files.is_empty()),
            (
                "old-files",
                changed_files
                    .iter()
                    .any(|(name, _)| plan.before_files[*name].is_some()),
            ),
        ] {
            if needed {
                fs::create_dir(checked(root, &format!("{TRANSACTION}/{group}"))?)
                    .map_err(|e| format!("Cannot prepare guidance backup directory: {e}"))?;
            }
        }
        sync_directory(&checked(root, TRANSACTION)?)?;
        maybe_fail("prepared")?;
        if !matches_snapshot(root, &plan.before_bundle, &plan.before_files)? {
            return Err("Guidance changed while staging; no live overwrite was attempted.".into());
        }
        for (index, (name, bytes)) in plan.after_files.iter().enumerate() {
            if bytes == &plan.before_files[name] {
                continue;
            }
            if plan.before_files[name].is_some() {
                rename(root, name, &format!("{TRANSACTION}/old-files/{index}"))?;
                maybe_fail(&format!("file-backed-up-{index}"))?;
            }
            maybe_fail("file-backed-up")?;
            rename(root, &format!("{TRANSACTION}/new-files/{index}"), name)?;
            maybe_fail(&format!("file-promoted-{index}"))?;
            maybe_fail("file-promoted")?;
        }
        if plan.before_bundle != plan.after_bundle {
            if plan.before_bundle.is_some() {
                rename(root, BUNDLE, &format!("{TRANSACTION}/old-bundle"))?;
            }
            maybe_fail("bundle-backed-up")?;
            rename(root, &format!("{TRANSACTION}/new-bundle"), BUNDLE)?;
            maybe_fail("bundle-promoted")?;
        }
        if !matches_snapshot(root, &plan.after_bundle, &plan.after_files)? {
            return Err(
                "Guidance changed during promotion; recovery requires verified bytes.".into(),
            );
        }
        maybe_fail("before-commit")?;
        write_new(
            root,
            &format!("{TRANSACTION}/committed"),
            hash(&journal).as_bytes(),
        )?;
        sync_directory(&checked(root, TRANSACTION)?)?;
        maybe_fail("committed")?;
        clean(root, plan, &journal, true)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) if interrupted(&error) => Err(format!("{error}; transaction retained at '{TRANSACTION}'.")),
        Err(error) => match recover(root) {
            Ok(()) if matches_snapshot(root, &plan.after_bundle, &plan.after_files)? => Ok(()),
            Ok(()) => Err(format!("{error}; the prior guidance files were restored.")),
            Err(recovery) => Err(format!("{error}; recovery is blocked: {recovery}. Preserve '{TRANSACTION}' and run explicit update after resolving the reported conflict.")),
        },
    }
}

// A terminal journal is renamed into place before deletion begins, then removed
// last. Its name distinguishes cleanup from rollback even after backups disappear.
const JOURNAL: &str = "journal.json";
const COMMITTED_JOURNAL: &str = "cleanup-committed.json";
const RESTORED_JOURNAL: &str = "cleanup-restored.json";

fn artifact_inventory(plan: &Plan, journal: &[u8], journal_name: &str) -> Tree {
    let mut expected = Tree::new();
    expected.insert(journal_name.into(), journal.to_vec());
    if journal_name != RESTORED_JOURNAL {
        expected.insert("committed".into(), hash(journal).into_bytes());
    }
    for (groups, tree) in [
        (&["new-bundle", "discarded-bundle"][..], &plan.after_bundle),
        (&["old-bundle"][..], &plan.before_bundle),
    ] {
        if let Some(tree) = tree {
            for group in groups {
                for (path, bytes) in tree {
                    expected.insert(format!("{group}/{path}"), bytes.clone());
                }
            }
        }
    }
    for (groups, files) in [
        (&["new-files", "displaced-files"][..], &plan.after_files),
        (&["old-files"][..], &plan.before_files),
    ] {
        for group in groups {
            for (index, bytes) in files.values().enumerate() {
                if let Some(bytes) = bytes {
                    expected.insert(format!("{group}/{index}"), bytes.clone());
                }
            }
        }
    }
    expected
}

fn validate_artifacts(
    root: &Path,
    plan: &Plan,
    journal: &[u8],
    journal_name: &str,
) -> Result<Tree, String> {
    let files = read_tree(root, TRANSACTION)?.ok_or("Guidance transaction disappeared")?;
    let expected = artifact_inventory(plan, journal, journal_name);
    if let Some(directory) = extra_directories(root, TRANSACTION, &expected)?.first() {
        return Err(format!(
            "Unrecognized recovery directory '{TRANSACTION}/{directory}'; it was preserved."
        ));
    }
    for (name, bytes) in &files {
        if expected.get(name) != Some(bytes) {
            return Err(format!(
                "Changed/unrecognized recovery artifact '{TRANSACTION}/{name}'; it was preserved."
            ));
        }
    }
    Ok(files)
}

fn remove_empty_transaction(root: &Path) -> Result<(), String> {
    // remove_dir never deletes an unexpected entry, including an empty directory.
    fs::remove_dir(checked(root, TRANSACTION)?)
        .map_err(|e| format!("Cannot remove empty guidance transaction: {e}"))?;
    sync_directory(&checked(root, ".cargo-ai")?)
}

fn clean(root: &Path, plan: &Plan, journal: &[u8], committed: bool) -> Result<(), String> {
    let terminal = if committed {
        COMMITTED_JOURNAL
    } else {
        RESTORED_JOURNAL
    };
    let original = read_file(root, &format!("{TRANSACTION}/{JOURNAL}"))?.is_some();
    let journal_name = if original { JOURNAL } else { terminal };
    let files = validate_artifacts(root, plan, journal, journal_name)?;
    maybe_fail("cleanup")?;
    if original {
        rename(
            root,
            &format!("{TRANSACTION}/{JOURNAL}"),
            &format!("{TRANSACTION}/{terminal}"),
        )?;
    }
    maybe_fail("cleanup-journal")?;
    for (index, name) in files
        .keys()
        .filter(|name| name.as_str() != journal_name)
        .enumerate()
    {
        let path = checked(root, &format!("{TRANSACTION}/{name}"))?;
        fs::remove_file(&path)
            .map_err(|e| format!("Cannot clean guidance artifact '{name}': {e}"))?;
        sync_directory(path.parent().unwrap())?;
        maybe_fail(&format!("cleanup-file-{index}"))?;
    }
    let mut directories = std::collections::BTreeSet::new();
    for name in artifact_inventory(plan, journal, terminal).keys() {
        let mut path = Path::new(name).parent();
        while let Some(parent) = path.filter(|p| !p.as_os_str().is_empty()) {
            directories.insert(parent.to_string_lossy().replace('\\', "/"));
            path = parent.parent();
        }
    }
    // Children sort after their parent; reverse traversal removes children first.
    for (index, directory) in directories.iter().rev().enumerate() {
        let path = checked(root, &format!("{TRANSACTION}/{directory}"))?;
        match fs::remove_dir(&path) {
            Ok(()) => sync_directory(path.parent().unwrap())?,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(format!(
                    "Cannot clean guidance directory '{directory}': {e}"
                ))
            }
        }
        maybe_fail(&format!("cleanup-directory-{index}"))?;
    }
    maybe_fail("cleanup-payload-removed")?;
    fs::remove_file(checked(root, &format!("{TRANSACTION}/{terminal}"))?)
        .map_err(|e| format!("Cannot finish guidance cleanup journal: {e}"))?;
    sync_directory(&checked(root, TRANSACTION)?)?;
    maybe_fail("cleanup-journal-removed")?;
    remove_empty_transaction(root)
}

pub(super) fn recover(root: &Path) -> Result<(), String> {
    let files = read_tree(root, TRANSACTION)?.ok_or("Guidance transaction disappeared")?;
    if files.is_empty() {
        // Either cleanup removed its last journal or preparation stopped before
        // writing one. Neither state can need a live-file mutation.
        return remove_empty_transaction(root);
    }
    let journals: Vec<_> = [JOURNAL, COMMITTED_JOURNAL, RESTORED_JOURNAL]
        .into_iter()
        .filter_map(|name| files.get(name).map(|bytes| (name, bytes)))
        .collect();
    let [(journal_name, journal)] = journals.as_slice() else {
        return Err("Missing or ambiguous recovery journal; preserve the transaction directory for inspection".into());
    };
    let plan: Plan = serde_json::from_slice(journal)
        .map_err(|e| format!("Malformed guidance recovery journal: {e}"))?;
    validate_plan(&plan)?;
    validate_artifacts(root, &plan, journal, journal_name)?;
    let committed = *journal_name == COMMITTED_JOURNAL || files.contains_key("committed");
    if committed || *journal_name == RESTORED_JOURNAL {
        let (bundle, sidecars) = if committed {
            (&plan.after_bundle, &plan.after_files)
        } else {
            (&plan.before_bundle, &plan.before_files)
        };
        if !matches_snapshot(root, bundle, sidecars)? {
            return Err(
                "Completed guidance changed; recovery will not overwrite user edits.".into(),
            );
        }
        return clean(root, &plan, journal, committed);
    }
    let live = read_tree(root, BUNDLE)?;
    if live.is_some()
        && !matches_bundle(root, &plan.before_bundle)?
        && !matches_bundle(root, &plan.after_bundle)?
    {
        return Err(
            "Live guidance contains unexpected bytes; recovery will not overwrite them.".into(),
        );
    }
    if live != plan.before_bundle
        && plan.before_bundle.is_some()
        && read_tree(root, &format!("{TRANSACTION}/old-bundle"))? != plan.before_bundle
    {
        return Err("Missing or incomplete original guidance backup; recovery preserves all remaining files.".into());
    }
    for (index, (name, before)) in plan.before_files.iter().enumerate() {
        let current = read_file(root, name)?;
        if current.is_some() && current != *before && current != plan.after_files[name] {
            return Err(format!(
                "'{name}' changed since the transaction; recovery will not overwrite it."
            ));
        }
        if current != *before
            && before.is_some()
            && read_file(root, &format!("{TRANSACTION}/old-files/{index}"))? != *before
        {
            return Err(format!(
                "Missing original backup for '{name}'; recovery preserves its remaining bytes and metadata."
            ));
        }
    }
    maybe_fail("rollback")?;
    if live != plan.before_bundle {
        if live.is_some() {
            rename(root, BUNDLE, &format!("{TRANSACTION}/discarded-bundle"))?;
            maybe_fail("bundle-displaced")?;
        }
        if plan.before_bundle.is_some() {
            let old = format!("{TRANSACTION}/old-bundle");
            rename(root, &old, BUNDLE)?;
            maybe_fail("bundle-restored")?;
        }
    }
    for (index, (name, before)) in plan.before_files.iter().enumerate() {
        let current = read_file(root, name)?;
        if current == *before {
            continue;
        }
        if current.is_some() {
            rename(
                root,
                name,
                &format!("{TRANSACTION}/displaced-files/{index}"),
            )?;
            maybe_fail(&format!("file-displaced-{index}"))?;
        }
        if before.is_some() {
            let old = format!("{TRANSACTION}/old-files/{index}");
            rename(root, &old, name)?;
        }
        maybe_fail(&format!("file-restored-{index}"))?;
        maybe_fail("file-restored")?;
    }
    if !matches_snapshot(root, &plan.before_bundle, &plan.before_files)? {
        return Err("Prior guidance state has not been completely restored.".into());
    }
    clean(root, &plan, journal, false)
}

#[cfg(test)]
thread_local! { static FAILURES: std::cell::RefCell<Vec<(String, bool)>> = const { std::cell::RefCell::new(Vec::new()) }; }
fn maybe_fail(point: &str) -> Result<(), String> {
    #[cfg(test)]
    if let Some(interrupt) = FAILURES.with(|failures| {
        let mut failures = failures.borrow_mut();
        failures
            .iter()
            .position(|(name, _)| name == point)
            .map(|index| failures.remove(index).1)
    }) {
        return Err(format!(
            "injected {} at {point}",
            if interrupt { "interruption" } else { "failure" }
        ));
    }
    #[cfg(not(test))]
    let _ = point;
    Ok(())
}
fn interrupted(error: &str) -> bool {
    #[cfg(test)]
    {
        error.starts_with("injected interruption")
    }
    #[cfg(not(test))]
    {
        let _ = error;
        false
    }
}
#[cfg(test)]
pub(super) fn failures(points: &[(&str, bool)]) {
    FAILURES.with(|value| {
        *value.borrow_mut() = points
            .iter()
            .map(|(name, interrupt)| ((*name).into(), *interrupt))
            .collect()
    });
}
