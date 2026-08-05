//! Cross-module name resolution — DESIGN.md's "Module system" section.
//!
//! This is a PRE-PASS, not a change to `plum-types`/`plum-ir` — it
//! takes a set of `(module_path, source)` pairs (a real filesystem
//! directory-walker would build this same shape from a project's file
//! tree; there's no directory-walking code here yet, this is the
//! in-memory core), parses each into an `ast::Program`, and rewrites
//! every name reference into its FULLY QUALIFIED form (`"shapes.Circle"`
//! as a single string, standing in for the dotted path) before merging
//! everything into ONE flat `ast::Program` and handing it to
//! `crate::run_resolved_program` — the exact same function
//! `typecheck_and_run` already uses for an ordinary single-file
//! program. `plum-types`/`plum-ir`/`plum-interp` never learn a module
//! system exists at all: a qualified name like `"shapes.Circle"` is
//! just an ordinary string key to their existing flat `HashMap`s,
//! indistinguishable from any other identifier (Plum's own lexer can
//! never produce an identifier containing `.`, so there's no collision
//! risk between a real qualified name and an ordinary one).
//!
//! The ROOT module (path `""`) is never qualified — its declarations
//! keep their bare names, exactly as `typecheck_and_run` already
//! produces today. This is also a deliberate rule, not just a
//! backward-compatibility artifact: root-module declarations (and the
//! prelude, which is injected into the root module) are visible from
//! EVERY module with no `use` needed, the same "always available, no
//! import" spirit as the prelude already had.
//!
//! Known scope boundaries (v1, not accidental gaps):
//! - Enum VARIANT TAGS (`Some`, `Circle`, ...) are NOT qualified or
//!   `pub`-checked by this pass — they stay in the SAME flat, global,
//!   look-up-by-bare-tag-name-alone namespace `plum-types`/`plum-ir`
//!   already used before modules existed (see `TypeContext::variant`'s
//!   doc comment: "a variant tag is looked up by name alone," a
//!   pre-existing precedent this pass doesn't touch). Two modules
//!   declaring a variant with the same tag name collide — a real,
//!   narrow limitation, not something silently wrong.
//! - `pub` is enforced for structs, enums, top-level functions/globals,
//!   and extern functions — NOT for individual struct fields (`is_pub`
//!   exists per-field in the AST but nothing here or elsewhere reads
//!   it yet — a separate, smaller gap).
//! - A local variable/parameter binding always wins over a same-named
//!   module-level declaration (ordinary shadowing) — tracked via a
//!   real (if approximate) scope walk, not just "hope names don't
//!   collide."

use plum_interp::Value;
use plum_syntax::ast;
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;
use plum_syntax::span::Span;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Runs a multi-module program: parses every `(module_path, source)`
/// pair, resolves every cross-module reference (enforcing `pub`),
/// merges everything into one flat, fully-qualified `ast::Program`,
/// and runs it exactly like `typecheck_and_run` does for a single
/// file. `module_path` is dot-joined (`""` for the root/top-level
/// module, `"shapes"`, `"net.http"` for a nested one) — there is no
/// `mod` declaration in Plum source itself (see DESIGN.md: a directory
/// IS a module, automatically), so the caller supplies this the same
/// way a real directory-walker eventually will (each `.plum` file's
/// module path is its containing directory, relative to the project
/// root). Multiple entries may share the same `module_path`, standing
/// in for multiple files in one directory — Go's rule that every file
/// in a directory shares one namespace.
pub fn typecheck_and_run_modules(modules: &[(&str, &str)], fn_name: &str, args: Vec<Value>) -> Result<Value, String> {
    typecheck_and_run_modules_diag(modules, fn_name, args).map_err(|e| e.to_string())
}

/// The `CompileError`-preserving sibling of `typecheck_and_run_modules`
/// — used only by the CLI (via `project::typecheck_and_run_project_
/// diag`), which needs the real `Span` to render a `file:line:col` +
/// snippet. `typecheck_and_run_modules` itself flattens this via
/// `Display` at its own boundary, so its own (many, pre-existing)
/// callers/tests need no changes at all.
pub(crate) fn typecheck_and_run_modules_diag(
    modules: &[(&str, &str)],
    fn_name: &str,
    args: Vec<Value>,
) -> Result<Value, plum_syntax::error::CompileError> {
    let program = resolve_modules_diag(modules)?;
    crate::run_resolved_program_diag(program, fn_name, args)
}

