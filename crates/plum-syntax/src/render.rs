//! Renders a parsed `Program`/`Expr`/`Pattern`/`Type` as a compact,
//! Lisp-like s-expression — spans deliberately omitted, so two
//! semantically-identical parses render identically regardless of
//! incidental whitespace/comment differences in the source that
//! produced them.
//!
//! This started as `plum-syntax::parser`'s own private, test-only
//! `render`/`render_program` helpers (used to assert on AST *shape*
//! in tests without hand-writing span boilerplate — see that module's
//! own test suite, which still uses these exact functions, now via
//! `use`). Promoted to a real, `pub` module for a second, equally
//! real purpose: `plum dump-ast <file>` (`plumc::main`) and the
//! `bootstrap/corpus/` golden-file test suite (DESIGN.md's
//! "Self-hosting bootstrap corpus" section) both need a STABLE, human-
//! readable canonical text form for "what did the Rust parser actually
//! produce for this source file" — the exact same shape the test
//! suite already needed, just consumed from outside this crate's own
//! tests now too. Kept as ONE function per AST node kind (not folded
//! into `Debug`) specifically so it stays independent of `#[derive(Debug)]`'s
//! own field-order/verbosity, which is free to change for reasons that
//! have nothing to do with this format's own stability contract.

use crate::ast::{
    BinaryOp, Block, Expr, FieldPattern, GenericParam, Item, ItemKind, MatchArm, Param, ParamKind, Pattern, Program,
    SelectArm, Stmt, Type, UnaryOp,
};
use crate::lexer::{InterpPart, Token, TokenKind};

/// Renders a real `Lexer::tokenize()` stream as a flat, space-separated
/// list of compact token names — the lexer's own counterpart to
/// `render_program` above (same span-free, stability-over-verbosity
/// reasoning: two runs of the SAME source always render identically,
/// and irrelevant lexer-internal details like exact byte spans never
/// leak into the comparison). A bare keyword/punctuation token (`Let`,
/// `LParen`, `Arrow`, ...) renders as just its name; a payload-carrying
/// token (`Ident`/`Int`/`Float`/`Str`/`InterpStr`) renders its payload
/// alongside. The trailing `Eof` token every real stream ends with is
/// deliberately OMITTED — constant, uninteresting boilerplate every
/// single fixture would otherwise end with, not real lexer output
/// worth comparing.
pub fn render_tokens(tokens: &[Token]) -> String {
    tokens
        .iter()
        .filter(|t| !matches!(t.kind, TokenKind::Eof))
        .map(|t| render_token_kind(&t.kind))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_token_kind(kind: &TokenKind) -> String {
    match kind {
        // `InterpPart::Expr`'s second field is a `Span` — the ONE token
        // payload that isn't already span-free, so this is one of two
        // variants that can't just fall through to `{other:?}` below.
        // `Ident`/`Int`/`Str` already derive EXACTLY the format wanted
        // here (`Ident("x")`, `Int(5)`, ...) with nothing to strip, so
        // there's no reason to hand-write those arms too.
        TokenKind::InterpStr(parts) => {
            let rendered: Vec<String> = parts
                .iter()
                .map(|p| match p {
                    InterpPart::Literal(s) => format!("Literal({s:?})"),
                    InterpPart::Expr(src, _) => format!("Expr({src:?})"),
                })
                .collect();
            format!("InterpStr({})", rendered.join(" "))
        }
        // The SECOND variant that can't fall through to `{other:?}` —
        // found empirically, not by inspection, while validating the
        // self-hosted Plum lexer against this exact golden format:
        // `{other:?}`'s derived `Debug` for an `f64` ALWAYS forces a
        // decimal point (`1.0`), but `Display` (`{f}`) doesn't (`1`) —
        // and `Display` is what `render_program`'s own `Expr::Float`
        // arm already uses (`f.to_string()`), and what Plum's own
        // `Float.to_string()` naturally produces. A self-hosted Plum
        // lexer/renderer would have no reason to ever reproduce Rust
        // `Debug`'s forced-decimal quirk, so `Debug` was the actual bug
        // here, not a formatting choice worth preserving — fixed to
        // match `render_program`'s own convention instead of standing
        // apart from it.
        TokenKind::Float(f) => format!("Float({f})"),
        other => format!("{other:?}"),
    }
}

pub fn op_symbol(op: &BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::Le => "<=",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
        BinaryOp::Range => "..",
        BinaryOp::Pipe => "|>",
    }
}

pub fn render_type(ty: &Type) -> String {
    match ty {
        Type::Path(segments, _) => segments.join("."),
        Type::Generic { base, args, .. } => {
            let mut parts = vec!["gt".to_string(), base.join(".")];
            parts.extend(args.iter().map(render_type));
            format!("({})", parts.join(" "))
        }
        Type::Function { params, ret, .. } => {
            let params_str = params.iter().map(render_type).collect::<Vec<_>>().join(" ");
            format!("(fn ({params_str}) -> {})", render_type(ret))
        }
        Type::Tuple(elems, _) => {
            let parts = elems.iter().map(render_type).collect::<Vec<_>>().join(" ");
            format!("(tup {parts})")
        }
    }
}

