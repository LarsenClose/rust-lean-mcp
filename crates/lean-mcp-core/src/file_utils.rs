//! File-path resolution and content reading utilities.
//!
//! Ported from the Python `file_utils.py` and `client_utils.py` modules.
//! All functions are synchronous since they only perform local filesystem
//! operations.

use std::path::{Path, PathBuf};

/// Convert a file path to be relative to the Lean project root.
///
/// Handles four cases in order:
/// 1. Absolute path under `project_path` — strip the project prefix.
/// 2. Relative path that exists when joined to `project_path`.
/// 3. Relative to CWD and inside the project tree.
/// 4. Returns `None` if the file cannot be resolved.
pub fn get_relative_file_path(project_path: &Path, file_path: &str) -> Option<String> {
    let fp = Path::new(file_path);

    // Case 1: absolute path that lives under the project root.
    if fp.is_absolute() {
        if fp.exists() {
            if let Ok(rel) = fp.strip_prefix(project_path) {
                return Some(rel.to_string_lossy().into_owned());
            }
        }
        return None;
    }

    // Case 2: relative path that resolves inside the project.
    let joined = project_path.join(fp);
    if joined.exists() {
        return Some(file_path.to_owned());
    }

    // Case 3: relative to CWD but inside the project tree.
    if let Ok(cwd) = std::env::current_dir() {
        let abs = cwd.join(fp);
        if abs.exists() {
            if let Ok(rel) = abs.strip_prefix(project_path) {
                return Some(rel.to_string_lossy().into_owned());
            }
        }
    }

    // Case 4: cannot resolve.
    None
}

