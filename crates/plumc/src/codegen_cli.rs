//! Compiles a Plum program to a real native binary via `plum-codegen`
//! (LLVM IR text) + `clang` (compile + link) — an ADDITIONAL backend
//! alongside the tree-walking interpreter (`run_resolved_program`),
//! not a replacement. See DESIGN.md's "Implementation plan" section:
//! the interpreter validated the memory model first; this is the
//! LLVM backend that was always the intended next step, scoped for
//! v1 to scalars + control flow + guaranteed tail calls (see
//! `plum_codegen::emit_program`'s own doc comment for the exact
//! supported subset).

use crate::with_prelude;
use plum_codegen::{CgType, FnSig};
use plum_ir::fbip::optimize_program;
use plum_ir::lower::{lower_program, LoweringContext};
use plum_syntax::ast;
use plum_syntax::lexer::Lexer;
use plum_syntax::parser::Parser;
use plum_types::context::TypeContext;
use plum_types::infer::Infer;
use plum_types::types::Type as PlumType;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// A concrete argument value for calling the compiled entry point —
/// deliberately separate from `plum_interp::Value` (which can hold a
/// `HeapRef`/`Closure`/etc. that codegen's v1 scalar-only scope has no
/// way to represent), matching this backend's own narrower CgType set.
#[derive(Debug, Clone, Copy)]
pub enum CgValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
}

impl CgValue {
    fn cg_type(self) -> CgType {
        match self {
            CgValue::Int(_) => CgType::Int,
            CgValue::Float(_) => CgType::Float,
            CgValue::Bool(_) => CgType::Bool,
            CgValue::Unit => CgType::Unit,
        }
    }
}

fn plum_type_to_cg_type(ty: &PlumType) -> Result<CgType, String> {
    match ty {
        PlumType::Int => Ok(CgType::Int),
        PlumType::Float => Ok(CgType::Float),
        PlumType::Bool => Ok(CgType::Bool),
        PlumType::Unit => Ok(CgType::Unit),
        // A struct/enum reference — GENERIC instantiations (`args`
        // non-empty, e.g. `Option[Int]`) are included too, now that
        // `plum_ir::monomorphize` resolves each one to its own mangled,
        // fully concrete `tag_fields` entry: at the LLVM level a heap
        // value is opaque either way (a plain `ptr` — see `CgType::Heap`'s
        // own doc comment in plum-codegen), so this conversion doesn't
        // need to know or care WHICH concrete instantiation a given
        // struct/enum-typed signature position holds. If the program
        // never actually reaches a construction of that specific
        // instantiation, `monomorphize::plan` simply never produces a
        // `tag_fields` entry for it — caught, if it matters, by
        // `plum_codegen::emit_program`'s own "unknown tag" error at the
        // point something actually tries to construct/match it, not
        // here.
        PlumType::Str => Ok(CgType::Str),
        // `.as_cstr()`'s own result type — see `CgType::CStr`'s doc
        // comment for why it's a genuinely distinct representation from
        // `Str`. `plum_types::context::extern_ast_type_to_type`'s own
        // doc comment confirms `Type::CStr` is ONLY ever produced by
        // `.as_cstr()` or an extern signature's own `CStr` annotation —
        // never an ordinary struct/enum field or ordinary function
        // parameter's declared type (`ast_type_to_type`, the resolver
        // EVERY other type-annotation position uses, never produces it)
        // — so this arm is reachable only for a LOCAL variable/function
        // return whose type happens to be `CStr` (e.g. `let f(s) =
        // s.as_cstr()`), never for `tag_fields`.
        PlumType::CStr => Ok(CgType::CStr),
        // `Array[T]` recurses into `T` — this is ALSO a genuine bug fix
        // over the previous chunk: before this arm existed, `Array[T]`
        // fell through to the `Struct(..)` wildcard arm just above,
        // silently mapping to plain `CgType::Heap` (indistinguishable
        // from any ordinary struct at the LLVM level), which would only
        // fail LATER with a confusing "unknown tag: 0Array" error the
        // first time codegen actually tried to construct/index one —
        // rather than this clean, specific `Array[T]`-shaped `CgType`
        // from the start. Every OTHER `Struct`/`Enum` reference is
        // still handled by the wildcard arm below.
        PlumType::Struct(name, args) if name == "Array" && args.len() == 1 => {
            Ok(CgType::Array(Box::new(plum_type_to_cg_type(&args[0])?)))
        }
        // `Task[T]` — the SAME "builtin pseudo-generic `Type::Struct`"
        // mechanism `Array[T]` already uses (see `plum_types::infer`'s
        // own doc comments on `Type::Struct("Task", ..)`), mirrored
        // here identically: without this arm, `Task[T]` would fall
        // through to the `Struct(..)` wildcard arm below and collapse
        // to plain `CgType::Heap` — indistinguishable from an ordinary
        // struct at the LLVM level, which would be actively WRONG for
        // a task handle (a `Heap` cell's first word is a refcount;
        // `CgType::Task`'s cell's first word is a `joined` FLAG — see
        // `CgType::Task`'s own doc comment in plum-codegen's lib.rs).
        PlumType::Struct(name, args) if name == "Task" && args.len() == 1 => {
            Ok(CgType::Task(Box::new(plum_type_to_cg_type(&args[0])?)))
        }
        // `Sender[T]`/`Receiver[T]` — the SAME "builtin pseudo-generic
        // `Type::Struct`" mechanism `Task[T]` immediately above already
        // uses, mirrored identically: without these two arms, either
        // would fall through to the `Struct(..)` wildcard arm below and
        // collapse to plain `CgType::Heap`, which would be actively
        // WRONG (a `Heap` cell's first word is a refcount; a channel
        // handle's "cell" is the shared queue struct itself, which has
        // no refcount word anywhere in its layout at all — see
        // `CgType::Sender`/`Receiver`'s own doc comment in plum-codegen).
        PlumType::Struct(name, args) if name == "Sender" && args.len() == 1 => {
            Ok(CgType::Sender(Box::new(plum_type_to_cg_type(&args[0])?)))
        }
        PlumType::Struct(name, args) if name == "Receiver" && args.len() == 1 => {
            Ok(CgType::Receiver(Box::new(plum_type_to_cg_type(&args[0])?)))
        }
        PlumType::Struct(..) | PlumType::Enum(..) => Ok(CgType::Heap),
        // A closure/function-typed signature position (a higher-order
        // function's parameter, most commonly) — `CgType::Closure`
        // deliberately carries param/return types (unlike `Heap`) since
        // an indirect CALL through a closure value needs to know them
        // to annotate the call correctly; see `CgType::Closure`'s own
        // doc comment.
        PlumType::Function(params, ret) => Ok(CgType::Closure(
            params.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?,
            Box::new(plum_type_to_cg_type(ret)?),
        )),
        other => Err(format!(
            "codegen only supports Int/Float/Bool/Unit/Str/Array[T]/Task[T] or a non-generic struct/enum, found a \
             signature involving {other:?}"
        )),
    }
}

/// Every non-generic struct/enum-variant declared in `program`,
/// resolved to its fields' `CgType`s via `type_ctx` — see `plum_codegen::
/// TagFields`'s doc comment for how this is used. A struct/enum with
/// type parameters of its own, or any field whose type resolves
/// through `plum_type_to_cg_type` as unsupported (including a field
/// that's itself a GENERIC instantiation, like `Option[Int]`), is
/// simply OMITTED here rather than failing this whole derivation —
/// `plum_codegen::emit_program` reports a clear "unknown tag" error
/// only if the program's REACHABLE code actually tries to construct or
/// match that specific type, rather than failing the whole compile
/// over an unrelated, unused generic declaration.
/// Every non-generic enum variant's declared payload types, resolved
/// via `type_ctx` — the ONLY thing `plum_ir::lower::LoweringContext::
/// variant_payload_types` needs (see that field's own doc comment): a
/// bare non-zero-arity variant reference used as a VALUE (`let f =
/// Circle; f(1.0)`) eta-expands into a synthetic closure whose params
/// need real types, just like an ordinary closure literal does. A
/// GENERIC variant is simply omitted (same "omit rather than fail the
/// whole derivation" precedent as `derive_tag_fields`) — a bare
/// reference to a generic variant constructor is out of this chunk's
/// scope (closures inside/around still-generic instantiation sites
/// aren't threaded through `monomorphize` this chunk either).
fn derive_variant_payload_types(program: &ast::Program, type_ctx: &TypeContext) -> HashMap<String, Vec<PlumType>> {
    let mut out = HashMap::new();
    for item in &program.items {
        if let ast::ItemKind::Enum(decl) = &item.kind {
            if !decl.generics.is_empty() {
                continue;
            }
            for variant in &decl.variants {
                if let Some((_, payload)) = type_ctx.variant(&variant.name) {
                    out.insert(variant.name.clone(), payload.to_vec());
                }
            }
        }
    }
    out
}

/// Collects every `channel[T]()` call site's `T` type ARGUMENT
/// (unresolved `ast::Type` — the caller resolves it via `plum_types::
/// infer::ast_type_to_type`), via a structural AST walk mirroring the
/// exact "GenericInst callee named `channel`, one type arg, zero value
/// args" shape check `plum_types::infer`/`plum_ir::lower` both already
/// use to recognize this call shape.
///
/// # The `channel[T]()` tuple-tagging resolution (read before touching this)
///
/// `channel[T]()` evaluates to a `(Sender[T], Receiver[T])` — an
/// ORDINARY tuple as far as `plum_types`/lowering are concerned, tagged
/// `"2Tuple"` by `lower.rs`'s own `tuple_tag(2)`, the exact SAME
/// synthetic tag literally every other 2-element tuple in the whole
/// language gets (confirmed by reading `lower.rs`'s tuple-pattern
/// lowering AND `plum_ir::monomorphize`'s `rewrite_expr`/`rewrite_
/// pattern` `Tuple` arms directly: neither ever renames/mangles a
/// tuple's tag per its element types — unlike a generic struct/enum,
/// which DOES get a distinct mangled tag per instantiation via
/// `monomorphize::mangle` — so "2Tuple" is genuinely one flat,
/// program-wide tag, not type-specialized at all). Plain (non-channel)
/// tuples aren't wired into codegen's `tag_fields` derivation ANYWHERE
/// else yet either (`plum_type_to_cg_type` has no `Type::Tuple` arm at
/// all — a tuple only ever reaches codegen as a fully-destructured
/// LOCAL value, never through a signature), so there is, today,
/// genuinely no OTHER 2-tuple shape in this whole backend that could
/// collide with a channel's.
///
/// The one LIVE collision risk this DOES leave open: two `channel[T]()`
/// calls with two DIFFERENT `T`s in the SAME program would both need
/// tag `"2Tuple"` to carry a DIFFERENT pair of field `CgType`s
/// simultaneously — `tag_fields` is a flat `HashMap<String, _>`, so
/// that's not representable. Giving each element type its OWN mangled
/// tag would need `channel[T]()`'s destructuring `Match` to also use a
/// T-specific tag at the IR level — but `ir::Expr::Channel` carries NO
/// type at all, by deliberate design (see its own doc comment), and the
/// tag lowering literally happens at the SAME fixed `tuple_tag(2)` call
/// site every other 2-tuple pattern uses, with zero visibility into
/// "this particular tuple came from `channel[T]()`". Resolving that
/// properly would mean threading a NEW span-keyed side channel through
/// `plum_types::Infer` and a matching special case into `lower.rs`'s
/// tuple-pattern lowering (mirroring `resolve_empty_array_elem_types`/
/// `EmptyArray`'s own precedent) — real, cross-crate work well beyond
/// this chunk's own file list. Rather than silently mis-tag a second
/// element type (a genuine memory-safety bug: `.recv()`'s `word_to_
/// value` conversion depends entirely on the Receiver's declared inner
/// `CgType` being correct), this is a documented, loudly-REJECTED
/// limitation instead — see `resolve_channel_elem_cg_type`'s own doc
/// comment for the actual guard.
fn find_channel_type_args(program: &ast::Program) -> Vec<ast::Type> {
    let mut out = Vec::new();
    for item in &program.items {
        if let ast::ItemKind::Let(def) = &item.kind {
            find_channel_type_args_expr(&def.body, &mut out);
        }
    }
    out
}

fn find_channel_type_args_expr(expr: &ast::Expr, out: &mut Vec<ast::Type>) {
    // The exact shape check `plum_types::infer`'s own `channel[T]()`
    // inference arm and `plum_ir::lower`'s own `Expr::Channel` lowering
    // arm both already use — kept in lockstep with those two
    // deliberately, not re-derived independently.
    if let ast::Expr::Call { callee, args, .. } = expr {
        if args.is_empty() {
            if let ast::Expr::GenericInst { callee: inner_callee, args: type_args, .. } = callee.as_ref() {
                if type_args.len() == 1 && matches!(inner_callee.as_ref(), ast::Expr::Ident(name, _) if name == "channel") {
                    out.push(type_args[0].clone());
                }
            }
        }
    }
    walk_channel_sub_exprs(expr, out);
}

/// A plain structural recursion into every sub-expression — mirrors
/// `check_no_closure_expr`'s own exhaustive-`ast::Expr`-variant walk
/// shape (this file's established precedent for "find every occurrence
/// of X anywhere in the AST" without needing type information).
fn walk_channel_sub_exprs(expr: &ast::Expr, out: &mut Vec<ast::Type>) {
    match expr {
        ast::Expr::Int(..) | ast::Expr::Float(..) | ast::Expr::Str(..) | ast::Expr::Bool(..) | ast::Expr::Ident(..) => {}
        ast::Expr::Tuple(elems, _) | ast::Expr::ArrayLiteral(elems, _) => {
            elems.iter().for_each(|e| find_channel_type_args_expr(e, out))
        }
        ast::Expr::Unary { expr, .. } => find_channel_type_args_expr(expr, out),
        ast::Expr::Binary { lhs, rhs, .. } => {
            find_channel_type_args_expr(lhs, out);
            find_channel_type_args_expr(rhs, out);
        }
        ast::Expr::Field { base, .. } => find_channel_type_args_expr(base, out),
        ast::Expr::Call { callee, args, .. } => {
            find_channel_type_args_expr(callee, out);
            args.iter().for_each(|a| find_channel_type_args_expr(a, out));
        }
        ast::Expr::GenericInst { callee, .. } => find_channel_type_args_expr(callee, out),
        ast::Expr::Index { base, index, .. } => {
            find_channel_type_args_expr(base, out);
            find_channel_type_args_expr(index, out);
        }
        ast::Expr::Block(block, _) => walk_channel_block(block, out),
        ast::Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            find_channel_type_args_expr(cond, out);
            walk_channel_block(then_branch, out);
            if let Some(e) = else_branch {
                find_channel_type_args_expr(e, out);
            }
        }
        ast::Expr::Match { scrutinee, arms, .. } => {
            find_channel_type_args_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    find_channel_type_args_expr(g, out);
                }
                find_channel_type_args_expr(&arm.body, out);
            }
        }
        ast::Expr::For { iter, body, .. } => {
            find_channel_type_args_expr(iter, out);
            walk_channel_block(body, out);
        }
        ast::Expr::Closure { body, .. } => find_channel_type_args_expr(body, out),
        ast::Expr::Unsafe(block, _) | ast::Expr::Spawn(block, _) => walk_channel_block(block, out),
        ast::Expr::StructLiteral { fields, spread, .. } => {
            for f in fields {
                find_channel_type_args_expr(&f.value, out);
            }
            if let Some(s) = spread {
                find_channel_type_args_expr(s, out);
            }
        }
        ast::Expr::Select { arms, .. } => {
            for arm in arms {
                find_channel_type_args_expr(&arm.expr, out);
                find_channel_type_args_expr(&arm.body, out);
            }
        }
    }
}

fn walk_channel_block(block: &ast::Block, out: &mut Vec<ast::Type>) {
    for stmt in &block.stmts {
        match stmt {
            ast::Stmt::Let { value, .. } => find_channel_type_args_expr(value, out),
            ast::Stmt::Assign { value, .. } => find_channel_type_args_expr(value, out),
            ast::Stmt::Expr(e) => find_channel_type_args_expr(e, out),
        }
    }
    if let Some(t) = &block.tail {
        find_channel_type_args_expr(t, out);
    }
}

/// Resolves every `channel[T]()` call site's `T` and registers
/// `tag_fields["2Tuple"]` for it — see `find_channel_type_args`'s own
/// doc comment for the full tuple-tagging story this implements
/// (single global element type per program, loudly rejected otherwise,
/// rather than silently mis-tagging a second one). A no-op (empty
/// `Ok(())`, `tag_fields` untouched) for a program that never uses
/// `channel[T]()` at all.
fn register_channel_tag(
    program: &ast::Program,
    type_ctx: &TypeContext,
    tag_fields: &mut plum_codegen::TagFields,
) -> Result<(), String> {
    let type_args = find_channel_type_args(program);
    if type_args.is_empty() {
        return Ok(());
    }
    let mut distinct: Vec<PlumType> = Vec::new();
    for t in &type_args {
        let resolved = plum_types::infer::ast_type_to_type(t, type_ctx, &[])
            .map_err(|e| format!("`channel[..]` type argument: {e}"))?;
        if !distinct.contains(&resolved) {
            distinct.push(resolved);
        }
    }
    if distinct.len() > 1 {
        return Err(format!(
            "codegen does not yet support more than one distinct `channel[T]()` element type in the same \
             program (found {distinct:?}) — tuple tagging isn't type-specialized per element type yet (every \
             2-element tuple, channels included, currently shares one flat \"2Tuple\" tag_fields entry; see \
             `find_channel_type_args`'s own doc comment in codegen_cli.rs for the full story and what real \
             follow-up work would need)"
        ));
    }
    let elem_cg = plum_type_to_cg_type(&distinct[0])?;
    tag_fields.insert(
        "2Tuple".to_string(),
        vec![CgType::Sender(Box::new(elem_cg.clone())), CgType::Receiver(Box::new(elem_cg))],
    );
    Ok(())
}