/// The front half of `typecheck_and_run_modules`: parses every module,
/// resolves every cross-module reference (enforcing `pub`), and merges
/// everything into one flat, fully-qualified `ast::Program` — stopping
/// short of `run_resolved_program`. Split out so `plumc build` (via
/// `codegen_cli::compile_program_to_ir`) can drive the LLVM codegen
/// backend directly off the SAME resolved program the interpreter path
/// uses, without going through `Value`/`Interpreter` at all.
pub fn resolve_modules(modules: &[(&str, &str)]) -> Result<ast::Program, String> {
    resolve_modules_diag(modules).map_err(|e| e.to_string())
}

/// The `CompileError`-preserving sibling of `resolve_modules` — see
/// `typecheck_and_run_modules_diag`'s doc comment for why this split
/// exists. Used by `project::resolve_project_diag` (`plum build`'s own
/// front half) and `typecheck_and_run_modules_diag` above.
pub(crate) fn resolve_modules_diag(modules: &[(&str, &str)]) -> Result<ast::Program, plum_syntax::error::CompileError> {
    let mut modules_by_path: BTreeMap<String, Vec<ast::Item>> = BTreeMap::new();
    // Each module file gets its OWN non-overlapping `Span` range,
    // starting PAST every prelude fragment's own range (see `crate::
    // PRELUDE_TOTAL_LEN`'s own doc comment) and increasing by each
    // file's own byte length in turn — without this, two DIFFERENT
    // module files (or a module file and the prelude) could
    // coincidentally share byte offsets and silently collide in any
    // `HashMap<Span, _>` keyed purely by `Span` (e.g. `Infer::
    // generic_sites`). `ModuleSources::new` (see below) rebuilds this
    // EXACT SAME cumulative-base scheme independently, purely from
    // `modules` itself, to translate a resulting `CompileError`'s span
    // back into a real file — the two must stay in lockstep.
    let mut base = crate::PRELUDE_TOTAL_LEN;
    for (mpath, src) in modules {
        let tokens = Lexer::with_base_offset(src, base).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().map_err(|e: plum_syntax::error::CompileError| e.context(format!("parse error in module {mpath:?}")))?;
        modules_by_path.entry(mpath.to_string()).or_default().extend(program.items);
        base += src.len();
    }

    // The prelude belongs to the root module — prepended so `Option`/
    // `Result` are declared before anything else needs them (order
    // doesn't actually matter to resolution, just matching `with_
    // prelude`'s own existing convention).
    {
        let prelude_items = crate::with_prelude(ast::Program { items: Vec::new() }).items;
        let root_items = modules_by_path.entry(String::new()).or_default();
        let mut merged = prelude_items;
        merged.append(root_items);
        *root_items = merged;
    }

    let mut all_declared: HashMap<String, bool> = HashMap::new();
    for (mpath, items) in &modules_by_path {
        for item in items {
            for name in declared_names(item) {
                all_declared.insert(qualify(mpath, &name), item.is_pub);
            }
        }
    }
    let module_paths: HashSet<String> = modules_by_path.keys().cloned().collect();

    let mut resolved_items = Vec::new();
    for (mpath, items) in &modules_by_path {
        let use_info = collect_use_info(mpath, items, &module_paths, &all_declared)?;
        let resolver = Resolver {
            module_path: mpath,
            all_declared: &all_declared,
            used_modules: &use_info.used_modules,
            bare_imports: &use_info.bare_imports,
        };
        for item in items {
            let mut item = item.clone();
            resolver.resolve_item(&mut item)?;
            resolved_items.push(item);
        }
    }

    Ok(ast::Program { items: resolved_items })
}

fn qualify(module_path: &str, name: &str) -> String {
    if module_path.is_empty() {
        name.to_string()
    } else {
        format!("{module_path}.{name}")
    }
}

