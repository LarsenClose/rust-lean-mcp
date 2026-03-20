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
// Stale olean detection
// ---------------------------------------------------------------------------

/// Check whether a single source file's olean is stale (source newer than olean).
///
/// Returns `true` when the source file exists and either:
/// - No corresponding `.olean` file exists in the build directory, or
/// - The source file's mtime is newer than the olean's mtime.
///
/// Returns `false` when the source file doesn't exist or mtimes can't be read.
fn is_olean_stale(project_path: &Path, build_lib: &Path, rel_lean: &str) -> bool {
    let source = project_path.join(rel_lean);
    let olean = build_lib.join(Path::new(rel_lean).with_extension("olean"));

    let source_mtime = match source.metadata().and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false, // Can't read source => assume not stale
    };

    if !olean.exists() {
        return true; // Never built
    }

    let olean_mtime = match olean.metadata().and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };

    source_mtime > olean_mtime
}

/// Check if a file or its direct imports have stale oleans.
///
/// Reads the source file, parses its `import` lines, and checks whether the
/// target file and each imported module have an olean that is older than the
/// corresponding source. Returns a list of relative paths whose oleans are
/// stale.
///
/// This is an O(imports) check: it reads one file and performs a handful of
/// mtime comparisons, making it safe to call on every diagnostic request.
pub fn check_stale_imports(project_path: &Path, file_path: &str) -> Vec<String> {
    let abs_path = project_path.join(file_path);
    let content = match std::fs::read_to_string(&abs_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let build_lib = project_path.join(".lake").join("build").join("lib");
    if !build_lib.exists() {
        return vec![]; // No build dir = never built, can't check
    }

    let mut stale = Vec::new();

    // Check the target file itself
    if is_olean_stale(project_path, &build_lib, file_path) {
        stale.push(file_path.to_string());
    }

    // Parse import lines and check each
    for line in content.lines() {
        let trimmed = line.trim();

        // Skip comments and blank lines within the import section
        if trimmed.is_empty() || trimmed.starts_with("--") || trimmed.starts_with("/-") {
            continue;
        }

        // Check for import statements
        if !trimmed.starts_with("import ") && !trimmed.starts_with("public import ") {
            // Past the import section (Lean imports must appear at the top)
            break;
        }

        // Extract module name: "import Foo.Bar" or "public import Foo.Bar"
        let module = trimmed
            .trim_start_matches("public ")
            .trim_start_matches("import ")
            .trim();

        // Convert module name to file path: Foo.Bar -> Foo/Bar.lean
        let rel_path = module.replace('.', "/") + ".lean";

        if is_olean_stale(project_path, &build_lib, &rel_path) {
            stale.push(rel_path);
        }
    }

    stale
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

    // ---- is_olean_stale ----

    /// Helper: create a fake project with .lake/build/lib/ directory.
    fn setup_project_with_build(dir: &TempDir) {
        let build_lib = dir.path().join(".lake").join("build").join("lib");
        fs::create_dir_all(&build_lib).unwrap();
    }

    #[test]
    fn is_olean_stale_returns_false_when_source_missing() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        assert!(!is_olean_stale(
            dir.path(),
            &dir.path().join(".lake/build/lib"),
            "NonExistent.lean"
        ));
    }

    #[test]
    fn is_olean_stale_returns_true_when_olean_missing() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        // Create source file but no olean
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();
        assert!(is_olean_stale(
            dir.path(),
            &dir.path().join(".lake/build/lib"),
            "Main.lean"
        ));
    }

    #[test]
    fn is_olean_stale_returns_true_when_source_newer() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Create olean first
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();
        // Small delay to ensure mtime difference
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Then create source (newer)
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();

        assert!(is_olean_stale(dir.path(), &build_lib, "Main.lean"));
    }

    #[test]
    fn is_olean_stale_returns_false_when_olean_newer() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Create source first
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();
        // Small delay to ensure mtime difference
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Then create olean (newer)
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();

        assert!(!is_olean_stale(dir.path(), &build_lib, "Main.lean"));
    }

    // ---- check_stale_imports ----

    #[test]
    fn check_stale_imports_no_build_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        // No .lake/build/lib => can't check
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();
        let result = check_stale_imports(dir.path(), "Main.lean");
        assert!(result.is_empty());
    }

    #[test]
    fn check_stale_imports_detects_stale_source() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Create olean first
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Source is newer
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();

        let result = check_stale_imports(dir.path(), "Main.lean");
        assert_eq!(result, vec!["Main.lean"]);
    }

    #[test]
    fn check_stale_imports_clean_when_olean_newer() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Source first
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Olean newer
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();

        let result = check_stale_imports(dir.path(), "Main.lean");
        assert!(result.is_empty());
    }

    #[test]
    fn check_stale_imports_detects_stale_import() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Target file is up to date
        fs::write(
            dir.path().join("Main.lean"),
            "import Foo.Bar\n\ndef main := 0",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();

        // Imported module's source dir
        let foo_dir = dir.path().join("Foo");
        fs::create_dir_all(&foo_dir).unwrap();
        let foo_build_dir = build_lib.join("Foo");
        fs::create_dir_all(&foo_build_dir).unwrap();

        // Import's olean is stale (olean first, then source)
        fs::write(foo_build_dir.join("Bar.olean"), "olean").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(foo_dir.join("Bar.lean"), "-- bar").unwrap();

        let result = check_stale_imports(dir.path(), "Main.lean");
        assert_eq!(result, vec!["Foo/Bar.lean"]);
    }

    #[test]
    fn check_stale_imports_missing_olean_is_stale() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);

        // Source exists but no olean at all
        fs::write(dir.path().join("Main.lean"), "-- main").unwrap();

        let result = check_stale_imports(dir.path(), "Main.lean");
        assert_eq!(result, vec!["Main.lean"]);
    }

    #[test]
    fn check_stale_imports_missing_source_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        // File doesn't exist => can't read, returns empty
        let result = check_stale_imports(dir.path(), "NonExistent.lean");
        assert!(result.is_empty());
    }

    #[test]
    fn check_stale_imports_parses_public_import() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Target file with public import
        fs::write(
            dir.path().join("Main.lean"),
            "public import Foo.Bar\n\ndef main := 0",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();

        // Import source with no olean
        let foo_dir = dir.path().join("Foo");
        fs::create_dir_all(&foo_dir).unwrap();
        fs::write(foo_dir.join("Bar.lean"), "-- bar").unwrap();

        let result = check_stale_imports(dir.path(), "Main.lean");
        assert_eq!(result, vec!["Foo/Bar.lean"]);
    }

    #[test]
    fn check_stale_imports_stops_at_non_import_line() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        // Non-import code appears before "import" deeper in the file
        let content = "def foo := 0\nimport Fake.Module\n";
        fs::write(dir.path().join("Main.lean"), content).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();

        // If Fake.Module source existed, it should NOT be checked
        // because we stopped parsing imports at "def foo"
        let result = check_stale_imports(dir.path(), "Main.lean");
        assert!(result.is_empty());
    }

    #[test]
    fn check_stale_imports_skips_comments_in_import_section() {
        let dir = TempDir::new().unwrap();
        setup_project_with_build(&dir);
        let build_lib = dir.path().join(".lake/build/lib");

        let content = "-- A comment\nimport Foo.Bar\n\ndef main := 0";
        fs::write(dir.path().join("Main.lean"), content).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(build_lib.join("Main.olean"), "olean").unwrap();

        // Foo.Bar source with no olean => stale
        let foo_dir = dir.path().join("Foo");
        fs::create_dir_all(&foo_dir).unwrap();
        fs::write(foo_dir.join("Bar.lean"), "-- bar").unwrap();

        let result = check_stale_imports(dir.path(), "Main.lean");
        assert_eq!(result, vec!["Foo/Bar.lean"]);
    }
}
