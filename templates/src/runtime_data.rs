//! Fixed project runtime-data adoption and confined filesystem paths.
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

pub(crate) const PROJECT_DATA_PATH: &str = ".cargo-ai/data";

#[derive(Clone, Debug)]
pub(crate) struct DataRoot {
    boundary: PathBuf,
    relative_root: PathBuf,
}

/// Reads the opt-in without creating data or changing project metadata.
pub(crate) fn project_data_root(project: Option<&Path>) -> Result<Option<DataRoot>, String> {
    let Some(project) = project else {
        return Ok(None);
    };
    let project = if project.as_os_str().is_empty() {
        Path::new(".")
    } else {
        project
    };
    let project_metadata = fs::symlink_metadata(project).map_err(|error| {
        format!(
            "Failed to inspect project boundary '{}': {error}",
            project.display()
        )
    })?;
    if link_like(&project_metadata) || !project_metadata.is_dir() {
        return Err(
            "Project boundary must be a real directory, not a symbolic link or reparse point"
                .to_string(),
        );
    }
    let metadata = confined_path(
        project,
        Path::new(".cargo-ai/project.toml"),
        "Project metadata",
    )?;
    let contents = fs::read_to_string(&metadata).map_err(|error| {
        format!(
            "Failed to read project metadata '{}': {error}",
            metadata.display()
        )
    })?;
    if !uses_project_data(&contents)? {
        return Ok(None);
    }
    let boundary = fs::canonicalize(project)
        .map_err(|error| format!("Failed to resolve project '{}': {error}", project.display()))?;
    let root = DataRoot {
        boundary,
        relative_root: PathBuf::from(PROJECT_DATA_PATH),
    };
    root.path()?;
    Ok(Some(root))
}

pub(crate) fn uses_project_data(contents: &str) -> Result<bool, String> {
    let document: toml::Table = toml::from_str(contents)
        .map_err(|error| format!("Failed to parse .cargo-ai/project.toml: {error}"))?;
    let Some(runtime) = document.get("runtime") else {
        return Ok(false);
    };
    let runtime = runtime
        .as_table()
        .ok_or_else(|| ".cargo-ai/project.toml runtime must be a table".to_string())?;
    match runtime.get("data_root") {
        None => Ok(false),
        Some(value) if value.as_str() == Some(PROJECT_DATA_PATH) => Ok(true),
        Some(_) => Err(format!(".cargo-ai/project.toml runtime.data_root must be `{PROJECT_DATA_PATH}`; remove the setting to retain legacy working-directory behavior")),
    }
}

impl DataRoot {
    pub(crate) fn path(&self) -> Result<PathBuf, String> {
        let path = confined_path(&self.boundary, &self.relative_root, "Project data root")?;
        require_directory_if_present(&path, "Project data root")?;
        Ok(path)
    }

    pub(crate) fn resolve(&self, relative: &Path) -> Result<PathBuf, String> {
        confined_path(&self.path()?, relative, "Project data path")
    }

    /// Called only when an allowed action is about to write or invoke a tool.
    pub(crate) fn ensure_directory(&self) -> Result<PathBuf, String> {
        let path = self.path()?;
        fs::create_dir_all(&path).map_err(|error| {
            format!(
                "Failed to create project data root '{}': {error}",
                path.display()
            )
        })?;
        self.path()
    }
}

pub(crate) fn portable_relative_path(path: &Path, label: &str) -> Result<PathBuf, String> {
    let raw = path
        .to_str()
        .ok_or_else(|| format!("{label} must be a Unicode relative path"))?;
    // Interpret both separators on every host so a definition cannot change authority
    // when it moves between Unix and Windows.
    if raw.trim().is_empty()
        || raw.starts_with(['/', '\\'])
        || raw.contains(':')
        || raw.contains('\0')
    {
        return Err(format!(
            "{label} must be a non-empty relative path without absolute, drive or UNC prefixes"
        ));
    }
    let mut relative = PathBuf::new();
    for part in raw.split(['/', '\\']) {
        match part {
            ".." => return Err(format!("{label} must not use parent traversal (`..`)")),
            "" | "." => {}
            part if part.ends_with(['.', ' ']) => {
                return Err(format!("{label} components must not end in a dot or space"))
            }
            part if is_windows_device_name(part) => {
                return Err(format!("{label} must not use reserved device names"));
            }
            part => {
                relative.push(part);
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(format!("{label} must be a non-empty relative path"));
    }
    Ok(relative)
}

fn is_windows_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

pub(crate) fn link_like(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn require_directory_if_present(path: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if link_like(&metadata) || !metadata.is_dir() => Err(format!(
            "{label} '{}' must be a real directory and not a symbolic link or reparse point",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Failed to inspect {label} '{}': {error}",
            path.display()
        )),
    }
}

/// Validates existing components without creating missing directories.
/// The caller supplies a trusted boundary; subprocesses retain their ambient authority.
pub(crate) fn confined_path(root: &Path, relative: &Path, label: &str) -> Result<PathBuf, String> {
    let relative = portable_relative_path(relative, label)?;
    require_directory_if_present(root, label)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if link_like(&metadata) => {
                return Err(format!(
                    "{label} must not traverse symbolic link or reparse point '{}'",
                    path.display()
                ))
            }
            Ok(metadata) if !metadata.is_dir() && path != root.join(&relative) => {
                return Err(format!(
                    "{label} ancestor '{}' must be a directory",
                    path.display()
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to inspect {label} '{}': {error}",
                    path.display()
                ))
            }
        }
    }
    Ok(path)
}