/// Every name `item` declares at module scope — a `Let` or `Struct` or
/// `Enum` declares exactly one; an `Extern` block declares one PER
/// contained function (all sharing the block's own `is_pub`, since
/// individual extern fns have no visibility marker of their own); a
/// `Use` declares none.
fn declared_names(item: &ast::Item) -> Vec<String> {
    match &item.kind {
        ast::ItemKind::Let(def) => vec![def.name.clone()],
        ast::ItemKind::Struct(decl) => vec![decl.name.clone()],
        ast::ItemKind::Enum(decl) => vec![decl.name.clone()],
        ast::ItemKind::Extern(block) => block.fns.iter().map(|f| f.name.clone()).collect(),
        ast::ItemKind::Use(_) => vec![],
    }
}

struct ModuleUseInfo {
    // Module paths brought into scope via `use shapes;` — enables
    // `shapes.X` qualified references anywhere in this module.
    used_modules: HashSet<String>,
    // The `use shapes.Circle;` escape hatch — brings ONE name into
    // scope unqualified. Existence and `pub` are both checked right
    // here, at the `use` site, rather than deferred to wherever
    // `Circle` might later be referenced (or never referenced at all,
    // silently hiding a typo).
    bare_imports: HashMap<String, String>,
}

fn collect_use_info(
    module_path: &str,
    items: &[ast::Item],
    module_paths: &HashSet<String>,
    all_declared: &HashMap<String, bool>,
) -> Result<ModuleUseInfo, plum_syntax::error::CompileError> {
    let mut used_modules = HashSet::new();
    let mut bare_imports = HashMap::new();
    for item in items {
        let ast::ItemKind::Use(decl) = &item.kind else {
            continue;
        };
        let joined = decl.path.join(".");
        if module_paths.contains(&joined) {
            used_modules.insert(joined);
            continue;
        }
        if decl.path.len() >= 2 {
            let prefix = decl.path[..decl.path.len() - 1].join(".");
            let name = decl.path.last().expect("checked len >= 2 above");
            if module_paths.contains(&prefix) {
                let qualified = format!("{prefix}.{name}");
                match all_declared.get(&qualified) {
                    Some(&is_pub) => {
                        if prefix != module_path && !is_pub {
                            return Err(plum_syntax::error::CompileError::new(
                                decl.span,
                                format!("use {joined:?}: {qualified:?} is private to module {prefix:?}"),
                            ));
                        }
                        bare_imports.insert(name.clone(), qualified);
                        continue;
                    }
                    None => {
                        return Err(plum_syntax::error::CompileError::new(
                            decl.span,
                            format!("use {joined:?}: module {prefix:?} has no item named {name:?}"),
                        ));
                    }
                }
            }
        }
        return Err(plum_syntax::error::CompileError::new(decl.span, format!("use {joined:?}: no such module")));
    }
    Ok(ModuleUseInfo { used_modules, bare_imports })
}

struct Resolver<'a> {
    module_path: &'a str,
    all_declared: &'a HashMap<String, bool>,
    used_modules: &'a HashSet<String>,
    bare_imports: &'a HashMap<String, String>,
}