/// Read file contents with encoding fallback.
///
/// Tries strict UTF-8 first; on failure falls back to lossy (replace
/// invalid bytes with U+FFFD).  This mirrors the Python pattern of
/// `utf-8 -> latin-1 -> replace`.
pub fn get_file_contents(abs_path: &str) -> std::io::Result<String> {
    let bytes = std::fs::read(abs_path)?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => Ok(String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

/// Walk up from `file_path` looking for a `lean-toolchain` file.
///
/// Returns the first ancestor directory that contains `lean-toolchain`,
/// or `None` if no such directory exists.
pub fn infer_project_path(file_path: &str) -> Option<PathBuf> {
    let mut current = Path::new(file_path).to_path_buf();

    // If the path points to a file, start from its parent directory.
    if current.is_file() {
        current = current.parent()?.to_path_buf();
    }

    loop {
        if valid_lean_project_path(&current) {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Detect a Lean project root by walking up from `start`.
///
/// Checks for `lakefile.lean`, `lakefile.toml`, and `lean-toolchain` at each level.
/// Returns the first directory containing any of these markers.
pub fn detect_lean_project(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join("lakefile.lean").exists()
            || dir.join("lakefile.toml").exists()
            || dir.join("lean-toolchain").is_file()
        {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Check whether `path` contains a `lean-toolchain` file.
pub fn valid_lean_project_path(path: &Path) -> bool {
    path.join("lean-toolchain").is_file()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ---- valid_lean_project_path ----

    #[test]
    fn valid_lean_project_path_true_when_toolchain_exists() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("lean-toolchain"), "leanprover/lean4:v4.3.0").unwrap();
        assert!(valid_lean_project_path(dir.path()));
    }

    #[test]
    fn valid_lean_project_path_false_when_missing() {
        let dir = TempDir::new().unwrap();
        assert!(!valid_lean_project_path(dir.path()));
    }

    #[test]
    fn valid_lean_project_path_false_when_toolchain_is_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("lean-toolchain")).unwrap();
        assert!(!valid_lean_project_path(dir.path()));
    }

    // ---- infer_project_path ----

    #[test]
    fn infer_project_path_finds_ancestor() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("a").join("b");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("lean-toolchain"), "v4").unwrap();

        let file = sub.join("Foo.lean");
        fs::write(&file, "-- empty").unwrap();

        let result = infer_project_path(file.to_str().unwrap());
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn infer_project_path_none_when_no_toolchain() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("Foo.lean");
        fs::write(&file, "-- empty").unwrap();
        assert!(infer_project_path(file.to_str().unwrap()).is_none());
    }

    #[test]
    fn infer_project_path_returns_immediate_dir() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("lean-toolchain"), "v4").unwrap();
        let file = dir.path().join("Main.lean");
        fs::write(&file, "-- main").unwrap();

        let result = infer_project_path(file.to_str().unwrap()).unwrap();
        assert_eq!(
            result.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    // ---- get_file_contents ----

    #[test]
    fn get_file_contents_reads_utf8() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("hello.lean");
        fs::write(&f, "-- hello world\ndef foo := 42\n").unwrap();
        let contents = get_file_contents(f.to_str().unwrap()).unwrap();
        assert!(contents.contains("hello world"));
        assert!(contents.contains("def foo := 42"));
    }

    #[test]
    fn get_file_contents_handles_invalid_utf8() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("bad.bin");
        // Write bytes that are invalid UTF-8: 0xFF 0xFE
        fs::write(&f, [0xFF, 0xFE, b'a', b'b']).unwrap();
        let contents = get_file_contents(f.to_str().unwrap()).unwrap();
        // Should not panic; lossy replacement inserts U+FFFD
        assert!(contents.contains("ab"));
    }

    #[test]
    fn get_file_contents_error_on_missing_file() {
        let result = get_file_contents("/nonexistent/path/to/file.lean");
        assert!(result.is_err());
    }

    // ---- get_relative_file_path ----

    #[test]
    fn relative_path_absolute_under_project() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("Foo.lean");
        fs::write(&file, "-- foo").unwrap();

        let result = get_relative_file_path(dir.path(), file.to_str().unwrap());
        assert_eq!(result, Some("src/Foo.lean".to_string()));
    }

    #[test]
    fn relative_path_already_relative() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("Main.lean");
        fs::write(&file, "-- main").unwrap();

        let result = get_relative_file_path(dir.path(), "Main.lean");
        assert_eq!(result, Some("Main.lean".to_string()));
    }

    #[test]
    fn relative_path_absolute_outside_project() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let file = dir2.path().join("Other.lean");
        fs::write(&file, "-- other").unwrap();

        let result = get_relative_file_path(dir1.path(), file.to_str().unwrap());
        assert!(result.is_none());
    }

    #[test]
    fn relative_path_nonexistent_relative() {
        let dir = TempDir::new().unwrap();
        let result = get_relative_file_path(dir.path(), "doesnotexist.lean");
        assert!(result.is_none());
    }

    #[test]
    fn relative_path_nonexistent_absolute() {
        let dir = TempDir::new().unwrap();
        let result = get_relative_file_path(dir.path(), "/absolutely/nonexistent/file.lean");
        assert!(result.is_none());
    }

    // ---- detect_lean_project ----

    #[test]
    fn detect_lean_project_finds_dir_with_lakefile_lean() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
        let result = detect_lean_project(dir.path());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn detect_lean_project_finds_dir_with_lakefile_toml() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("lakefile.toml"), "[package]").unwrap();
        let result = detect_lean_project(dir.path());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn detect_lean_project_finds_dir_with_lean_toolchain() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("lean-toolchain"), "leanprover/lean4:v4.3.0").unwrap();
        let result = detect_lean_project(dir.path());
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn detect_lean_project_walks_up_from_nested_subdir() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
        let result = detect_lean_project(&sub);
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn detect_lean_project_starts_from_file_path() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
        let file = sub.join("Foo.lean");
        fs::write(&file, "-- code").unwrap();
        let result = detect_lean_project(&file);
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn detect_lean_project_returns_none_when_no_markers() {
        let dir = TempDir::new().unwrap();
        // Create a deeply nested structure with no markers
        let sub = dir.path().join("x").join("y");
        fs::create_dir_all(&sub).unwrap();
        // Note: This test relies on the temp dir not being inside an actual Lean project.
        // We test by verifying that starting from within the temp dir, we don't find
        // the test repo's markers (the temp dir is typically under /tmp which is outside
        // any Lean project).
        let result = detect_lean_project(&sub);
        // The temp dir is under /tmp, so there should be no Lean project above it.
        assert!(result.is_none());
    }

    #[test]
    fn detect_lean_project_prefers_nearest_ancestor() {
        let outer = TempDir::new().unwrap();
        let inner_dir = outer.path().join("inner");
        let deep = inner_dir.join("sub");
        fs::create_dir_all(&deep).unwrap();
        // Both outer and inner have markers
        fs::write(outer.path().join("lakefile.lean"), "-- outer").unwrap();
        fs::write(inner_dir.join("lakefile.lean"), "-- inner").unwrap();
        let result = detect_lean_project(&deep);
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            inner_dir.canonicalize().unwrap()
        );
    }
}
