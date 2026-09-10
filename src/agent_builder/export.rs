//! Handles exporting the compiled agent binary to the requested output directory.

use super::build_target::{BuildTarget, CargoCompileProfile};
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

fn export_binary_to_path(
    source_path: &Path,
    dest_path: &Path,
    force_overwrite: bool,
) -> io::Result<PathBuf> {
    if !source_path.exists() {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!("Expected binary not found at {:?}", source_path),
        ));
    }

    if dest_path.exists() {
        if !force_overwrite {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!(
                    "Output already exists at {:?}. Re-run with --force to overwrite.",
                    dest_path
                ),
            ));
        }
    }

    fs::copy(source_path, dest_path)?;
    Ok(dest_path.to_path_buf())
}

/// Copies the built binary from `.cargo-ai/agents/{agent}/target/...`
/// into either the requested output directory or the current working directory.
pub fn export_binary(
    agent_name: &str,
    force_overwrite: bool,
    build_target: &BuildTarget,
    output_dir: Option<&Path>,
    source_project_path: Option<&Path>,
) -> io::Result<PathBuf> {
    let project_path = super::agent_workspace_path(agent_name);
    let source_root = source_project_path.unwrap_or(project_path.as_path());
    let source_path =
        build_target.compiled_binary_path(source_root, agent_name, CargoCompileProfile::Release);
    let dest_dir = match output_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };

    if dest_dir.exists() {
        if !dest_dir.is_dir() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Output directory {:?} is not a directory. Re-run with --output-dir <DIR>.",
                    dest_dir
                ),
            ));
        }
    } else {
        fs::create_dir_all(&dest_dir)?;
    }

    let dest_path = build_target.exported_binary_path(&dest_dir, agent_name);

    export_binary_to_path(&source_path, &dest_path, force_overwrite)
}

#[cfg(test)]
mod tests {
    use super::{export_binary, export_binary_to_path};
    use crate::agent_builder::build_target::{BuildTarget, CargoCompileProfile};
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_test_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let sequence = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("cargo-ai-export-test-{nanos}-{sequence}"));
        fs::create_dir_all(&path).expect("test directory should be creatable");
        path
    }

    #[test]
    fn fails_when_destination_exists_without_force() {
        let dir = temp_test_dir();
        let source = dir.join("source-bin");
        let dest = dir.join("dest-bin");
        fs::write(&source, b"source").expect("source should be writable");
        fs::write(&dest, b"existing").expect("dest should be writable");

        let err = export_binary_to_path(&source, &dest, false).expect_err("should fail");
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("--force"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrites_when_force_enabled() {
        let dir = temp_test_dir();
        let source = dir.join("source-bin");
        let dest = dir.join("dest-bin");
        fs::write(&source, b"source").expect("source should be writable");
        fs::write(&dest, b"existing").expect("dest should be writable");

        export_binary_to_path(&source, &dest, true).expect("force overwrite should succeed");
        let copied = fs::read(&dest).expect("dest should be readable");
        assert_eq!(copied, b"source");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_creates_missing_output_directory() {
        let workspace = super::super::agent_workspace_path("export_create_dir");
        let build_target = BuildTarget::from_cli(None).expect("default target should resolve");
        let source_path = build_target.compiled_binary_path(
            &workspace,
            "export_create_dir",
            CargoCompileProfile::Release,
        );
        let source_parent = source_path
            .parent()
            .expect("compiled binary should have a parent directory");
        fs::create_dir_all(source_parent).expect("source parent should be creatable");
        fs::write(&source_path, b"binary").expect("source binary should be writable");

        let output_dir = temp_test_dir().join("nested").join("bin");
        export_binary(
            "export_create_dir",
            false,
            &build_target,
            Some(Path::new(&output_dir)),
            None,
        )
        .expect("export should create missing output directory");

        let exported = build_target.exported_binary_path(&output_dir, "export_create_dir");
        assert!(exported.exists());

        let _ = fs::remove_dir_all(output_dir.parent().unwrap().parent().unwrap());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn export_rejects_output_dir_that_is_a_file() {
        let workspace = super::super::agent_workspace_path("export_bad_output_dir");
        let build_target = BuildTarget::from_cli(None).expect("default target should resolve");
        let source_path = build_target.compiled_binary_path(
            &workspace,
            "export_bad_output_dir",
            CargoCompileProfile::Release,
        );
        let source_parent = source_path
            .parent()
            .expect("compiled binary should have a parent directory");
        fs::create_dir_all(source_parent).expect("source parent should be creatable");
        fs::write(&source_path, b"binary").expect("source binary should be writable");

        let file_path = temp_test_dir().join("not-a-dir");
        fs::write(&file_path, b"file").expect("file path should be writable");

        let err = export_binary(
            "export_bad_output_dir",
            false,
            &build_target,
            Some(Path::new(&file_path)),
            None,
        )
        .expect_err("file output path should fail");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(err.to_string().contains("--output-dir"));

        let _ = fs::remove_file(file_path);
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn export_selects_release_and_never_falls_back_to_debug() {
        let workspace = temp_test_dir();
        let target = BuildTarget::from_cli(None).unwrap();
        let dev =
            target.compiled_binary_path(&workspace, "profile_probe", CargoCompileProfile::Dev);
        let release =
            target.compiled_binary_path(&workspace, "profile_probe", CargoCompileProfile::Release);
        for (path, bytes) in [
            (&dev, "wrong debug artifact"),
            (&release, "release artifact"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let destination = workspace.join("out");
        let exported = export_binary(
            "profile_probe",
            false,
            &target,
            Some(&destination),
            Some(&workspace),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&exported).unwrap(), "release artifact");
        fs::remove_file(&release).unwrap();
        let error = export_binary(
            "profile_probe",
            true,
            &target,
            Some(&destination),
            Some(&workspace),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(fs::read_to_string(exported).unwrap(), "release artifact");
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn export_can_read_from_shared_build_root() {
        let workspace = super::super::agent_workspace_path("export_shared_build_root");
        let build_target = BuildTarget::from_cli(None).expect("default target should resolve");
        let shared_root = temp_test_dir().join("template-root");
        let source_path = build_target.compiled_binary_path(
            &shared_root,
            "export_shared_build_root",
            CargoCompileProfile::Release,
        );
        let source_parent = source_path
            .parent()
            .expect("compiled binary should have a parent directory");
        fs::create_dir_all(source_parent).expect("shared source parent should be creatable");
        fs::write(&source_path, b"binary").expect("shared source binary should be writable");
        fs::create_dir_all(&workspace).expect("workspace should be creatable");

        let output_dir = temp_test_dir().join("bin");
        export_binary(
            "export_shared_build_root",
            false,
            &build_target,
            Some(Path::new(&output_dir)),
            Some(Path::new(&shared_root)),
        )
        .expect("export should use the shared build root");

        let exported = build_target.exported_binary_path(&output_dir, "export_shared_build_root");
        assert!(exported.exists());

        let _ = fs::remove_dir_all(output_dir.parent().unwrap());
        let _ = fs::remove_dir_all(shared_root.parent().unwrap());
        let _ = fs::remove_dir_all(workspace);
    }
}