impl Resolver<'_> {
    /// The core of this whole pass: given a dotted-path reference
    /// (`["Circle"]`, `["shapes", "Circle"]`, `["net", "http", "Get"]`),
    /// decides what it refers to. `Ok(None)` means "not a recognized
    /// module-level declaration reference" — the caller leaves the
    /// node UNCHANGED, which correctly covers a local variable, a
    /// generic type parameter, a builtin primitive type name
    /// (`Int`/`Float`/...), an enum variant tag (deliberately outside
    /// this system — see this module's own doc comment), or a genuine
    /// typo that `plum-types`' own "unbound"/"unknown type" errors will
    /// catch downstream, unchanged from today's behavior. `Ok(Some(q))`
    /// is the fully-qualified replacement. `Err` is a hard failure —
    /// only raised when the reference UNAMBIGUOUSLY names a used
    /// module (so silently falling through would produce a confusing
    /// generic error instead of a clear one).
    fn resolve_segments(&self, segments: &[String], locals: &HashSet<String>, span: Span) -> Result<Option<String>, plum_syntax::error::CompileError> {
        if segments.len() == 1 {
            let name = &segments[0];
            if locals.contains(name) {
                return Ok(None);
            }
            let same_module = qualify(self.module_path, name);
            if self.all_declared.contains_key(&same_module) {
                return Ok(Some(same_module));
            }
            if let Some(q) = self.bare_imports.get(name) {
                return Ok(Some(q.clone()));
            }
            // Root-module fallback — see this module's doc comment on
            // why root declarations (and the prelude, injected into
            // the root) are visible everywhere with no `use`.
            if self.all_declared.contains_key(name) {
                return Ok(Some(name.clone()));
            }
            return Ok(None);
        }
        let prefix = segments[..segments.len() - 1].join(".");
        let item_name = &segments[segments.len() - 1];
        if self.used_modules.contains(&prefix) || prefix == self.module_path {
            let candidate = format!("{prefix}.{item_name}");
            return match self.all_declared.get(&candidate) {
                Some(&is_pub) => {
                    if prefix != self.module_path && !is_pub {
                        Err(plum_syntax::error::CompileError::new(span, format!("{candidate:?} is private to module {prefix:?}")))
                    } else {
                        Ok(Some(candidate))
                    }
                }
                None => Err(plum_syntax::error::CompileError::new(span, format!("module {prefix:?} has no item named {item_name:?}"))),
            };
        }
        Ok(None)
    }

    fn resolve_type(&self, ty: &mut ast::Type, locals: &HashSet<String>) -> Result<(), plum_syntax::error::CompileError> {
        match ty {
            ast::Type::Path(segments, span) => {
                if let Some(q) = self.resolve_segments(segments, locals, *span)? {
                    *segments = vec![q];
                }
                Ok(())
            }
            ast::Type::Generic { base, args, span } => {
                if let Some(q) = self.resolve_segments(base, locals, *span)? {
                    *base = vec![q];
                }
                for a in args.iter_mut() {
                    self.resolve_type(a, locals)?;
                }
                Ok(())
            }
            ast::Type::Function { params, ret, .. } => {
                for p in params.iter_mut() {
                    self.resolve_type(p, locals)?;
                }
                self.resolve_type(ret, locals)
            }
        }
    }

    fn resolve_pattern(&self, pattern: &mut ast::Pattern, locals: &HashSet<String>) -> Result<(), plum_syntax::error::CompileError> {
        match pattern {
            ast::Pattern::Variant { path, args, span } => {
                if let Some(q) = self.resolve_segments(path, locals, *span)? {
                    *path = vec![q];
                }
                for a in args.iter_mut() {
                    self.resolve_pattern(a, locals)?;
                }
                Ok(())
            }
            ast::Pattern::Struct { path, fields, span, .. } => {
                if let Some(q) = self.resolve_segments(path, locals, *span)? {
                    *path = vec![q];
                }
                for f in fields.iter_mut() {
                    self.resolve_pattern(&mut f.pattern, locals)?;
                }
                Ok(())
            }
            ast::Pattern::Tuple(pats, _) | ast::Pattern::Or(pats, _) => {
                for p in pats.iter_mut() {
                    self.resolve_pattern(p, locals)?;
                }
                Ok(())
            }
            ast::Pattern::Ident(..)
            | ast::Pattern::Wildcard(_)
            | ast::Pattern::Int(..)
            | ast::Pattern::Float(..)
            | ast::Pattern::Str(..)
            | ast::Pattern::Bool(..) => Ok(()),
        }
    }

    fn resolve_expr(&self, expr: &mut ast::Expr, locals: &mut HashSet<String>) -> Result<(), plum_syntax::error::CompileError> {
        // A `Field`/`Ident` node is checked for a WHOLE dotted-path
        // shape FIRST, before any structural recursion — `shapes.
        // Circle` must be resolved as ONE reference, not `shapes`
        // (nonsense as a standalone expression) field-accessed by
        // `.Circle`. If the whole chain doesn't resolve to a
        // recognized declaration, it falls through to ordinary
        // handling below (a real field access like `p.x`, or a bare
        // local/unbound name left for existing machinery to handle).
        if matches!(expr, ast::Expr::Ident(..) | ast::Expr::Field { .. }) {
            if let Some(segments) = expr_path_segments(expr) {
                let span = expr.span();
                if let Some(q) = self.resolve_segments(&segments, locals, span)? {
                    *expr = ast::Expr::Ident(q, span);
                    return Ok(());
                }
            }
        }
        match expr {
            ast::Expr::Ident(..) | ast::Expr::Int(..) | ast::Expr::Float(..) | ast::Expr::Str(..) | ast::Expr::Bool(..) => {
                Ok(())
            }
            ast::Expr::Field { base, .. } => self.resolve_expr(base, locals),
            ast::Expr::Tuple(elems, _) | ast::Expr::ArrayLiteral(elems, _) => {
                for e in elems.iter_mut() {
                    self.resolve_expr(e, locals)?;
                }
                Ok(())
            }
            ast::Expr::Unary { expr: inner, .. } => self.resolve_expr(inner, locals),
            ast::Expr::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs, locals)?;
                self.resolve_expr(rhs, locals)
            }
            ast::Expr::Call { callee, args, .. } => {
                self.resolve_expr(callee, locals)?;
                for a in args.iter_mut() {
                    self.resolve_expr(a, locals)?;
                }
                Ok(())
            }
            ast::Expr::GenericInst { callee, args, .. } => {
                self.resolve_expr(callee, locals)?;
                for t in args.iter_mut() {
                    self.resolve_type(t, locals)?;
                }
                Ok(())
            }
            ast::Expr::Index { base, index, .. } => {
                self.resolve_expr(base, locals)?;
                self.resolve_expr(index, locals)
            }
            ast::Expr::Block(block, _) => self.resolve_block(block, &mut locals.clone()),
            ast::Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                self.resolve_expr(cond, locals)?;
                self.resolve_block(then_branch, &mut locals.clone())?;
                if let Some(e) = else_branch {
                    self.resolve_expr(e, locals)?;
                }
                Ok(())
            }
            ast::Expr::Match { scrutinee, arms, .. } => {
                self.resolve_expr(scrutinee, locals)?;
                for arm in arms.iter_mut() {
                    let mut arm_locals = locals.clone();
                    self.resolve_pattern(&mut arm.pattern, &arm_locals)?;
                    pattern_bindings(&arm.pattern, &mut arm_locals);
                    if let Some(g) = &mut arm.guard {
                        self.resolve_expr(g, &mut arm_locals)?;
                    }
                    self.resolve_expr(&mut arm.body, &mut arm_locals)?;
                }
                Ok(())
            }
            ast::Expr::For { pattern, iter, body, .. } => {
                self.resolve_expr(iter, locals)?;
                let mut body_locals = locals.clone();
                self.resolve_pattern(pattern, &body_locals)?;
                pattern_bindings(pattern, &mut body_locals);
                self.resolve_block(body, &mut body_locals)
            }
            ast::Expr::Closure { params, body, .. } => {
                let mut inner = locals.clone();
                for p in params.iter_mut() {
                    if let Some(ty) = &mut p.ty {
                        self.resolve_type(ty, locals)?;
                    }
                    inner.insert(p.name.clone());
                }
                self.resolve_expr(body, &mut inner)
            }
            ast::Expr::Unsafe(block, _) => self.resolve_block(block, &mut locals.clone()),
            ast::Expr::Spawn(block, _) => self.resolve_block(block, &mut locals.clone()),
            ast::Expr::StructLiteral { path, fields, spread, span } => {
                if let Some(q) = self.resolve_segments(path, locals, *span)? {
                    *path = vec![q];
                }
                for f in fields.iter_mut() {
                    self.resolve_expr(&mut f.value, locals)?;
                }
                if let Some(s) = spread {
                    self.resolve_expr(s, locals)?;
                }
                Ok(())
            }
            ast::Expr::Select { arms, .. } => {
                for arm in arms.iter_mut() {
                    self.resolve_expr(&mut arm.expr, locals)?;
                    let mut arm_locals = locals.clone();
                    self.resolve_pattern(&mut arm.pattern, &arm_locals)?;
                    pattern_bindings(&arm.pattern, &mut arm_locals);
                    self.resolve_expr(&mut arm.body, &mut arm_locals)?;
                }
                Ok(())
            }
        }
    }

    fn resolve_block(&self, block: &mut ast::Block, locals: &mut HashSet<String>) -> Result<(), plum_syntax::error::CompileError> {
        for stmt in block.stmts.iter_mut() {
            match stmt {
                ast::Stmt::Let { pattern, ty, value, .. } => {
                    self.resolve_expr(value, locals)?;
                    if let Some(t) = ty {
                        self.resolve_type(t, locals)?;
                    }
                    self.resolve_pattern(pattern, locals)?;
                    pattern_bindings(pattern, locals);
                }
                ast::Stmt::Assign { value, .. } => {
                    self.resolve_expr(value, locals)?;
                }
                ast::Stmt::Expr(e) => {
                    self.resolve_expr(e, locals)?;
                }
            }
        }
        if let Some(tail) = &mut block.tail {
            self.resolve_expr(tail, locals)?;
        }
        Ok(())
    }

    /// Rewrites both HALVES of an item: its own declared name(s) —
    /// `all_declared`/every REFERENCE elsewhere was already built
    /// against the QUALIFIED form (`resolve_segments`'s whole job) —
    /// so the declaration itself must be renamed to match, or nothing
    /// downstream (`TypeContext`, `Interpreter::functions`, ...) will
    /// ever find it under the name references now expect. Root-module
    /// items (`module_path` empty) keep their bare name — `qualify`
    /// is a no-op there, preserving `typecheck_and_run`-identical
    /// behavior for single-file programs.
    fn resolve_item(&self, item: &mut ast::Item) -> Result<(), plum_syntax::error::CompileError> {
        match &mut item.kind {
            ast::ItemKind::Let(def) => {
                def.name = qualify(self.module_path, &def.name);
                let mut locals = HashSet::new();
                param_bindings(&def.params, &mut locals);
                for p in def.params.iter_mut() {
                    if let ast::ParamKind::Pattern(_, Some(ty)) = &mut p.kind {
                        self.resolve_type(ty, &locals)?;
                    }
                }
                if let Some(t) = &mut def.ret_ty {
                    self.resolve_type(t, &locals)?;
                }
                self.resolve_expr(&mut def.body, &mut locals)
            }
            ast::ItemKind::Struct(decl) => {
                decl.name = qualify(self.module_path, &decl.name);
                let locals = HashSet::new();
                for f in decl.fields.iter_mut() {
                    self.resolve_type(&mut f.ty, &locals)?;
                }
                Ok(())
            }
            ast::ItemKind::Enum(decl) => {
                decl.name = qualify(self.module_path, &decl.name);
                let locals = HashSet::new();
                for v in decl.variants.iter_mut() {
                    for t in v.payload.iter_mut() {
                        self.resolve_type(t, &locals)?;
                    }
                }
                Ok(())
            }
            ast::ItemKind::Extern(block) => {
                let locals = HashSet::new();
                for f in block.fns.iter_mut() {
                    f.name = qualify(self.module_path, &f.name);
                    for p in f.params.iter_mut() {
                        self.resolve_type(&mut p.ty, &locals)?;
                    }
                    if let Some(t) = &mut f.ret_ty {
                        self.resolve_type(t, &locals)?;
                    }
                }
                Ok(())
            }
            // Fully consumed already (`collect_use_info`) — nothing
            // left to rewrite, and `Use` items themselves aren't
            // meaningful to `plum-types`/`plum-ir` (see their own
            // `_ => {}` handling), so leaving it in the merged program
            // unchanged is harmless.
            ast::ItemKind::Use(_) => Ok(()),
        }
    }
}