/// Also returns each STRUCT tag's field NAMES (in the same declared
/// order as the returned `TagFields`'s own `Vec<CgType>`) — enum
/// variants contribute nothing to that second map, since Plum variant
/// payloads are already positional at the language level (see
/// `plum_codegen::StructFieldNames`'s own doc comment). `type_ctx.
/// struct_fields` already returns `(String, Type)` pairs; earlier this
/// only kept the type half.
fn derive_tag_fields(program: &ast::Program, type_ctx: &TypeContext) -> (plum_codegen::TagFields, plum_codegen::StructFieldNames) {
    let mut tag_fields = plum_codegen::TagFields::new();
    let mut struct_field_names = plum_codegen::StructFieldNames::new();
    for item in &program.items {
        match &item.kind {
            ast::ItemKind::Struct(decl) if decl.generics.is_empty() => {
                if let Some(fields) = type_ctx.struct_fields(&decl.name) {
                    if let Ok(cg_fields) = fields.iter().map(|(_, ty)| plum_type_to_cg_type(ty)).collect::<Result<Vec<_>, _>>() {
                        tag_fields.insert(decl.name.clone(), cg_fields);
                        struct_field_names.insert(decl.name.clone(), fields.iter().map(|(n, _)| n.clone()).collect());
                    }
                }
            }
            ast::ItemKind::Enum(decl) if decl.generics.is_empty() => {
                for variant in &decl.variants {
                    if let Some((_, payload)) = type_ctx.variant(&variant.name) {
                        if let Ok(cg_fields) = payload.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>() {
                            tag_fields.insert(variant.name.clone(), cg_fields);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (tag_fields, struct_field_names)
}

/// A heap-shaped (struct/enum) OR array-shaped OR closure-shaped entry-
/// point RETURN isn't printable (this chunk's `ToString` only covers
/// Int/Float/Bool/Str — no positional-field/element rendering for a
/// compiled heap or array value) — real programs construct/consume
/// those INTERNALLY, only ever exposing a scalar or `Str` result at the
/// entry point itself; `Str` IS printable, via `emit_main`'s own `Str`
/// case, so it's excluded from this rejection. Shared by both
/// `compile_and_run`'s test harness and `plumc build`'s `run_build` —
/// a `main` returning one of these shapes is a clear BUILD-time error,
/// not a panic; extending printing to those shapes is real, separate
/// follow-up work (would need a `ToString`-style dispatcher for
/// compiled heap values, which doesn't exist anywhere yet).
pub fn reject_unprintable_return(entry_fn: &str, ret: CgType) -> Result<(), String> {
    if ret == CgType::Heap {
        return Err(format!(
            "codegen: {entry_fn:?} returns a heap-shaped value, which the compiled entry point can't print yet"
        ));
    }
    if matches!(ret, CgType::Array(_)) {
        return Err(format!(
            "codegen: {entry_fn:?} returns an array-shaped value, which the compiled entry point can't print yet"
        ));
    }
    if matches!(ret, CgType::Closure(..)) {
        return Err(format!(
            "codegen: {entry_fn:?} returns a closure-shaped value, which the compiled entry point can't print yet"
        ));
    }
    if matches!(ret, CgType::Task(_)) {
        return Err(format!(
            "codegen: {entry_fn:?} returns a task-shaped value, which the compiled entry point can't print yet \
             (call `.join()` on it inside the entry function itself instead)"
        ));
    }
    if matches!(ret, CgType::Sender(_) | CgType::Receiver(_)) {
        return Err(format!(
            "codegen: {entry_fn:?} returns a channel handle, which the compiled entry point can't print yet"
        ));
    }
    // A bare `CStr` return isn't printable via the SAME `emit_main` path
    // as `Str` — deliberately not just aliased onto `Str`'s own `%s`
    // print case: a `CStr` is a genuinely unowned pointer of unknown
    // provenance/lifetime (see `plum_codegen::CgType::CStr`'s own doc
    // comment), and a Plum entry point returning one directly (rather
    // than the far more realistic `.as_cstr()`-consuming extern-call
    // shape this chunk's real tests exercise) is out of scope — no
    // test in this codebase's own FFI story needs it.
    if matches!(ret, CgType::CStr) {
        return Err(format!(
            "codegen: {entry_fn:?} returns a bare CStr value, which the compiled entry point can't print yet"
        ));
    }
    Ok(())
}

/// Compiles `src` all the way to a native executable and runs it,
/// calling `entry_fn` with `args` and returning captured stdout.
///
/// Runs the SAME parse → prelude → type-check pipeline
/// `run_resolved_program` uses, up through `lower_program`/
/// `optimize_program` — the exact point DESIGN.md's own sequencing
/// note describes as where the interpreter and a future codegen
/// backend diverge ("the frontend and refcount-insertion pass should
/// barely change" when swapping from interpret to codegen). From
/// there: derive a `HashMap<String, FnSig>` from `Infer::
/// infer_program`'s own concrete monomorphic types (the ONLY place
/// real type information exists in this pipeline — `ir::Function`
/// itself carries none), hand the lowered `ir::Program` to
/// `plum_codegen::emit_program`, append a hand-written LLVM `main`
/// that calls `entry_fn` and prints its result, write the `.ll` to a
/// temp file, shell out to `clang` to compile+link it, then run the
/// resulting binary and capture its stdout.
pub fn compile_and_run(src: &str, entry_fn: &str, args: &[CgValue]) -> Result<String, String> {
    let (body_ir, signatures, resolved_entry, has_globals) = compile_to_ir(src, entry_fn)?;
    let sig = signatures
        .get(&resolved_entry)
        .ok_or_else(|| format!("codegen: no such function {entry_fn:?}"))?
        .clone();
    if sig.params.len() != args.len() {
        return Err(format!(
            "codegen: {entry_fn:?} expects {} argument(s), found {}",
            sig.params.len(),
            args.len()
        ));
    }
    for (arg, expected) in args.iter().zip(&sig.params) {
        if arg.cg_type() != *expected {
            return Err(format!(
                "codegen: argument type mismatch calling {entry_fn:?} — expected {expected:?}, found {:?}",
                arg.cg_type()
            ));
        }
    }
    reject_unprintable_return(entry_fn, sig.ret.clone())?;

    let main_ir = emit_main(&resolved_entry, sig.ret, args, has_globals);
    let full_ir = format!("{body_ir}\n{main_ir}");

    run_via_clang(&full_ir)
}

/// The shared front-half of `compile_and_run`: parse → prelude → type-
/// check → monomorphize → lower → codegen, stopping just short of
/// appending a `main` wrapper and shelling out to `clang`. Split out
/// (rather than inlined into `compile_and_run` alone) so tests can
/// inspect the raw generated LLVM IR TEXT directly — e.g. asserting two
/// distinct mangled tags/function definitions both appear — without
/// needing a real `clang` toolchain or process spawn just to check
/// static text shape. Returns the generated IR body, every function's
/// concrete `FnSig` (including every monomorphized instantiation, keyed
/// by its MANGLED name), and `entry_fn`'s own resolved (possibly
/// mangled) name.
fn compile_to_ir(src: &str, entry_fn: &str) -> Result<(String, HashMap<String, FnSig>, String, bool), String> {
    // Base-offset PAST every prelude fragment's span range — see
    // `crate::PRELUDE_TOTAL_LEN`'s own doc comment for why.
    let tokens = Lexer::with_base_offset(src, crate::PRELUDE_TOTAL_LEN).tokenize();
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program().map_err(|e| format!("parse error: {e}"))?;
    let program = with_prelude(program);
    compile_program_to_ir(&program, entry_fn)
}

/// The `ast::Program`-based core of `compile_to_ir` — everything AFTER
/// parsing and prelude injection (type-check -> movecheck -> monomorphize
/// -> lower -> optimize -> derive signatures -> `emit_program`). Split out
/// so callers that already HAVE a merged, prelude-injected `ast::Program`
/// (namely `plumc build`, via `resolve_project`/`resolve_modules` in
/// `project.rs`/`modules.rs`) can drive codegen directly, without
/// re-parsing source text or double-injecting the prelude (`resolve_
/// modules` already injects it once at the root module before merging —
/// see that module's own doc comment). `compile_to_ir` above is now just
/// a two-line parse+prelude shim in front of this.
pub fn compile_program_to_ir(program: &ast::Program, entry_fn: &str) -> Result<(String, HashMap<String, FnSig>, String, bool), String> {
    compile_program_to_ir_diag(program, entry_fn).map_err(|e| e.to_string())
}

/// The `CompileError`-preserving sibling of `compile_program_to_ir` —
/// used only by `plum build`'s own error path in `main.rs`, which needs
/// the real `Span` (via `ModuleSources::render`) for a `file:line:col` +
/// snippet. `compile_program_to_ir` itself flattens this via `Display`
/// at its own boundary, so its own (many, pre-existing) test callers
/// need no changes at all. Codegen-specific helpers further down this
/// function (`plum_type_to_cg_type`, `register_channel_tag`, `plum_ir::
/// monomorphize::plan`) stay plain `String`-returning — confirmed
/// genuinely spanless (`plum-codegen`/`monomorphize` don't carry `Span`
/// at all) — their `?` sites convert for free via `CompileError`'s
/// blanket `From<String>`.
pub fn compile_program_to_ir_diag(
    program: &ast::Program,
    entry_fn: &str,
) -> Result<(String, HashMap<String, FnSig>, String, bool), plum_syntax::error::CompileError> {
    let type_ctx = TypeContext::from_items(&program.items).map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;
    let (mut tag_fields, mut struct_field_names) = derive_tag_fields(program, &type_ctx);
    let variant_payload_types = derive_variant_payload_types(program, &type_ctx);
    let mut infer = Infer::with_context(type_ctx);
    let types = infer.infer_program(program).map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;

    let resolved_sites = infer.resolve_generic_sites().map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;
    let empty_array_elem_types = infer.resolve_empty_array_elem_types().map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;
    let closure_types = infer.resolve_closure_types().map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;

    plum_ir::movecheck::check_moves(program).map_err(|e: plum_syntax::error::CompileError| e.context("move error"))?;

    // `resolve_generic_sites` needs its own `TypeContext` too (the
    // first one was moved into `infer` above — see `Infer::with_context`)
    // — cheap to rebuild from the same, already-validated items rather
    // than threading a second owned copy through `Infer` itself.
    let type_ctx_for_mono =
        TypeContext::from_items(&program.items).map_err(|e: plum_syntax::error::CompileError| e.context("type error"))?;
    register_channel_tag(program, &type_ctx_for_mono, &mut tag_fields)?;
    let mono_plan = plum_ir::monomorphize::plan(
        program,
        &type_ctx_for_mono,
        &resolved_sites,
        infer.fn_generics(),
        &types,
        infer.field_owners(),
        infer.array_for_loops(),
        &closure_types,
        &empty_array_elem_types,
        &variant_payload_types,
    )
    .map_err(|e| format!("monomorphization error: {e}"))?;

    let lowering_ctx = LoweringContext::from_items(&program.items)
        .with_field_owners(infer.field_owners().clone())
        .with_array_for_loops(infer.array_for_loops().clone())
        .with_empty_array_elem_types(empty_array_elem_types)
        .with_closure_types(closure_types)
        .with_variant_payload_types(variant_payload_types);
    let mut ir_program = lower_program(program, &lowering_ctx).map_err(|e: plum_syntax::error::CompileError| e.context("lowering error"))?;
    // `mono_plan.functions` REPLACES `lower_program`'s own function list
    // wholesale — it already covers every function actually needed,
    // including ordinary (never-generic) ones re-lowered with mangled
    // tags/callee names wherever their body touches a generic
    // instantiation (see `monomorphize::MonoPlan::functions`'s doc
    // comment for why the plain `lower_program` output can't just be
    // spliced alongside it: an ordinary function's PLAIN-tagged body
    // would reference tags `tag_fields` never has an entry for).
    // `mono_plan.globals` REPLACES `lower_program`'s own globals list the
    // same way, and for the same reason — a global's initializer can
    // call a still-generic function, and needs that call site rewritten
    // to reference the concrete, mangled instantiation `mono_plan`
    // already built (see `monomorphize::MonoPlan::globals`'s doc
    // comment). `ir_program.externs` stays untouched — an `extern`
    // declaration has no body, so generics can't reach into it.
    ir_program.functions = mono_plan.functions;
    ir_program.globals = mono_plan.globals;
    let ir_program = optimize_program(ir_program);

    for (mangled, field_types) in &mono_plan.tag_fields {
        let cg_fields = field_types.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?;
        tag_fields.insert(mangled.clone(), cg_fields);
    }
    for (mangled, names) in &mono_plan.struct_field_names {
        struct_field_names.insert(mangled.clone(), names.clone());
    }

    // Every top-level FUNCTION's signature — a global's `types` entry is
    // filtered out here (rather than accidentally treated as a
    // function's) because it's just its VALUE's type directly, not a
    // `Type::Function` — the `let PlumType::Function(..) = ty else {
    // ... }` below would otherwise report a confusing internal-error
    // panic-shaped message for every ordinary global. See the dedicated
    // global-type derivation loop just below this one instead. A GENERIC
    // function's own (unmangled) name is never in `function_names`
    // (only its mangled instantiations are — see `MonoPlan::functions`),
    // so this loop naturally skips it; its mangled entries come from
    // `mono_plan.signatures` in the loop right after instead, since
    // `types[name]` for a generic function is a nonsensically
    // unresolved, var-templated signature, not a concrete one.
    let function_names: std::collections::HashSet<&str> =
        ir_program.functions.iter().map(|f| f.name.as_str()).collect();
    let mut signatures = HashMap::new();
    for (name, ty) in &types {
        if !function_names.contains(name.as_str()) {
            continue;
        }
        let PlumType::Function(params, ret) = ty else {
            return Err(plum_syntax::error::CompileError::spanless(format!(
                "codegen: internal error — function {name:?} has a non-function type {ty:?}"
            )));
        };
        let cg_params = params.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?;
        let cg_ret = plum_type_to_cg_type(ret)?;
        signatures.insert(name.clone(), FnSig { params: cg_params, ret: cg_ret });
    }
    for (mangled, (params, ret)) in &mono_plan.signatures {
        let cg_params = params.iter().map(plum_type_to_cg_type).collect::<Result<Vec<_>, _>>()?;
        let cg_ret = plum_type_to_cg_type(ret)?;
        signatures.insert(mangled.clone(), FnSig { params: cg_params, ret: cg_ret });
    }

    // Every top-level GLOBAL's own concrete `CgType`, mirroring exactly
    // how function signatures are derived just above: a global's own
    // `types[name]` entry IS its value's type directly (no `Type::
    // Function` destructuring needed, unlike a function's entry). A
    // global whose initializer calls a still-generic function is fully
    // handled by this point: `mono_plan.globals` (swapped into
    // `ir_program.globals` above) already has that call site rewritten
    // to reference the concrete, mangled instantiation, and that
    // instantiation's own signature is already in `signatures` via the
    // `mono_plan.signatures` loop just above — so no special-casing is
    // needed here, only the plain, unmangled surface name lookup below.
    let mut global_types: HashMap<String, CgType> = HashMap::new();
    for g in &ir_program.globals {
        let ty = types
            .get(&g.name)
            .ok_or_else(|| format!("codegen: internal error — no type known for global {:?}", g.name))?;
        global_types.insert(g.name.clone(), plum_type_to_cg_type(ty)?);
    }

    // `entry_fn` may name a GENERIC function with more than one reachable
    // instantiation — there's no single concrete signature to compile a
    // `main` wrapper against in that case, so it's rejected with a clear
    // error rather than silently picking one. A non-generic name (or a
    // generic one instantiated exactly once) resolves straight through.
    let mut resolved_entry: String = match mono_plan.entry_rename.get(entry_fn) {
        Some(names) if names.len() == 1 => names[0].clone(),
        Some(names) if names.len() > 1 => {
            return Err(plum_syntax::error::CompileError::spanless(format!(
                "codegen: {entry_fn:?} is ambiguous as an entry point — it has {} reachable generic \
                 instantiation(s) ({names:?}); call it from a concrete, non-generic wrapper function instead",
                names.len()
            )));
        }
        _ => entry_fn.to_string(),
    };

    // `has_globals` is read from `ir_program.globals` BEFORE the `main`-
    // collision rename below (that rename only ever touches `body_ir`'s
    // text/`signatures`' keys, never `ir_program` itself) — mirroring
    // the interpreter's own `load_program` ordering invariant: every
    // global must be fully materialized before `emit_main`'s generated
    // native `main()` calls the resolved entry function.
    let has_globals = !ir_program.globals.is_empty();
    let mut body_ir = plum_codegen::emit_program(&ir_program, &signatures, &tag_fields, &global_types, &struct_field_names)?;

    // A real collision, not a hypothetical one: `plumc build`'s own
    // fixed convention (matching the interpreter CLI's — see main.rs)
    // is that a project's entry point is literally named `main`, and
    // `plum_codegen::emit_program` emits every function under its own
    // unmangled Plum name with no namespacing of its own (see e.g.
    // `emit_program`'s `define {ret} @{f.name}(...)`). Left alone, a
    // Plum-level `main` would compile to the SAME LLVM symbol `@main`
    // that `emit_main` below also defines for the process's real native
    // entry point — a `clang`-level "invalid redefinition of function
    // 'main'" — so it's renamed here to a symbol that can never
    // collide with a real Plum identifier (Plum's own lexer never
    // produces a name starting with `__`, mirroring the module system's
    // qualified-name collision-avoidance precedent — see `modules.rs`'s
    // own doc comment). A plain textual rename is safe and sufficient:
    // `@main(` only ever appears as this one function's own `define`/
    // `call`/`musttail call` sites in codegen's generated text, never as
    // a substring of a longer identifier (LLVM's `@name(` syntax always
    // has a non-identifier character — `(` — directly after the name).
    if resolved_entry == "main" {
        body_ir = body_ir.replace("@main(", "@__plum_entry_main(");
        // `signatures` is keyed by the ORIGINAL (unmangled) name — every
        // caller (`compile_and_run`, `plumc build`'s `run_build`) looks
        // its entry point's signature up by the RETURNED `resolved_
        // entry`, so the renamed key needs its own entry too, not just
        // the renamed LLVM symbol in `body_ir` above.
        if let Some(sig) = signatures.get("main").cloned() {
            signatures.insert("__plum_entry_main".to_string(), sig);
        }
        resolved_entry = "__plum_entry_main".to_string();
    }

    Ok((body_ir, signatures, resolved_entry, has_globals))
}

/// A hand-written LLVM `main` — not something `plum_codegen` itself
/// generates, since "what does a Plum program's entry point look
/// like as a native executable" (argument marshaling, how the result
/// becomes observable) is a `plumc`-level concern, not a codegen-
/// library one. Declares `printf` from libc (which `clang` links
/// against automatically) to make the entry point's result
/// observable via stdout.
pub fn emit_main(entry_fn: &str, ret_ty: CgType, args: &[CgValue], has_globals: bool) -> String {
    let args_ir = args
        .iter()
        .map(|a| match a {
            CgValue::Int(n) => format!("i64 {n}"),
            CgValue::Float(f) => format!("double 0x{:016X}", f.to_bits()),
            CgValue::Bool(b) => format!("i1 {}", if *b { 1 } else { 0 }),
            CgValue::Unit => "i1 0".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let (fmt_bytes, fmt_len, call_line) = match ret_ty {
        CgType::Int => (
            "%lld\\0A\\00",
            6,
            format!("  %r = call i64 @{entry_fn}({args_ir})\n  call i32 (ptr, ...) @printf(ptr @fmt, i64 %r)\n"),
        ),
        CgType::Float => (
            "%f\\0A\\00",
            4,
            format!("  %r = call double @{entry_fn}({args_ir})\n  call i32 (ptr, ...) @printf(ptr @fmt, double %r)\n"),
        ),
        CgType::Bool | CgType::Unit => (
            "%d\\0A\\00",
            4,
            format!(
                "  %r = call i1 @{entry_fn}({args_ir})\n  %rz = zext i1 %r to i32\n  call i32 (ptr, ...) @printf(ptr @fmt, i32 %rz)\n"
            ),
        ),
        // Plum strings are length-prefixed, not NUL-terminated on their
        // own — `printf`-ing one directly via `%s` would read past the
        // end of its actual content. Safe here ONLY because every
        // string cell `plum-codegen` allocates keeps one extra trailing
        // `\0` byte in sync (see `plum_codegen::emit_runtime`'s own
        // string-runtime doc comment) purely to make THIS print path
        // safe — an implementation detail invisible to Plum code
        // itself; `.len()` still always reports the true byte length,
        // never `len+1`.
        CgType::Str => (
            "%s\\0A\\00",
            4,
            format!(
                "  %r = call ptr @{entry_fn}({args_ir})\n  \
                 %bytes = getelementptr i8, ptr %r, i64 16\n  \
                 call i32 (ptr, ...) @printf(ptr @fmt, ptr %bytes)\n"
            ),
        ),
        // `compile_and_run` already rejects a `Heap`-, `Array`-, or
        // `Closure`-returning entry point before ever calling this
        // function — see its own doc comment on why. Unreachable in
        // practice, kept as a defensive error (not a panic) rather than
        // silently producing garbage IR if that check is ever bypassed.
        CgType::Heap
        | CgType::Array(_)
        | CgType::Closure(..)
        | CgType::Task(_)
        | CgType::Sender(_)
        | CgType::Receiver(_)
        | CgType::CStr => {
            return "; unreachable: compile_and_run rejects a Heap/Array/Closure/Task/Sender/Receiver/CStr-returning \
                     entry point before this point"
                .to_string()
        }
    };
    // `@printf` is NOT re-declared here — `plum_codegen::emit_runtime`
    // already declares it unconditionally (needed by `@plum_abort`), and
    // LLVM IR rejects a duplicate `declare` for the same function.
    //
    // `has_globals` prepends a call to `plum_codegen::emit_program`'s
    // generated `@plum_init_globals()` — BEFORE `call_line`'s own call
    // to `entry_fn` — matching the interpreter's own `load_program`
    // ordering invariant (every global fully materialized before any
    // user code runs). Omitted entirely (not even an empty no-op call)
    // when the compiled program has no globals at all, since `plum_
    // codegen::emit_program` itself never emits `@plum_init_globals` in
    // that case — calling it unconditionally would be an undefined-
    // symbol link error.
    let init_globals_call = if has_globals { "  call void @plum_init_globals()\n" } else { "" };
    // `@plum_locale_init()` — sets the process-wide locale to `C.utf8`
    // once, unconditionally (matching `plum_codegen::emit_runtime`'s own
    // "always emit every runtime helper" precedent, rather than gating
    // this one call behind a flag), so `towupper`/`towlower` (which
    // `.to_upper()`/`.to_lower()` compile down to — see `plum-codegen`'s
    // `emit_runtime`) do real Unicode case mapping instead of silently
    // degrading to the ASCII-only "C" locale glibc otherwise defaults
    // to. Placed BEFORE `@plum_init_globals()` in case a global
    // initializer itself calls `.to_upper()`/`.to_lower()`.
    format!(
        "@fmt = constant [{fmt_len} x i8] c\"{fmt_bytes}\"\n\ndefine i32 @main() {{\nentry:\n  call void @plum_locale_init()\n{init_globals_call}{call_line}  ret i32 0\n}}\n"
    )
}

/// A unique temp-directory NAME, not just per process — test threads
/// within the same process (`cargo test` runs them in parallel by
/// default) would otherwise race to write/execute the SAME binary path,
/// surfacing as a spurious "Text file busy" error, not a real
/// correctness bug. Shared by `run_via_clang` (its own scratch dir) and
/// `compile_ir_to_binary` (a scratch dir for the intermediate `.ll`
/// only — the final binary itself goes to the CALLER-chosen `out_path`,
/// not here).
pub(crate) fn unique_temp_dir(prefix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// The shared "write `.ll` + invoke `clang -o <path>`" step — extracted
/// out of `run_via_clang` so `compile_ir_to_binary` (persist a binary,
/// don't run it) and `run_via_clang` (the existing test harness: build,
/// run, capture stdout) share the exact same compile step rather than
/// two independent copies drifting apart.
fn clang_compile(ir: &str, ll_path: &std::path::Path, bin_path: &std::path::Path) -> Result<(), String> {
    std::fs::write(ll_path, ir).map_err(|e| format!("failed to write generated IR: {e}"))?;
    let compile = Command::new("clang")
        .arg(ll_path)
        // `-lm` — a Plum program can now `extern "C"` a real libm
        // function (`sqrt`, ...) via `plum_codegen::emit_program`'s new
        // FFI support; unlike `plum-interp` (which resolves against the
        // CURRENT PROCESS's own dynamic symbol table, so its own
        // `build.rs` links `libm` into the interpreter binary itself —
        // see DESIGN.md's "Symbol resolution" note), a compiled Plum
        // program is its OWN separate binary, so ITS link step needs the
        // same `-lm` explicitly. Harmless to pass unconditionally even
        // for a program that never calls a libm function — `clang`/`ld`
        // only pull in the specific object code actually referenced.
        .arg("-lm")
        .arg("-o")
        .arg(bin_path)
        .output()
        .map_err(|e| format!("could not run `clang` (required to compile generated LLVM IR — is it on PATH?): {e}"))?;
    if !compile.status.success() {
        return Err(format!(
            "clang failed to compile the generated IR:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        ));
    }
    Ok(())
}

/// Compiles `ir` and PERSISTS the resulting binary at `out_path` —
/// unlike `run_via_clang` (the existing test harness), this never
/// executes it; `plumc build`'s whole point is to hand the user a real,
/// standalone executable, not to run it on their behalf. The `.ll`
/// intermediate is written to a scratch temp directory (not polluting
/// wherever `out_path` lives) via the same `clang_compile` helper
/// `run_via_clang` uses, so both paths compile identically.
pub fn compile_ir_to_binary(ir: &str, out_path: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create output directory {parent:?}: {e}"))?;
        }
    }
    let dir = unique_temp_dir("plumc-build");
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create temp build directory: {e}"))?;
    let ll_path = dir.join("program.ll");
    clang_compile(ir, &ll_path, out_path)
}

/// The compile-and-run test harness's C-fixture VARIANT — links an
/// extra, small, hand-written `.c` helper file ALONGSIDE the generated
/// `.ll`, via the SAME `clang` invocation (`clang program.ll helper.c
/// -o program`), not a separate build system. Needed for exactly two
/// tests (`bool_width_round_trips_through_a_real_c_abi_boundary`,
/// `a_real_c_callback_invocation_round_trips_through_native_code`)
/// where no existing REAL libc function has a narrow enough Int/Float/
/// Bool-only signature to prove either the `Bool`-width ABI conversion
/// or a genuine callback invocation end-to-end — the exact same wall
/// `plum-interp`'s own test suite hit (see that crate's
/// `call_with_10_and_20`/`call_with_true`/`identity_cstr` Rust-level
/// stand-ins), except codegen's tests compile to a REAL native binary,
/// so a Rust `extern "C" fn`'s address can't be borrowed in-process the
/// way the interpreter's tests do — an actual, separately compiled `.c`
/// translation unit is required instead.
#[cfg(test)]
fn run_via_clang_with_c_helper(ir: &str, c_source: &str) -> Result<String, String> {
    let dir = unique_temp_dir("plumc-codegen-cfixture");
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create temp build directory: {e}"))?;
    let ll_path = dir.join("program.ll");
    let c_path = dir.join("helper.c");
    let bin_path: PathBuf = dir.join("program");
    std::fs::write(&ll_path, ir).map_err(|e| format!("failed to write generated IR: {e}"))?;
    std::fs::write(&c_path, c_source).map_err(|e| format!("failed to write C helper source: {e}"))?;
    let compile = Command::new("clang")
        .arg(&ll_path)
        .arg(&c_path)
        .arg("-lm")
        .arg("-o")
        .arg(&bin_path)
        .output()
        .map_err(|e| {
            format!("could not run `clang` (required to compile the generated LLVM IR together with the C helper — is it on PATH?): {e}")
        })?;
    if !compile.status.success() {
        return Err(format!(
            "clang failed to compile the generated IR together with the C helper:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        ));
    }

    let run = Command::new(&bin_path)
        .output()
        .map_err(|e| format!("failed to run compiled binary {bin_path:?}: {e}"))?;
    if !run.status.success() {
        return Err(format!(
            "compiled program exited with a non-zero status: {:?}\nstdout: {}\nstderr: {}",
            run.status.code(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
}

/// The C-fixture counterpart to `compile_and_run` — see `run_via_clang_
/// with_c_helper`'s own doc comment for why this variant exists at all.
#[cfg(test)]
fn compile_and_run_with_c_helper(src: &str, entry_fn: &str, args: &[CgValue], c_source: &str) -> Result<String, String> {
    let (body_ir, signatures, resolved_entry, has_globals) = compile_to_ir(src, entry_fn)?;
    let sig = signatures
        .get(&resolved_entry)
        .ok_or_else(|| format!("codegen: no such function {entry_fn:?}"))?
        .clone();
    if sig.params.len() != args.len() {
        return Err(format!(
            "codegen: {entry_fn:?} expects {} argument(s), found {}",
            sig.params.len(),
            args.len()
        ));
    }
    for (arg, expected) in args.iter().zip(&sig.params) {
        if arg.cg_type() != *expected {
            return Err(format!(
                "codegen: argument type mismatch calling {entry_fn:?} — expected {expected:?}, found {:?}",
                arg.cg_type()
            ));
        }
    }
    reject_unprintable_return(entry_fn, sig.ret.clone())?;

    let main_ir = emit_main(&resolved_entry, sig.ret, args, has_globals);
    let full_ir = format!("{body_ir}\n{main_ir}");

    run_via_clang_with_c_helper(&full_ir, c_source)
}

fn run_via_clang(ir: &str) -> Result<String, String> {
    let dir = unique_temp_dir("plumc-codegen");
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create temp build directory: {e}"))?;
    let ll_path = dir.join("program.ll");
    let bin_path: PathBuf = dir.join("program");
    clang_compile(ir, &ll_path, &bin_path)?;

    let run = Command::new(&bin_path)
        .output()
        .map_err(|e| format!("failed to run compiled binary {bin_path:?}: {e}"))?;
    if !run.status.success() {
        return Err(format!(
            "compiled program exited with a non-zero status: {:?}\nstdout: {}\nstderr: {}",
            run.status.code(),
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arithmetic_compiles_and_runs() {
        let out = compile_and_run("let go () = 2 + 3 * 4", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "14");
    }

    // --- Bug 2: empty array literal crossing a generic-function boundary ---

    #[test]
    fn gap_a_an_empty_array_literal_from_a_non_generic_context_can_be_passed_into_a_generic_function() {
        // The EXACT original reproducer that blocked `map_keys`/`map_
        // values`/`set_to_array` in the "Set algebra" chunk: `[]`,
        // concretely typed via an explicit annotation in a NON-generic
        // caller (`go`), passed as an argument into a generic function
        // (`map_keys_into[K, V]`). Root cause: `monomorphize::plan`
        // never threaded `empty_array_elem_types` through AT ALL (every
        // function/global plumc emits gets re-lowered through `plan`'s
        // own `base_lctx`, which had no such map) — fixed by threading
        // it exactly like `closure_types` already was.
        let src = "\
            let map_keys_into[K, V] (m: Map[K, V]) (acc: Array[K]): Array[K] = match m {\n\
                MapNode(k, _, rest) => map_keys_into(rest, acc.push(k)),\n\
                MapEnd => acc,\n\
            }\n\
            \n\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 100), 2, 200);\n\
                let ks: Array[Int] = [];\n\
                let ks2 = map_keys_into(m, ks);\n\
                ks2[0] + ks2[1]\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn gap_b_an_empty_array_literal_pinned_only_by_its_enclosing_generic_functions_own_param_works() {
        // `let map_keys[K, V] (m: Map[K, V]): Array[K] = match m { ...
        // MapEnd => [] ... }` — the empty literal's element type is
        // pinned ONLY to `map_keys`'s OWN generic `K`, never to
        // anything concrete anywhere in its own declaration. Root
        // cause: `Infer::empty_array_elem_types` had no tier-2 template
        // fallback at all (unlike `closure_types`) — a still-
        // unresolved `Var` was ALWAYS a hard ambiguity error, even
        // though it's genuinely resolvable once monomorphization
        // instantiates the function. Fixed by mirroring `resolve_
        // closure_types`'s existing tier-2 mechanism exactly (reusing
        // `resolve_closure_component` directly) plus a matching
        // `extra_empty_array_elem_types` per-instantiation side-channel
        // in `monomorphize.rs`. Instantiated at TWO different concrete
        // types (`Map[Int, Int]` and `Map[Str, Bool]`) in the SAME
        // compiled program, mirroring this project's established
        // "prove independent instantiations, not just one" pattern.
        // Uses the REAL stdlib `map_keys` directly (this bug fix is
        // exactly what makes `map_keys`/`map_values`/`set_to_array`
        // possible at all — see `STDLIB_COLLECTIONS_SRC`'s own updated
        // doc comment) rather than a local, hand-rolled redeclaration.
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 100), 2, 200);\n\
                let ks = map_keys(m);\n\
                let sm = map_insert(map_new(()), \"a\", true);\n\
                let sks = map_keys(sm);\n\
                ks[0] + ks[1] + ks.len() * 100 + sks.len() * 1000\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1203");
    }

    #[test]
    fn map_values_and_set_to_array_work_now_that_both_bugs_are_fixed() {
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 10), 2, 20);\n\
                let vs = map_values(m);\n\
                let s = set_from_array([1, 2, 3]);\n\
                let arr = set_to_array(s);\n\
                vs[0] + vs[1] + arr.len() * 100\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "330");
    }

    #[test]
    fn a_closure_passed_to_fold_can_tail_call_a_same_shaped_curried_function() {
        // The exact original reproducer for a real compiler bug: a
        // closure body's own `caller_sig` never accounted for its
        // implicit leading `ptr %env` parameter, so a closure whose
        // declared shape happened to match `set_insert`'s exactly
        // spuriously passed the `musttail` eligibility check — `clang`
        // rejected the resulting IR outright (`cannot guarantee tail
        // call due to mismatched parameter counts`). Fixed via `Ctx::
        // is_closure_body`, unconditionally disallowing `musttail` from
        // a closure body. See the next test for direct proof of the
        // mechanism (an ordinary `call`, not `musttail`), not just this
        // end-to-end symptom.
        let src = "\
            let go (): Int = {\n\
                let s = [1, 2, 2, 3].fold(set_new(()), |acc, x| set_insert(acc, x));\n\
                set_len(s)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn a_closure_body_tail_call_to_a_same_shaped_function_is_an_ordinary_call_not_musttail() {
        // Direct proof of the mechanism `Ctx::is_closure_body` fixes —
        // not just the end-to-end symptom above. `add_one`'s own
        // declared shape (`Int -> Int`) exactly matches the closure's,
        // which is exactly the condition that used to spuriously pass
        // the (incomplete) `musttail` eligibility check.
        let src = "\
            let add_one (n: Int): Int = n + 1\n\
            let go (): Int = [1, 2, 3].map(|x| add_one(x))[0]\n\
        ";
        let (body_ir, ..) = compile_to_ir(src, "go").unwrap();
        // Scoped to the specific `@add_one` call site (not "no `musttail`
        // anywhere in the whole compiled program") — the prelude itself
        // legitimately contains OTHER, unrelated tail-recursive
        // functions (e.g. JSON's own `skip_ws`), so a whole-program
        // check would be invalidated by prelude growth having nothing
        // to do with this closure-body mechanism.
        assert!(!body_ir.contains("musttail call i64 @add_one") && body_ir.contains("@add_one"), "{body_ir}");
        assert!(body_ir.contains("call i64 @add_one"), "{body_ir}");
    }


    // --- standard library: `println` (see `plumc::STDLIB_IO_SRC`) ---

    #[test]
    fn println_works_for_every_primitive_type_and_prints_before_the_entrys_own_return_value() {
        // `println` is ordinary Plum source merged in via `with_prelude`
        // (no `use` needed, like `Option`/`Result`) — this proves it,
        // end to end, for every type `.to_string()` supports
        // (Int/Float/Bool/Str), via REAL compiled-and-executed native
        // code, not just a type-check. `run_via_clang` captures the
        // whole process's stdout, so this also proves each `puts()`
        // call happens in the right order relative to `emit_main`'s own
        // final `printf` of the entry function's return value.
        let src = "let go (): Int = { println(42); println(3.5); println(true); println(\"hi\"); 0 }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        // `3.5`, not `3.500000` — `Expr::ToString`'s `Float` codegen
        // now renders via `%.15g` (matching the interpreter's Rust-
        // `Display`-style output), not the old always-6-decimals `%f`.
        assert_eq!(out, "42\n3.5\ntrue\nhi\n0");
    }

    // --- standard library: basic file I/O (see `plumc::STDLIB_FILE_SRC`) ---
    //
    // The native-codegen counterpart to `plumc::lib.rs`'s own interpreter-
    // path file I/O tests — a REAL compiled binary actually opening/
    // reading/writing a real file via `clang`-compiled `fopen`/`fread`/
    // `fwrite`, not just a type-check.

    #[test]
    fn write_file_then_read_file_round_trips_in_native_codegen() {
        let path = unique_temp_dir("plum-codegen-file-io").with_extension("txt");
        let path_str = path.to_str().unwrap();
        let src = format!(
            "let go (): Bool = {{ \
                let w = write_file(\"{path_str}\", \"hello file io\"); \
                match w {{ \
                    Ok(_) => match read_file(\"{path_str}\") {{ Ok(s) => s == \"hello file io\", Err(_) => false }}, \
                    Err(_) => false \
                }} \
            }}"
        );
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_file_on_a_nonexistent_path_returns_err_in_native_codegen() {
        let path = unique_temp_dir("plum-codegen-file-io-missing").with_extension("txt");
        let src = format!(
            "let go (): Bool = match read_file(\"{}\") {{ Ok(_) => false, Err(_) => true }}",
            path.to_str().unwrap()
        );
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn write_file_to_an_invalid_path_returns_err_in_native_codegen() {
        let src = "let go (): Bool = match write_file(\"/plum_test_nonexistent_dir_xyz/f.txt\", \"x\") { \
                    Ok(_) => false, Err(_) => true }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    // --- testing framework: `panic_raw` (see `ir::Expr::PanicRaw`) ---

    #[test]
    fn panic_raw_aborts_the_compiled_process_with_its_message_on_stdout() {
        // Unlike the interpreter (an ordinary catchable `Err`), a
        // native-compiled `panic_raw` is a hard process abort (`@plum_
        // abort`: `printf` + `exit(1)`) — `compile_and_run`/`run_via_
        // clang` surface that as an `Err` whose text embeds the
        // compiled binary's own captured stdout, which is where `@plum_
        // abort`'s message actually landed.
        let src = "let go (): Unit = panic_raw(\"boom\")";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected panic_raw to abort the process");
        assert!(err.contains("boom"), "expected the abort message in the error, got: {err}");
    }

    #[test]
    fn panic_raw_inside_an_if_else_is_not_reached_when_the_condition_holds_in_native_codegen() {
        let src = "let go (): Bool = { if true { () } else { panic_raw(\"should not run\") }; true }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    // --- testing framework: `assert`/`assert_eq`/`assert_ne` (see `plumc::STDLIB_ASSERT_SRC`) ---

    #[test]
    fn assert_passes_silently_on_a_true_condition_in_native_codegen() {
        let src = "let go (): Bool = { assert(true); true }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn assert_fails_with_a_clear_message_on_a_false_condition_in_native_codegen() {
        let src = "let go (): Unit = assert(false)";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected assert(false) to abort");
        assert!(err.contains("assertion failed"), "unexpected error: {err}");
    }

    #[test]
    fn assert_eq_fails_with_left_and_right_values_in_native_codegen() {
        let src = "let go (): Unit = assert_eq(1, 2)";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected assert_eq(1, 2) to abort");
        assert!(err.contains("left != right"), "unexpected error: {err}");
    }

    #[test]
    fn assert_ne_passes_on_different_values_in_native_codegen() {
        let src = "let go (): Bool = { assert_ne(1, 2); true }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    // --- standard library: array utilities (see `plumc::STDLIB_ARRAY_SRC`) ---

    #[test]
    fn array_reverse_take_drop_and_slice_run_through_native_codegen() {
        let src = "let go (): Int = { let arr = array_reverse([1, 2, 3]); arr[0] * 100 + arr[1] * 10 + arr[2] }";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "321");
        let src = "let go (): Int = { let arr = array_slice(array_concat([1, 2], [3, 4, 5]), 1, 4); arr[0] * 100 + arr[1] * 10 + arr[2] }";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "234");
    }

    #[test]
    fn array_find_any_all_index_of_and_contains_run_through_native_codegen() {
        let src = "let go (): Int = match array_find([1, 2, 3, 4], |x| x % 2 == 0) { Some(x) => x, None => -1 }";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "2");
        let src = "let go (): Bool = array_all([2, 4, 6], |x| x % 2 == 0)";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
        let src = "let go (): Bool = array_contains([10, 20, 30], 99)";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "0");
    }

    // --- standard library: number utilities (see `plumc::STDLIB_NUMBER_SRC`) ---

    #[test]
    fn int_and_float_min_max_abs_clamp_run_through_native_codegen() {
        let src = "let go (): Int = int_clamp(int_abs(-15), int_min(3, 7), int_max(3, 7))";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
        let src = "let go (): Float = float_clamp(float_abs(-15.0), float_min(3.0, 7.0), float_max(3.0, 7.0))";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7.000000");
    }

    #[test]
    fn float_floor_ceil_round_pow_sqrt_run_through_libm_through_native_codegen() {
        let src = "let go (): Float = float_floor(3.7) + float_ceil(3.2) + float_round(3.5) + float_pow(2.0, 4.0) + float_sqrt(81.0)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        // floor(3.7)=3, ceil(3.2)=4, round(3.5)=4, pow(2,4)=16, sqrt(81)=9 -> 36
        assert_eq!(out, "36.000000");
    }

    // --- standard library: Option/Result combinators (see `plumc::STDLIB_OPTION_RESULT_SRC`) ---
    //
    // The native-codegen counterpart to `plumc::lib.rs`'s own
    // interpreter-path tests for the same functions — proves each
    // combinator actually monomorphizes/codegens correctly (closures
    // passed as ordinary `(T) -> U`-typed values, `Option`/`Result`
    // constructed and matched) through a REAL compiled binary, not just
    // the interpreter.

    #[test]
    fn option_map_and_unwrap_or_run_through_native_codegen() {
        let src = "let go (): Int = option_unwrap_or(option_map(Some(1), |x| x + 1), -1)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "2");
        let src = "let go (): Int = option_unwrap_or(option_map(None, |x: Int| x + 1), -1)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "-1");
    }

    #[test]
    fn result_and_then_and_unwrap_or_else_run_through_native_codegen() {
        let src = "let half x = if x % 2 == 0 { Ok(x / 2) } else { Err(\"odd\") }\n\
                    let go (): Int = result_unwrap_or_else(result_and_then(Ok(4), half), |e: String| -1)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "2");
        let src = "let half x = if x % 2 == 0 { Ok(x / 2) } else { Err(\"odd\") }\n\
                    let go (): Int = result_unwrap_or_else(result_and_then(Ok(3), half), |e: String| -1)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "-1");
    }

    // --- standard library: JSON (see `plumc::STDLIB_JSON_SRC`) ---
    //
    // The native-codegen counterpart to `plumc::lib.rs`'s own
    // interpreter-path JSON tests — a REAL compiled binary running the
    // whole recursive-descent parser/serializer through `musttail`-
    // backed native tail calls, not the interpreter's own unbounded-
    // native-stack-growth `eval`. Unlike the interpreter-path tests,
    // this needs no special big-stack-thread handling (see `plumc::
    // lib.rs`'s own `run_json_test` doc comment for why the
    // interpreter side does).

    #[test]
    fn json_parse_handles_every_value_kind_in_native_codegen() {
        let src = "let go (): Bool = match json_parse(\"{\\\"a\\\": 1, \\\"b\\\": [true, null, \\\"s\\\"]}\") { \
                    Ok(JsonObject(entries)) => entries.len() == 2, Err(_) => false }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn json_parse_numbers_in_native_codegen() {
        let src = "let go (): Bool = { \
            let a = match json_parse(\"42\") { Ok(JsonNumber(n)) => n == 42.0, Err(_) => false }; \
            let b = match json_parse(\"-3.5\") { Ok(JsonNumber(n)) => n == -3.5, Err(_) => false }; \
            let c = match json_parse(\"2e-2\") { Ok(JsonNumber(n)) => n == 0.02, Err(_) => false }; \
            a && b && c \
        }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn json_parse_error_cases_in_native_codegen() {
        let src = "let go (): Bool = { \
            let a = match json_parse(\"\") { Err(_) => true, Ok(_) => false }; \
            let b = match json_parse(\"[1, 2,]\") { Err(_) => true, Ok(_) => false }; \
            let c = match json_parse(\"{\\\"a\\\" 1}\") { Err(_) => true, Ok(_) => false }; \
            let d = match json_parse(\"42 extra\") { Err(_) => true, Ok(_) => false }; \
            let e = match json_parse(\"\\\"unterminated\") { Err(_) => true, Ok(_) => false }; \
            a && b && c && d && e \
        }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn json_stringify_and_round_trip_in_native_codegen() {
        let src = "let go (): Bool = { \
            let n = json_stringify(JsonNull) == \"null\"; \
            let arr = json_stringify(JsonArray([JsonNumber(1.0), JsonNumber(2.0)])) == \"[1,2]\"; \
            let a = json_parse(\"{\\\"x\\\": 1, \\\"y\\\": [true, null, \\\"s\\\"]}\"); \
            let roundtrip = match a { \
                Ok(v) => match json_parse(json_stringify(v)) { Ok(v2) => v == v2, Err(_) => false }, \
                Err(_) => false \
            }; \
            n && arr && roundtrip \
        }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    // --- integration: chunk 8's file I/O composed with chunk 9's JSON ---
    //
    // The native-codegen counterpart to `plumc::lib.rs`'s own
    // interpreter-path integration test — a REAL compiled binary
    // writing a real JSON document to disk via `write_file`, reading
    // it back via `read_file`, and parsing/comparing it via `json_
    // parse`, all through ordinary `Result[T, String]` chaining.

    #[test]
    fn write_json_to_a_file_then_read_and_parse_it_back_in_native_codegen() {
        let path = unique_temp_dir("plum-codegen-json-file-io").with_extension("json");
        let path_str = path.to_str().unwrap();
        let src = format!(
            "let go (): Bool = {{ \
                let doc = JsonObject([JsonEntry {{ key: \"name\", value: JsonString(\"plum\") }}, \
                                       JsonEntry {{ key: \"tags\", value: JsonArray([JsonString(\"lang\"), JsonString(\"llvm\")]) }}]); \
                let w = write_file(\"{path_str}\", json_stringify(doc)); \
                match w {{ \
                    Ok(_) => match read_file(\"{path_str}\") {{ \
                        Ok(contents) => match json_parse(contents) {{ \
                            Ok(parsed) => parsed == doc, \
                            Err(_) => false \
                        }}, \
                        Err(_) => false \
                    }}, \
                    Err(_) => false \
                }} \
            }}"
        );
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
        let _ = std::fs::remove_file(&path);
    }

    // --- standard library: `print` (see `plumc::STDLIB_IO_SRC`) ---

    #[test]
    fn print_does_not_append_a_newline_unlike_println() {
        // `print` uses the raw `write(2)` syscall (see `STDLIB_IO_SRC`'s
        // own doc comment for why `fputs`/variadic `printf` were both
        // real dead ends), so back-to-back `print` calls — and the
        // entry's own final printed return value — all run together
        // with NO separating newline at all, unlike `println`.
        let src = "let go (): Int = { print(\"hi\"); print(\" there\"); 0 }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "hi there0");
    }

    #[test]
    fn print_and_println_can_be_mixed_in_the_same_program() {
        let src = "let go (): Int = { print(\"a\"); println(\"b\"); print(\"c\"); 0 }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "ab\nc0");
    }

    #[test]
    fn to_string_on_a_still_generic_type_parameter_works_in_native_codegen() {
        // The one genuinely unverified claim `println`'s whole design
        // rests on, isolated and proven directly (not just incidentally
        // exercised by a larger test): monomorphization fully
        // specializes a generic function's body per concrete
        // instantiation before codegen ever runs, so `.to_string()`
        // inside a still-generic function's own body sees a concrete,
        // resolved `CgType` by the time codegen visits it. Called at
        // TWO different concrete types in the same compiled program,
        // mirroring this project's established "prove independent
        // instantiations, not just one" pattern for generics.
        let src = "\
            let stringify[T] (x: T) = x.to_string()\n\
            let go () = stringify(5).concat(stringify(true))\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "5true");
    }

    // --- standard library: Str equality (LLVM-backend fix) ---
    //
    // Before `@plum_str_eq` existed, `Str` had NO arm at all in
    // `codegen_binop` — `"a" == "b"` didn't even compile natively. These
    // prove the fix through the real, compiled-and-executed path (not
    // just the IR-shape unit tests in `plum_codegen::lib::tests`).

    #[test]
    fn str_equality_is_true_for_equal_strings_in_native_codegen() {
        // `emit_main`'s `Bool` case prints via `%d` (0/1), not "true"/
        // "false" text — matching this file's own existing Bool-return
        // convention elsewhere (see `emit_main`'s `CgType::Bool` arm).
        let out = compile_and_run("let go (): Bool = \"abc\" == \"abc\"", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn str_equality_is_false_for_different_strings_in_native_codegen() {
        let out = compile_and_run("let go (): Bool = \"abc\" == \"abd\"", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "0");
    }

    #[test]
    fn str_inequality_works_in_native_codegen() {
        let out = compile_and_run("let go (): Bool = \"abc\" != \"abd\"", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    // --- structural equality for structs/enums/arrays (`@plum_struct_
    // eq`/`@plum_array_eq_<mangled>`) ---

    #[test]
    fn struct_equality_compares_fields_structurally_in_native_codegen() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = Point { x: 1, y: 2 } == Point { x: 1, y: 2 }\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");

        let src2 = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = Point { x: 1, y: 2 } == Point { x: 1, y: 3 }\n\
        ";
        assert_eq!(compile_and_run(src2, "go", &[CgValue::Unit]).unwrap(), "0");
    }

    #[test]
    fn enum_variant_equality_distinguishes_different_variants_in_native_codegen() {
        let src = "\
            enum Shape { Circle(Float), Square(Float) }\n\
            let go (): Bool = Circle(1.0) == Square(1.0)\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "0");

        let src2 = "\
            enum Shape { Circle(Float), Square(Float) }\n\
            let go (): Bool = Circle(1.0) == Circle(1.0)\n\
        ";
        assert_eq!(compile_and_run(src2, "go", &[CgValue::Unit]).unwrap(), "1");

        let src3 = "\
            enum Shape { Circle(Float), Square(Float) }\n\
            let go (): Bool = Circle(1.0) == Circle(2.0)\n\
        ";
        assert_eq!(compile_and_run(src3, "go", &[CgValue::Unit]).unwrap(), "0");
    }

    #[test]
    fn nested_struct_equality_recurses_into_heap_shaped_fields_in_native_codegen() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            struct Line { a: Point, b: Point }\n\
            let go (): Bool = Line { a: Point { x: 1, y: 2 }, b: Point { x: 3, y: 4 } } == \
                              Line { a: Point { x: 1, y: 2 }, b: Point { x: 3, y: 4 } }\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");

        let src2 = "\
            struct Point { x: Int, y: Int }\n\
            struct Line { a: Point, b: Point }\n\
            let go (): Bool = Line { a: Point { x: 1, y: 2 }, b: Point { x: 3, y: 4 } } == \
                              Line { a: Point { x: 1, y: 2 }, b: Point { x: 3, y: 9 } }\n\
        ";
        assert_eq!(compile_and_run(src2, "go", &[CgValue::Unit]).unwrap(), "0");
    }

    #[test]
    fn array_of_structs_equality_compares_elements_pairwise_in_native_codegen() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }] == \
                              [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");

        let src2 = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = [Point { x: 1, y: 2 }] == \
                              [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]\n\
        ";
        assert_eq!(compile_and_run(src2, "go", &[CgValue::Unit]).unwrap(), "0");
    }

    #[test]
    fn deep_recursive_list_equality_terminates_correctly_in_native_codegen() {
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let go (): Bool = Cons(1, Cons(2, Cons(3, Nil))) == Cons(1, Cons(2, Cons(3, Nil)))\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");

        let src2 = "\
            enum List { Cons(Int, List), Nil }\n\
            let go (): Bool = Cons(1, Cons(2, Cons(3, Nil))) == Cons(1, Cons(2, Cons(4, Nil)))\n\
        ";
        assert_eq!(compile_and_run(src2, "go", &[CgValue::Unit]).unwrap(), "0");

        let src3 = "\
            enum List { Cons(Int, List), Nil }\n\
            let go (): Bool = Cons(1, Cons(2, Nil)) == Cons(1, Cons(2, Cons(3, Nil)))\n\
        ";
        assert_eq!(compile_and_run(src3, "go", &[CgValue::Unit]).unwrap(), "0");
    }

    #[test]
    fn map_insert_get_contains_work_for_struct_keys_in_native_codegen() {
        // The direct payoff of real structural equality: a `Map` keyed
        // by a struct now genuinely works, not just type-checks — see
        // `plum-types::infer::satisfies_bound`'s tightened `Eq` bound
        // and `@plum_struct_eq`.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), Point { x: 1, y: 1 }, 100), Point { x: 2, y: 2 }, 200);\n\
                let got = match map_get(m, Point { x: 1, y: 1 }) { Some(v) => v, None => -1 };\n\
                let has = map_contains(m, Point { x: 2, y: 2 });\n\
                let missing = map_contains(m, Point { x: 9, y: 9 });\n\
                got + (if has { 10 } else { 0 }) + (if missing { 100 } else { 0 })\n\
            }\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "110");
    }

    #[test]
    fn struct_equality_emits_a_call_to_plum_struct_eq() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = Point { x: 1, y: 2 } == Point { x: 1, y: 2 }\n\
        ";
        let (body_ir, ..) = compile_to_ir(src, "go").unwrap();
        assert!(body_ir.contains("call i1 @plum_struct_eq"), "{body_ir}");
    }

    #[test]
    fn array_equality_emits_a_call_to_a_mangled_plum_array_eq_function() {
        let src = "let go (): Bool = [1, 2, 3] == [1, 2, 3]\n";
        let (body_ir, ..) = compile_to_ir(src, "go").unwrap();
        assert!(body_ir.contains("call i1 @plum_array_eq_Int"), "{body_ir}");
    }

    #[test]
    fn tuple_element_in_a_set_is_rejected_at_type_checking_time_by_the_tightened_eq_bound() {
        // Direct concrete `(1, 2) == (1, 2)` isn't gated by
        // `satisfies_bound` at all (that check only fires for a
        // GENERIC `[T: Eq]` bound being instantiated, not a plain
        // operator use on two already-concrete operand types) — so the
        // tightened bound's effect shows up here instead: `set_insert`
        // is declared `[T: Eq]`, and a `Set` of tuples now gets a
        // clear type-checking-time rejection instead of type-checking
        // fine and only failing later, inside codegen, with a much
        // less clear error (`CgType` has no `Tuple` variant at all —
        // see `plum_type_to_cg_type`'s own doc comment).
        let src = "let go (): Bool = { let s = set_insert(set_new(()), (1, 2)); set_contains(s, (1, 2)) }\n";
        let err = crate::typecheck_and_run(src, "go", vec![plum_interp::Value::Unit]).expect_err("expected a type error");
        assert!(err.contains("Eq"), "unexpected error: {err}");
    }

    // --- `.to_string()`/`println` for structs/enums/arrays (`@plum_
    // struct_to_string`/`@plum_array_to_string_<mangled>`) ---

    #[test]
    fn struct_to_string_renders_named_fields_in_native_codegen() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = Point { x: 1, y: 2 }.to_string() == \"Point { x: 1, y: 2 }\"\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn nested_struct_to_string_recurses_in_native_codegen() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            struct Line { a: Point, b: Point }\n\
            let go (): Bool = Line { a: Point { x: 1, y: 2 }, b: Point { x: 3, y: 4 } }.to_string() == \
                              \"Line { a: Point { x: 1, y: 2 }, b: Point { x: 3, y: 4 } }\"\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn enum_variant_to_string_renders_positionally_in_native_codegen() {
        let src = "\
            enum Shape { Circle(Float), Square(Float) }\n\
            let go (): Bool = Circle(5.0).to_string() == \"Circle(5)\"\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn bare_zero_field_variant_to_string_renders_just_the_tag_in_native_codegen() {
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let go (): Bool = Nil.to_string() == \"Nil\"\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn array_to_string_renders_bracketed_elements_in_native_codegen() {
        let src = "let go (): Bool = [1, 2, 3].to_string() == \"[1, 2, 3]\"\n";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn array_of_structs_to_string_recurses_into_each_element_in_native_codegen() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }].to_string() == \
                              \"[Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]\"\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn struct_with_a_str_field_to_string_quotes_and_escapes_it_in_native_codegen() {
        let src = r#"struct Named { label: String }
            let go (): Bool = Named { label: "a\"b\\c" }.to_string() == "Named { label: \"a\\\"b\\\\c\" }""#;
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn bare_top_level_str_to_string_is_still_unquoted_in_native_codegen() {
        let src = "let go (): Bool = \"hi\".to_string() == \"hi\"\n";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn map_to_string_renders_the_underlying_recursive_enum_generically_in_native_codegen() {
        // `map_insert` PREPENDS (see `plumc::STDLIB_COLLECTIONS_SRC`'s
        // own doc comment) — the most-recently-inserted key ends up
        // OUTERMOST, so `(2, 200)` (inserted last) wraps `(1, 100)`.
        let src = "\
            let go (): Bool = { \
                let m = map_insert(map_insert(map_new(()), 1, 100), 2, 200); \
                m.to_string() == \"MapNode(2, 200, MapNode(1, 100, MapEnd))\" \
            }\n\
        ";
        assert_eq!(compile_and_run(src, "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn float_to_string_uses_shortest_form_not_always_six_decimals_in_native_codegen() {
        // The float-format fix: `%.15g`, not `%f` — `3.0` now renders
        // as `"3"`, matching the interpreter's Rust-`Display`-style
        // output, not `printf`'s old always-6-decimal-places `"3.000000"`.
        assert_eq!(compile_and_run("let go (): Bool = 3.0.to_string() == \"3\"", "go", &[CgValue::Unit]).unwrap(), "1");
        assert_eq!(compile_and_run("let go (): Bool = 3.5.to_string() == \"3.5\"", "go", &[CgValue::Unit]).unwrap(), "1");
    }

    #[test]
    fn struct_to_string_emits_a_call_to_plum_struct_to_string() {
        // The Plum SOURCE keyword for the string type is `String`
        // (`Str` is only `Type::Str`'s internal Rust name, never a
        // valid annotation — `ast_type_to_type` only recognizes
        // `"String"`) — wrapped in a `Bool` comparison anyway, matching
        // this file's own established convention for every other
        // IR-shape assertion, rather than fighting an unrelated return-
        // type-annotation edge case.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Bool = Point { x: 1, y: 2 }.to_string() == \"Point { x: 1, y: 2 }\"\n\
        ";
        let (body_ir, ..) = compile_to_ir(src, "go").unwrap();
        assert!(body_ir.contains("call ptr @plum_struct_to_string"), "{body_ir}");
    }

    #[test]
    fn array_to_string_emits_a_call_to_a_mangled_plum_array_to_string_function() {
        let src = "let go (): Bool = [1, 2, 3].to_string() == \"[1, 2, 3]\"\n";
        let (body_ir, ..) = compile_to_ir(src, "go").unwrap();
        assert!(body_ir.contains("call ptr @plum_array_to_string_Int"), "{body_ir}");
    }

    // --- standard library: Map[K,V]/Set[T] (see `plumc::STDLIB_COLLECTIONS_SRC`) ---
    //
    // Association-list-backed recursive generic enums, mirroring the
    // `List[T]` pattern proven elsewhere in this file. `map_new()`/
    // `set_new()` are curried, single-Unit-typed-parameter functions
    // (`(): Map[K, V]`) — so call sites need an EXPLICIT unit argument,
    // `map_new(())`/`set_new(())`, not `map_new()`. This was found
    // empirically while writing these tests: a bare `f()` call site
    // parses to zero arguments (`Expr::Call { args: vec![] }`), which
    // doesn't match a declared single Unit parameter — the same
    // curried-parameter convention that makes every OTHER function in
    // this stdlib take `(a: T) (b: T)`, not `(a: T, b: T)`, also means
    // a declared-with-`()` function is a genuine one-parameter function,
    // not a zero-parameter one. `STDLIB_COLLECTIONS_SRC` itself never
    // calls `map_new`/`set_new` (it only DEFINES them), so this doesn't
    // require any change there — it only affects how callers write
    // call sites, same as any other curried function.

    #[test]
    fn map_insert_get_contains_remove_work_for_int_keys() {
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 100), 2, 200);\n\
                let got = match map_get(m, 1) { Some(v) => v, None => -1 };\n\
                let has2 = map_contains(m, 2);\n\
                let has3 = map_contains(m, 3);\n\
                let m2 = map_remove(m, 1);\n\
                let has1_after = map_contains(m2, 1);\n\
                got + (if has2 { 10 } else { 0 }) + (if has3 { 100 } else { 0 }) + (if has1_after { 1000 } else { 0 })\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "110");
    }

    #[test]
    fn map_insert_get_contains_remove_work_for_str_keys() {
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), \"a\", 1), \"b\", 2);\n\
                let got_a = match map_get(m, \"a\") { Some(v) => v, None => -1 };\n\
                let got_b = match map_get(m, \"b\") { Some(v) => v, None => -1 };\n\
                let has_c = map_contains(m, \"c\");\n\
                let m2 = map_remove(m, \"a\");\n\
                let has_a_after = map_contains(m2, \"a\");\n\
                got_a + got_b * 10 + (if has_c { 100 } else { 0 }) + (if has_a_after { 1000 } else { 0 })\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "21");
    }

    #[test]
    fn map_get_returns_the_most_recently_inserted_value_for_a_repeated_key() {
        // Documented v1 semantics: `map_insert` always PREPENDS, and
        // `map_get` scans from the head, so the LAST insert for a given
        // key wins — proven here by inserting key 1 twice with
        // different values and checking the second one is what comes
        // back, not the first.
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 100), 1, 200);\n\
                match map_get(m, 1) { Some(v) => v, None => -1 }\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "200");
    }

    #[test]
    fn map_remove_uncovers_the_older_value_when_a_key_was_inserted_twice() {
        // Documented v1 semantics: `map_remove` deletes only the FIRST
        // (most recent) matching node — removing once after a key was
        // inserted twice uncovers the OLDER value rather than erasing
        // all trace of the key.
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_new(()), 1, 100), 1, 200);\n\
                let m2 = map_remove(m, 1);\n\
                match map_get(m2, 1) { Some(v) => v, None => -1 }\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "100");
    }

    #[test]
    fn map_len_counts_nodes_not_unique_keys() {
        let src = "\
            let go (): Int = {\n\
                let m = map_insert(map_insert(map_insert(map_new(()), 1, 1), 1, 2), 2, 3);\n\
                map_len(m)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn set_insert_dedupes_and_len_contains_remove_work() {
        let src = "\
            let go (): Int = {\n\
                let s = set_insert(set_insert(set_insert(set_new(()), 1), 1), 2);\n\
                let n_before = set_len(s);\n\
                let has1 = set_contains(s, 1);\n\
                let has3 = set_contains(s, 3);\n\
                let s2 = set_remove(s, 1);\n\
                let has1_after = set_contains(s2, 1);\n\
                let n_after = set_len(s2);\n\
                n_before * 1000 + (if has1 { 100 } else { 0 }) + (if has3 { 10 } else { 0 }) + (if has1_after { 1 } else { 0 }) + n_after\n\
            }\n\
        ";
        // n_before = 2 (dedup: {1, 2}), has1 = true (100), has3 = false
        // (0), after removing 1: has1_after = false (0), n_after = 1.
        // Total: 2*1000 + 100 + 0 + 0 + 1 = 2101.
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "2101");
    }

    #[test]
    fn map_and_set_instantiated_at_two_different_concrete_types_in_the_same_program() {
        // Mirrors this project's established "prove independent
        // instantiations, not just one" pattern for generics (see
        // `a_generic_recursive_enum_instantiated_at_two_concrete_types_
        // produces_two_distinct_tags` above): `Map[Int, Str]` and
        // `Map[Str, Int]` both used in the SAME compiled program.
        let src = "\
            let go (): Int = {\n\
                let m1 = map_insert(map_new(()), 1, \"one\");\n\
                let m2 = map_insert(map_new(()), \"two\", 2);\n\
                let s1 = set_insert(set_new(()), 1);\n\
                let s2 = set_insert(set_new(()), \"x\");\n\
                let v1 = match map_get(m1, 1) { Some(v) => v, None => \"?\" };\n\
                let v2 = match map_get(m2, \"two\") { Some(v) => v, None => -1 };\n\
                v1.len() + v2 + set_len(s1) + set_len(s2)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        // v1 = "one" (len 3), v2 = 2, set_len(s1) = 1, set_len(s2) = 1
        assert_eq!(out, "7");
    }

    #[test]
    fn set_union_intersection_difference_and_from_array_all_work() {
        let src = "\
            let go (): Int = {\n\
                let a = set_from_array([1, 2, 2, 3]);\n\
                let b = set_from_array([2, 3, 4]);\n\
                let u = set_union(a, b);\n\
                let i = set_intersection(a, b);\n\
                let d = set_difference(a, b);\n\
                set_len(a) + set_len(u) * 10 + set_len(i) * 100 + set_len(d) * 1000\n\
            }\n\
        ";
        // a = {1,2,3} (len 3), b = {2,3,4}, union = {1,2,3,4} (len 4),
        // intersection = {2,3} (len 2), difference (a - b) = {1} (len 1).
        // Total: 3 + 4*10 + 2*100 + 1*1000 = 1243.
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1243");
    }

    #[test]
    fn map_from_arrays_zips_keys_and_values_by_index() {
        let src = "\
            let go (): Int = {\n\
                let m = map_from_arrays([1, 2, 3], [10, 20, 30]);\n\
                let v = match map_get(m, 2) { Some(v) => v, None => -1 };\n\
                map_len(m) * 100 + v\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "320");
    }

    #[test]
    fn set_algebra_and_map_from_arrays_work_at_a_str_keyed_instantiation_too() {
        // Mirrors this project's established "prove independent
        // instantiations, not just one" pattern for generics.
        let src = "\
            let go (): Int = {\n\
                let a = set_from_array([\"x\", \"y\"]);\n\
                let b = set_from_array([\"y\", \"z\"]);\n\
                let u = set_union(a, b);\n\
                let m = map_from_arrays([\"a\", \"b\"], [1, 2]);\n\
                let v = match map_get(m, \"b\") { Some(v) => v, None => -1 };\n\
                set_len(u) * 10 + v\n\
            }\n\
        ";
        // union({x,y}, {y,z}) = {x,y,z} (len 3), v = 2. Total: 3*10+2=32.
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "32");
    }

    // --- Non-constant global initializers ---
    //
    // Mirrors `plum-interp`'s own precedent for each shape one-to-one
    // (see that crate's "Zero-parameter top-level `let` (globals)"
    // section) — proving the exact same source programs the interpreter
    // already runs correctly ALSO compile and run correctly through the
    // LLVM backend now that `plum_codegen::emit_program` supports
    // globals at all.

    #[test]
    fn a_simple_constant_global_compiles_and_runs() {
        let out = compile_and_run("let x = 5\nlet go (): Int = x + 1", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn a_global_can_reference_an_earlier_global() {
        let src = "let a = 1\nlet b = a + 1\nlet go (): Int = b + 1";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn a_global_can_call_a_function_declared_after_it_in_source() {
        // The genuinely non-constant case this whole chunk exists for:
        // `double` is declared AFTER `x` textually, which only works
        // because EVERY function is registered (its `FnSig` known to
        // codegen) before `@plum_init_globals()` ever runs any
        // initializer — matching the interpreter's own `load_program`
        // "functions first and unconditionally" ordering invariant.
        let src = "let x = double(5)\nlet double (n: Int): Int = n * 2\nlet go (): Int = x";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "10");
    }

    #[test]
    fn a_function_can_reference_a_global_declared_earlier() {
        let src = "let pi_ish = 3\nlet area (r: Int): Int = pi_ish * r * r\nlet go (): Int = area(2)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "12");
    }

    #[test]
    fn a_self_referential_global_closure_can_call_itself() {
        // The global-scope counterpart to `plum-interp`'s own local
        // `a_self_referential_local_closure_can_call_itself` test (`{
        // let fib = |n| if n < 2 { n } else { fib(n-1) + fib(n-2) };
        // fib(10) }`) — except `fib` is now a top-level GLOBAL, not a
        // local `let`, proving the plan's "self-referential global
        // closures need no special-casing" claim holds for REAL
        // generated code, not just in the design reasoning: `fib`'s own
        // body is a separate top-level `define`, only ever `call`ed
        // AFTER `@plum_init_globals()` has already fully run and stored
        // the closure cell into `@global.fib`, so its own internal
        // `Var("fib")` reference resolves through the ordinary third
        // `Var`-resolution tier (a `load`) and finds a fully-
        // materialized value every time.
        let src = "let fib = |n: Int| if n < 2 { n } else { fib(n - 1) + fib(n - 2) }\nlet go (): Int = fib(10)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "55");
    }

    #[test]
    fn a_heap_allocating_global_is_referenced_from_multiple_functions_without_double_alloc_or_crash() {
        // `origin` is allocated exactly ONCE by `@plum_init_globals`
        // and never released for the rest of the program's life — both
        // `sum_x`/`sum_y` read its fields through the SAME slot's
        // `load`, never through a second, independent allocation.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let origin = Point { x: 3, y: 4 }\n\
            let sum_x (dummy: Int): Int = match origin { Point(x, y) => x + dummy }\n\
            let sum_y (dummy: Int): Int = match origin { Point(x, y) => y + dummy }\n\
            let go (): Int = sum_x(0) + sum_y(0)\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
    }

    /// A tiny C helper with a static call counter — the observable proof
    /// that a global's initializer runs EXACTLY once, not once per
    /// reference: if `@plum_init_globals` ever re-evaluated `counter`'s
    /// own initializer (rather than every later reference correctly
    /// LOADING the already-stored slot), `ffitest_bump_and_get`'s own
    /// static counter would be greater than 1 by the time `go` reads it.
    const CALL_COUNTER_C_HELPER: &str = r#"
        static long long ffitest_call_count = 0;
        long long ffitest_bump_and_get(void) {
            ffitest_call_count += 1;
            return ffitest_call_count;
        }
        long long ffitest_get_call_count(void) {
            return ffitest_call_count;
        }
    "#;

    #[test]
    fn a_global_initializer_is_evaluated_exactly_once() {
        let src = r#"
            extern "C" {
                fn ffitest_bump_and_get() -> Int;
                fn ffitest_get_call_count() -> Int;
            }
            let counter = unsafe { ffitest_bump_and_get() }
            let use_it (dummy: Int): Int = counter + counter + dummy
            let go (): Int = {
                let _a = use_it(0);
                let _b = use_it(1);
                let _c = counter;
                unsafe { ffitest_get_call_count() }
            }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], CALL_COUNTER_C_HELPER).unwrap();
        assert_eq!(out, "1", "the global's C-calling initializer must run exactly once, not once per reference");
    }

    #[test]
    fn a_failing_global_initializer_crashes_the_process_cleanly() {
        // No new failure mode — `@plum_init_globals()` runs as ordinary
        // generated code with the same crash semantics any other Plum
        // expression already has in this backend (an integer division
        // by zero aborts via `@plum_abort`, see `emit_runtime`'s own
        // doc comment), not a distinct "the whole program fails to
        // load" concept the way the interpreter's `load_program`
        // returning `Err` is.
        let src = "let x = 1 / 0\nlet go (): Int = x";
        let err = compile_and_run(src, "go", &[CgValue::Unit])
            .expect_err("a failing global initializer must crash the compiled process, not silently produce a wrong answer");
        assert!(
            err.contains("non-zero status"),
            "expected a clean non-zero-exit crash, got: {err}"
        );
    }

    #[test]
    fn simple_scalar_spawn_and_join_compiles_and_runs() {
        let src = "let go (): Int = spawn { 1 + 41 }.join()";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn spawn_rejects_capturing_a_closure_across_the_thread_boundary() {
        // A closure's captured environment is tied to the thread that
        // created it — mirrors the interpreter's own `to_portable`
        // rejection (`plum_interp::Interpreter::to_portable`'s `Value::
        // Closure` arm). `add_one` is captured (used inside `block`),
        // and its resolved type is `Closure`.
        let src = "\
            let go (): Int = {\n\
              let add_one = |x: Int| x + 1;\n\
              spawn { add_one(1) }.join()\n\
            }\n\
        ";
        let err = compile_and_run(src, "go", &[CgValue::Unit])
            .expect_err("expected a closure-typed spawn capture to be rejected at compile time");
        assert!(err.contains("spawn") && err.contains("thread boundary"), "unexpected error: {err}");
    }

    #[test]
    fn spawn_can_capture_one_heap_value_and_return_a_different_one_independently() {
        // Proves capture-IN and return-OUT are handled completely
        // independently: `p`'s deep copy crosses INTO the spawned
        // thread, and a FRESH, unrelated `Point` (never touching the
        // captured `p` at all) crosses back OUT via `.join()` — the
        // task's own boxed result adoption (`codegen_task_join`) must
        // not confuse or entangle the two directions.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
              let p = Point { x: 10, y: 20 };\n\
              let t = spawn { let _unused = p.x + p.y; Point { x: 100, y: 200 } };\n\
              match t.join() { Point(a, b) => a + b }\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "300");
    }

    /// The crux correctness test's `-fsanitize=address` variant — same
    /// program `spawn_many_tasks_each_capturing_a_distinct_heap_value_
    /// sums_correctly_at_scale` below compiles, but linked against
    /// ASan instead of plain `clang`, and RUN rather than just
    /// compiled: ASan instruments every heap access at the machine-code
    /// level, so if `spawn`'s deep-copy ever let two threads alias the
    /// SAME allocation (a race on a non-atomic refcount word being
    /// exactly the failure mode this whole feature exists to prevent —
    /// see `deep_copy_capture`'s own doc comment in codegen.rs), or if
    /// `.join()`'s cell/box frees ever double-freed or leaked past
    /// what ASan tolerates, this would abort loudly with a diagnostic
    /// and a non-zero exit rather than silently passing. `-fsanitize=
    /// thread` (a true race detector) was also considered but doesn't
    /// mix with `pthread_create`'s bare function-pointer ABI cleanly
    /// without more TSan-specific runtime support than this backend
    /// has any other precedent for pulling in — ASan's heap/use-after-
    /// free/double-free detection is still a real, independent signal
    /// beyond eyeballing the generated IR text, and was the one
    /// explicitly suggested in the implementation plan.
    #[test]
    fn spawn_many_tasks_each_capturing_a_distinct_heap_value_sums_correctly_at_scale_under_asan() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let sum_one (n: Int): Int = { let p = Point { x: n, y: n * 2 }; let t = spawn { p.x + p.y }; t.join() }\n\
            let go (): Int = { let mut acc = 0; for i in 0..1000 { acc = acc + sum_one(i); }; acc }\n\
        ";
        let (body_ir, signatures, resolved_entry, has_globals) = compile_to_ir(src, "go").unwrap();
        let sig = signatures.get(&resolved_entry).unwrap().clone();
        let main_ir = emit_main(&resolved_entry, sig.ret, &[CgValue::Unit], has_globals);
        let full_ir = format!("{body_ir}\n{main_ir}");

        let dir = unique_temp_dir("plumc-asan");
        std::fs::create_dir_all(&dir).unwrap();
        let ll_path = dir.join("program.ll");
        let bin_path = dir.join("program-asan");
        std::fs::write(&ll_path, &full_ir).unwrap();

        let compile = Command::new("clang")
            .arg("-fsanitize=address")
            .arg(&ll_path)
            .arg("-o")
            .arg(&bin_path)
            .output()
            .expect("could not run clang with -fsanitize=address");
        if !compile.status.success() {
            panic!(
                "clang -fsanitize=address failed to compile the generated IR:\n{}",
                String::from_utf8_lossy(&compile.stderr)
            );
        }

        // `detect_leaks=0`: deliberately isolates genuine memory
        // CORRUPTION (double-free, use-after-free, heap-buffer-overflow
        // — real UB, exactly this test's actual purpose) from plain
        // LEAKS. A `spawn` capture's deep copy is INTENTIONALLY never
        // released once the entry function finishes with it — matching
        // this codebase's own established "accepted leak, not a
        // soundness gap" precedent already documented for `Assign`/
        // `For`/closure-body captures elsewhere in `fbip.rs`/
        // `codegen.rs` — so a leak-detection failure here would be
        // EXPECTED, not a signal of anything new or unsound; see
        // `emit_spawn_entry_fn`'s own doc comment for exactly why doing
        // better would need real (currently absent) last-use analysis
        // INSIDE a spawned block.
        let run = Command::new(&bin_path)
            .env("ASAN_OPTIONS", "detect_leaks=0")
            .output()
            .expect("failed to run the ASan-instrumented binary");
        let stdout = String::from_utf8_lossy(&run.stdout).trim_end().to_string();
        let stderr = String::from_utf8_lossy(&run.stderr).to_string();
        assert!(
            run.status.success(),
            "ASan-instrumented binary reported a failure (a real memory-safety bug, not a hang or timeout):\n\
             stdout: {stdout}\nstderr: {stderr}"
        );
        assert!(!stderr.contains("ERROR: AddressSanitizer"), "ASan flagged an error:\n{stderr}");
        let expected: i64 = (0..1000i64).map(|i| i + i * 2).sum();
        assert_eq!(stdout, expected.to_string());
    }

    #[test]
    fn spawn_many_tasks_each_capturing_a_distinct_heap_value_sums_correctly_at_scale() {
        // The crux correctness test: 1000 tasks, each capturing its OWN
        // distinct `Point` (a direct struct LITERAL — see `sum_one`'s
        // own doc comment on why this matters for FBIP tracking) and
        // returning a value DERIVED from it (not the captured value
        // itself), joined and summed. If `spawn`'s deep-copy were ever
        // wrong (e.g. sharing the original pointer instead of copying
        // it), this would be exactly the shape to expose it: 1000
        // concurrently-running threads all touching DIFFERENT `Point`
        // cells that must never alias one another.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let sum_one (n: Int): Int = { let p = Point { x: n, y: n * 2 }; let t = spawn { p.x + p.y }; t.join() }\n\
            let go (): Int = { let mut acc = 0; for i in 0..1000 { acc = acc + sum_one(i); }; acc }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        let expected: i64 = (0..1000i64).map(|i| i + i * 2).sum();
        assert_eq!(out, expected.to_string());
    }

    #[test]
    fn deep_tail_recursion_does_not_stack_overflow() {
        // The key correctness proof for this whole chunk: a million
        // levels of "recursion" would overflow the stack without real
        // tail-call elimination — this only succeeds because `musttail`
        // actually reused the same stack frame.
        let src = "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }";
        let out = compile_and_run(src, "sum", &[CgValue::Int(1_000_000), CgValue::Int(0)]).unwrap();
        assert_eq!(out, "500000500000");
    }

    #[test]
    fn mutual_tail_recursion_does_not_stack_overflow() {
        let src = "\
            let is_even n = if n == 0 { true } else { is_odd(n - 1) }\n\
            let is_odd n = if n == 0 { false } else { is_even(n - 1) }\n\
        ";
        let out = compile_and_run(src, "is_even", &[CgValue::Int(1_000_001)]).unwrap();
        assert_eq!(out, "0");
    }

    #[test]
    fn if_and_comparison_compile_and_run() {
        let src = "let max a b = if a > b { a } else { b }";
        let out = compile_and_run(src, "max", &[CgValue::Int(3), CgValue::Int(7)]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn short_circuit_and_never_evaluates_the_untaken_side() {
        // `false && (1 / 0 == 0)` — if `&&` were compiled as a plain,
        // eager instruction (both sides always evaluated), this would
        // trap on the integer division by zero. Success here proves
        // the untaken branch's code genuinely never executes.
        let src = "let go () = false && (1 / 0 == 0)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "0");
    }

    #[test]
    fn short_circuit_or_never_evaluates_the_untaken_side() {
        let src = "let go () = true || (1 / 0 == 0)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn float_arithmetic_compiles_and_runs() {
        let out = compile_and_run("let go () = 1.5 + 2.5", "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "4.000000");
    }

    #[test]
    fn a_construct_outside_codegen_scope_is_a_clear_error() {
        // `go`'s own DECLARED signature is Int -> Int (fully within
        // supported scope), but its BODY calls `ref(v)` — `Ref`'s whole
        // shared-mutable-cell runtime has no codegen at all yet (a
        // genuinely separate, still-unimplemented feature; ALL SIX
        // Unicode-aware string ops, including `.runes()`, are supported
        // as of this chunk — see the string tests below) — exercising
        // `plum_codegen`'s own per-expression rejection, not just
        // `plumc`'s signature-conversion gate (see the next test for
        // that one).
        let src = "let go (n: Int): Int = { let r = ref(1); 5 }";
        let err = compile_and_run(src, "go", &[CgValue::Int(1)]).expect_err("expected a codegen scope error");
        assert!(err.contains("does not yet support"), "unexpected error: {err}");
    }

    #[test]
    fn a_function_returning_a_generic_heap_value_directly_at_the_entry_point_is_a_clear_error() {
        // Monomorphization now resolves `Option[Int]`'s signature just
        // fine (see the generics tests below) — but a heap-shaped value
        // still isn't PRINTABLE by the compiled entry point's hand-
        // written `main` wrapper (no `ToString`-equivalent for compiled
        // heap values yet), so this is still a clear error, just a
        // DIFFERENT one than "generics aren't supported at all".
        let src = "let go (): Option[Int] = Some(1)";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected a heap-return error");
        assert!(err.contains("heap-shaped value"), "unexpected error: {err}");
    }

    // --- heap values: structs, enums, refcounting, Match ---

    #[test]
    fn a_struct_is_constructed_and_its_fields_read_back_via_match() {
        // Field access (`p.x`) desugars through `Match`, same as
        // everywhere else in this codebase — so this exercises Ctor
        // construction AND Match-based field extraction together.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = { let p = Point { x: 3, y: 4 }; p.x + p.y }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn a_recursive_enum_linked_list_sums_via_tail_recursion() {
        // Proves Ctor/Match/refcounting and guaranteed tail calls all
        // compose correctly: a real self-referential enum (`List`
        // contains a `List`), built via nested `Ctor`s, summed via a
        // TAIL-recursive accumulator function.
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let sum_acc (lst: List) (acc: Int): Int = match lst {\n\
                Cons(h, t) => sum_acc(t, acc + h),\n\
                Nil => acc,\n\
            }\n\
            let go (): Int = sum_acc(Cons(1, Cons(2, Cons(3, Nil))), 0)\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn a_match_guard_falls_through_to_the_next_arm_when_it_fails() {
        let src = "\
            enum Shape { Circle(Int), Square(Int) }\n\
            let classify (s: Shape): Int = match s {\n\
                Circle(r) if r > 10 => 1,\n\
                Circle(r) => 2,\n\
                Square(side) => 3,\n\
            }\n\
            let go (): Int = classify(Circle(5))\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "2");
    }

    #[test]
    fn ctor_reuse_produces_correct_output_for_a_real_reuse_eligible_program() {
        // `inc_all`'s `Cons(h + 1, inc_all(t))` arm is exactly FBIP's
        // reuse-in-place shape (bare-`Var` scrutinee `lst`, arm body a
        // direct same-arity `Ctor`) — `plum-codegen`'s own unit test
        // (`ctor_reuse_never_calls_plum_alloc_on_the_reuse_path`)
        // already verifies the REUSE-vs-fresh-alloc branch SHAPE
        // structurally; this test verifies the shape actually EXECUTES
        // correctly end to end, which a text-only IR inspection can't
        // prove by itself.
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let inc_all (lst: List): List = match lst {\n\
                Cons(h, t) => Cons(h + 1, inc_all(t)),\n\
                Nil => Nil,\n\
            }\n\
            let sum_acc (lst: List) (acc: Int): Int = match lst {\n\
                Cons(h, t) => sum_acc(t, acc + h),\n\
                Nil => acc,\n\
            }\n\
            let go (): Int = sum_acc(inc_all(Cons(1, Cons(2, Cons(3, Nil)))), 0)\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "9");
    }

    // --- generics / monomorphization ---

    /// Runs the pipeline exactly through `monomorphize::plan` (the SAME
    /// real `with_prelude` pipeline `compile_and_run` uses, not a hand-
    /// built plum-ir-only program) and returns every mangled TAG it
    /// produced — needed because, unlike a function name, a struct/enum
    /// tag never appears as readable text in the generated `.ll` itself
    /// (`plum_codegen` interns every tag to a small integer — see
    /// `plum_codegen::intern_tags` — so there's no `.ll`-text assertion
    /// that could prove two tags stayed distinct; this inspects
    /// `MonoPlan::tag_fields`'s keys directly instead).
    fn mono_tags(src: &str) -> std::collections::HashSet<String> {
        let tokens = Lexer::with_base_offset(src, crate::PRELUDE_TOTAL_LEN).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser.parse_program().unwrap_or_else(|e| panic!("parse error: {e}"));
        let program = with_prelude(program);
        let type_ctx = TypeContext::from_items(&program.items).unwrap_or_else(|e| panic!("context error: {e}"));
        let mut infer = Infer::with_context(type_ctx);
        let types = infer.infer_program(&program).unwrap_or_else(|e| panic!("type error: {e}"));
        let resolved_sites = infer.resolve_generic_sites().unwrap_or_else(|e| panic!("resolve error: {e}"));
        let type_ctx2 = TypeContext::from_items(&program.items).unwrap();
        let closure_types = infer.resolve_closure_types().unwrap_or_else(|e| panic!("closure type error: {e}"));
        let mono_plan = plum_ir::monomorphize::plan(
            &program,
            &type_ctx2,
            &resolved_sites,
            infer.fn_generics(),
            &types,
            infer.field_owners(),
            infer.array_for_loops(),
            &closure_types,
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap_or_else(|e| panic!("monomorphization error: {e}"));
        mono_plan.tag_fields.into_keys().collect()
    }

    #[test]
    fn generic_struct_single_instantiation_compiles_and_runs() {
        let src = "\
            struct Pair[A, B] { first: A, second: B }\n\
            let go (): Int = { let p = Pair { first: 3, second: 4 }; p.first + p.second }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn a_generic_recursive_enum_instantiated_at_two_concrete_types_produces_two_distinct_tags() {
        // The direct mangling-collision-avoidance proof: `List[Int]`
        // and `List[Bool]` must each get their OWN `Cons$..`/`Nil$..`
        // tags, not share one `Cons`/`Nil` pair — see `mono_tags`'s
        // doc comment for why this is checked via `MonoPlan::tag_fields`
        // directly rather than `.ll` text.
        let src = "\
            enum List[T] { Cons(T, List[T]), Nil }\n\
            let sum_int (lst: List[Int]): Int = match lst {\n\
                Cons(h, t) => h + sum_int(t),\n\
                Nil => 0,\n\
            }\n\
            let count_true (lst: List[Bool]): Int = match lst {\n\
                Cons(h, t) => (if h { 1 } else { 0 }) + count_true(t),\n\
                Nil => 0,\n\
            }\n\
            let go (): Int = sum_int(Cons(1, Cons(2, Nil))) + count_true(Cons(true, Cons(false, Cons(true, Nil))))\n\
        ";
        let tags = mono_tags(src);
        assert!(tags.contains("Cons$Int"), "tags: {tags:?}");
        assert!(tags.contains("Cons$Bool"), "tags: {tags:?}");
        assert!(tags.contains("Nil$Int"), "tags: {tags:?}");
        assert!(tags.contains("Nil$Bool"), "tags: {tags:?}");
        assert!(!tags.contains("Cons"));
        assert!(!tags.contains("Nil"));

        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "5");
    }

    #[test]
    fn a_generic_function_called_at_multiple_concrete_types_produces_two_distinct_definitions() {
        let src = "\
            let identity[T] (x: T): T = x\n\
            let go (): Int = { let n = identity(5); let b = identity(true); n + (if b { 1 } else { 0 }) }\n\
        ";
        let (_body_ir, signatures, _entry, _has_globals) = compile_to_ir(src, "go").unwrap();
        assert!(signatures.contains_key("identity$Int"), "signatures: {:?}", signatures.keys());
        assert!(signatures.contains_key("identity$Bool"), "signatures: {:?}", signatures.keys());
        assert!(!signatures.contains_key("identity"));

        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn nested_generic_calling_generic_monomorphizes_correctly() {
        // Exercises the tier-2 template/worklist propagation
        // specifically: `wrap`'s own body constructs `Box[T]` from ITS
        // OWN still-generic `T`, never pinned to anything concrete
        // until `go` actually calls `wrap(5)`.
        let src = "\
            struct Box[T] { val: T }\n\
            let wrap[T] (x: T): Box[T] = Box { val: x }\n\
            let go (): Int = wrap(41).val + 1\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn a_recursive_generic_function_monomorphizes_and_terminates() {
        // Proves self-recursive monomorphization terminates (the
        // worklist's "already specialized" guard makes repeated calls
        // to `len[Int]` a no-op re-lookup, not infinite re-expansion)
        // AND is correct end to end.
        let src = "\
            enum List[T] { Cons(T, List[T]), Nil }\n\
            let len[T] (lst: List[T]): Int = match lst {\n\
                Cons(h, t) => 1 + len(t),\n\
                Nil => 0,\n\
            }\n\
            let go (): Int = len(Cons(1, Cons(2, Cons(3, Nil))))\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn instantiating_a_generic_type_at_an_unsupported_concrete_type_is_a_clear_error() {
        // `Str` USED to be the example here — it was, until the
        // `Map`/`Set` stdlib chunk, genuinely unsupported as a generic
        // struct/enum field type in `plum_ir::monomorphize::
        // validate_field_type` (a stale mismatch against
        // `plum_type_to_cg_type`'s own non-generic path, which has
        // ALWAYS supported a `Str` field fine — see that function's own
        // updated doc comment). That gap is now fixed (needed for a
        // Str-keyed `Map`/`Set`), so this test now uses a genuinely
        // still-unsupported field type instead: a closure/function
        // type, which `validate_field_type` has no arm for at all.
        let src = "\
            struct Box[T] { val: T }\n\
            let go (): Int = { let f = || 1; let b = Box { val: f }; 0 }\n\
        ";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected an unsupported-instantiation error");
        assert!(
            err.contains("monomorphization") || err.contains("outside codegen's supported scope"),
            "unexpected error: {err}"
        );
    }

    // --- strings and arrays: real compile-and-run tests ---

    #[test]
    fn string_literal_concat_and_to_string_print_correctly() {
        // Exercises a string LITERAL, `.concat()`, `Int::to_string()`,
        // and a further `.concat()` of the result, all the way through
        // to the compiled entry point's own `Str`-printing path
        // (`emit_main`'s new `CgType::Str` case).
        let src = "\
            let go (): String = { let n = 42; \"answer: \".concat(n.to_string()) }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "answer: 42");
    }

    // --- Unicode string ops: real compile-and-run tests ---
    //
    // These are exactly the category of correctness property IR
    // inspection can't verify (real UTF-8 decoding, trim boundaries,
    // split piece-counts) — see `plum_codegen`'s own mechanical shape
    // tests for the IR-text-only half of this chunk's test coverage.

    #[test]
    fn runes_correctly_decodes_multi_byte_utf8_cafe() {
        // "café" is 4 CHARACTERS but 5 BYTES (the "é" is a 2-byte UTF-8
        // sequence) — `.runes()` must see 4, `.len()` must still see 5.
        let src = "let go (): Int = { let s = \"café\"; s.runes().len() * 100 + s.len() }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "405");
    }

    #[test]
    fn runes_decodes_each_codepoint_correctly_not_just_the_count() {
        // Proves the DECODED VALUES, not just the piece count — sums
        // each rune's own codepoint value via tail-recursive indexing
        // (no general array iteration yet — see the array tests below
        // for why this indexing workaround is this codebase's existing
        // precedent). 'c'=99, 'a'=97, 'f'=102, 'é'=233 (U+00E9); sum=531.
        let src = "\
            let sum_from (a: Array[Int]) (i: Int) (acc: Int): Int = \
                if i == a.len() { acc } else { sum_from(a, i + 1, acc + a[i]) }\n\
            let go (): Int = sum_from(\"café\".runes(), 0, 0)\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "531");
    }

    #[test]
    fn trim_strips_genuinely_non_ascii_unicode_whitespace() {
        // U+00A0 (NO-BREAK SPACE) and U+3000 (IDEOGRAPHIC SPACE) — real
        // Unicode `White_Space` codepoints, neither of them ASCII, on
        // EACH side, proving `@plum_is_unicode_whitespace` (not just an
        // ASCII `' '`/`'\t'`/`'\n'` check) drives both the forward and
        // backward scan.
        let src = "let go (): String = \"\u{00A0}hi\u{3000}\".trim()";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "hi");
    }

    #[test]
    fn trim_reuse_path_also_strips_non_ascii_whitespace() {
        // `s.trim()` on a bare `Var` parameter lowers to `StrTrimReuse`
        // (see `fbip::mark_reuse`) — proves the REUSE branch (`@plum_
        // str_trim_inplace`'s `@memmove`-then-shrink) is exactly as
        // correct as the fresh branch, not just structurally present.
        let src = "\
            let go (): String = { let s = \"\u{3000}hi\u{00A0}\"; s.trim() }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "hi");
    }

    #[test]
    fn to_upper_maps_real_unicode_non_ascii_letters() {
        // `é`(U+00E9) -> `É`(U+00C9) via real libc `towupper` under the
        // `C.utf8` locale `@plum_locale_init` sets — proves genuine
        // non-ASCII Unicode case mapping in actually-executed native
        // code, not just ASCII bytes.
        let src = "let go (): String = \"Hello café\".to_upper()";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "HELLO CAFÉ");
    }

    #[test]
    fn to_lower_maps_real_unicode_non_ascii_letters() {
        // The reverse direction: `É`(U+00C9) -> `é`(U+00E9) via `towlower`.
        let src = "let go (): String = \"HELLO CAFÉ\".to_lower()";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "hello café");
    }

    #[test]
    fn to_upper_still_converts_plain_ascii() {
        let src = "let go (): String = \"abc\".to_upper()";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "ABC");
    }

    #[test]
    fn to_upper_leaves_sharp_s_unchanged_documenting_the_remaining_gap() {
        // The one remaining, precisely-scoped divergence from the
        // interpreter's full Unicode `str::to_uppercase()`: German `ß`
        // expands to TWO codepoints (`"SS"`), which cannot happen
        // through `towupper`'s one-codepoint-in-one-codepoint-out C
        // signature, so `ß` passes through unchanged.
        let src = "let go (): String = \"ß\".to_upper()";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "ß");
    }

    #[test]
    fn to_upper_reuse_path_also_maps_real_unicode() {
        // `s.to_upper()` on a bare `Var` lowers to `StrToUpperReuse` —
        // proves the free-then-fresh-call reuse branch produces the
        // exact same mapped result as the fresh branch, not just the
        // same general shape.
        let src = "let go (): String = { let s = \"abc é\"; s.to_upper() }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "ABC É");
    }

    #[test]
    fn to_lower_reuse_path_also_maps_real_unicode() {
        let src = "let go (): String = { let s = \"ABC É\"; s.to_lower() }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "abc é");
    }

    /// Shared by every `.split()` test below — flattens the resulting
    /// `Array[Str]` back down to a single, directly-assertable `Str` (no
    /// general array iteration yet, same recursive-indexing workaround
    /// `array_built_via_literal_push_and_set_sums_correctly_via_
    /// recursive_indexing` already establishes) by joining every piece
    /// with a trailing `|`, so an EMPTY piece is still visibly present
    /// in the output (as two adjacent `|`s, or a leading/trailing `|`)
    /// rather than silently disappearing the way a bare space-join
    /// would hide it.
    const JOIN_FROM_HELPER: &str = "\
        let join_from (parts: Array[String]) (i: Int) (acc: String): String = \
            if i == parts.len() { acc } else { join_from(parts, i + 1, acc.concat(parts[i]).concat(\"|\")) }\n\
    ";

    #[test]
    fn split_on_an_ascii_separator() {
        let src = format!(
            "{}let go (): String = join_from(\"a,b,c\".split(\",\"), 0, \"\")\n",
            JOIN_FROM_HELPER
        );
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "a|b|c|");
    }

    #[test]
    fn split_on_a_multi_byte_separator() {
        // Separator itself (`"é"`) is a 2-byte UTF-8 sequence — proves
        // `@plum_str_count_matches`/`@plum_str_split`'s byte-level match
        // loop advances past a FULL multi-byte match, not just one byte.
        let src = format!("{}let go (): String = join_from(\"aébéc\".split(\"é\"), 0, \"\")\n", JOIN_FROM_HELPER);
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "a|b|c|");
    }

    #[test]
    fn split_with_consecutive_separators_yields_an_empty_piece() {
        let src = format!("{}let go (): String = join_from(\"a,,b\".split(\",\"), 0, \"\")\n", JOIN_FROM_HELPER);
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "a||b|");
    }

    #[test]
    fn split_with_no_match_yields_the_whole_string_as_one_piece() {
        let src = format!("{}let go (): String = join_from(\"abc\".split(\",\"), 0, \"\")\n", JOIN_FROM_HELPER);
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "abc|");
    }

    #[test]
    fn split_with_separator_at_start_and_end_yields_leading_and_trailing_empty_pieces() {
        let src = format!("{}let go (): String = join_from(\",a,\".split(\",\"), 0, \"\")\n", JOIN_FROM_HELPER);
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "|a||");
    }

    #[test]
    fn split_with_empty_separator_on_a_multi_byte_string_splits_at_every_char_boundary() {
        // Confirmed via a real `rustc` run during design (not assumed):
        // `"café".split("") == ["", "c", "a", "f", "é", ""]` — an empty
        // leading AND trailing piece, one piece per CHARACTER (not per
        // byte — "é" stays one piece, not two).
        let src = format!("{}let go (): String = join_from(\"café\".split(\"\"), 0, \"\")\n", JOIN_FROM_HELPER);
        let out = compile_and_run(&src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "|c|a|f|é||");
    }

    #[test]
    fn replace_ascii_from_and_to() {
        let src = "let go (): String = \"hello world\".replace(\"world\", \"there\")";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "hello there");
    }

    #[test]
    fn replace_multi_byte_from_shrinking_the_result() {
        // `to` shorter than `from` (`"é"`, 2 bytes -> `"e"`, 1 byte) —
        // proves the two-pass length computation in the SHRINK
        // direction.
        let src = "let go (): String = \"café au lait\".replace(\"é\", \"e\")";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "cafe au lait");
    }

    #[test]
    fn replace_growing_the_result_when_to_is_longer_than_from() {
        // `to` longer than `from` — proves the two-pass length
        // computation in the GROW direction (and, since `"ab"` is a bare
        // string LITERAL rather than a `Var`, exercises the FRESH path,
        // not `StrReplaceReuse`).
        let src = "let go (): String = \"ab\".replace(\"a\", \"XYZ\")";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "XYZb");
    }

    #[test]
    fn replace_reuse_path_also_grows_correctly() {
        // `s.replace(..)` on a bare `Var` lowers to `StrReplaceReuse` —
        // proves the documented "still fresh-allocates, but frees the
        // old cell directly" reuse path (see `codegen.rs`'s
        // `StrReplaceReuse` arm) produces the SAME correct, grown result
        // as the ordinary fresh path.
        let src = "let go (): String = { let s = \"ab\"; s.replace(\"a\", \"XYZ\") }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "XYZb");
    }

    #[test]
    fn replace_with_an_empty_from_inserts_at_every_char_boundary() {
        // Confirmed via a real `rustc` run during design (not assumed):
        // `"abc".replace("", "-") == "-a-b-c-"` — `to` inserted at EVERY
        // character boundary, N+1 times for an N-character string, the
        // SAME char-boundary logic `.split("")` uses.
        let src = "let go (): String = \"abc\".replace(\"\", \"-\")";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "-a-b-c-");
    }

    #[test]
    fn array_built_via_literal_push_and_set_sums_correctly_via_recursive_indexing() {
        // No `for` iteration yet (general array iteration is explicitly
        // deferred — see DESIGN.md/the plan) — summed via a small
        // TAIL-recursive direct-indexing wrapper instead, exactly the
        // workaround the plan calls for.
        let src = "\
            let sum_from (a: Array[Int]) (i: Int) (acc: Int): Int = \
                if i == a.len() { acc } else { sum_from(a, i + 1, acc + a[i]) }\n\
            let go (): Int = { \
                let a = [1, 2].push(3).set(0, 10); \
                sum_from(a, 0, 0) \
            }\n\
        ";
        // a = [1, 2, 3] after push, then set(0, 10) -> [10, 2, 3] -> sum 15
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "15");
    }

    #[test]
    fn array_push_reuse_is_correct_end_to_end_on_a_real_reuse_eligible_program() {
        // `grow`'s `a.push(v)` is exactly FBIP's reuse-in-place shape
        // (bare-`Var` receiver, uniquely owned by the time it's called
        // here) — `plum-codegen`'s own unit test already verifies the
        // reuse-vs-fresh branch SHAPE structurally; this proves it
        // actually EXECUTES correctly end to end.
        let src = "\
            let grow (a: Array[Int]) (v: Int): Array[Int] = a.push(v)\n\
            let sum_from (a: Array[Int]) (i: Int) (acc: Int): Int = \
                if i == a.len() { acc } else { sum_from(a, i + 1, acc + a[i]) }\n\
            let go (): Int = { let a = grow([1, 2, 3], 4); sum_from(a, 0, 0) }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "10");
    }

    #[test]
    fn nested_heap_array_elements_are_refcounted_correctly_when_popped() {
        // `Array[List[Int]]` — proves `@plum_rc_dec_array_Heap` (the
        // array release function for a HEAP-shaped, tag-dispatched
        // element type) composes correctly with the existing `@plum_rc_
        // dec`/`@plum_release_fields` machinery: popping one off leaves
        // a SHORTER array that still needs every SURVIVING element
        // correctly incremented (see `inc_copied_array_elements`), and
        // the popped-off list's own refcount must still end up correct
        // (accessed independently below, proving it wasn't
        // double-freed/leaked into an inconsistent state).
        let src = "\
            enum List { Cons(Int, List), Nil }\n\
            let sum (lst: List): Int = match lst {\n\
                Cons(h, t) => h + sum(t),\n\
                Nil => 0,\n\
            }\n\
            let go (): Int = {\n\
                let a = [Cons(1, Nil), Cons(2, Nil), Cons(3, Nil)];\n\
                let popped = a.pop();\n\
                sum(popped[0]) + sum(popped[1]) + sum(a[2])\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "6");
    }

    #[test]
    fn a_runtime_array_bounds_check_failure_exits_non_zero() {
        // `a[10]` on a length-3 array — a RUNTIME-checked failure (see
        // `plum_codegen::codegen`'s `emit_runtime_check`/`@plum_abort`),
        // never a silent wraparound or a Rust-level panic. Asserted via
        // the compiled BINARY's own non-zero exit status, not a
        // `Result::Err` — codegen itself succeeds; it's the COMPILED
        // PROGRAM that fails at runtime.
        let src = "let go (): Int = { let a = [1, 2, 3]; a[10] }";
        let err = compile_and_run(src, "go", &[CgValue::Unit]).expect_err("expected the compiled binary to exit non-zero");
        assert!(err.contains("non-zero"), "unexpected error: {err}");
    }

    // --- closures: real compile-and-run tests ---

    #[test]
    fn closure_capturing_a_scalar_called_immediately_runs_correctly() {
        let src = "\
            let go (): Int = {\n\
                let n = 10;\n\
                let f = |x| x + n;\n\
                f(5)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "15");
    }

    #[test]
    fn closure_capturing_a_heap_value_passed_to_a_higher_order_function_reads_it_correctly() {
        // The CRUX correctness test for this whole chunk: `b` (a HEAP-
        // shaped struct) is captured by `f`, which is then passed to
        // `apply` (a genuinely separate function, called through an
        // INDIRECT call) and invoked there — this only produces the
        // right answer if the captured reference is still genuinely
        // live and correctly readable from inside the closure body at
        // the point `apply` actually calls it, not a stale/freed/
        // uninitialized value (a use-after-free would typically show up
        // as garbage output or a crash, not silently wrong-but-
        // plausible output).
        let src = "\
            struct Box { val: Int }\n\
            let apply (f: (Int) -> Int) (x: Int): Int = f(x)\n\
            let go (): Int = {\n\
                let b = Box { val: 100 };\n\
                let f = |x| x + b.val;\n\
                apply(f, 1)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "101");
    }

    #[test]
    fn self_referential_recursive_local_closure_fib_computes_correctly() {
        let src = "\
            let go (): Int = {\n\
                let fib = |n| if n < 2 { n } else { fib(n - 1) + fib(n - 2) };\n\
                fib(10)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "55");
    }

    #[test]
    fn a_closure_returned_from_a_function_still_works_after_its_creating_scope_is_gone() {
        // Genuinely exploratory (per the plan this chunk implements):
        // this exact escape pattern — a closure created inside one
        // function, returned, and called only AFTER that function has
        // already returned (its own stack frame gone) — is untested
        // anywhere else in this project, including the interpreter.
        // Only passes if the captured `n` was copied into the closure's
        // OWN heap cell at creation time, genuinely independent of
        // `make_adder`'s now-defunct stack frame.
        let src = "\
            let make_adder (n: Int): (Int) -> Int = |x| x + n\n\
            let go (): Int = {\n\
                let add5 = make_adder(5);\n\
                add5(10)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "15");
    }

    #[test]
    fn a_heap_capturing_closure_called_many_times_still_produces_correct_results() {
        // Regression guard for the fbip.rs fix that stopped planting an
        // extra `Inc` on every static mention of a captured heap value
        // INSIDE a closure body (which used to fire once per CALL, an
        // unbounded leak proportional to call count). This doesn't
        // detect the leak's absence directly (no leak-detection infra
        // in this project — the real proof is the fbip.rs unit test
        // asserting no Inc node is emitted at all), but confirms the
        // fix didn't break correctness across many repeated calls to
        // the same heap-capturing closure (direct repeated calls, since
        // general `for`-iteration is still out of codegen's scope).
        let src = "\
            struct Box { val: Int }\n\
            let go (): Int = {\n\
                let b = Box { val: 7 };\n\
                let f = |x| x + b.val;\n\
                f(1) + f(2) + f(3) + f(4) + f(5) + f(6) + f(7) + f(8) + f(9) + f(10)\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "125");
    }

    #[test]
    fn a_closure_literal_inside_a_generic_function_body_works_independently_per_instantiation() {
        // Was a hard rejection (see this test's own git history) before
        // `plum_types::Infer::resolve_closure_types` gained a template
        // fallback and `monomorphize.rs` learned to substitute a
        // closure's own type per instantiation — now a real success
        // path. `wrap` gets called at TWO different concrete types in
        // the SAME program: a plain scalar (`Int`) and a heap-shaped
        // struct (`Box`), so this also proves capture refcounting
        // (the closure captures nothing here, but `x` itself flows
        // through the closure call and back out) works correctly and
        // independently per instantiation, not just for scalars.
        let src = "\
            struct Box { val: Int }\n\
            let wrap[T] (x: T): T = { let f = |y| y; f(x) }\n\
            let go (): Int = {\n\
                let a = wrap(5);\n\
                let b = wrap(Box { val: 42 });\n\
                a + b.val\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "47");
    }

    // --- a `Global` initializer calling a still-generic function ---

    #[test]
    fn a_global_initializer_calling_a_generic_function_works() {
        let src = "\
            let make[T] (x: T): T = x\n\
            let g = make(5)\n\
            let go (): Int = g\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "5");
    }

    #[test]
    fn a_global_initializer_calling_a_generic_function_works_alongside_a_heap_shaped_instantiation() {
        // Proves both the scalar-global case AND heap-shaped refcounting
        // soundness in one compiled program, WITHOUT combining them the
        // one specific way that hits a genuine, narrower, documented
        // structural limit found while building this chunk: a function
        // can't do FIELD ACCESS on a global whose value came from
        // calling a still-generic function (that global's concrete
        // shape isn't knowable during Phase 2 — see `plum-types`'s own
        // regression test, `a_later_global_can_do_field_access_on_an_
        // earlier_global_that_called_a_generic_function`). Field access
        // on the heap-shaped instantiation happens on a plain LOCAL
        // (`b`, built by calling `make` directly inside `go`), not on a
        // global, so it's unaffected.
        let src = "\
            struct Box { val: Int }\n\
            let make[T] (x: T): T = x\n\
            let g = make(5)\n\
            let go (): Int = { let b = make(Box { val: 42 }); g + b.val }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "47");
    }

    #[test]
    fn a_generic_function_called_from_a_global_and_from_a_function_at_different_types_both_work_when_compiled_and_run() {
        // The actual soundness bug this chunk fixes, proven end-to-end:
        // `identity` is instantiated at Int (from the global `g`) AND at
        // Bool (from `go`'s own body) in the SAME compiled program —
        // before the `plum-types` phase-ordering fix, the global's call
        // permanently pinned `identity`'s type variable to Int, making
        // the Bool call a type error that should never have been one.
        let src = "\
            let identity[T] (x: T): T = x\n\
            let g = identity(5)\n\
            let go (): Int = if identity(true) { g } else { 0 }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "5");
    }

    // --- general array iteration (`for`, `Assign`, `.map`/`.filter`/`.fold`) ---

    #[test]
    fn a_large_non_mutating_range_for_loop_does_not_crash_or_corrupt_surrounding_state() {
        // The loop-shape-at-scale proxy, mirroring `deep_tail_recursion_
        // does_not_stack_overflow`'s own role for `musttail`: a million
        // real iterations through the loop header's phi machinery, then
        // ordinary code AFTER the loop still produces the right answer
        // — proof the loop itself neither crashed nor corrupted
        // anything reachable after it finished.
        let src = "let go () = { for i in 0..1000000 { i }; 42 }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn the_canonical_sum_accumulator_for_loop_produces_the_correct_sum() {
        let src = "let go () = { let mut sum = 0; for i in 0..10 { sum = sum + i; }; sum }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "45");
    }

    #[test]
    fn for_x_in_arr_iterates_a_real_array_not_just_a_literal_range() {
        let src = "let go () = { let mut sum = 0; let arr = [10, 20, 30]; for x in arr { sum = sum + x; }; sum }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "60");
    }

    #[test]
    fn array_map_produces_the_correct_transformed_elements() {
        let src = "let go () = [1, 2, 3].map(|x| x * 2).fold(0, |acc, x| acc + x)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "12");
    }

    #[test]
    fn array_filter_keeps_only_the_matching_elements_with_correct_values() {
        let src = "let go () = [1, 2, 3, 4, 5].filter(|x| x > 2).fold(0, |acc, x| acc + x)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "12");
    }

    #[test]
    fn array_fold_produces_the_correct_accumulated_value() {
        let src = "let go () = [1, 2, 3, 4].fold(0, |acc, x| acc + x)";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "10");
    }

    #[test]
    fn a_nested_loop_accumulator_sums_i_times_j_over_two_ranges() {
        // Proves the nested-loop design end to end: each loop level
        // gets its OWN independent header phi for the shared
        // accumulator (see `codegen_for`'s doc comment) — this only
        // produces the right VALUE if both levels' phis are wired up
        // correctly, not just well-formed LLVM IR.
        // sum_{i=0..3} sum_{j=0..3} i*j == (0+1+2) * (0+1+2) == 9.
        let src = "let go () = { let mut total = 0; for i in 0..3 { for j in 0..3 { total = total + i * j; }; }; total }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "9");
    }

    // --- `plumc build`: resolve_project -> compile_program_to_ir -> emit_main -> compile_ir_to_binary ---

    /// Mirrors `run_build` in `main.rs` exactly (see that function) —
    /// duplicated here rather than shared, since `main.rs`'s own
    /// version also handles CLI-level concerns (arg parsing, `eprintln!`
    /// + `exit(1)`) that don't belong in a test helper. Returns the
    /// persisted binary's own stdout, run via `std::process::Command`
    /// — the real end-to-end proof this pipeline actually produces a
    /// working native executable, not just that codegen succeeds.
    fn build_and_run_project(root: &std::path::Path, out_path: &std::path::Path) -> Result<String, String> {
        let program = crate::project::resolve_project(root)?;
        let (body_ir, signatures, resolved_entry, has_globals) = compile_program_to_ir(&program, "main")?;
        let sig = signatures
            .get(&resolved_entry)
            .ok_or_else(|| "codegen: no such function \"main\"".to_string())?
            .clone();
        if sig.params.len() != 1 {
            return Err(format!(
                "codegen: \"main\" must take exactly one Unit parameter, found {} parameter(s)",
                sig.params.len()
            ));
        }
        reject_unprintable_return("main", sig.ret.clone())?;
        let main_ir = emit_main(&resolved_entry, sig.ret, &[CgValue::Unit], has_globals);
        let full_ir = format!("{body_ir}\n{main_ir}");
        compile_ir_to_binary(&full_ir, out_path)?;

        let run = Command::new(out_path)
            .output()
            .map_err(|e| format!("failed to run built binary {out_path:?}: {e}"))?;
        if !run.status.success() {
            return Err(format!(
                "built program exited with a non-zero status: {:?}\nstdout: {}\nstderr: {}",
                run.status.code(),
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
    }

    #[test]
    fn plumc_build_compiles_and_runs_a_multi_module_project() {
        // Mirrors `project.rs`'s own `a_multi_directory_project_
        // resolves_across_modules` fixture exactly (cross-module struct
        // + function call) — the real proof that a multi-file project
        // resolved via `resolve_project` compiles and links through the
        // LLVM backend, not just the interpreter path.
        let project = crate::test_util::TempProject::new();
        project.write("shapes/circle.plum", "pub struct Circle { radius: Float }");
        project.write(
            "shapes/area.plum",
            "pub let area (c: Circle): Float = c.radius * c.radius * 3.0",
        );
        project.write(
            "main.plum",
            r#"
            use shapes;
            let main (): Float = shapes.area(shapes.Circle { radius: 2.0 })
            "#,
        );
        let out_bin = project.path.join("built-multi-module");

        let out = build_and_run_project(&project.path, &out_bin).unwrap();
        assert_eq!(out, "12.000000");
    }

    #[test]
    fn plumc_build_compiles_and_runs_a_single_file_project() {
        let project = crate::test_util::TempProject::new();
        project.write("main.plum", "let main (): Int = 20 + 22");
        let out_bin = project.path.join("built-single-file");

        let out = build_and_run_project(&project.path, &out_bin).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn plumc_build_rejects_a_main_returning_an_unsupported_type() {
        let project = crate::test_util::TempProject::new();
        project.write(
            "main.plum",
            "struct Point { x: Int, y: Int }\nlet main (): Point = Point { x: 1, y: 2 }",
        );
        let out_bin = project.path.join("should-not-be-built");

        let err = build_and_run_project(&project.path, &out_bin)
            .expect_err("expected a clear build-time error, not a panic or a confusing clang failure");
        assert!(err.contains("heap-shaped value"), "unexpected error: {err}");
        assert!(!out_bin.exists(), "no binary should have been written for a rejected build");
    }

    // --- channels / select (this chunk) ---

    #[test]
    fn channel_scalar_send_recv_round_trips_single_threaded() {
        let src = "let go (): Int = { let (tx, rx) = channel[Int](); tx.send(42); rx.recv() }";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn channel_heap_values_at_scale_single_threaded() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
              let (tx, rx) = channel[Point]();\n\
              for i in 0..1000 { tx.send(Point { x: i, y: i * 2 }); };\n\
              let mut acc = 0;\n\
              for i in 0..1000 { acc = acc + (match rx.recv() { Point(a, b) => a + b }); };\n\
              acc\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        let expected: i64 = (0..1000i64).map(|i| i + i * 2).sum();
        assert_eq!(out, expected.to_string());
    }

    #[test]
    fn channel_heap_values_at_scale_across_a_real_spawned_thread() {
        // Unlike the single-threaded variant above, the sender runs on
        // a REAL spawned OS thread while the main thread receives — the
        // shape the plan flags as needing the highest scrutiny (a
        // shared, mutex-guarded queue genuinely touched from two
        // different threads, not just exercised for its own text
        // shape).
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
              let (tx, rx) = channel[Point]();\n\
              let t = spawn { for i in 0..1000 { tx.send(Point { x: i, y: i * 2 }); }; 0 };\n\
              let mut acc = 0;\n\
              for i in 0..1000 { acc = acc + (match rx.recv() { Point(a, b) => a + b }); };\n\
              t.join();\n\
              acc\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        let expected: i64 = (0..1000i64).map(|i| i + i * 2).sum();
        assert_eq!(out, expected.to_string());
    }

    #[test]
    fn channel_spawn_and_join_combined_mirrors_the_interpreters_own_point_test() {
        // The exact same shape as `plum_interp`'s own `a_heap_shaped_
        // value_crosses_a_real_thread_boundary_via_a_channel` test —
        // `tx` crosses INTO a spawned thread (the SAME captured-
        // environment mechanism `spawn` already uses), the spawned task
        // sends a heap value on it, and the ORIGINAL thread receives.
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
              let (tx, rx) = channel[Point]();\n\
              let t = spawn { tx.send(Point { x: 5, y: 6 }) };\n\
              let p = rx.recv();\n\
              t.join();\n\
              match p { Point(a, b) => a + b }\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "11");
    }

    #[test]
    fn select_picks_arm_0_when_it_is_the_one_ready() {
        let src = "\
            let go (): Int = {\n\
              let (tx1, rx1) = channel[Int]();\n\
              let (tx2, rx2) = channel[Int]();\n\
              tx1.send(7);\n\
              select { v = rx1.recv() => v, w = rx2.recv() => w }\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn select_picks_arm_1_when_it_is_the_one_ready() {
        // The direct proxy that the poll loop doesn't just always
        // return arm 0's result — here only arm 1's channel ever gets
        // a value.
        let src = "\
            let go (): Int = {\n\
              let (tx1, rx1) = channel[Int]();\n\
              let (tx2, rx2) = channel[Int]();\n\
              tx2.send(99);\n\
              select { v = rx1.recv() => v, w = rx2.recv() => w }\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "99");
    }

    #[test]
    fn select_combined_with_spawn_and_join() {
        // `select` genuinely BLOCKS (busy-polls) until the spawned
        // thread's send lands — arm 1's channel never gets anything.
        let src = "\
            let go (): Int = {\n\
              let (tx1, rx1) = channel[Int]();\n\
              let (tx2, rx2) = channel[Int]();\n\
              let t = spawn { tx1.send(55) };\n\
              let result = select { v = rx1.recv() => v, w = rx2.recv() => w };\n\
              t.join();\n\
              result\n\
            }\n\
        ";
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "55");
    }

    /// The single highest-risk test in this whole chunk: FOUR real OS
    /// threads concurrently `.send()`ing onto ONE shared channel queue
    /// — the exact multi-writer shape a lost update to `tail` (an
    /// unsynchronized race between concurrent senders) would corrupt.
    /// Each producer's value RANGE is disjoint (`0..100`, `1000..1100`,
    /// `2000..2100`, `3000..3100`) specifically so a corrupted/lost
    /// node would show up as a wrong SUM, not just a coincidentally-
    /// still-plausible one.
    fn many_producer_src() -> String {
        "\
            let go (): Int = {\n\
              let (tx, rx) = channel[Int]();\n\
              let t0 = spawn { for i in 0..100 { tx.send(i); }; 0 };\n\
              let t1 = spawn { for i in 0..100 { tx.send(1000 + i); }; 0 };\n\
              let t2 = spawn { for i in 0..100 { tx.send(2000 + i); }; 0 };\n\
              let t3 = spawn { for i in 0..100 { tx.send(3000 + i); }; 0 };\n\
              let mut acc = 0;\n\
              for i in 0..400 { acc = acc + rx.recv(); };\n\
              t0.join();\n\
              t1.join();\n\
              t2.join();\n\
              t3.join();\n\
              acc\n\
            }\n\
        "
            .to_string()
    }

    fn many_producer_expected() -> i64 {
        [0i64, 1000, 2000, 3000]
            .iter()
            .map(|base| (0..100i64).map(|i| base + i).sum::<i64>())
            .sum()
    }

    #[test]
    fn many_producers_concurrently_sending_to_one_channel_sums_correctly() {
        let out = compile_and_run(&many_producer_src(), "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, many_producer_expected().to_string());
    }

    /// The ASan variant of the many-producer test — same program,
    /// compiled with `-fsanitize=address` and RUN (not just compiled),
    /// same rationale as `spawn_many_tasks_..._under_asan` above: ASan
    /// instruments every heap access at the machine-code level, so a
    /// lost/corrupted queue node (a stray write into freed memory, a
    /// double-free of a node popped by two racing `recv`s, ...) would
    /// abort loudly with a diagnostic here even if the SUM happened to
    /// still come out right by coincidence.
    #[test]
    fn many_producers_concurrently_sending_to_one_channel_under_asan() {
        run_under_sanitizer("-fsanitize=address", "plumc-channel-asan", &many_producer_expected().to_string());
    }

    /// The ThreadSanitizer variant — unlike ASan (heap corruption),
    /// TSan specifically instruments every memory access to detect
    /// DATA RACES directly: two threads touching the same memory
    /// without a happens-before edge between them, exactly the hazard
    /// class a mutex-guarded multi-producer queue is vulnerable to if
    /// the mutex ever failed to actually serialize `head`/`tail`
    /// mutation. Explicitly requested by the plan as a SEPARATE run
    /// from ASan, not a substitute for it.
    #[test]
    fn many_producers_concurrently_sending_to_one_channel_under_tsan() {
        run_under_sanitizer("-fsanitize=thread", "plumc-channel-tsan", &many_producer_expected().to_string());
    }

    /// The ASan variant of the cross-thread heap-values-at-scale test —
    /// same rationale as the many-producer sanitizer tests above, for
    /// the single-producer/single-consumer shape.
    #[test]
    fn channel_heap_values_at_scale_across_a_spawned_thread_under_asan() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
              let (tx, rx) = channel[Point]();\n\
              let t = spawn { for i in 0..1000 { tx.send(Point { x: i, y: i * 2 }); }; 0 };\n\
              let mut acc = 0;\n\
              for i in 0..1000 { acc = acc + (match rx.recv() { Point(a, b) => a + b }); };\n\
              t.join();\n\
              acc\n\
            }\n\
        ";
        let expected: i64 = (0..1000i64).map(|i| i + i * 2).sum();
        run_under_sanitizer_with_src(src, "-fsanitize=address", "plumc-channel-heap-asan", &expected.to_string());
    }

    /// The TSan variant of the same cross-thread heap-values-at-scale
    /// test.
    #[test]
    fn channel_heap_values_at_scale_across_a_spawned_thread_under_tsan() {
        let src = "\
            struct Point { x: Int, y: Int }\n\
            let go (): Int = {\n\
              let (tx, rx) = channel[Point]();\n\
              let t = spawn { for i in 0..1000 { tx.send(Point { x: i, y: i * 2 }); }; 0 };\n\
              let mut acc = 0;\n\
              for i in 0..1000 { acc = acc + (match rx.recv() { Point(a, b) => a + b }); };\n\
              t.join();\n\
              acc\n\
            }\n\
        ";
        let expected: i64 = (0..1000i64).map(|i| i + i * 2).sum();
        run_under_sanitizer_with_src(src, "-fsanitize=thread", "plumc-channel-heap-tsan", &expected.to_string());
    }

    /// Shared "compile with `-fsanitize=<kind>`, run, assert clean exit
    /// + expected stdout" helper — factored out of the hand-rolled ASan
    /// block `spawn_many_tasks_..._under_asan` above (which predates
    /// this chunk) so the many-producer/heap-at-scale sanitizer tests
    /// don't each duplicate the same compile-and-run boilerplate.
    /// `detect_leaks=0` (ASan only — irrelevant/unrecognized by TSan,
    /// harmless to still set) isolates genuine memory CORRUPTION from
    /// plain leaks: this backend's channel queue struct/nodes are a
    /// documented, deliberate permanent leak (see `emit_channel_
    /// runtime`'s own doc comment), not a soundness gap — a leak-
    /// detector failure here is EXPECTED, not a signal of anything new.
    fn run_under_sanitizer(sanitizer_flag: &str, dir_prefix: &str, expected_stdout: &str) {
        run_under_sanitizer_with_src(&many_producer_src(), sanitizer_flag, dir_prefix, expected_stdout);
    }

    fn run_under_sanitizer_with_src(src: &str, sanitizer_flag: &str, dir_prefix: &str, expected_stdout: &str) {
        let (body_ir, signatures, resolved_entry, has_globals) = compile_to_ir(src, "go").unwrap();
        let sig = signatures.get(&resolved_entry).unwrap().clone();
        let main_ir = emit_main(&resolved_entry, sig.ret, &[CgValue::Unit], has_globals);
        let full_ir = format!("{body_ir}\n{main_ir}");

        let dir = unique_temp_dir(dir_prefix);
        std::fs::create_dir_all(&dir).unwrap();
        let ll_path = dir.join("program.ll");
        let bin_path = dir.join("program-sanitized");
        std::fs::write(&ll_path, &full_ir).unwrap();

        let compile = Command::new("clang")
            .arg(sanitizer_flag)
            .arg("-pthread")
            .arg(&ll_path)
            .arg("-o")
            .arg(&bin_path)
            .output()
            .unwrap_or_else(|e| panic!("could not run clang with {sanitizer_flag}: {e}"));
        if !compile.status.success() {
            panic!(
                "clang {sanitizer_flag} failed to compile the generated IR:\n{}",
                String::from_utf8_lossy(&compile.stderr)
            );
        }

        let run = Command::new(&bin_path)
            .env("ASAN_OPTIONS", "detect_leaks=0")
            .output()
            .unwrap_or_else(|e| panic!("failed to run the {sanitizer_flag}-instrumented binary: {e}"));
        let stdout = String::from_utf8_lossy(&run.stdout).trim_end().to_string();
        let stderr = String::from_utf8_lossy(&run.stderr).to_string();
        assert!(
            run.status.success(),
            "{sanitizer_flag}-instrumented binary reported a failure (a real bug, not a hang or timeout):\n\
             stdout: {stdout}\nstderr: {stderr}"
        );
        assert!(!stderr.contains("ERROR: AddressSanitizer"), "ASan flagged an error:\n{stderr}");
        assert!(!stderr.contains("WARNING: ThreadSanitizer"), "TSan flagged a data race:\n{stderr}");
        assert_eq!(stdout, expected_stdout);
    }

    // --- FFI: real compile-and-run tests ---

    #[test]
    fn a_real_sqrt_extern_call_compiles_and_runs() {
        // `cbrt`, not `sqrt` — `sqrt` is now declared by the prelude's
        // own `STDLIB_NUMBER_SRC` (backing `float_sqrt`), so a user
        // program can no longer redeclare it itself.
        let src = r#"
            extern "C" {
                fn cbrt(x: Float) -> Float;
            }
            let go (): Float = unsafe { cbrt(1728.0) }
        "#;
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "12.000000");
    }

    #[test]
    fn a_real_strlen_based_cstr_round_trip_compiles_and_runs() {
        // `.as_cstr()` validates + copies "hello world" into a fresh,
        // unrefcounted `CStr` buffer; the real libc `strlen` measures it
        // across the actual FFI boundary — proving `AsCStr`'s codegen
        // (embedded-NUL check, fresh malloc+memcpy+NUL-store, and the
        // ownership-discharge dec on the original `Str` cell) all
        // produce a buffer real C code can correctly walk.
        let src = r#"
            extern "C" {
                fn strlen(s: CStr) -> Int;
            }
            let go (): Int = unsafe { strlen("hello world".as_cstr()) }
        "#;
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        assert_eq!(out, "11");
    }

    // NOTE: no real-compile-and-run test exercises `.as_cstr()`'s
    // embedded-NUL abort here — `plum-syntax`'s lexer has no string-
    // literal escape that produces a real embedded NUL byte (`\0` lexes
    // to the literal character `'0'`, not `'\0'`; see `lex_string`), so
    // there's no way to express one in actual Plum SOURCE TEXT for this
    // source-driven test harness to compile. The check itself IS
    // exercised directly: `plum_codegen`'s own unit test (`as_cstr_
    // validates_copies_and_decs_the_original_str_register`, which
    // asserts the `@memchr`/`@plum_abort` shape) and, at the interpreter
    // parity level, `plum_interp::as_cstr_rejects_a_string_with_an_
    // embedded_null_byte` (built directly via `ir::Expr::Str("a\0b")`,
    // bypassing the lexer entirely, exactly because Rust source CAN
    // embed a real NUL where Plum source can't).

    #[test]
    fn a_real_extern_call_returning_a_null_string_pointer_aborts_at_runtime() {
        // `getenv` on a variable that (almost certainly) isn't set
        // returns a real NULL `char*` — proving the NULL-return
        // `@plum_abort` runtime check fires against an ACTUAL libc call,
        // not just a hand-built IR assertion. `getenv`'s own extern
        // signature is declared `-> CStr` (the only surface spelling
        // `plum-types` accepts for an extern return at all — see
        // DESIGN.md's "Type scope" note); the returned value is
        // discarded immediately (`let _ = ..`) since `Type::CStr` has no
        // further operations of its own and `go`'s own return stays
        // `Int` so `reject_unprintable_return` doesn't reject the ENTRY
        // POINT itself before the real abort ever gets a chance to fire.
        let src = r#"
            extern "C" {
                fn getenv(name: CStr) -> CStr;
            }
            let go (): Int = unsafe {
                let ignored = getenv("PLUM_CODEGEN_FFI_TEST_VAR_DEFINITELY_UNSET_XYZ".as_cstr());
                0
            }
        "#;
        let err = compile_and_run(src, "go", &[CgValue::Unit])
            .expect_err("expected a null string return from a real extern call to abort at runtime");
        assert!(err.contains("null string pointer") || err.contains("non-zero status"), "unexpected error: {err}");
    }

    /// A tiny, self-contained C helper proving the `Bool` ABI conversion
    /// is correct in BOTH directions via REAL native code, not just
    /// inspected IR text: `ffitest_bool_widen_check` proves the
    /// ARGUMENT direction (`zext i1 to i32` produces EXACTLY C's `1`,
    /// never some garbage-upper-bits pattern a naive width mismatch
    /// could produce); `ffitest_bool_return_nonzero` deliberately
    /// returns `2`, not `1` — proving the RETURN direction reads C's
    /// "any nonzero value is true" convention correctly (`icmp ne i32 ..,
    /// 0`), since a buggy `trunc i32 .. to i1` implementation would read
    /// `2`'s low bit (`0`) as `false` and silently produce the WRONG
    /// answer.
    const BOOL_WIDTH_C_HELPER: &str = r#"
        int ffitest_bool_widen_check(int x) {
            return x == 1 ? 1 : 0;
        }
        int ffitest_bool_return_nonzero(void) {
            return 2;
        }
    "#;

    #[test]
    fn bool_width_round_trips_through_a_real_c_abi_boundary() {
        let src = r#"
            extern "C" {
                fn ffitest_bool_widen_check(x: Bool) -> Bool;
                fn ffitest_bool_return_nonzero() -> Bool;
            }
            let go (): Bool = unsafe {
                if ffitest_bool_widen_check(true) {
                    ffitest_bool_return_nonzero()
                } else {
                    false
                }
            }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], BOOL_WIDTH_C_HELPER).unwrap();
        // `emit_main`'s Bool case prints via `zext i1 to i32` + `%d` —
        // `"1"` here is only reachable if BOTH the argument-direction
        // `zext` and the return-direction `icmp ne` are correct: a
        // `trunc`-based return bug would make `plum_test_bool_return_
        // nonzero`'s real `2` read as `false`, taking the `else` branch
        // and printing `"0"` instead.
        assert_eq!(out, "1");
    }

    /// A tiny C helper invoking a Plum-supplied function pointer
    /// SYNCHRONOUSLY, during the call — exactly the shape of a typical C
    /// callback API (a comparator, a visitor). No real libc function has
    /// a narrow enough `(Int, Int) -> Int`-shaped signature to prove
    /// this end-to-end (the same wall `plum-interp`'s own test suite
    /// hit — see `call_with_10_and_20` there), so this stands in for one.
    const CALLBACK_C_HELPER: &str = r#"
        long long ffitest_apply(long long (*f)(long long, long long), long long a, long long b) {
            return f(a, b);
        }
    "#;

    #[test]
    fn a_real_c_callback_invocation_round_trips_through_native_code() {
        // Proves something no existing test in this codebase (the
        // interpreter included) has ever proven: a REAL C function,
        // compiled and linked as a genuinely separate native translation
        // unit, calling BACK into compiled Plum code through a real
        // function-pointer value — not just a structural "the trampoline
        // has the right shape" assertion, an actual successful round
        // trip through native machine code.
        let src = r#"
            extern "C" {
                fn ffitest_apply(f: (Int, Int) -> Int, a: Int, b: Int) -> Int;
            }
            let add (a: Int) (b: Int): Int = a + b
            let go (): Int = unsafe { ffitest_apply(add, 10, 32) }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], CALLBACK_C_HELPER).unwrap();
        assert_eq!(out, "42");
    }

    #[test]
    fn a_c_callback_that_receives_and_returns_bool_round_trips_correctly() {
        // The callback-trampoline counterpart to `bool_width_round_
        // trips_through_a_real_c_abi_boundary`: proves the TRAMPOLINE's
        // OWN `icmp ne i32 %p, 0` (converting an incoming C `Bool`
        // parameter to Plum's `i1`) and `zext i1 %r to i32` (converting
        // the Plum function's `i1` result back to C's `Bool`) are BOTH
        // correct, via a real native call through a real C function
        // pointer — not just the argument/return marshaling at the
        // OUTER `ExternCall` site (already covered above).
        let c_helper = r#"
            int ffitest_apply_bool(int (*f)(int), int x) {
                return f(x);
            }
        "#;
        let src = r#"
            extern "C" {
                fn ffitest_apply_bool(f: (Bool) -> Bool, x: Bool) -> Bool;
            }
            let negate (x: Bool): Bool = !x
            let go (): Bool = unsafe { ffitest_apply_bool(negate, true) }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], c_helper).unwrap();
        assert_eq!(out, "0");
    }

    // --- FFI: struct-by-value, real compile-and-run tests ---
    //
    // These are the crux correctness tests for struct-by-value FFI: a
    // real `clang`-compiled, LLVM-classified native ABI round trip, the
    // only thing actually capable of catching a codegen bug that "looks
    // right" in inspected IR text but is byte-misaligned once compiled
    // (a named LLVM aggregate type carries no explicit offset text at
    // all — see `plum_codegen::collect_extern_struct_types`'s own doc
    // comment) — mirroring how earlier FFI chunks used a real ASan run
    // as their own crux proof.

    /// A `Point{Int, Float}` C helper — the simplest two-DIFFERENT-
    /// scalar-width struct shape, proving both the argument direction
    /// (Plum `Ctor` -> LLVM aggregate -> real C struct) and the return
    /// direction (real C struct -> LLVM aggregate -> fresh Plum `Ctor`)
    /// round-trip correctly through actual native code.
    const POINT_C_HELPER: &str = r#"
        typedef struct { long long x; double y; } Point;
        Point ffitest_make_point(long long x, double y) {
            Point p;
            p.x = x;
            p.y = y;
            return p;
        }
        long long ffitest_point_sum(Point p) {
            return p.x + (long long)p.y;
        }
    "#;

    #[test]
    fn point_struct_argument_and_return_round_trip_through_a_real_c_abi_boundary() {
        let src = r#"
            struct Point { x: Int, y: Float }
            extern "C" {
                fn ffitest_make_point(x: Int, y: Float) -> Point;
                fn ffitest_point_sum(p: Point) -> Int;
            }
            let go (): Int = unsafe {
                let p = ffitest_make_point(3, 4.5);
                ffitest_point_sum(p)
            }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], POINT_C_HELPER).unwrap();
        // 3 + (long long)4.5 == 3 + 4 == 7 — only reachable if BOTH the
        // `Int`->`i64` and `Float`->`double` fields round-tripped through
        // the real C struct at their correct offsets in both directions.
        assert_eq!(out, "7");
    }

    /// The deliberately-padding-inducing `Mixed{Bool, Int}` C helper —
    /// `int flag` (4 bytes) followed by `long long big` (8 bytes) forces
    /// 4 bytes of real System V padding between the two fields, exactly
    /// the shape verified empirically (via a real `clang` compile)
    /// during this feature's planning. `ffitest_make_mixed_nonzero`
    /// deliberately returns `flag = 2`, not `1` — proving the RETURN-
    /// direction struct-field `Bool` normalization is `icmp ne i32 ..,
    /// 0` (C's "any nonzero is true" convention), NOT a bare truncating
    /// `zext`/`trunc`: a buggy bare-truncation implementation would read
    /// `2`'s low bit (`0`) as `false`, take the `else` branch below, and
    /// print `-1` instead of `777` — the same class of bug `plum_codegen::
    /// codegen::codegen_extern_call`'s own ordinary scalar `Bool`-return
    /// handling already avoids, now proven for a STRUCT field too.
    const MIXED_C_HELPER: &str = r#"
        typedef struct { int flag; long long big; } Mixed;
        Mixed ffitest_make_mixed_nonzero(void) {
            Mixed m;
            m.flag = 2;
            m.big = 777;
            return m;
        }
    "#;

    #[test]
    fn mixed_bool_int_struct_padding_and_bool_normalization_round_trip_correctly() {
        let src = r#"
            struct Mixed { flag: Bool, big: Int }
            extern "C" {
                fn ffitest_make_mixed_nonzero() -> Mixed;
            }
            let go (): Int = unsafe {
                match ffitest_make_mixed_nonzero() {
                    Mixed(flag, big) => if flag { big } else { -1 }
                }
            }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], MIXED_C_HELPER).unwrap();
        // `777`, not `-1` — proves BOTH the `icmp ne` Bool normalization
        // (`flag` reads as `true`) AND that `big` was read from the
        // CORRECT, padding-adjusted byte offset (a wrong offset would
        // read garbage, not necessarily `777`, but never reliably this
        // exact value across a real run).
        assert_eq!(out, "777");
    }

    /// A nested-struct C helper (`Outer { inner: Inner, b: Int }`, `Inner
    /// { a: Int }`) — proves the recursive `insertvalue`/`extractvalue`
    /// case in both `build_c_struct_value`/`build_ctor_from_c_struct`:
    /// the inner struct is built/read as a genuine SUB-AGGREGATE nested
    /// inside the outer one, never passed as a pointer at the C boundary.
    const NESTED_C_HELPER: &str = r#"
        typedef struct { long long a; } Inner;
        typedef struct { Inner inner; long long b; } Outer;
        Outer ffitest_make_outer(long long a, long long b) {
            Outer o;
            o.inner.a = a;
            o.b = b;
            return o;
        }
        long long ffitest_outer_sum(Outer o) {
            return o.inner.a + o.b;
        }
    "#;

    #[test]
    fn nested_struct_argument_and_return_round_trip_through_a_real_c_abi_boundary() {
        let src = r#"
            struct Inner { a: Int }
            struct Outer { inner: Inner, b: Int }
            extern "C" {
                fn ffitest_make_outer(a: Int, b: Int) -> Outer;
                fn ffitest_outer_sum(o: Outer) -> Int;
            }
            let go (): Int = unsafe {
                let o = ffitest_make_outer(3, 4);
                ffitest_outer_sum(o)
            }
        "#;
        let out = compile_and_run_with_c_helper(src, "go", &[CgValue::Unit], NESTED_C_HELPER).unwrap();
        assert_eq!(out, "7");
    }

    #[test]
    fn a_real_libc_div_computes_the_correct_quotient_and_remainder() {
        // Mirrors `plumc::lib`'s existing interpreter-path pipeline test
        // (`extern_call_returning_a_struct_by_value_runs_through_the_
        // full_gated_pipeline`) EXACTLY — same `DivResult { quot: Bool,
        // rem: Bool }` shape, matching real libc `div_t`'s `int`-width
        // fields via `ExternType::Bool`'s C-ABI mapping (see that test's
        // own doc comment for why `Bool`, not `Int`, is required here) —
        // but run through the REAL `compile_and_run` codegen pipeline
        // against genuine system `div()`, and going further than that
        // existing test by asserting the ACTUAL computed quotient/
        // remainder values, not just that the call succeeds. No custom
        // C helper needed — `div` is an ordinary libc function, already
        // linked into every compiled Plum binary.
        let src = r#"
            struct DivResult { quot: Bool, rem: Bool }
            extern "C" {
                fn div(numer: Bool, denom: Bool) -> DivResult;
            }
            let go (): Int = unsafe {
                match div(true, true) {
                    DivResult(quot, rem) => (if quot { 1 } else { 0 }) * 10 + (if rem { 1 } else { 0 })
                }
            }
        "#;
        let out = compile_and_run(src, "go", &[CgValue::Unit]).unwrap();
        // `div(1, 1)` => `quot = 1` (true), `rem = 0` (false) => `10`.
        assert_eq!(out, "10");
    }
}