pub fn render(expr: &Expr) -> String {
    match expr {
        Expr::Int(n, _) => n.to_string(),
        Expr::Float(f, _) => f.to_string(),
        Expr::Str(s, _) => format!("{s:?}"),
        Expr::Bool(b, _) => b.to_string(),
        Expr::Ident(name, _) => name.clone(),
        Expr::Tuple(elems, _) => {
            let mut parts = vec!["tuple".to_string()];
            parts.extend(elems.iter().map(render));
            format!("({})", parts.join(" "))
        }
        Expr::ArrayLiteral(elems, _) => {
            let mut parts = vec!["array".to_string()];
            parts.extend(elems.iter().map(render));
            format!("({})", parts.join(" "))
        }
        Expr::Unary { op, expr, .. } => {
            let sym = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "!",
            };
            format!("({sym} {})", render(expr))
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            format!("({} {} {})", op_symbol(op), render(lhs), render(rhs))
        }
        Expr::Field { base, name, .. } => format!("(field {} {name})", render(base)),
        Expr::Call { callee, args, .. } => {
            let mut parts = vec!["call".to_string(), render(callee)];
            parts.extend(args.iter().map(render));
            format!("({})", parts.join(" "))
        }
        Expr::GenericInst { callee, args, .. } => {
            let mut parts = vec!["generic".to_string(), render(callee)];
            parts.extend(args.iter().map(render_type));
            format!("({})", parts.join(" "))
        }
        Expr::Index { base, index, .. } => format!("(index {} {})", render(base), render(index)),
        Expr::Block(block, _) => render_block(block),
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            let mut parts = vec!["if".to_string(), render(cond), render_block(then_branch)];
            if let Some(e) = else_branch {
                parts.push(render(e));
            }
            format!("({})", parts.join(" "))
        }
        Expr::Match { scrutinee, arms, .. } => {
            let mut parts = vec!["match".to_string(), render(scrutinee)];
            parts.extend(arms.iter().map(render_arm));
            format!("({})", parts.join(" "))
        }
        Expr::For { pattern, iter, body, .. } => {
            format!("(for {} {} {})", render_pattern(pattern), render(iter), render_block(body))
        }
        Expr::Closure { params, body, .. } => {
            let mut parts = vec!["closure".to_string()];
            parts.push(format!(
                "({})",
                params.iter().map(|p| p.name.clone()).collect::<Vec<_>>().join(" ")
            ));
            parts.push(render(body));
            format!("({})", parts.join(" "))
        }
        Expr::Unsafe(block, _) => format!("(unsafe {})", render_block(block)),
        Expr::Spawn(block, _) => format!("(spawn {})", render_block(block)),
        Expr::StructLiteral { path, fields, spread, .. } => {
            let mut parts = vec!["struct-lit".to_string(), path.join(".")];
            parts.extend(fields.iter().map(|f| {
                let mut key = f.name.clone();
                for (seg, _) in &f.extra_path {
                    key.push('.');
                    key.push_str(seg);
                }
                format!("{key}={}", render(&f.value))
            }));
            if let Some(s) = spread {
                parts.push(format!("..{}", render(s)));
            }
            format!("({})", parts.join(" "))
        }
        Expr::Select { arms, .. } => {
            let mut parts = vec!["select".to_string()];
            parts.extend(arms.iter().map(render_select_arm));
            format!("({})", parts.join(" "))
        }
    }
}

pub fn render_block(block: &Block) -> String {
    let mut parts = vec!["block".to_string()];
    parts.extend(block.stmts.iter().map(render_stmt));
    if let Some(tail) = &block.tail {
        parts.push(render(tail));
    }
    format!("({})", parts.join(" "))
}

pub fn render_stmt(stmt: &Stmt) -> String {
    match stmt {
        Stmt::Let {
            is_mut, pattern, value, ..
        } => {
            let mut_marker = if *is_mut { "mut " } else { "" };
            format!("(let {mut_marker}{} {})", render_pattern(pattern), render(value))
        }
        Stmt::Assign { name, value, .. } => format!("(= {name} {})", render(value)),
        Stmt::Expr(e) => format!("(stmt {})", render(e)),
    }
}

pub fn render_arm(arm: &MatchArm) -> String {
    match &arm.guard {
        Some(g) => format!(
            "(arm {} if={} {})",
            render_pattern(&arm.pattern),
            render(g),
            render(&arm.body)
        ),
        None => format!("(arm {} {})", render_pattern(&arm.pattern), render(&arm.body)),
    }
}