/// Recognizes a dotted-path-SHAPED expression (`shapes.Circle`,
/// `net.http.Get`) — an `Ident` alone, or a `Field` whose `base` is
/// ITSELF path-shaped, recursively. Anything else (a real computed
/// expression as `Field`'s base, e.g. `f().x`) returns `None`,
/// correctly excluding it from module-qualified-reference resolution.
fn expr_path_segments(expr: &ast::Expr) -> Option<Vec<String>> {
    match expr {
        ast::Expr::Ident(name, _) => Some(vec![name.clone()]),
        ast::Expr::Field { base, name, .. } => {
            let mut segments = expr_path_segments(base)?;
            segments.push(name.clone());
            Some(segments)
        }
        _ => None,
    }
}

/// Every name a pattern BINDS (as opposed to references) — `Pattern::
/// Ident` is always a fresh binding, never a reference (capitalization
/// already disambiguates a binding from a tag pattern at PARSE time —
/// see `ast::Pattern::Variant`'s own doc comment), so this is the
/// inverse of `resolve_pattern`: that function rewrites REFERENCES
/// (`Variant`/`Struct` paths), this collects BINDINGS.
fn pattern_bindings(pattern: &ast::Pattern, out: &mut HashSet<String>) {
    match pattern {
        ast::Pattern::Ident(name, _) => {
            out.insert(name.clone());
        }
        ast::Pattern::Tuple(pats, _) | ast::Pattern::Or(pats, _) => {
            for p in pats {
                pattern_bindings(p, out);
            }
        }
        ast::Pattern::Variant { args, .. } => {
            for p in args {
                pattern_bindings(p, out);
            }
        }
        ast::Pattern::Struct { fields, .. } => {
            for f in fields {
                pattern_bindings(&f.pattern, out);
            }
        }
        ast::Pattern::Int(..) | ast::Pattern::Float(..) | ast::Pattern::Str(..) | ast::Pattern::Bool(..) | ast::Pattern::Wildcard(_) => {}
    }
}

