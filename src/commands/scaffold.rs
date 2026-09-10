//! Shared scaffolding logic for `cargo ai init` and `cargo ai new`.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GITIGNORE_BEGIN_MARKER: &str = "# BEGIN cargo-ai managed artifacts";
const GITIGNORE_END_MARKER: &str = "# END cargo-ai managed artifacts";
const GITIGNORE_ENTRIES: [&str; 11] = [
    "/.cargo-ai/data/",
    "AGENTS.md",
    "CLAUDE.md",
    ".cargo-ai/guidance/",
    "/.cargo-ai/guidance.lock",
    "/.cargo-ai/guidance-transaction/",
    ".cargo-ai/docs/",
    ".cargo-ai/examples/",
    ".cargo-ai/tools/",
    ".cargo-ai/agents/",
    "tools/*/target/",
];

/// Supported version-control initialization modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VcsMode {
    Git,
    None,
}

impl VcsMode {
    /// Parses VCS mode from CLI argument text.
    pub fn from_cli(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("git") {
            "git" => Ok(Self::Git),
            "none" => Ok(Self::None),
            other => Err(format!(
                "Unsupported VCS mode '{}'. Use `--vcs git` or `--vcs none`.",
                other
            )),
        }
    }
}

/// Git setup result for scaffold execution reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitSetup {
    Initialized,
    AlreadyPresent,
    Skipped,
}

impl fmt::Display for GitSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialized => write!(f, "initialized"),
            Self::AlreadyPresent => write!(f, "already-present"),
            Self::Skipped => write!(f, "skipped"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedFileStatus {
    Created,
    Updated,
    Unchanged,
    Skipped,
}

impl fmt::Display for ManagedFileStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Updated => write!(f, "updated"),
            Self::Unchanged => write!(f, "unchanged"),
            Self::Skipped => write!(f, "skipped"),
        }
    }
}

/// Structured scaffold output for CLI reporting and tests.
#[derive(Debug)]
pub struct ScaffoldReport {
    pub project_root: PathBuf,
    pub metadata_path: PathBuf,
    pub metadata_status: ManagedFileStatus,
    pub gitignore_path: PathBuf,
    pub gitignore_status: ManagedFileStatus,
    pub git_setup: GitSetup,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProjectMetadataDocument {
    #[serde(default)]
    format_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    vcs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<ProjectIdentityDocument>,
    #[serde(default)]
    tools: Option<ProjectToolsPolicyDocument>,
    #[serde(flatten)]
    extra: toml::Table,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProjectIdentityDocument {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(flatten)]
    extra: toml::Table,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProjectToolsPolicyDocument {
    #[serde(default)]
    allow_global_fallback: Option<bool>,
    #[serde(flatten)]
    extra: toml::Table,
}

/// Creates a new Cargo-AI project directory and initializes managed files.
pub fn scaffold_new(target_dir: &Path, vcs_mode: VcsMode) -> Result<ScaffoldReport, String> {
    if target_dir.exists() {
        return Err(format!(
            "Target path '{}' already exists. Use `cargo ai init <path>` for existing directories.",
            target_dir.display()
        ));
    }

    fs::create_dir_all(target_dir).map_err(|error| {
        format!(
            "Failed to create project directory '{}': {}",
            target_dir.display(),
            error
        )
    })?;

    match scaffold_in_place(target_dir, vcs_mode, false) {
        Ok(report) => Ok(report),
        Err(error) => {
            let _ = fs::remove_dir_all(target_dir);
            Err(error)
        }
    }
}

/// Initializes managed files in an existing Cargo-AI project directory.
pub fn scaffold_init(target_dir: &Path, vcs_mode: VcsMode) -> Result<ScaffoldReport, String> {
    if !target_dir.exists() {
        return Err(format!(
            "Target path '{}' does not exist. Use `cargo ai new <path>` to create a new directory.",
            target_dir.display()
        ));
    }

    if !target_dir.is_dir() {
        return Err(format!(
            "Target path '{}' is not a directory.",
            target_dir.display()
        ));
    }

    scaffold_in_place(target_dir, vcs_mode, true)
}

fn scaffold_in_place(
    target_dir: &Path,
    vcs_mode: VcsMode,
    allow_existing_metadata: bool,
) -> Result<ScaffoldReport, String> {
    let metadata_path = super::runtime_data::confined_path(
        target_dir,
        Path::new(".cargo-ai/project.toml"),
        "Project metadata",
    )?;
    let metadata_exists = metadata_path.exists();
    let gitignore_path = target_dir.join(".gitignore");
    if vcs_mode == VcsMode::Git {
        super::runtime_data::confined_path(
            target_dir,
            Path::new(".gitignore"),
            "Project ignore file",
        )?;
    }

    let mut managed_paths = Vec::new();
    if !metadata_exists || !allow_existing_metadata {
        managed_paths.push(metadata_path.clone());
    }

    ensure_no_conflicts(&managed_paths)?;
    // Validate existing metadata before Git initialization or any managed write.
    if metadata_exists {
        let contents = fs::read_to_string(&metadata_path)
            .map_err(|error| format!("Failed to read project metadata: {error}"))?;
        toml::from_str::<ProjectMetadataDocument>(&contents).map_err(|error| {
            format!(
                "Failed to parse project metadata '{}': {error}",
                metadata_path.display()
            )
        })?;
        super::runtime_data::uses_project_data(&contents)?;
    }

    let git_setup = setup_git(target_dir, vcs_mode)?;
    let include_git_metadata = vcs_mode == VcsMode::Git && git_setup != GitSetup::Skipped;

    if let Some(parent) = metadata_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create metadata directory '{}': {}",
                parent.display(),
                error
            )
        })?;
    }

    let metadata_status = write_project_metadata(
        &metadata_path,
        include_git_metadata,
        default_project_name(target_dir),
        !allow_existing_metadata,
    )?;
    let gitignore_status = ensure_gitignore(&gitignore_path, include_git_metadata)?;

    Ok(ScaffoldReport {
        project_root: target_dir.to_path_buf(),
        metadata_path,
        metadata_status,
        gitignore_path,
        gitignore_status,
        git_setup,
    })
}

