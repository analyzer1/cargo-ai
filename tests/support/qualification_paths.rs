//! Portable, link-free paths inside an anonymous source checkout.
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn relative_file(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, ':' | '\\'))
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn confined_file(root: &Path, relative: &str, max_bytes: u64) -> Result<PathBuf, &'static str> {
    if !relative_file(relative) {
        return Err("expected a portable relative file path");
    }
    let root = root
        .canonicalize()
        .map_err(|_| "missing package checkout")?;
    let mut path = root.clone();
    for component in relative.split('/') {
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|_| "missing package input")?;
        if metadata.file_type().is_symlink() {
            return Err("package inputs cannot traverse links");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("package inputs cannot traverse reparse points");
            }
        }
    }
    let canonical = path.canonicalize().map_err(|_| "invalid package input")?;
    let metadata = fs::metadata(&canonical).map_err(|_| "invalid package input")?;
    if !canonical.starts_with(root) || !metadata.is_file() || metadata.len() > max_bytes {
        return Err("package input exceeds its path/type/size boundary");
    }
    Ok(canonical)
}