fn param_bindings(params: &[ast::Param], out: &mut HashSet<String>) {
    for p in params {
        match &p.kind {
            ast::ParamKind::Ident(name) => {
                out.insert(name.clone());
            }
            ast::ParamKind::Pattern(pattern, _) => pattern_bindings(pattern, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calling_a_pub_function_in_another_module_via_qualified_access() {
        let shapes = r#"
            pub struct Circle { radius: Float }
            pub let area (c: Circle): Float = c.radius * c.radius * 3.14159
        "#;
        let root = r#"
            use shapes;
            let go unused = shapes.area(shapes.Circle { radius: 2.0 })
        "#;
        let result = typecheck_and_run_modules(&[("shapes", shapes), ("", root)], "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Float(2.0 * 2.0 * 3.14159)));
    }

    #[test]
    fn bare_import_of_a_single_item_via_use() {
        let shapes = r#"
            pub struct Circle { radius: Float }
        "#;
        let root = r#"
            use shapes.Circle;
            let go unused = Circle { radius: 3.0 }.radius
        "#;
        let result = typecheck_and_run_modules(&[("shapes", shapes), ("", root)], "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Float(3.0)));
    }

    #[test]
    fn accessing_a_private_item_from_another_module_is_rejected() {
        let shapes = r#"
            struct Circle { radius: Float }
            let area (c: Circle): Float = c.radius * c.radius
        "#;
        let root = r#"
            use shapes;
            let go unused = shapes.area(shapes.Circle { radius: 2.0 })
        "#;
        let err = typecheck_and_run_modules(&[("shapes", shapes), ("", root)], "go", vec![Value::Int(0)])
            .expect_err("expected private-item access to be rejected");
        assert!(err.contains("private"), "unexpected error: {err}");
    }

    #[test]
    fn use_referencing_a_nonexistent_module_is_rejected() {
        let root = r#"
            use nope;
            let go unused = 1
        "#;
        let err = typecheck_and_run_modules(&[("", root)], "go", vec![Value::Int(0)])
            .expect_err("expected an unknown module to be rejected");
        assert!(err.contains("no such module"), "unexpected error: {err}");
    }

    #[test]
    fn use_referencing_a_nonexistent_item_in_a_real_module_is_rejected() {
        let shapes = "pub struct Circle { radius: Float }";
        let root = r#"
            use shapes.Square;
            let go unused = 1
        "#;
        let err = typecheck_and_run_modules(&[("shapes", shapes), ("", root)], "go", vec![Value::Int(0)])
            .expect_err("expected an unknown item to be rejected");
        assert!(err.contains("no item named"), "unexpected error: {err}");
    }

    #[test]
    fn nested_module_paths_resolve_correctly() {
        let net_http = r#"
            pub let get unused = 200
        "#;
        let root = r#"
            use net.http;
            let go unused = net.http.get(0)
        "#;
        let result =
            typecheck_and_run_modules(&[("net.http", net_http), ("", root)], "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(200)));
    }

    #[test]
    fn root_module_declarations_are_visible_from_any_module_without_use() {
        let helpers_module = r#"
            let double n = n * 2
            let go unused = double(21)
        "#;
        // `fn_name` must name a function in the ROOT module (module
        // path "") — `go` here lives in "helpers", so it's registered
        // as "helpers.go" after qualification; passing that qualified
        // name directly is how a caller reaches a non-root entry
        // point. `double` is declared in the SAME module here, so this
        // alone doesn't prove the root-visibility rule — the real
        // proof is the next assertion, where `double` lives in the
        // ROOT module and `helpers` calls it with no `use` at all.
        let result =
            typecheck_and_run_modules(&[("helpers", helpers_module)], "helpers.go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(42)));

        let root = r#"
            use helpers;
            let double n = n * 2
            let go unused = helpers.call_double(0)
        "#;
        let helpers_calls_root = "pub let call_double unused = double(21)";
        let result = typecheck_and_run_modules(
            &[("", root), ("helpers", helpers_calls_root)],
            "go",
            vec![Value::Int(0)],
        );
        assert_eq!(result, Ok(Value::Int(42)));
    }

    #[test]
    fn a_local_variable_shadows_a_same_named_module_level_declaration() {
        let root = r#"
            let double n = n * 2
            let go x = { let double = x; double }
        "#;
        let result = typecheck_and_run_modules(&[("", root)], "go", vec![Value::Int(5)]);
        assert_eq!(result, Ok(Value::Int(5)));
    }

    #[test]
    fn enum_variant_tags_stay_globally_visible_across_modules_without_use() {
        // A deliberate v1 scope boundary (see this module's own doc
        // comment): variant TAGS aren't module-qualified or `pub`-
        // checked at all, so `Circle`/`Square` construction and
        // matching works from any module with no `use` — unlike a
        // struct or function, which DO need `use` for qualified
        // access.
        let shapes = "pub enum Shape { Circle(Float), Square(Float) }";
        let root = r#"
            let area s = match s {
                Circle(r) => r * r * 3.0,
                Square(side) => side * side,
            }
            let go unused = area(Circle(2.0))
        "#;
        let result = typecheck_and_run_modules(&[("shapes", shapes), ("", root)], "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Float(12.0)));
    }

    #[test]
    fn two_modules_can_declare_a_struct_with_the_same_name_without_colliding() {
        let a = "pub struct Point { value: Int }";
        let b = "pub struct Point { value: Int }";
        let root = r#"
            use a;
            use b;
            let go unused = a.Point { value: 1 }.value + b.Point { value: 2 }.value
        "#;
        let result = typecheck_and_run_modules(&[("a", a), ("b", b), ("", root)], "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Int(3)));
    }

    #[test]
    fn cross_module_struct_type_annotation_resolves() {
        let shapes = "pub struct Circle { radius: Float }";
        let root = r#"
            use shapes;
            let radius_of (c: shapes.Circle): Float = c.radius
            let go unused = radius_of(shapes.Circle { radius: 9.0 })
        "#;
        let result = typecheck_and_run_modules(&[("shapes", shapes), ("", root)], "go", vec![Value::Int(0)]);
        assert_eq!(result, Ok(Value::Float(9.0)));
    }

    #[test]
    fn single_file_typecheck_and_run_is_unaffected_by_the_module_pass() {
        // `typecheck_and_run` never goes through `typecheck_and_run_
        // modules` at all (see lib.rs) — this just confirms the
        // refactor extracting `run_resolved_program` didn't change its
        // behavior.
        let result = crate::typecheck_and_run("let double n = n * 2", "double", vec![Value::Int(21)]);
        assert_eq!(result, Ok(Value::Int(42)));
    }
}