fn ensure_no_conflicts(managed_paths: &[PathBuf]) -> Result<(), String> {
    let mut conflicts = Vec::new();

    for path in managed_paths {
        if path.exists() {
            conflicts.push(path.display().to_string());
        }
    }

    if conflicts.is_empty() {
        return Ok(());
    }

    Err(format!(
        "Scaffold conflicts detected. The following managed file(s) already exist: {}. Remove conflicting files or choose a different target path.",
        conflicts.join(", ")
    ))
}

fn setup_git(target_dir: &Path, vcs_mode: VcsMode) -> Result<GitSetup, String> {
    if vcs_mode == VcsMode::None {
        return Ok(GitSetup::Skipped);
    }

    if target_dir.join(".git").exists() {
        return Ok(GitSetup::AlreadyPresent);
    }

    let status = Command::new("git")
        .arg("init")
        .current_dir(target_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                format!(
                    "Git initialization could not be completed in '{}'. Install Git or re-run with `--vcs none`.",
                    target_dir.display()
                )
            } else {
                format!(
                    "Git initialization could not be completed in '{}': {}. Install Git or re-run with `--vcs none`.",
                    target_dir.display(),
                    error
                )
            }
        })?;

    if !status.success() {
        return Err(format!(
            "Git initialization failed in '{}'. Install Git or re-run with `--vcs none` if you do not want version control. Exit status: {}.",
            target_dir.display(), status
        ));
    }

    Ok(GitSetup::Initialized)
}

fn write_project_metadata(
    metadata_path: &Path,
    include_git_metadata: bool,
    default_project_name: String,
    adopt_data: bool,
) -> Result<ManagedFileStatus, String> {
    let existing = match fs::read_to_string(metadata_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "Failed to read metadata file '{}': {}",
                metadata_path.display(),
                error
            ))
        }
    };
    let rendered = render_project_metadata(
        existing.as_deref(),
        include_git_metadata,
        default_project_name.as_str(),
        adopt_data,
    )?;

    let status = match existing.as_deref() {
        None => ManagedFileStatus::Created,
        Some(contents) if contents == rendered => ManagedFileStatus::Unchanged,
        Some(_) => ManagedFileStatus::Updated,
    };

    if status != ManagedFileStatus::Unchanged {
        fs::write(metadata_path, rendered).map_err(|error| {
            format!(
                "Failed to write metadata file '{}': {}",
                metadata_path.display(),
                error
            )
        })?;
    }

    Ok(status)
}

