//! Real filesystem directory-as-module support — the thin layer
//! `modules.rs`'s own doc comment promises: turns a project directory
//! tree into the same `&[(module_path, source)]` shape `typecheck_and_
//! run_modules` already accepts, so all the actual resolution logic
//! lives in exactly one place. A `.plum` file's module path is its
//! containing directory's path relative to the project root, dot-
//! joined (`""` for a file directly in the root, `"shapes"` for
//! `shapes/circle.plum`, `"net.http"` for `net/http/get.plum`) —
//! matching DESIGN.md's "a directory IS a module" rule exactly: there
//! is no per-file declaration of which module a file belongs to, only
//! its location in the tree.

use crate::typecheck_and_run_modules;
use plum_interp::Value;
use std::fs;
use std::path::Path;

/// Walks `root`, reads every `.plum` file, and runs the whole project
/// through `typecheck_and_run_modules`. See this module's doc comment
/// for how a file's path maps to its module path.
pub fn typecheck_and_run_project(root: &Path, fn_name: &str, args: Vec<Value>) -> Result<Value, String> {
    let files = collect_plum_files(root, root)?;
    let modules: Vec<(&str, &str)> = files.iter().map(|(path, src)| (path.as_str(), src.as_str())).collect();
    typecheck_and_run_modules(&modules, fn_name, args)
}

/// Returns `(module_path, source)` for every `.plum` file found under
/// `dir`, recursing into subdirectories. Sorted by file path — `fs::
/// read_dir`'s own order isn't guaranteed, and a deterministic order
/// keeps error messages (when a project has more than one problem)
/// reproducible from run to run, even though `TypeContext`'s own
/// two-phase resolution doesn't actually depend on it for correctness.
fn collect_plum_files(root: &Path, dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("failed to read directory {dir:?}: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("failed to read a directory entry in {dir:?}: {e}"))?;
    entries.sort_by_key(std::fs::DirEntry::path);

    let mut result = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            result.extend(collect_plum_files(root, &path)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("plum") {
            let source = fs::read_to_string(&path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
            result.push((module_path_for(root, &path)?, source));
        }
    }
    Ok(result)
}

fn module_path_for(root: &Path, file: &Path) -> Result<String, String> {
    let dir = file.parent().ok_or_else(|| format!("{file:?} has no parent directory"))?;
    let rel = dir
        .strip_prefix(root)
        .map_err(|e| format!("{file:?} is not inside project root {root:?}: {e}"))?;
    let segments: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(segments.join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal, dependency-free temp-directory helper — creates a
    /// UNIQUE directory under `std::env::temp_dir()` (process id +
    /// an incrementing counter, so parallel test threads never
    /// collide) and removes it on drop, so a panicking assertion still
    /// cleans up rather than leaking a directory per failed test run.
    struct TempProject {
        path: std::path::PathBuf,
    }

    impl TempProject {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("plumc-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).expect("failed to create temp project directory");
            TempProject { path }
        }

        fn write(&self, rel_path: &str, contents: &str) {
            let full = self.path.join(rel_path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("failed to create parent directory");
            }
            fs::write(&full, contents).expect("failed to write test file");
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_single_file_project_runs() {
        let project = TempProject::new();
        project.write("main.plum", "let main unused = 1 + 2");

        let result = typecheck_and_run_project(&project.path, "main", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(3)));
    }

    #[test]
    fn a_multi_directory_project_resolves_across_modules() {
        let project = TempProject::new();
        project.write("shapes/circle.plum", "pub struct Circle { radius: Float }");
        project.write(
            "shapes/area.plum",
            "pub let area (c: Circle): Float = c.radius * c.radius * 3.0",
        );
        project.write(
            "main.plum",
            r#"
            use shapes;
            let main unused = shapes.area(shapes.Circle { radius: 2.0 })
            "#,
        );

        let result = typecheck_and_run_project(&project.path, "main", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Float(12.0)));
    }

    #[test]
    fn multiple_files_in_the_same_directory_share_one_namespace() {
        // Go's rule: every file in a directory is the SAME module, no
        // per-file declaration needed — `circle.plum`'s `Circle` is
        // visible to `area.plum` with no `use` at all, since they're
        // both just "the shapes module."
        let project = TempProject::new();
        project.write("shapes/circle.plum", "pub struct Circle { radius: Float }");
        project.write("shapes/area.plum", "pub let area (c: Circle): Float = c.radius * c.radius");
        project.write(
            "main.plum",
            r#"
            use shapes;
            let main unused = shapes.area(shapes.Circle { radius: 3.0 })
            "#,
        );

        let result = typecheck_and_run_project(&project.path, "main", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Float(9.0)));
    }

    #[test]
    fn nested_subdirectories_become_nested_module_paths() {
        let project = TempProject::new();
        project.write("net/http/get.plum", "pub let get unused = 200");
        project.write(
            "main.plum",
            r#"
            use net.http;
            let main unused = net.http.get(0)
            "#,
        );

        let result = typecheck_and_run_project(&project.path, "main", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(200)));
    }
}