/// Runtime data is reserved even if it was accidentally tracked or nested in tool sources.
#[allow(dead_code)]
pub(crate) fn is_runtime_data(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    components.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case(".cargo-ai") && pair[1].eq_ignore_ascii_case("data")
    })
}

#[allow(dead_code)]
pub(crate) fn validate_declared_input(relative: &str) -> Result<PathBuf, String> {
    let path = portable_relative_path(Path::new(relative), "Declared input")?;
    if is_runtime_data(&path) || path.to_string_lossy().eq_ignore_ascii_case(".cargo-ai") {
        return Err(format!("Declared input `{relative}` overlaps reserved runtime data `{PROJECT_DATA_PATH}`; declare immutable assets outside that root"));
    }
    Ok(path)
}

#[allow(dead_code)]
pub(crate) fn validate_output_root(project: &Path, output: &Path) -> Result<PathBuf, String> {
    let project =
        fs::canonicalize(project).map_err(|error| format!("Failed to resolve project: {error}"))?;
    let absolute = if output.is_absolute() {
        output.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(output)
    };
    if let Ok(relative) = absolute.strip_prefix(&project) {
        if !relative.as_os_str().is_empty() {
            confined_path(&project, relative, "Output path")?;
        }
    }
    // The OS may spell a temporary directory through a system alias. Locate
    // the project boundary before checking its descendants for redirects.
    for ancestor in absolute.ancestors() {
        if fs::canonicalize(ancestor).ok().as_deref() == Some(project.as_path()) {
            let relative = absolute
                .strip_prefix(ancestor)
                .map_err(|error| error.to_string())?;
            if !relative.as_os_str().is_empty() {
                confined_path(&project, relative, "Output path")?;
            }
            break;
        }
    }
    // Resolve the existing prefix while rejecting redirects in the output tree.
    let mut prefix = absolute.as_path();
    let mut missing = Vec::new();
    while !prefix.exists() {
        if fs::symlink_metadata(prefix).is_ok() {
            return Err(format!(
                "Output '{}' must not be a dangling link",
                absolute.display()
            ));
        }
        missing.push(prefix.file_name().ok_or("Invalid output root")?.to_owned());
        prefix = prefix.parent().ok_or("Invalid output root")?;
    }
    if link_like(&fs::symlink_metadata(prefix).map_err(|error| error.to_string())?) {
        return Err(format!(
            "Output '{}' must not traverse a symbolic link or reparse point",
            absolute.display()
        ));
    }
    let mut resolved = fs::canonicalize(prefix).map_err(|error| error.to_string())?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    let data = project.join(PROJECT_DATA_PATH);
    let reserved_relative = resolved
        .strip_prefix(&project)
        .ok()
        .is_some_and(|relative| {
            is_runtime_data(relative)
                || relative.to_string_lossy().eq_ignore_ascii_case(".cargo-ai")
        });
    if reserved_relative || resolved.starts_with(&data) || data.starts_with(&resolved) {
        return Err(format!(
            "Output '{}' overlaps reserved runtime data `{PROJECT_DATA_PATH}`",
            output.display()
        ));
    }
    Ok(resolved)
}