fn render_project_metadata(
    existing: Option<&str>,
    include_git_metadata: bool,
    default_project_name: &str,
    adopt_data: bool,
) -> Result<String, String> {
    let mut document = match existing {
        Some(contents) => toml::from_str::<ProjectMetadataDocument>(contents)
            .map_err(|error| format!("Failed to parse project metadata: {error}"))?,
        None => ProjectMetadataDocument::default(),
    };
    if adopt_data {
        let runtime = document
            .extra
            .entry("runtime".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        runtime
            .as_table_mut()
            .ok_or("Project runtime must be a table")?
            .insert(
                "data_root".to_string(),
                toml::Value::String(super::runtime_data::PROJECT_DATA_PATH.to_string()),
            );
    }
    document.format_version = 1;
    document.vcs = include_git_metadata.then(|| "git".to_string());
    document.extra.remove("tool");
    document.extra.remove("tool_version");
    document.extra.remove("template");
    document.extra.remove("managed_by");
    document.extra.remove("managed_by_version");

    let mut tools = document.tools.unwrap_or_default();
    if tools.allow_global_fallback.is_none() {
        tools.allow_global_fallback = Some(true);
    }
    document.tools = Some(tools);

    let mut project = document.project.unwrap_or_default();
    if project
        .name
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        project.name = Some(default_project_name.to_string());
    }
    if project
        .version
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        project.version = Some("0.1.0".to_string());
    }
    document.project = Some(project);

    let mut rendered =
        toml::to_string_pretty(&document).expect("project metadata should serialize to TOML");
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

fn ensure_gitignore(
    gitignore_path: &Path,
    include_gitignore_block: bool,
) -> Result<ManagedFileStatus, String> {
    if !include_gitignore_block {
        return Ok(ManagedFileStatus::Skipped);
    }

    let existing = match fs::read_to_string(gitignore_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "Failed to read ignore file '{}': {}",
                gitignore_path.display(),
                error
            ))
        }
    };

    let Some(rendered) = updated_gitignore(existing.as_deref())? else {
        return Ok(ManagedFileStatus::Unchanged);
    };
    write_gitignore(gitignore_path, &rendered)
}

/// Computes the managed-block repair without writing any project file.
fn updated_gitignore(existing: Option<&str>) -> Result<Option<String>, String> {
    if existing
        .map(gitignore_has_required_entries)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let block = render_gitignore_block();
    let rendered = match existing {
        None => block,
        Some(existing) => {
            let mut contents = existing.to_owned();
            let begin_count = contents.matches(GITIGNORE_BEGIN_MARKER).count();
            let end_count = contents.matches(GITIGNORE_END_MARKER).count();
            if begin_count != end_count || begin_count > 1 {
                return Err("The Cargo AI ignore block has incomplete or duplicate markers. Repair its markers before updating guidance.".to_string());
            }
            if let (Some(begin), Some(end)) = (
                contents.find(GITIGNORE_BEGIN_MARKER),
                contents.find(GITIGNORE_END_MARKER),
            ) {
                if begin < end {
                    let managed = &contents[begin..end];
                    let missing = GITIGNORE_ENTRIES
                        .iter()
                        .filter(|entry| !managed.lines().any(|line| line.trim() == **entry))
                        .map(|entry| format!("{entry}\n"))
                        .collect::<String>();
                    contents.insert_str(end, &missing);
                    return Ok(Some(contents));
                }
                return Err("The Cargo AI ignore block has reversed markers. Repair its markers before updating guidance.".to_string());
            }
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            if !contents.trim_end().is_empty() {
                contents.push('\n');
            }
            contents.push_str(&block);
            contents
        }
    };

    Ok(Some(rendered))
}