pub fn render_select_arm(arm: &SelectArm) -> String {
    format!(
        "(arm {} = {} {})",
        render_pattern(&arm.pattern),
        render(&arm.expr),
        render(&arm.body)
    )
}

pub fn render_field_pattern(fp: &FieldPattern) -> String {
    format!("{}={}", fp.name, render_pattern(&fp.pattern))
}

pub fn render_pattern(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Int(n, _) => n.to_string(),
        Pattern::Float(f, _) => f.to_string(),
        Pattern::Str(s, _) => format!("{s:?}"),
        Pattern::Bool(b, _) => b.to_string(),
        Pattern::Wildcard(_) => "_".to_string(),
        Pattern::Ident(name, _) => name.clone(),
        Pattern::Tuple(elems, _) => {
            let mut parts = vec!["tuple".to_string()];
            parts.extend(elems.iter().map(render_pattern));
            format!("({})", parts.join(" "))
        }
        Pattern::Variant { path, args, .. } => {
            let mut parts = vec!["variant".to_string(), path.join(".")];
            parts.extend(args.iter().map(render_pattern));
            format!("({})", parts.join(" "))
        }
        Pattern::Struct { path, fields, has_rest, .. } => {
            let mut parts = vec!["struct".to_string(), path.join(".")];
            parts.extend(fields.iter().map(render_field_pattern));
            if *has_rest {
                parts.push("..".to_string());
            }
            format!("({})", parts.join(" "))
        }
        Pattern::Or(alts, _) => {
            let mut parts = vec!["or".to_string()];
            parts.extend(alts.iter().map(render_pattern));
            format!("({})", parts.join(" "))
        }
    }
}

pub fn render_generic_params(params: &[GenericParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let rendered: Vec<String> = params
        .iter()
        .map(|p| {
            if p.bound.is_empty() {
                p.name.clone()
            } else {
                format!("{}:{}", p.name, p.bound.join("+"))
            }
        })
        .collect();
    format!("[{}]", rendered.join(","))
}

pub fn render_param(param: &Param) -> String {
    match &param.kind {
        ParamKind::Ident(name) => name.clone(),
        ParamKind::Pattern(pattern, Some(ty)) => format!("({}:{})", render_pattern(pattern), render_type(ty)),
        ParamKind::Pattern(pattern, None) => format!("({})", render_pattern(pattern)),
    }
}

pub fn render_item(item: &Item) -> String {
    let pub_marker = if item.is_pub { "pub " } else { "" };
    match &item.kind {
        ItemKind::Let(def) => {
            let mut parts = vec!["let".to_string(), format!("{pub_marker}{}", def.name)];
            let generics = render_generic_params(&def.generics);
            if !generics.is_empty() {
                parts.push(generics);
            }
            parts.push(format!(
                "({})",
                def.params.iter().map(render_param).collect::<Vec<_>>().join(" ")
            ));
            if let Some(ty) = &def.ret_ty {
                parts.push(format!("->{}", render_type(ty)));
            }
            parts.push(render(&def.body));
            format!("({})", parts.join(" "))
        }
        ItemKind::Struct(decl) => {
            let mut parts = vec!["struct".to_string(), format!("{pub_marker}{}", decl.name)];
            let generics = render_generic_params(&decl.generics);
            if !generics.is_empty() {
                parts.push(generics);
            }
            for f in &decl.fields {
                let fpub = if f.is_pub { "pub " } else { "" };
                parts.push(format!("{fpub}{}:{}", f.name, render_type(&f.ty)));
            }
            format!("({})", parts.join(" "))
        }
        ItemKind::Enum(decl) => {
            let mut parts = vec!["enum".to_string(), format!("{pub_marker}{}", decl.name)];
            let generics = render_generic_params(&decl.generics);
            if !generics.is_empty() {
                parts.push(generics);
            }
            for v in &decl.variants {
                if v.payload.is_empty() {
                    parts.push(v.name.clone());
                } else {
                    let payload: Vec<String> = v.payload.iter().map(render_type).collect();
                    parts.push(format!("{}({})", v.name, payload.join(",")));
                }
            }
            format!("({})", parts.join(" "))
        }
        ItemKind::Extern(block) => {
            let mut parts = vec!["extern".to_string(), format!("{:?}", block.abi)];
            for f in &block.fns {
                let params: Vec<String> = f.params.iter().map(|p| format!("{}:{}", p.name, render_type(&p.ty))).collect();
                let ret = match &f.ret_ty {
                    Some(ty) => format!("->{}", render_type(ty)),
                    None => String::new(),
                };
                parts.push(format!("fn {}({}){ret}", f.name, params.join(",")));
            }
            format!("({})", parts.join(" "))
        }
        ItemKind::Use(decl) => format!("({pub_marker}use {})", decl.path.join(".")),
    }
}

pub fn render_program(program: &Program) -> String {
    let items: Vec<String> = program.items.iter().map(render_item).collect();
    format!("({})", items.join(" "))
}