/// Preflights copied inputs before output replacement. Reserved data and Cargo
/// build caches under source tools are never inspected or copied.
#[allow(dead_code)]
pub(crate) fn validate_source_tree(
    project: &Path,
    relative: &Path,
    skip_target: bool,
) -> Result<(), String> {
    let source = confined_path(project, relative, "Assembly source")?;
    let metadata = fs::symlink_metadata(&source).map_err(|error| {
        format!(
            "Failed to inspect assembly source '{}': {error}",
            source.display()
        )
    })?;
    if metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "Assembly source '{}' must be a regular file or directory",
            source.display()
        ));
    }
    for entry in fs::read_dir(&source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let child = relative.join(entry.file_name());
        if is_runtime_data(&child) || (skip_target && entry.file_name() == "target") {
            continue;
        }
        validate_source_tree(project, &child, skip_target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".cargo-ai")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    #[test]
    fn adoption_is_explicit_and_resolution_is_lazy() {
        let root = fixture();
        let metadata = root.join(".cargo-ai/project.toml");
        fs::write(&metadata, "format_version = 1\n").unwrap();
        assert!(project_data_root(Some(&root)).unwrap().is_none());
        fs::write(&metadata, "[runtime]\ndata_root = '.cargo-ai/data'\n").unwrap();
        let context = project_data_root(Some(&root)).unwrap().unwrap();
        assert_eq!(
            context.resolve(Path::new("logs/events.jsonl")).unwrap(),
            root.join(".cargo-ai/data/logs/events.jsonl")
        );
        assert!(!root.join(PROJECT_DATA_PATH).exists());
        context.ensure_directory().unwrap();
        assert!(root.join(PROJECT_DATA_PATH).is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_adoption_and_portable_escape_forms() {
        for value in ["'data'", "'../data'", "'/data'", "false", "12", "[]"] {
            assert!(uses_project_data(&format!("[runtime]\ndata_root = {value}")).is_err());
        }
        for raw in [
            "",
            ".",
            "../x",
            "a/../x",
            "/x",
            "C:x",
            "C:\\x",
            "\\\\server\\share",
            "\\x",
            "a\\..\\x",
        ] {
            assert!(
                portable_relative_path(Path::new(raw), "test").is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn rejects_runtime_inputs_and_output_overlap_without_mutation() {
        let root = fixture();
        for path in [
            ".cargo-ai",
            ".cargo-ai/data",
            ".cargo-ai/data/x",
            "tools/a/.cargo-ai/data/x",
        ] {
            assert!(validate_declared_input(path).is_err());
        }
        assert!(validate_declared_input("data/seed.csv").is_ok());
        for output in [
            &root,
            &root.join(".cargo-ai"),
            &root.join(".cargo-ai/data/out"),
        ] {
            assert!(validate_output_root(&root, output).is_err());
        }
        assert!(!root.join(PROJECT_DATA_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_linked_ancestors_roots_and_leaves() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = fixture();
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "[runtime]\ndata_root = '.cargo-ai/data'",
        )
        .unwrap();
        let context = project_data_root(Some(&root)).unwrap().unwrap();
        symlink(&outside, root.join(PROJECT_DATA_PATH)).unwrap();
        assert!(context.resolve(Path::new("file")).is_err());
        fs::remove_file(root.join(PROJECT_DATA_PATH)).unwrap();
        context.ensure_directory().unwrap();
        symlink(&outside, root.join(PROJECT_DATA_PATH).join("nested")).unwrap();
        assert!(context.resolve(Path::new("nested/file")).is_err());
        symlink(
            outside.join("missing"),
            root.join(PROJECT_DATA_PATH).join("leaf"),
        )
        .unwrap();
        assert!(context.resolve(Path::new("leaf")).is_err());
        fs::remove_dir_all(root.join(".cargo-ai")).unwrap();
        symlink(&outside, root.join(".cargo-ai")).unwrap();
        assert!(context.resolve(Path::new("file")).is_err());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn rejects_native_directory_junctions() {
        let root = fixture();
        let outside = fixture();
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "[runtime]\ndata_root = '.cargo-ai/data'",
        )
        .unwrap();
        let context = project_data_root(Some(&root)).unwrap().unwrap();
        let junction = root.join(PROJECT_DATA_PATH);
        let result = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(context.resolve(Path::new("output.txt")).is_err());
        assert!(context.ensure_directory().is_err());
        fs::remove_dir(junction).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn reserved_names_do_not_change_meaning_between_platforms() {
        for path in [
            "NUL",
            "con.txt",
            "AUX/report",
            "LPT1.txt",
            "COM1",
            "COM¹",
            "file.",
            "file ",
            "image.png:stream",
        ] {
            assert!(
                portable_relative_path(Path::new(path), "data").is_err(),
                "{path}"
            );
        }
        assert!(validate_declared_input(".CARGO-AI").is_err());
        assert!(portable_relative_path(Path::new("console/file.txt"), "data").is_ok());
    }
}