/// Plans guidance ignore maintenance only for a declared, exact Git project.
/// Git commands are read-only; the caller owns any resulting file transaction.
pub(crate) fn guidance_vcs_plan(
    root: &Path,
    generated_paths: &[String],
) -> Result<(Option<Vec<u8>>, Vec<String>), String> {
    let mut notices = Vec::new();
    let metadata_path = super::runtime_data::confined_path(
        root,
        Path::new(".cargo-ai/project.toml"),
        "Project metadata",
    )?;
    let metadata = match fs::read_to_string(&metadata_path) {
        Ok(contents) => {
            match toml::from_str::<ProjectMetadataDocument>(&contents) {
                Ok(document) if document.format_version == 1 => document,
                _ => {
                    notices.push("Project metadata is malformed or unsupported; ignore entries are unchanged.".into());
                    return Ok((None, notices));
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            notices.push("No Cargo AI project metadata; ignore entries are unchanged.".into());
            return Ok((None, notices));
        }
        Err(error) => return Err(format!("Cannot read project metadata: {error}")),
    };
    if metadata.vcs.as_deref() != Some("git") {
        notices.push("Project metadata does not select Git; ignore entries are unchanged.".into());
        return Ok((None, notices));
    }
    let git_path =
        super::runtime_data::confined_path(root, Path::new(".git"), "Project Git boundary")?;
    if !git_path.exists() {
        notices.push("No Git boundary at this project root; ignore entries are unchanged.".into());
        return Ok((None, notices));
    }
    let canonical_root =
        fs::canonicalize(root).map_err(|error| format!("Cannot resolve project root: {error}"))?;
    let git_root = guidance_git(root)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    let exact_boundary = git_root
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let path = String::from_utf8(output.stdout).ok()?;
            fs::canonicalize(path.trim_end_matches(['\r', '\n'])).ok()
        })
        == Some(canonical_root);
    if !exact_boundary {
        notices.push(
            "Git did not confirm this exact project boundary; ignore entries are unchanged.".into(),
        );
        return Ok((None, notices));
    }

    if !generated_paths.is_empty() {
        let output = guidance_git(root)
            .args(["ls-files", "-z", "--"])
            .args(
                generated_paths
                    .iter()
                    .map(|path| format!(":(top,literal){path}")),
            )
            .output()
            .map_err(|error| format!("Cannot inspect tracked guidance files: {error}"))?;
        if !output.status.success() {
            return Err(
                "Git could not inspect tracked guidance files; no index changes were attempted."
                    .into(),
            );
        }
        for path in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            notices.push(format!(
                "Generated guidance is tracked: {}. The Git index is unchanged.",
                String::from_utf8_lossy(path)
            ));
        }
    }
    let ignore_path =
        super::runtime_data::confined_path(root, Path::new(".gitignore"), "Project ignore file")?;
    let existing = match fs::read_to_string(ignore_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Cannot read project ignore file: {error}")),
    };
    let update = updated_gitignore(existing.as_deref())?;
    if update.is_some() {
        notices.push("Cargo AI ignore entries are incomplete; explicit add/update can repair the managed block.".into());
    }
    Ok((update.map(String::into_bytes), notices))
}

fn guidance_git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stderr(Stdio::null());
    // An inherited Git worktree/index override must not redirect this query.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") && key != "GIT_OPTIONAL_LOCKS" {
            command.env_remove(key);
        }
    }
    command
}

fn write_gitignore(gitignore_path: &Path, rendered: &str) -> Result<ManagedFileStatus, String> {
    let status = if gitignore_path.exists() {
        ManagedFileStatus::Updated
    } else {
        ManagedFileStatus::Created
    };

    fs::write(gitignore_path, rendered).map_err(|error| {
        format!(
            "Failed to write ignore file '{}': {}",
            gitignore_path.display(),
            error
        )
    })?;

    Ok(status)
}

fn render_gitignore_block() -> String {
    let mut lines = vec![GITIGNORE_BEGIN_MARKER.to_string()];
    lines.extend(GITIGNORE_ENTRIES.iter().map(|entry| entry.to_string()));
    lines.push(GITIGNORE_END_MARKER.to_string());
    format!("{}\n", lines.join("\n"))
}

