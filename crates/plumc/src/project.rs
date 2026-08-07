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

use crate::modules::{resolve_modules, resolve_modules_diag, typecheck_and_run_modules_with_process_args_diag};
use crate::typecheck_and_run_modules;
use plum_interp::Value;
use plum_syntax::ast;
use std::fs;
use std::path::{Path, PathBuf};

/// Walks `root`, reads every `.plum` file, and runs the whole project
/// through `typecheck_and_run_modules`. See this module's doc comment
/// for how a file's path maps to its module path.
pub fn typecheck_and_run_project(root: &Path, fn_name: &str, args: Vec<Value>) -> Result<Value, String> {
    let files = collect_plum_files(root, root)?;
    let modules: Vec<(&str, &str)> = files.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
    typecheck_and_run_modules(&modules, fn_name, args)
}

/// The `CompileError`-preserving sibling of `typecheck_and_run_project`
/// — used only by the CLI (`plum <project>`'s interpreter path in
/// `main.rs`), which needs the real `Span` (via `ModuleSources::
/// render`) to print a `file:line:col` + snippet instead of a bare
/// message. `typecheck_and_run_project` itself flattens this via
/// `Display` at its own boundary, so its own (pre-existing) tests need
/// no changes at all.
pub fn typecheck_and_run_project_diag(root: &Path, fn_name: &str, args: Vec<Value>) -> Result<Value, plum_syntax::error::CompileError> {
    typecheck_and_run_project_with_process_args_diag(root, fn_name, args, Vec::new())
}

/// The `args()`-aware sibling of `typecheck_and_run_project_diag` —
/// used only by `plum run <project-dir> -- <arg>...`'s own CLI path in
/// `main.rs`. See `run_resolved_program_with_process_args_diag`'s own
/// doc comment for why this exists as a separate function rather than
/// a new parameter on the existing one.
pub fn typecheck_and_run_project_with_process_args_diag(
    root: &Path,
    fn_name: &str,
    args: Vec<Value>,
    process_args: Vec<String>,
) -> Result<Value, plum_syntax::error::CompileError> {
    let files = collect_plum_files(root, root).map_err(plum_syntax::error::CompileError::spanless)?;
    let modules: Vec<(&str, &str)> = files.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
    typecheck_and_run_modules_with_process_args_diag(&modules, fn_name, args, process_args)
}

/// The front half of `typecheck_and_run_project`: directory walk +
/// `resolve_modules`, stopping short of running anything — the
/// `Program`-producing counterpart `plumc build` needs (see `modules::
/// resolve_modules`'s own doc comment for why this split exists).
pub fn resolve_project(root: &Path) -> Result<ast::Program, String> {
    let files = collect_plum_files(root, root)?;
    let modules: Vec<(&str, &str)> = files.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
    resolve_modules(&modules)
}

/// The `CompileError`-preserving sibling of `resolve_project` — used
/// only by `plum build`'s own error path in `main.rs`. See
/// `typecheck_and_run_project_diag`'s doc comment for why this split
/// exists.
pub fn resolve_project_diag(root: &Path) -> Result<ast::Program, plum_syntax::error::CompileError> {
    let files = collect_plum_files(root, root).map_err(plum_syntax::error::CompileError::spanless)?;
    let modules: Vec<(&str, &str)> = files.iter().map(|(_path, mpath, src)| (mpath.as_str(), src.as_str())).collect();
    resolve_modules_diag(&modules)
}

/// Public wrapper around `collect_plum_files`, for `main.rs` to build a
/// `ModuleSources` from the SAME file list `typecheck_and_run_project_
/// diag`/`resolve_project_diag` resolve against — needed since neither
/// of those functions returns the file table on its own error path.
pub fn collect_project_files(root: &Path) -> Result<Vec<(String, String)>, String> {
    Ok(collect_plum_files(root, root)?.into_iter().map(|(_path, mpath, src)| (mpath, src)).collect())
}

/// The `plum lsp` sibling of `collect_project_files` — same walk, but
/// keeps each file's own absolute path alongside its module path and
/// source. `plum lsp` needs this to overlay an editor buffer's UNSAVED
/// content onto the on-disk file it came from: `collect_project_files`
/// alone can't do that because, once two files share a directory (and
/// so share a module path — see this module's own doc comment), the
/// module path isn't enough to tell them apart.
pub fn collect_project_files_with_paths(root: &Path) -> Result<Vec<(PathBuf, String, String)>, String> {
    collect_plum_files(root, root)
}

/// Returns `(absolute_path, module_path, source)` for every `.plum`
/// file found under `dir`, recursing into subdirectories. Sorted by
/// file path — `fs::read_dir`'s own order isn't guaranteed, and a
/// deterministic order keeps error messages (when a project has more
/// than one problem) reproducible from run to run, even though
/// `TypeContext`'s own two-phase resolution doesn't actually depend on
/// it for correctness.
fn collect_plum_files(root: &Path, dir: &Path) -> Result<Vec<(PathBuf, String, String)>, String> {
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
            let mpath = module_path_for(root, &path)?;
            result.push((path, mpath, source));
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
    use crate::test_util::TempProject;

    #[test]
    fn a_single_file_project_runs() {
        let project = TempProject::new();
        project.write("main.plum", "let main unused = 1 + 2");

        let result = typecheck_and_run_project(&project.path, "main", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(3)));
    }

    #[test]
    fn process_args_reach_args_only_when_explicitly_threaded_through() {
        // `typecheck_and_run_project` (and so `typecheck_and_run_project
        // _diag`) always sees an EMPTY `args()` — proving the "existing
        // callers stay untouched" half of `run_resolved_program_with_
        // process_args_diag`'s own doc comment.
        let project = TempProject::new();
        project.write("main.plum", "let main unused = args(()).len()");
        let result = typecheck_and_run_project(&project.path, "main", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(0)));

        // The `_with_process_args` sibling actually threads them through.
        let result = typecheck_and_run_project_with_process_args_diag(
            &project.path,
            "main",
            vec![Value::Int(0)],
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        );
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