fn default_project_name(target_dir: &Path) -> String {
    target_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("cargo-ai-project")
        .to_string()
}

fn gitignore_has_required_entries(contents: &str) -> bool {
    GITIGNORE_ENTRIES
        .iter()
        .all(|entry| contents.lines().any(|line| line.trim() == *entry))
}

#[cfg(test)]
mod tests {
    use super::{scaffold_init, scaffold_new, ManagedFileStatus, VcsMode};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use toml::Value;

    fn init_test_git(root: &std::path::Path) {
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn guidance_vcs_plan_is_read_only_and_preserves_index_and_unrelated_ignores() {
        let root = temp_dir_path("guidance-vcs");
        fs::create_dir_all(root.join(".cargo-ai")).unwrap();
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "format_version = 1\nvcs = \"git\"\n",
        )
        .unwrap();
        init_test_git(&root);
        fs::write(root.join("AGENTS.md"), "User instructions\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "--", "AGENTS.md"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        let index = fs::read(root.join(".git/index")).unwrap();
        let ignore = "# user choices\nsecret.log\n";
        fs::write(root.join(".gitignore"), ignore).unwrap();
        let paths = vec!["AGENTS.md".to_string(), ".cargo-ai/guidance".to_string()];
        let (update, notices) = super::guidance_vcs_plan(&root, &paths).unwrap();
        let update = String::from_utf8(update.unwrap()).unwrap();
        assert!(update.starts_with(ignore));
        assert!(update.contains("/.cargo-ai/guidance-transaction/"));
        assert!(notices
            .iter()
            .any(|notice| notice.contains("tracked: AGENTS.md")));
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), ignore);
        assert!(!root.join(".cargo-ai/guidance.lock").exists());
        fs::write(root.join(".gitignore"), update).unwrap();
        assert!(super::guidance_vcs_plan(&root, &paths).unwrap().0.is_none());
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guidance_vcs_requires_valid_metadata_and_its_own_git_boundary() {
        let root = temp_dir_path("guidance-boundary");
        fs::create_dir_all(root.join("nested/.cargo-ai")).unwrap();
        init_test_git(&root);
        let project = root.join("nested");
        for metadata in [
            None,
            Some("format_version = 1\nvcs = \"none\"\n"),
            Some("not = [valid"),
            Some("format_version = 99\nvcs = \"git\"\n"),
            Some("format_version = 1\nvcs = \"git\"\n"),
        ] {
            let path = project.join(".cargo-ai/project.toml");
            if let Some(contents) = metadata {
                fs::write(&path, contents).unwrap();
            } else if path.exists() {
                fs::remove_file(path).unwrap();
            }
            let (update, notices) = super::guidance_vcs_plan(&project, &[]).unwrap();
            assert!(update.is_none(), "{metadata:?}");
            assert!(!notices.is_empty());
            assert!(!project.join(".gitignore").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guidance_vcs_accepts_a_git_file_at_the_exact_project_boundary() {
        let root = temp_dir_path("guidance-git-file");
        let project = root.join("project");
        fs::create_dir_all(project.join(".cargo-ai")).unwrap();
        fs::write(
            project.join(".cargo-ai/project.toml"),
            "format_version = 1\nvcs = \"git\"\n",
        )
        .unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "--quiet", "--separate-git-dir"])
            .arg(root.join("git-metadata"))
            .current_dir(&project)
            .status()
            .unwrap()
            .success());
        assert!(project.join(".git").is_file());
        assert!(super::guidance_vcs_plan(&project, &[]).unwrap().0.is_some());
        assert!(!project.join(".gitignore").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignore_repairs_reject_ambiguous_markers_and_preserve_user_content() {
        for contents in [
            "# BEGIN cargo-ai managed artifacts\n",
            "# END cargo-ai managed artifacts\n# BEGIN cargo-ai managed artifacts\n",
            "# BEGIN cargo-ai managed artifacts\n# END cargo-ai managed artifacts\n# BEGIN cargo-ai managed artifacts\n# END cargo-ai managed artifacts\n",
        ] {
            assert!(super::updated_gitignore(Some(contents)).is_err());
        }
        let old = "before\n# BEGIN cargo-ai managed artifacts\ncustom-pattern\n# END cargo-ai managed artifacts\nafter\n";
        let updated = super::updated_gitignore(Some(old)).unwrap().unwrap();
        assert!(updated.starts_with("before\n"));
        assert!(updated.ends_with("after\n"));
        assert!(updated.contains("custom-pattern\n"));
        assert!(super::updated_gitignore(Some(&updated)).unwrap().is_none());
    }

    fn temp_dir_path(stem: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cargo-ai-scaffold-test-{}-{}", stem, nanos))
    }

    #[test]
    fn scaffold_new_fails_if_target_exists() {
        let dir = temp_dir_path("existing");
        fs::create_dir_all(&dir).expect("test dir should be created");

        let err = scaffold_new(&dir, VcsMode::None).expect_err("should fail");
        assert!(err.contains("already exists"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_writes_metadata_only_for_phase_one_bootstrap() {
        let dir = temp_dir_path("init-minimal");
        fs::create_dir_all(&dir).expect("test dir should be created");

        let report = scaffold_init(&dir, VcsMode::None).expect("init should succeed");
        assert_eq!(report.metadata_status, ManagedFileStatus::Created);
        assert!(report.metadata_path.exists());
        assert_eq!(report.gitignore_status, ManagedFileStatus::Skipped);

        let metadata_contents =
            fs::read_to_string(&report.metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        let expected_project_name = super::default_project_name(&dir);
        assert_eq!(
            parsed.get("format_version").and_then(Value::as_integer),
            Some(1)
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some(expected_project_name.as_str())
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(!dir.join("AGENTS.md").exists());
        assert!(!dir.join("CLAUDE.md").exists());
        assert!(!dir.join(".cargo-ai/guidance").exists());
        assert!(!dir.join(".cargo-ai/docs").exists());
        assert!(!dir.join(".cargo-ai/examples").exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_adds_default_tool_policy_to_existing_minimal_metadata() {
        let dir = temp_dir_path("init-preserve");
        let metadata_path = dir.join(".cargo-ai").join("project.toml");
        fs::create_dir_all(
            metadata_path
                .parent()
                .expect("metadata parent should exist"),
        )
        .expect("metadata dir should be created");
        fs::write(&metadata_path, "format_version = 1\n")
            .expect("metadata fixture should be written");

        let report = scaffold_init(&dir, VcsMode::None).expect("init should succeed");
        assert_eq!(report.metadata_status, ManagedFileStatus::Updated);
        assert_eq!(report.gitignore_status, ManagedFileStatus::Skipped);

        let metadata_contents =
            fs::read_to_string(&metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        let expected_project_name = super::default_project_name(&dir);
        assert_eq!(
            parsed.get("format_version").and_then(Value::as_integer),
            Some(1)
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some(expected_project_name.as_str())
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(true)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_normalizes_existing_metadata_to_phase_one_contract() {
        let dir = temp_dir_path("init-normalize");
        let metadata_path = dir.join(".cargo-ai").join("project.toml");
        fs::create_dir_all(
            metadata_path
                .parent()
                .expect("metadata parent should exist"),
        )
        .expect("metadata dir should be created");
        fs::write(
            &metadata_path,
            "# Managed by cargo-ai init/new.\n\
tool = \"cargo-ai\"\n\
tool_version = \"0.1.0\"\n\
template = \"codex\"\n\
existing = true\n",
        )
        .expect("metadata fixture should be written");

        let report = scaffold_init(&dir, VcsMode::None).expect("init should succeed");
        assert_eq!(report.metadata_status, ManagedFileStatus::Updated);

        let metadata_contents =
            fs::read_to_string(&metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        let expected_project_name = super::default_project_name(&dir);
        assert_eq!(
            parsed.get("format_version").and_then(Value::as_integer),
            Some(1)
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some(expected_project_name.as_str())
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );
        assert_eq!(parsed.get("existing").and_then(Value::as_bool), Some(true));
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(true)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_preserves_explicit_existing_tool_policy() {
        let dir = temp_dir_path("init-preserve-tool-policy");
        let metadata_path = dir.join(".cargo-ai").join("project.toml");
        fs::create_dir_all(
            metadata_path
                .parent()
                .expect("metadata parent should exist"),
        )
        .expect("metadata dir should be created");
        fs::write(
            &metadata_path,
            "format_version = 1\n\n[tools]\nallow_global_fallback = false\n",
        )
        .expect("metadata fixture should be written");

        let report = scaffold_init(&dir, VcsMode::None).expect("init should succeed");
        assert_eq!(report.metadata_status, ManagedFileStatus::Updated);

        let metadata_contents =
            fs::read_to_string(&metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        let expected_project_name = super::default_project_name(&dir);
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some(expected_project_name.as_str())
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_preserves_existing_build_section() {
        let dir = temp_dir_path("init-preserve-build");
        let metadata_path = dir.join(".cargo-ai").join("project.toml");
        fs::create_dir_all(
            metadata_path
                .parent()
                .expect("metadata parent should exist"),
        )
        .expect("metadata dir should be created");
        fs::write(
            &metadata_path,
            "format_version = 1\n\n[build.default]\nagent_definitions = [\"agents/demo.json\"]\nhatched_agents = [\"agents/cli.json\"]\ntools = [\"hello_tool\"]\nassets = [\"assets/prompts/\"]\n",
        )
        .expect("metadata fixture should be written");

        let report = scaffold_init(&dir, VcsMode::None).expect("init should succeed");
        assert_eq!(report.metadata_status, ManagedFileStatus::Updated);

        let metadata_contents =
            fs::read_to_string(&metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        let expected_project_name = super::default_project_name(&dir);
        assert_eq!(
            parsed
                .get("build")
                .and_then(Value::as_table)
                .and_then(|build| build.get("default"))
                .and_then(Value::as_table)
                .and_then(|profile| profile.get("hatched_agents"))
                .and_then(Value::as_array)
                .and_then(|entries| entries.first())
                .and_then(Value::as_str),
            Some("agents/cli.json")
        );
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some(expected_project_name.as_str())
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_preserves_existing_project_identity() {
        let dir = temp_dir_path("init-preserve-project");
        let metadata_path = dir.join(".cargo-ai").join("project.toml");
        fs::create_dir_all(
            metadata_path
                .parent()
                .expect("metadata parent should exist"),
        )
        .expect("metadata dir should be created");
        fs::write(
            &metadata_path,
            "format_version = 1\n\n[project]\nname = \"shared_tools\"\nversion = \"1.2.3\"\n",
        )
        .expect("metadata fixture should be written");

        let report = scaffold_init(&dir, VcsMode::None).expect("init should succeed");
        assert_eq!(report.metadata_status, ManagedFileStatus::Updated);

        let metadata_contents =
            fs::read_to_string(&metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some("shared_tools")
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("1.2.3")
        );
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(true)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scaffold_init_with_git_writes_vcs_and_gitignore_when_git_boundary_exists() {
        let dir = temp_dir_path("init-git");
        fs::create_dir_all(dir.join(".git")).expect("git dir should be created");

        let report = scaffold_init(&dir, VcsMode::Git).expect("init should succeed");
        assert_eq!(report.git_setup, super::GitSetup::AlreadyPresent);
        assert_eq!(report.metadata_status, ManagedFileStatus::Created);
        assert_eq!(report.gitignore_status, ManagedFileStatus::Created);

        let metadata_contents =
            fs::read_to_string(&report.metadata_path).expect("metadata should be readable");
        let parsed: Value = toml::from_str(&metadata_contents).expect("metadata should parse");
        let expected_project_name = super::default_project_name(&dir);
        assert_eq!(
            parsed.get("format_version").and_then(Value::as_integer),
            Some(1)
        );
        assert_eq!(parsed.get("vcs").and_then(Value::as_str), Some("git"));
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("name"))
                .and_then(Value::as_str),
            Some(expected_project_name.as_str())
        );
        assert_eq!(
            parsed
                .get("project")
                .and_then(Value::as_table)
                .and_then(|project| project.get("version"))
                .and_then(Value::as_str),
            Some("0.1.0")
        );
        assert_eq!(
            parsed
                .get("tools")
                .and_then(Value::as_table)
                .and_then(|tools| tools.get("allow_global_fallback"))
                .and_then(Value::as_bool),
            Some(true)
        );

        let gitignore_contents =
            fs::read_to_string(&report.gitignore_path).expect("gitignore should be readable");
        assert!(gitignore_contents.contains("AGENTS.md"));
        assert!(gitignore_contents.contains(".cargo-ai/guidance/"));
        assert!(gitignore_contents.contains("tools/*/target/"));

        let second = scaffold_init(&dir, VcsMode::Git).expect("second init should succeed");
        assert_eq!(second.metadata_status, ManagedFileStatus::Unchanged);
        assert_eq!(second.gitignore_status, ManagedFileStatus::Unchanged);

        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn new_adopts_data_but_init_preserves_legacy_and_explicit_metadata() {
        let root = temp_dir_path("runtime-data");
        scaffold_new(&root, VcsMode::None).unwrap();
        let metadata = root.join(".cargo-ai/project.toml");
        assert!(super::super::runtime_data::uses_project_data(
            &fs::read_to_string(&metadata).unwrap()
        )
        .unwrap());
        assert!(!root.join(".cargo-ai/data").exists());
        fs::write(
            &metadata,
            "[project]\nname = 'kept'\nversion = '2.0.0'\n[custom]\nvalue = 'keep'\n",
        )
        .unwrap();
        scaffold_init(&root, VcsMode::None).unwrap();
        let legacy = fs::read_to_string(&metadata).unwrap();
        assert!(!super::super::runtime_data::uses_project_data(&legacy).unwrap());
        assert!(legacy.contains("kept") && legacy.contains("keep"));
        fs::write(
            &metadata,
            format!("{legacy}\n[runtime]\ndata_root = '.cargo-ai/data'\n"),
        )
        .unwrap();
        scaffold_init(&root, VcsMode::None).unwrap();
        assert!(super::super::runtime_data::uses_project_data(
            &fs::read_to_string(metadata).unwrap()
        )
        .unwrap());
        assert!(!root.join(".cargo-ai/data").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_metadata_fails_before_git_or_file_mutation() {
        let root = temp_dir_path("malformed-preserve");
        fs::create_dir_all(root.join(".cargo-ai")).unwrap();
        let metadata = root.join(".cargo-ai/project.toml");
        let bytes = "[project\nname = 'do not replace'\n";
        fs::write(&metadata, bytes).unwrap();
        fs::write(root.join("AGENTS.md"), "user instructions").unwrap();
        assert!(scaffold_init(&root, VcsMode::Git).is_err());
        assert_eq!(fs::read_to_string(metadata).unwrap(), bytes);
        assert_eq!(
            fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "user instructions"
        );
        assert!(!root.join(".git").exists());
        assert!(!root.join(".gitignore").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn data_ignore_updates_one_managed_block_and_preserves_user_entries() {
        let root = temp_dir_path("ignore-data");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(".gitignore");
        let old = format!(
            "user-before\n{}\ncustom-managed\nAGENTS.md\n{}\nuser-after\n",
            super::GITIGNORE_BEGIN_MARKER,
            super::GITIGNORE_END_MARKER
        );
        fs::write(&path, old).unwrap();
        super::ensure_gitignore(&path, true).unwrap();
        let rendered = fs::read_to_string(&path).unwrap();
        assert_eq!(rendered.matches(super::GITIGNORE_BEGIN_MARKER).count(), 1);
        assert_eq!(rendered.matches("/.cargo-ai/data/").count(), 1);
        assert!(rendered.starts_with("user-before\n") && rendered.ends_with("user-after\n"));
        assert!(rendered.contains("custom-managed\n"));
        assert!(!rendered.lines().any(|line| line == "/data/"));
        assert_eq!(
            super::ensure_gitignore(&path, true).unwrap(),
            ManagedFileStatus::Unchanged
        );
        fs::remove_dir_all(root).unwrap();
    }
}
