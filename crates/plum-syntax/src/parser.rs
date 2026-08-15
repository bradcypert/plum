use crate::ast::{
    BinaryOp, Block, ClosureParam, EnumDecl, EnumVariant, Expr, ExternBlock, ExternFn,
    ExternParam, FieldInit, FieldPattern, GenericParam, Item, ItemKind, LetDef, MatchArm, Param,
    ParamKind, Pattern, Program, SelectArm, Stmt, StructDecl, StructField, Type, UnaryOp, UseDecl,
};
use crate::lexer::{InterpPart, Lexer, Token, TokenKind};
use crate::span::Span;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    // Suppresses bare struct-literal parsing (`Path { ... }`) while
    // parsing an if-condition or match-scrutinee, to resolve the
    // ambiguity GRAMMAR.md flags between a struct literal and the
    // construct's own body block. Reset to false whenever a nested
    // bracketed context (parens, call args, index brackets) is
    // entered, since those are unambiguous regardless. See
    // `parse_expr_no_struct_literal`/`parse_expr_allowing_struct_literal`.
    no_struct_literal: bool,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            no_struct_literal: false,
        }
    }

    pub fn is_at_eof(&self) -> bool {
        matches!(self.tokens[self.pos].kind, TokenKind::Eof)
    }

    // --- item grammar: a Program is just `{ Item }` — see GRAMMAR.md's
    // "Program structure" section. No `mod` declaration exists; a
    // directory is a module (see DESIGN.md's "Module system"), so this
    // is enough to parse one whole `.plum` file. ---

    pub fn parse_program(&mut self) -> Result<Program, crate::error::CompileError> {
        let mut items = Vec::new();
        while !self.is_at_eof() {
            items.push(self.parse_item()?);
        }
        Ok(Program { items })
    }

    fn parse_item(&mut self) -> Result<Item, crate::error::CompileError> {
        let pub_tok = if self.check(&TokenKind::Pub) {
            Some(self.advance())
        } else {
            None
        };
        let is_pub = pub_tok.is_some();
        let kind_start = self.peek().span;
        let kind = match self.peek_kind() {
            TokenKind::Let => ItemKind::Let(self.parse_let_def()?),
            TokenKind::Struct => ItemKind::Struct(self.parse_struct_decl()?),
            TokenKind::Enum => ItemKind::Enum(self.parse_enum_decl()?),
            TokenKind::Extern => ItemKind::Extern(self.parse_extern_block()?),
            TokenKind::Use => ItemKind::Use(self.parse_use_decl()?),
            other => {
                return Err(crate::error::CompileError::new(
                    self.peek().span,
                    format!("expected an item (let/struct/enum/extern/use), found {other:?}"),
                ));
            }
        };
        let kind_span = match &kind {
            ItemKind::Let(d) => d.span,
            ItemKind::Struct(d) => d.span,
            ItemKind::Enum(d) => d.span,
            ItemKind::Extern(d) => d.span,
            ItemKind::Use(d) => d.span,
        };
        let start = pub_tok.map(|t| t.span).unwrap_or(kind_start);
        Ok(Item {
            is_pub,
            kind,
            span: start.to(kind_span),
        })
    }

    fn parse_generic_params(&mut self) -> Result<Vec<GenericParam>, crate::error::CompileError> {
        self.expect(TokenKind::LBracket, "'['")?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            params.push(self.parse_generic_param()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RBracket) {
                    break;
                }
                params.push(self.parse_generic_param()?);
            }
        }
        self.expect(TokenKind::RBracket, "']'")?;
        Ok(params)
    }

    fn parse_generic_param(&mut self) -> Result<GenericParam, crate::error::CompileError> {
        let name_tok = self.expect_ident("a generic parameter")?;
        let name = Self::ident_text(&name_tok);
        let mut end = name_tok.span;
        let mut bound = Vec::new();
        if self.bump_if(&TokenKind::Colon) {
            let first = self.expect_ident("a trait bound")?;
            end = first.span;
            bound.push(Self::ident_text(&first));
            while self.bump_if(&TokenKind::Plus) {
                let next = self.expect_ident("a trait bound")?;
                end = next.span;
                bound.push(Self::ident_text(&next));
            }
        }
        Ok(GenericParam {
            name,
            bound,
            span: name_tok.span.to(end),
        })
    }

    fn parse_let_def(&mut self) -> Result<LetDef, crate::error::CompileError> {
        let start = self.expect(TokenKind::Let, "'let'")?.span;
        let name_tok = self.expect_ident("a name")?;
        let mut name = Self::ident_text(&name_tok);
        // `let Type.func (...) = ...` — a real, per-type ASSOCIATED
        // function declaration (`Point.add`, `Option.map`), not a
        // module-qualified name (those only ever appear on the
        // resolver's rewritten OUTPUT, never typed by hand in source —
        // see `plumc::modules::qualify`) and not a struct-literal path
        // (`Type.Variant { .. }`/`shapes.Circle { .. }` are parsed
        // through the EXPRESSION path, `parse_path_shaped_expr`, a
        // completely different function from this one). Storing the
        // combined `"Type.func"` directly as `LetDef.name` mirrors
        // `qualify()`'s own module-qualification trick exactly — every
        // downstream consumer (duplicate-name checking, monomorphization,
        // codegen's LLVM symbol emission) already treats `LetDef.name`
        // as an opaque string key, so `"Point.add"`/`"Circle.add"` are
        // simply two different names, for free, with no other code
        // needing to change. See `plumc::assoc_fns` for how a CALL site
        // (`Point.add(a, b)`) gets resolved back to this exact name.
        if self.check(&TokenKind::Dot) {
            self.advance();
            let second_tok = self.expect_ident("an associated function name")?;
            name = format!("{name}.{}", Self::ident_text(&second_tok));
        }
        let generics = if self.check(&TokenKind::LBracket) {
            self.parse_generic_params()?
        } else {
            Vec::new()
        };
        let mut params = Vec::new();
        while matches!(self.peek_kind(), TokenKind::Ident(_) | TokenKind::LParen) {
            params.push(self.parse_param()?);
        }
        let ret_ty = if self.bump_if(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let (requires, ensures) = self.parse_contract_clauses()?;
        self.expect(TokenKind::Eq, "'='")?;
        let raw_body = self.parse_expr()?;
        let span = start.to(raw_body.span());
        let body = Self::desugar_contracts(&params, requires, ensures, raw_body, span)?;
        Ok(LetDef {
            name,
            generics,
            params,
            ret_ty,
            body,
            span,
        })
    }

    // `{ RequireClause } { EnsureClause }` — see GRAMMAR.md's "Contracts"
    // section. Fixed order (every `require` before any `ensure`) rather
    // than free interleaving, purely to keep both this parse and the
    // desugaring below simple; a stray `require` after an `ensure` is a
    // clear parse error, not silently accepted.
    #[allow(clippy::type_complexity)]
    fn parse_contract_clauses(
        &mut self,
    ) -> Result<(Vec<(Expr, Option<String>, Span)>, Vec<(Expr, Option<String>, Span)>), crate::error::CompileError> {
        let mut requires = Vec::new();
        while self.peek_is_contextual_kw("require") {
            requires.push(self.parse_contract_clause()?);
        }
        let mut ensures = Vec::new();
        while self.peek_is_contextual_kw("ensure") {
            ensures.push(self.parse_contract_clause()?);
        }
        if self.peek_is_contextual_kw("require") {
            return Err(crate::error::CompileError::new(
                self.peek().span,
                "'require' clauses must all come before any 'ensure' clauses".to_string(),
            ));
        }
        Ok((requires, ensures))
    }

    fn parse_contract_clause(&mut self) -> Result<(Expr, Option<String>, Span), crate::error::CompileError> {
        let start = self.advance().span; // consume `require`/`ensure`
        let cond = self.parse_expr()?;
        let mut end = cond.span();
        let message = if self.bump_if(&TokenKind::Colon) {
            let tok = self.expect_str("a string literal")?;
            end = tok.span;
            Some(Self::str_text(&tok))
        } else {
            None
        };
        Ok((cond, message, start.to(end)))
    }

    // Rewrites `require`/`ensure` clauses into plain `assert`-shaped
    // calls, entirely at parse time — mirrors string interpolation's own
    // precedent exactly (lexer+parser only, zero IR/backend changes):
    // by the time `plum-types`/`plum-ir` see this `LetDef`, its `body`
    // is ordinary Plum, indistinguishable from anything a user could
    // have written by hand. See DESIGN.md's "Contracts" section for the
    // full "why" (including the deliberate `ensure`-breaks-tail-calls
    // trade-off this shape implies).
    fn desugar_contracts(
        params: &[Param],
        requires: Vec<(Expr, Option<String>, Span)>,
        ensures: Vec<(Expr, Option<String>, Span)>,
        raw_body: Expr,
        span: Span,
    ) -> Result<Expr, crate::error::CompileError> {
        if requires.is_empty() && ensures.is_empty() {
            return Ok(raw_body);
        }
        if !ensures.is_empty() {
            if let Some(p) = params.iter().find(|p| matches!(&p.kind, ParamKind::Ident(n) if n == "result")
                || matches!(&p.kind, ParamKind::Pattern(Pattern::Ident(n, _), _) if n == "result"))
            {
                return Err(crate::error::CompileError::new(
                    p.span,
                    "a function with 'ensure' clauses can't have a parameter named 'result' \
                     (the postcondition needs that name for the return value)"
                        .to_string(),
                ));
            }
        }
        let mut stmts = Vec::new();
        for (cond, message, clause_span) in requires {
            stmts.push(Stmt::Expr(Self::contract_check_call(
                "__contract_require",
                "precondition failed",
                cond,
                message,
                clause_span,
            )));
        }
        if ensures.is_empty() {
            // `require`-only: the original body stays in TAIL position,
            // not a statement — preserves whatever tail-call shape it
            // had (`require` alone never costs TCO; only `ensure` does,
            // since it has to intercept the return value to check it).
            return Ok(Expr::Block(
                Block {
                    stmts,
                    tail: Some(Box::new(raw_body)),
                    span,
                },
                span,
            ));
        }
        let body_span = raw_body.span();
        stmts.push(Stmt::Let {
            is_mut: false,
            pattern: Pattern::Ident("result".to_string(), body_span),
            ty: None,
            value: raw_body,
            span: body_span,
        });
        for (cond, message, clause_span) in ensures {
            stmts.push(Stmt::Expr(Self::contract_check_call(
                "__contract_ensure",
                "postcondition failed",
                cond,
                message,
                clause_span,
            )));
        }
        let tail = Some(Box::new(Expr::Ident("result".to_string(), body_span)));
        Ok(Expr::Block(Block { stmts, tail, span }, span))
    }

    fn contract_check_call(
        callee: &str,
        base_msg: &str,
        cond: Expr,
        message: Option<String>,
        span: Span,
    ) -> Expr {
        let msg = match message {
            Some(m) => format!("{base_msg}: {m}"),
            None => base_msg.to_string(),
        };
        Expr::Call {
            callee: Box::new(Expr::Ident(callee.to_string(), span)),
            args: vec![cond, Expr::Str(msg, span)],
            span,
        }
    }

    fn parse_param(&mut self) -> Result<Param, crate::error::CompileError> {
        let tok = self.peek().clone();
        match &tok.kind {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Param {
                    kind: ParamKind::Ident(name.clone()),
                    span: tok.span,
                })
            }
            TokenKind::LParen => {
                let open = self.advance();
                if self.check(&TokenKind::RParen) {
                    // `()` — the Unit pattern.
                    let close = self.advance();
                    let span = open.span.to(close.span);
                    return Ok(Param {
                        kind: ParamKind::Pattern(Pattern::Tuple(vec![], span), None),
                        span,
                    });
                }
                let first = self.parse_pattern()?;
                if self.bump_if(&TokenKind::Comma) {
                    // Tuple-destructuring param, single-paren form —
                    // `let swap (a, b) = ...` from examples/overview.plum.
                    // No `: Type` suffix in this form (unlike the
                    // singleton case below); resolves the ambiguity
                    // GRAMMAR.md flags between Param's own parens and
                    // Pattern's tuple-pattern parens by treating them
                    // as the same parens rather than requiring
                    // double-wrapping.
                    let mut elems = vec![first];
                    if !self.check(&TokenKind::RParen) {
                        elems.push(self.parse_pattern()?);
                        while self.bump_if(&TokenKind::Comma) {
                            if self.check(&TokenKind::RParen) {
                                break;
                            }
                            elems.push(self.parse_pattern()?);
                        }
                    }
                    let close = self.expect(TokenKind::RParen, "')'")?;
                    let span = open.span.to(close.span);
                    return Ok(Param {
                        kind: ParamKind::Pattern(Pattern::Tuple(elems, span), None),
                        span,
                    });
                }
                let ty = if self.bump_if(&TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let close = self.expect(TokenKind::RParen, "')'")?;
                Ok(Param {
                    kind: ParamKind::Pattern(first, ty),
                    span: open.span.to(close.span),
                })
            }
            other => Err(crate::error::CompileError::new(tok.span, format!("expected a parameter, found {other:?}"))),
        }
    }

    fn parse_struct_decl(&mut self) -> Result<StructDecl, crate::error::CompileError> {
        let start = self.expect(TokenKind::Struct, "'struct'")?.span;
        let name_tok = self.expect_ident("a struct name")?;
        let name = Self::ident_text(&name_tok);
        let generics = if self.check(&TokenKind::LBracket) {
            self.parse_generic_params()?
        } else {
            Vec::new()
        };
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut fields = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            fields.push(self.parse_struct_field()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                fields.push(self.parse_struct_field()?);
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(StructDecl {
            name,
            generics,
            fields,
            span: start.to(close.span),
        })
    }

    fn parse_struct_field(&mut self) -> Result<StructField, crate::error::CompileError> {
        let is_pub = self.bump_if(&TokenKind::Pub);
        let name_tok = self.expect_ident("a field name")?;
        let name = Self::ident_text(&name_tok);
        self.expect(TokenKind::Colon, "':'")?;
        let ty = self.parse_type()?;
        let span = name_tok.span.to(ty.span());
        Ok(StructField { is_pub, name, ty, span })
    }

    fn parse_enum_decl(&mut self) -> Result<EnumDecl, crate::error::CompileError> {
        let start = self.expect(TokenKind::Enum, "'enum'")?.span;
        let name_tok = self.expect_ident("an enum name")?;
        let name = Self::ident_text(&name_tok);
        let generics = if self.check(&TokenKind::LBracket) {
            self.parse_generic_params()?
        } else {
            Vec::new()
        };
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut variants = Vec::new();
        if !self.check(&TokenKind::RBrace) {
            variants.push(self.parse_enum_variant()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                variants.push(self.parse_enum_variant()?);
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(EnumDecl {
            name,
            generics,
            variants,
            span: start.to(close.span),
        })
    }

    fn parse_enum_variant(&mut self) -> Result<EnumVariant, crate::error::CompileError> {
        let name_tok = self.expect_ident("a variant name")?;
        let name = Self::ident_text(&name_tok);
        let mut end = name_tok.span;
        let mut payload = Vec::new();
        if self.bump_if(&TokenKind::LParen) {
            if !self.check(&TokenKind::RParen) {
                payload.push(self.parse_type()?);
                while self.bump_if(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    payload.push(self.parse_type()?);
                }
            }
            let close = self.expect(TokenKind::RParen, "')'")?;
            end = close.span;
        }
        Ok(EnumVariant {
            name,
            payload,
            span: name_tok.span.to(end),
        })
    }

    fn parse_extern_block(&mut self) -> Result<ExternBlock, crate::error::CompileError> {
        let start = self.expect(TokenKind::Extern, "'extern'")?.span;
        let abi_tok = self.expect_str("a string literal (the ABI, e.g. \"C\")")?;
        let abi = Self::str_text(&abi_tok);
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut fns = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            fns.push(self.parse_extern_fn()?);
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(ExternBlock {
            abi,
            fns,
            span: start.to(close.span),
        })
    }

    fn parse_extern_fn(&mut self) -> Result<ExternFn, crate::error::CompileError> {
        let start = self.expect(TokenKind::Fn, "'fn'")?.span;
        let name_tok = self.expect_ident("a function name")?;
        let name = Self::ident_text(&name_tok);
        self.expect(TokenKind::LParen, "'('")?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            params.push(self.parse_extern_param()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_extern_param()?);
            }
        }
        self.expect(TokenKind::RParen, "')'")?;
        let ret_ty = if self.bump_if(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let semi = self.expect(TokenKind::Semicolon, "';'")?;
        Ok(ExternFn {
            name,
            params,
            ret_ty,
            span: start.to(semi.span),
        })
    }

    fn parse_extern_param(&mut self) -> Result<ExternParam, crate::error::CompileError> {
        let name_tok = self.expect_ident("a parameter name")?;
        let name = Self::ident_text(&name_tok);
        self.expect(TokenKind::Colon, "':'")?;
        let ty = self.parse_type()?;
        let span = name_tok.span.to(ty.span());
        Ok(ExternParam { name, ty, span })
    }

    fn parse_use_decl(&mut self) -> Result<UseDecl, crate::error::CompileError> {
        let start = self.expect(TokenKind::Use, "'use'")?.span;
        let segments = self.parse_expr_path()?;
        let path: Vec<String> = segments.iter().map(|(name, _)| name.clone()).collect();
        let semi = self.expect(TokenKind::Semicolon, "';'")?;
        Ok(UseDecl {
            path,
            span: start.to(semi.span),
        })
    }

    // --- pattern grammar, matching GRAMMAR.md's "Patterns" section ---

    pub fn parse_pattern(&mut self) -> Result<Pattern, crate::error::CompileError> {
        let first = self.parse_primary_pattern()?;
        if !self.check(&TokenKind::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.bump_if(&TokenKind::Pipe) {
            alts.push(self.parse_primary_pattern()?);
        }
        let span = alts[0].span().to(alts[alts.len() - 1].span());
        Ok(Pattern::Or(alts, span))
    }

    fn parse_primary_pattern(&mut self) -> Result<Pattern, crate::error::CompileError> {
        let tok = self.peek().clone();
        match &tok.kind {
            TokenKind::Int(n) => {
                self.advance();
                Ok(Pattern::Int(*n, tok.span))
            }
            TokenKind::Float(f) => {
                self.advance();
                Ok(Pattern::Float(*f, tok.span))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(Pattern::Str(s.clone(), tok.span))
            }
            TokenKind::True => {
                self.advance();
                Ok(Pattern::Bool(true, tok.span))
            }
            TokenKind::False => {
                self.advance();
                Ok(Pattern::Bool(false, tok.span))
            }
            TokenKind::Underscore => {
                self.advance();
                Ok(Pattern::Wildcard(tok.span))
            }
            TokenKind::Ident(name) => {
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    // Capitalized — a variant/struct path, same
                    // disambiguation convention used for `.` and `[T]`
                    // elsewhere in the grammar.
                    self.parse_path_shaped_pattern()
                } else {
                    self.advance();
                    Ok(Pattern::Ident(name.clone(), tok.span))
                }
            }
            TokenKind::LParen => self.parse_tuple_pattern(),
            other => Err(crate::error::CompileError::new(tok.span, format!("expected a pattern, found {other:?}"))),
        }
    }

    fn parse_pattern_path(&mut self) -> Result<(Vec<String>, Span), crate::error::CompileError> {
        let first = self.expect_ident("a pattern path segment")?;
        let start = first.span;
        let mut end = first.span;
        let mut segments = vec![Self::ident_text(&first)];
        while self.check(&TokenKind::Dot) {
            self.advance();
            let seg = self.expect_ident("a path segment")?;
            end = seg.span;
            segments.push(Self::ident_text(&seg));
        }
        Ok((segments, start.to(end)))
    }

    fn parse_path_shaped_pattern(&mut self) -> Result<Pattern, crate::error::CompileError> {
        let (path, path_span) = self.parse_pattern_path()?;
        match self.peek_kind() {
            TokenKind::LParen => {
                self.advance();
                let mut args = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    args.push(self.parse_pattern()?);
                    while self.bump_if(&TokenKind::Comma) {
                        if self.check(&TokenKind::RParen) {
                            break;
                        }
                        args.push(self.parse_pattern()?);
                    }
                }
                let close = self.expect(TokenKind::RParen, "')'")?;
                Ok(Pattern::Variant {
                    path,
                    args,
                    span: path_span.to(close.span),
                })
            }
            TokenKind::LBrace => self.parse_struct_pattern(path, path_span),
            _ => Ok(Pattern::Variant {
                path,
                args: vec![],
                span: path_span,
            }),
        }
    }

    fn parse_struct_pattern(&mut self, path: Vec<String>, path_span: Span) -> Result<Pattern, crate::error::CompileError> {
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut fields = Vec::new();
        let mut has_rest = false;
        if self.bump_if(&TokenKind::DotDot) {
            has_rest = true;
        } else if !self.check(&TokenKind::RBrace) {
            fields.push(self.parse_field_pattern()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.bump_if(&TokenKind::DotDot) {
                    has_rest = true;
                    break;
                }
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                fields.push(self.parse_field_pattern()?);
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(Pattern::Struct {
            path,
            fields,
            has_rest,
            span: path_span.to(close.span),
        })
    }

    fn parse_field_pattern(&mut self) -> Result<FieldPattern, crate::error::CompileError> {
        let name_tok = self.expect_ident("a field name")?;
        let name = Self::ident_text(&name_tok);
        if self.bump_if(&TokenKind::Colon) {
            let pattern = self.parse_pattern()?;
            let span = name_tok.span.to(pattern.span());
            Ok(FieldPattern { name, pattern, span })
        } else {
            Ok(FieldPattern {
                pattern: Pattern::Ident(name.clone(), name_tok.span),
                name,
                span: name_tok.span,
            })
        }
    }

    fn parse_tuple_pattern(&mut self) -> Result<Pattern, crate::error::CompileError> {
        let open = self.expect(TokenKind::LParen, "'('")?;
        if self.check(&TokenKind::RParen) {
            let close = self.advance();
            return Ok(Pattern::Tuple(vec![], open.span.to(close.span)));
        }

        let first = self.parse_pattern()?;

        if !self.bump_if(&TokenKind::Comma) {
            let close = self.expect(TokenKind::RParen, "')'")?;
            let _ = close;
            return Ok(first);
        }

        let mut elems = vec![first];
        if !self.check(&TokenKind::RParen) {
            elems.push(self.parse_pattern()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                elems.push(self.parse_pattern()?);
            }
        }
        let close = self.expect(TokenKind::RParen, "')'")?;
        Ok(Pattern::Tuple(elems, open.span.to(close.span)))
    }

    pub fn parse_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        self.parse_pipe()
    }

    fn parse_expr_no_struct_literal(&mut self) -> Result<Expr, crate::error::CompileError> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let result = self.parse_expr();
        self.no_struct_literal = saved;
        result
    }

    fn parse_expr_allowing_struct_literal(&mut self) -> Result<Expr, crate::error::CompileError> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = false;
        let result = self.parse_expr();
        self.no_struct_literal = saved;
        result
    }

    // --- token stream helpers ---

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn check(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == kind
    }

    // Contextual-keyword check — `require`/`ensure` (see `parse_contract_
    // clauses`) are ordinary identifiers everywhere EXCEPT this one
    // grammar slot (directly after a `LetDef`'s params/ret_ty, where the
    // only other legal token is `=`), so recognizing them by text here
    // rather than adding real `TokenKind` variants keeps them fully
    // usable as ordinary names anywhere else in the language — no lexer
    // change, no new reserved word.
    fn peek_is_contextual_kw(&self, word: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(name) if name == word)
    }

    fn bump_if(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<Token, crate::error::CompileError> {
        if self.check(&kind) {
            Ok(self.advance())
        } else {
            Err(crate::error::CompileError::new(
                self.peek().span,
                format!("expected {what}, found {:?}", self.peek_kind()),
            ))
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<Token, crate::error::CompileError> {
        if matches!(self.peek_kind(), TokenKind::Ident(_)) {
            Ok(self.advance())
        } else {
            Err(crate::error::CompileError::new(
                self.peek().span,
                format!("expected {what}, found {:?}", self.peek_kind()),
            ))
        }
    }

    fn ident_text(tok: &Token) -> String {
        match &tok.kind {
            TokenKind::Ident(s) => s.clone(),
            _ => unreachable!("ident_text called on a non-identifier token"),
        }
    }

    fn expect_str(&mut self, what: &str) -> Result<Token, crate::error::CompileError> {
        if matches!(self.peek_kind(), TokenKind::Str(_)) {
            Ok(self.advance())
        } else {
            Err(crate::error::CompileError::new(
                self.peek().span,
                format!("expected {what}, found {:?}", self.peek_kind()),
            ))
        }
    }

    fn str_text(tok: &Token) -> String {
        match &tok.kind {
            TokenKind::Str(s) => s.clone(),
            _ => unreachable!("str_text called on a non-string token"),
        }
    }

    // --- expression grammar, precedence loosest-to-tightest, matching
    // GRAMMAR.md's "Expressions" section exactly (one function per
    // precedence level) ---

    fn parse_pipe(&mut self) -> Result<Expr, crate::error::CompileError> {
        let mut lhs = self.parse_or()?;
        while self.bump_if(&TokenKind::PipeGt) {
            let rhs = self.parse_or()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op: BinaryOp::Pipe,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> Result<Expr, crate::error::CompileError> {
        let mut lhs = self.parse_and()?;
        while self.bump_if(&TokenKind::OrOr) {
            let rhs = self.parse_and()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op: BinaryOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, crate::error::CompileError> {
        let mut lhs = self.parse_compare()?;
        while self.bump_if(&TokenKind::AndAnd) {
            let rhs = self.parse_compare()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op: BinaryOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_compare(&mut self) -> Result<Expr, crate::error::CompileError> {
        let lhs = self.parse_range()?;
        let op = match self.peek_kind() {
            TokenKind::EqEq => BinaryOp::Eq,
            TokenKind::NotEq => BinaryOp::Ne,
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::LtEq => BinaryOp::Le,
            TokenKind::GtEq => BinaryOp::Ge,
            _ => return Ok(lhs),
        };
        self.advance();
        let rhs = self.parse_range()?;
        if is_compare_op(self.peek_kind()) {
            return Err(crate::error::CompileError::new(
                self.peek().span,
                "comparison operators do not chain — add parentheses (found another comparison operator here)",
            ));
        }
        let span = lhs.span().to(rhs.span());
        Ok(Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span,
        })
    }

    fn parse_range(&mut self) -> Result<Expr, crate::error::CompileError> {
        let lhs = self.parse_add()?;
        if !self.bump_if(&TokenKind::DotDot) {
            return Ok(lhs);
        }
        let rhs = self.parse_add()?;
        if self.check(&TokenKind::DotDot) {
            return Err(crate::error::CompileError::new(
                self.peek().span,
                "ranges do not chain — add parentheses (found another '..' here)",
            ));
        }
        let span = lhs.span().to(rhs.span());
        Ok(Expr::Binary {
            op: BinaryOp::Range,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span,
        })
    }

    fn parse_add(&mut self) -> Result<Expr, crate::error::CompileError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, crate::error::CompileError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Rem,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, crate::error::CompileError> {
        let op = match self.peek_kind() {
            TokenKind::Minus => Some(UnaryOp::Neg),
            TokenKind::Bang => Some(UnaryOp::Not),
            _ => None,
        };
        let Some(op) = op else {
            return self.parse_postfix();
        };
        let start = self.advance().span;
        let expr = self.parse_unary()?;
        let span = start.to(expr.span());
        Ok(Expr::Unary {
            op,
            expr: Box::new(expr),
            span,
        })
    }

    fn parse_postfix(&mut self) -> Result<Expr, crate::error::CompileError> {
        let expr = self.parse_primary()?;
        self.parse_postfix_from(expr)
    }

    // Continues a postfix chain (`.field`, `.method(...)`, `[index]`,
    // generic instantiation) from an already-parsed base expression,
    // rather than starting from `parse_primary`. Factored out of
    // `parse_postfix` so `_`-placeholder argument sugar (see
    // `parse_argument`) can build a chain starting from a synthetic
    // `_` base instead of a real primary expression.
    fn parse_postfix_from(&mut self, mut expr: Expr) -> Result<Expr, crate::error::CompileError> {
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.advance();
                    let name_tok = self.expect_ident("a field name")?;
                    let span = expr.span().to(name_tok.span);
                    expr = Expr::Field {
                        base: Box::new(expr),
                        name: Self::ident_text(&name_tok),
                        span,
                    };
                }
                TokenKind::LParen => {
                    let (args, close_span) = self.parse_arguments()?;
                    let span = expr.span().to(close_span);
                    expr = Expr::Call {
                        callee: Box::new(expr),
                        args,
                        span,
                    };
                }
                TokenKind::LBracket if self.next_bracket_is_generic() => {
                    let (args, close_span) = self.parse_generic_args()?;
                    let span = expr.span().to(close_span);
                    expr = Expr::GenericInst {
                        callee: Box::new(expr),
                        args,
                        span,
                    };
                }
                TokenKind::LBracket => {
                    self.advance();
                    let index = self.parse_expr_allowing_struct_literal()?;
                    let close = self.expect(TokenKind::RBracket, "']'")?;
                    let span = expr.span().to(close.span);
                    expr = Expr::Index {
                        base: Box::new(expr),
                        index: Box::new(index),
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    // Disambiguates `Thing[T]` (generic instantiation) from `arr[i]`
    // (indexing) by capitalization, per DESIGN.md/GRAMMAR.md. Checks the
    // LAST segment of a dotted identifier chain, not just the first —
    // a module-qualified type like `shapes.Circle` has a lowercase
    // first segment (the module) but is still a type, so looking only
    // at the first token gets this wrong. Bounded lookahead, no
    // backtracking.
    fn next_bracket_is_generic(&self) -> bool {
        let mut i = self.pos + 1;
        loop {
            let Some(TokenKind::Ident(name)) = self.tokens.get(i).map(|t| &t.kind) else {
                return false;
            };
            let capitalized = name.chars().next().is_some_and(|c| c.is_uppercase());
            i += 1;
            if matches!(self.tokens.get(i).map(|t| &t.kind), Some(TokenKind::Dot)) {
                i += 1;
                continue;
            }
            return capitalized;
        }
    }

    fn parse_arguments(&mut self) -> Result<(Vec<Expr>, Span), crate::error::CompileError> {
        self.expect(TokenKind::LParen, "'('")?;
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_argument()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                args.push(self.parse_argument()?);
            }
        }
        let close = self.expect(TokenKind::RParen, "')'")?;
        Ok((args, close.span))
    }

    // Placeholder-lambda sugar: a call argument that starts with `_`
    // is a postfix chain off an implicit single parameter, e.g.
    // `numbers.map(_.toString)` desugars to
    // `numbers.map(|_| _.toString)` (equivalent to `|n| n.toString()`).
    // Deliberately narrow, by design (see DESIGN.md): `_` is only
    // recognized as the RECEIVER of a postfix chain that is the
    // entire argument, not a general Scala-style placeholder usable
    // anywhere in an expression (`_ + 1`, `_.x + _.y`) — that's a
    // strictly more powerful, scoping-ambiguous feature that was
    // considered and intentionally deferred. Bare `_` (no chain, e.g.
    // `xs.map(_)`) is the identity function, the natural zero-length
    // case of the same rule.
    //
    // `"_"` is used as the synthetic parameter's name because it can
    // never collide with a real user identifier: the lexer emits a
    // distinct `Underscore` token for `_`, so `Ident("_")` is not
    // otherwise reachable by parsing real source text.
    fn parse_argument(&mut self) -> Result<Expr, crate::error::CompileError> {
        if self.check(&TokenKind::Underscore) {
            let underscore = self.advance();
            let base = Expr::Ident("_".to_string(), underscore.span);
            let chain = self.parse_postfix_from(base)?;
            let span = underscore.span.to(chain.span());
            return Ok(Expr::Closure {
                params: vec![ClosureParam {
                    name: "_".to_string(),
                    ty: None,
                    span: underscore.span,
                }],
                body: Box::new(chain),
                span,
            });
        }
        self.parse_expr_allowing_struct_literal()
    }

    // `[e1, e2, ...]` — mirrors `parse_arguments`'s comma-separated,
    // optional-trailing-comma shape exactly, just bracketed instead of
    // parenthesized and with no callee to attach to.
    fn parse_array_literal(&mut self) -> Result<Expr, crate::error::CompileError> {
        let start = self.expect(TokenKind::LBracket, "'['")?.span;
        let mut elements = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            elements.push(self.parse_expr_allowing_struct_literal()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RBracket) {
                    break;
                }
                elements.push(self.parse_expr_allowing_struct_literal()?);
            }
        }
        let close = self.expect(TokenKind::RBracket, "']'")?;
        Ok(Expr::ArrayLiteral(elements, start.to(close.span)))
    }

    fn parse_generic_args(&mut self) -> Result<(Vec<Type>, Span), crate::error::CompileError> {
        self.expect(TokenKind::LBracket, "'['")?;
        let mut args = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            args.push(self.parse_type()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RBracket) {
                    break;
                }
                args.push(self.parse_type()?);
            }
        }
        let close = self.expect(TokenKind::RBracket, "']'")?;
        Ok((args, close.span))
    }

    // A bare `(A)` (no `->`) is just grouping — parentheses around a
    // single type, degenerating to that type itself, same spirit as
    // grouping parens around an expression. `(A, B)` with no `->` (zero
    // or 2+ elements, no arrow) has no meaning yet — Plum has tuple
    // VALUES but no tuple-type annotation syntax — and is rejected with
    // a clear error rather than silently guessed at. `(A, B) -> R` /
    // `() -> R` build a real `Type::Function`.
    fn parse_type(&mut self) -> Result<Type, crate::error::CompileError> {
        if self.check(&TokenKind::LParen) {
            return self.parse_paren_or_function_type();
        }
        let first = self.expect_ident("a type name")?;
        let start = first.span;
        let mut end = first.span;
        let mut segments = vec![Self::ident_text(&first)];
        while self.check(&TokenKind::Dot) {
            self.advance();
            let seg = self.expect_ident("a path segment")?;
            end = seg.span;
            segments.push(Self::ident_text(&seg));
        }
        if self.check(&TokenKind::LBracket) {
            let (args, close_span) = self.parse_generic_args()?;
            return Ok(Type::Generic {
                base: segments,
                args,
                span: start.to(close_span),
            });
        }
        Ok(Type::Path(segments, start.to(end)))
    }

    fn parse_paren_or_function_type(&mut self) -> Result<Type, crate::error::CompileError> {
        let open = self.expect(TokenKind::LParen, "'('")?.span;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            params.push(self.parse_type()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_type()?);
            }
        }
        let close = self.expect(TokenKind::RParen, "')'")?.span;
        if self.bump_if(&TokenKind::Arrow) {
            let ret = self.parse_type()?;
            let span = open.to(ret.span());
            return Ok(Type::Function {
                params,
                ret: Box::new(ret),
                span,
            });
        }
        match params.len() {
            1 => Ok(params.into_iter().next().expect("just checked len == 1")),
            _ => Err(crate::error::CompileError::new(
                open.to(close),
                format!(
                    "expected a single parenthesized type or a function type ('(...) -> Type'), found {}",
                    if params.is_empty() { "'()'" } else { "multiple types" }
                ),
            )),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, crate::error::CompileError> {
        let tok = self.peek().clone();
        match &tok.kind {
            TokenKind::Int(n) => {
                self.advance();
                Ok(Expr::Int(*n, tok.span))
            }
            TokenKind::Float(f) => {
                self.advance();
                Ok(Expr::Float(*f, tok.span))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(Expr::Str(s.clone(), tok.span))
            }
            TokenKind::InterpStr(parts) => {
                self.advance();
                Self::desugar_interp_str(parts, tok.span)
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::Bool(true, tok.span))
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::Bool(false, tok.span))
            }
            // Every identifier goes through the path-aware parser, not
            // just capitalized-starting ones — a module-qualified
            // struct literal like `shapes.Circle { ... }` has a
            // lowercase FIRST segment (the module) but is still a
            // struct type, same lesson as the `[T]`-vs-indexing fix
            // above (next_bracket_is_generic): check the LAST segment,
            // not the first.
            TokenKind::Ident(_) => self.parse_path_shaped_expr(),
            TokenKind::LParen => self.parse_paren_or_tuple(),
            TokenKind::LBracket => self.parse_array_literal(),
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                let span = block.span;
                Ok(Expr::Block(block, span))
            }
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Match => self.parse_match_expr(),
            TokenKind::Select => self.parse_select_expr(),
            TokenKind::For => self.parse_for_expr(),
            // `||` lexes as a single OrOr token (maximal munch), which
            // must be treated as an empty closure parameter list rather
            // than a syntax error — the same lexer/parser interaction
            // Rust itself has for zero-arg closures.
            TokenKind::Pipe | TokenKind::OrOr => self.parse_closure_expr(),
            TokenKind::Unsafe => {
                let start = self.advance().span;
                let block = self.parse_block()?;
                let span = start.to(block.span);
                Ok(Expr::Unsafe(block, span))
            }
            TokenKind::Spawn => {
                let start = self.advance().span;
                let block = self.parse_block()?;
                let span = start.to(block.span);
                Ok(Expr::Spawn(block, span))
            }
            other => Err(crate::error::CompileError::new(tok.span, format!("expected an expression, found {other:?}"))),
        }
    }

    /// Desugars an interpolated string's `InterpPart`s into ordinary
    /// `.concat()`/`.to_string()` calls — `"a${x}b"` becomes `"a".
    /// concat(x.to_string()).concat("b")` — reusing the core builtins
    /// that already exist and already work generically over every
    /// type, rather than any new IR/backend machinery. Skips an empty
    /// literal segment's own `.concat("")` call (the common `"${x}"`
    /// case, no leading/trailing text) purely to keep the generated
    /// expression tree (and what it compiles to) a little leaner; not
    /// needed for correctness, `.concat("")` would be a no-op anyway.
    fn desugar_interp_str(parts: &[InterpPart], span: Span) -> Result<Expr, crate::error::CompileError> {
        let mut iter = parts.iter();
        let Some(InterpPart::Literal(first_lit)) = iter.next() else {
            unreachable!("lex_string always starts an InterpStr with a Literal part");
        };
        let mut acc: Option<Expr> = if first_lit.is_empty() { None } else { Some(Expr::Str(first_lit.clone(), span)) };
        while let Some(part) = iter.next() {
            let InterpPart::Expr(src, expr_span) = part else {
                unreachable!("lex_string always alternates Literal/Expr — two Literals in a row can't happen");
            };
            let inner = Self::parse_interp_expr(src, *expr_span)?;
            let stringified = Expr::Call {
                callee: Box::new(Expr::Field { base: Box::new(inner), name: "to_string".to_string(), span: *expr_span }),
                args: vec![],
                span: *expr_span,
            };
            acc = Some(match acc {
                None => stringified,
                Some(prev) => Expr::Call {
                    callee: Box::new(Expr::Field { base: Box::new(prev), name: "concat".to_string(), span }),
                    args: vec![stringified],
                    span,
                },
            });
            let Some(InterpPart::Literal(lit)) = iter.next() else {
                unreachable!("lex_string always alternates Expr/Literal, ending on a Literal");
            };
            if !lit.is_empty() {
                acc = Some(Expr::Call {
                    callee: Box::new(Expr::Field { base: Box::new(acc.expect("just set above")), name: "concat".to_string(), span }),
                    args: vec![Expr::Str(lit.clone(), span)],
                    span,
                });
            }
        }
        Ok(acc.expect("lex_string only emits InterpStr when at least one Expr part is present"))
    }

    /// Re-lexes and parses ONE `${...}` interpolation's raw source text
    /// as an ordinary expression — `Lexer::with_base_offset` anchors
    /// every token's span back to where `src` actually sits in the
    /// original file, so error locations (and anything else keyed by
    /// `Span`, like `plum_types::infer::Infer::field_owners`) are
    /// exactly as if this text had been parsed in place, not as a
    /// separate fragment. A parse failure here is enriched with a hint
    /// when `src` contains a `{` or another `"` — the two shapes
    /// `InterpPart`'s own doc comment documents as unsupported (block
    /// expressions/closures with block bodies, and nested interpolated
    /// strings) — since THAT'S the overwhelmingly likely reason,
    /// rather than an ordinary typo, that something here failed to
    /// parse.
    fn parse_interp_expr(src: &str, span: Span) -> Result<Expr, crate::error::CompileError> {
        let tokens = Lexer::with_base_offset(src, span.start as usize).tokenize();
        let mut parser = Parser::new(tokens);
        let expr = parser.parse_expr().map_err(|e| Self::enrich_interp_parse_error(e, src, span))?;
        if !matches!(parser.peek_kind(), TokenKind::Eof) {
            return Err(Self::enrich_interp_parse_error(
                crate::error::CompileError::new(parser.peek().span, "unexpected trailing content inside `${...}`".to_string()),
                src,
                span,
            ));
        }
        Ok(expr)
    }

    fn enrich_interp_parse_error(e: crate::error::CompileError, src: &str, span: Span) -> crate::error::CompileError {
        if src.contains('{') || src.contains('"') {
            crate::error::CompileError::new(
                span,
                format!(
                    "{e} (hint: block expressions, closures with a block body, and nested interpolated \
                     strings aren't supported inside `${{...}}` — pull it into a variable first)"
                ),
            )
        } else {
            e
        }
    }

    fn parse_paren_or_tuple(&mut self) -> Result<Expr, crate::error::CompileError> {
        let open = self.expect(TokenKind::LParen, "'('")?;
        if self.check(&TokenKind::RParen) {
            // `()` — Unit's literal form (see DESIGN.md's "Tuples and
            // closures").
            let close = self.advance();
            return Ok(Expr::Tuple(vec![], open.span.to(close.span)));
        }

        let first = self.parse_expr_allowing_struct_literal()?;

        if !self.bump_if(&TokenKind::Comma) {
            // Just grouping, not a tuple: `(x)` is `x`.
            let close = self.expect(TokenKind::RParen, "')'")?;
            let _ = close;
            return Ok(first);
        }

        let mut elems = vec![first];
        if !self.check(&TokenKind::RParen) {
            elems.push(self.parse_expr_allowing_struct_literal()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break; // trailing comma after the last element
                }
                elems.push(self.parse_expr_allowing_struct_literal()?);
            }
        }
        let close = self.expect(TokenKind::RParen, "')'")?;
        Ok(Expr::Tuple(elems, open.span.to(close.span)))
    }

    // A capitalized leading identifier may continue as a dotted path
    // (`shapes.Circle`), and that path may then be a struct literal
    // (`Point { ... }`) or just an ordinary value (rebuilt as the same
    // Field-chain shape `.` postfix parsing already produces, e.g. for
    // `Shape.Circle(r)` used as a call).
    fn parse_expr_path(&mut self) -> Result<Vec<(String, Span)>, crate::error::CompileError> {
        let first = self.expect_ident("an identifier")?;
        let mut segments = vec![(Self::ident_text(&first), first.span)];
        while self.check(&TokenKind::Dot) {
            self.advance();
            let seg = self.expect_ident("a path segment")?;
            segments.push((Self::ident_text(&seg), seg.span));
        }
        Ok(segments)
    }

    fn parse_path_shaped_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        let segments = self.parse_expr_path()?;
        let last_capitalized = segments[segments.len() - 1]
            .0
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase());
        if self.check(&TokenKind::LBrace) && last_capitalized && !self.no_struct_literal {
            let path: Vec<String> = segments.iter().map(|(name, _)| name.clone()).collect();
            let path_span = segments[0].1.to(segments[segments.len() - 1].1);
            return self.parse_struct_literal(path, path_span);
        }
        let mut iter = segments.into_iter();
        let (first_name, first_span) = iter.next().expect("path always has at least one segment");
        let mut expr = Expr::Ident(first_name, first_span);
        for (name, span) in iter {
            let combined = expr.span().to(span);
            expr = Expr::Field {
                base: Box::new(expr),
                name,
                span: combined,
            };
        }
        Ok(expr)
    }

    fn parse_struct_literal(&mut self, path: Vec<String>, path_span: Span) -> Result<Expr, crate::error::CompileError> {
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut fields = Vec::new();
        let mut spread = None;
        if self.bump_if(&TokenKind::DotDot) {
            spread = Some(Box::new(self.parse_expr()?));
        } else if !self.check(&TokenKind::RBrace) {
            fields.push(self.parse_field_init()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.bump_if(&TokenKind::DotDot) {
                    spread = Some(Box::new(self.parse_expr()?));
                    break;
                }
                if self.check(&TokenKind::RBrace) {
                    break;
                }
                fields.push(self.parse_field_init()?);
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(Expr::StructLiteral {
            path,
            fields,
            spread,
            span: path_span.to(close.span),
        })
    }

    // `Identifier { "." Identifier } [ ":" Expr ]` — a plain field
    // (`x` or `x: expr`) is the zero-dot case; further `.segment` steps
    // are the nested field-update path sugar (`ship.position.x: nx`,
    // see `plumc::nested_struct_update` for what it expands to). The
    // shorthand form (no `: expr`) is only valid with ZERO extra
    // segments — `ship.position` alone doesn't mean anything (there's
    // no local named `ship.position` for it to shorthand to).
    fn parse_field_init(&mut self) -> Result<FieldInit, crate::error::CompileError> {
        let name_tok = self.expect_ident("a field name")?;
        let name = Self::ident_text(&name_tok);
        let mut extra_path = Vec::new();
        let mut last_span = name_tok.span;
        while self.check(&TokenKind::Dot) {
            self.advance();
            let seg_tok = self.expect_ident("a nested field-update path segment")?;
            last_span = seg_tok.span;
            extra_path.push((Self::ident_text(&seg_tok), seg_tok.span));
        }
        if self.bump_if(&TokenKind::Colon) {
            let value = self.parse_expr()?;
            let span = name_tok.span.to(value.span());
            Ok(FieldInit { name, name_span: name_tok.span, extra_path, value, span })
        } else if extra_path.is_empty() {
            Ok(FieldInit {
                value: Expr::Ident(name.clone(), name_tok.span),
                name,
                name_span: name_tok.span,
                extra_path,
                span: name_tok.span,
            })
        } else {
            Err(crate::error::CompileError::new(
                last_span,
                "a nested field-update path (`a.b.c`) needs an explicit `: value` — there's no shorthand form for it".to_string(),
            ))
        }
    }

    fn parse_if_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        let start = self.expect(TokenKind::If, "'if'")?.span;
        let cond = self.parse_expr_no_struct_literal()?;
        let then_branch = self.parse_block()?;
        let mut end = then_branch.span;
        let else_branch = if self.bump_if(&TokenKind::Else) {
            let e = if self.check(&TokenKind::If) {
                self.parse_if_expr()?
            } else {
                let block = self.parse_block()?;
                let span = block.span;
                Expr::Block(block, span)
            };
            end = e.span();
            Some(Box::new(e))
        } else {
            None
        };
        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch,
            else_branch,
            span: start.to(end),
        })
    }

    fn parse_match_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        let start = self.expect(TokenKind::Match, "'match'")?.span;
        let scrutinee = self.parse_expr_no_struct_literal()?;
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let pattern = self.parse_pattern()?;
            let guard = if self.bump_if(&TokenKind::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(TokenKind::FatArrow, "'=>'")?;
            let body = self.parse_expr()?;
            let span = pattern.span().to(body.span());
            arms.push(MatchArm {
                pattern,
                guard,
                body,
                span,
            });
            if !self.bump_if(&TokenKind::Comma) {
                break;
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: start.to(close.span),
        })
    }

    // `select { pattern = expr => body, ... }` — deliberately mirrors
    // `parse_match_expr`'s shape (comma-separated arms, optional
    // trailing comma) as closely as possible; the only real difference
    // is each arm has a `pattern = expr` prefix instead of matching
    // against one shared scrutinee. `expr` is parsed as an ordinary
    // expression here — this parser does NOT require it to look like
    // `X.recv()`; that shape check happens downstream (lowering/
    // inference), the same "grammar doesn't know about method-call-
    // shaped builtins" precedent `.join()`/`.send()`/`.recv()`
    // themselves already established.
    fn parse_select_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        let start = self.expect(TokenKind::Select, "'select'")?.span;
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let pattern = self.parse_pattern()?;
            self.expect(TokenKind::Eq, "'='")?;
            let expr = self.parse_expr_no_struct_literal()?;
            self.expect(TokenKind::FatArrow, "'=>'")?;
            let body = self.parse_expr()?;
            let span = pattern.span().to(body.span());
            arms.push(SelectArm {
                pattern,
                expr,
                body,
                span,
            });
            if !self.bump_if(&TokenKind::Comma) {
                break;
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(Expr::Select {
            arms,
            span: start.to(close.span),
        })
    }

    fn parse_for_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        let start = self.expect(TokenKind::For, "'for'")?.span;
        let pattern = self.parse_pattern()?;
        self.expect(TokenKind::In, "'in'")?;
        let iter = self.parse_expr_no_struct_literal()?;
        let body = self.parse_block()?;
        let span = start.to(body.span);
        Ok(Expr::For {
            pattern,
            iter: Box::new(iter),
            body,
            span,
        })
    }

    fn parse_closure_expr(&mut self) -> Result<Expr, crate::error::CompileError> {
        let start_tok = self.advance(); // Pipe or OrOr
        let mut params = Vec::new();
        // OrOr means both delimiters were already consumed as one
        // token (`||`) — an empty parameter list.
        if start_tok.kind == TokenKind::Pipe {
            if !self.check(&TokenKind::Pipe) {
                params.push(self.parse_closure_param()?);
                while self.bump_if(&TokenKind::Comma) {
                    params.push(self.parse_closure_param()?);
                }
            }
            self.expect(TokenKind::Pipe, "'|'")?;
        }
        let body = self.parse_expr()?;
        let span = start_tok.span.to(body.span());
        Ok(Expr::Closure {
            params,
            body: Box::new(body),
            span,
        })
    }

    fn parse_closure_param(&mut self) -> Result<ClosureParam, crate::error::CompileError> {
        let name_tok = self.expect_ident("a closure parameter")?;
        let name = Self::ident_text(&name_tok);
        let ty = if self.bump_if(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(ClosureParam {
            name,
            ty,
            span: name_tok.span,
        })
    }

    fn parse_block(&mut self) -> Result<Block, crate::error::CompileError> {
        let open = self.expect(TokenKind::LBrace, "'{'")?;
        let mut stmts = Vec::new();
        let mut tail = None;
        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Let) {
                let stmt = self.parse_let_stmt()?;
                self.expect(TokenKind::Semicolon, "';'")?;
                stmts.push(stmt);
                continue;
            }

            let expr = self.parse_expr_allowing_struct_literal()?;

            if self.check(&TokenKind::Eq) {
                let name = match &expr {
                    Expr::Ident(name, _) => name.clone(),
                    _ => {
                        return Err(crate::error::CompileError::new(
                            expr.span(),
                            "invalid assignment target — only a plain local variable may be assigned",
                        ));
                    }
                };
                self.advance(); // consume '='
                let value = self.parse_expr()?;
                let span = expr.span().to(value.span());
                self.expect(TokenKind::Semicolon, "';'")?;
                stmts.push(Stmt::Assign { name, value, span });
                continue;
            }

            if self.bump_if(&TokenKind::Semicolon) {
                stmts.push(Stmt::Expr(expr));
            } else {
                tail = Some(Box::new(expr));
                break;
            }
        }
        let close = self.expect(TokenKind::RBrace, "'}'")?;
        Ok(Block {
            stmts,
            tail,
            span: open.span.to(close.span),
        })
    }

    fn parse_let_stmt(&mut self) -> Result<Stmt, crate::error::CompileError> {
        let start = self.expect(TokenKind::Let, "'let'")?.span;
        let is_mut = self.bump_if(&TokenKind::Mut);
        let pattern = self.parse_pattern()?;
        let ty = if self.bump_if(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(TokenKind::Eq, "'='")?;
        let value = self.parse_expr()?;
        let span = start.to(value.span());
        Ok(Stmt::Let {
            is_mut,
            pattern,
            ty,
            value,
            span,
        })
    }
}

fn is_compare_op(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::EqEq
            | TokenKind::NotEq
            | TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::LtEq
            | TokenKind::GtEq
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::render::*;

    fn parse(src: &str) -> Expr {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let expr = parser
            .parse_expr()
            .unwrap_or_else(|e| panic!("parse error for {src:?}: {e}"));
        assert!(
            parser.is_at_eof(),
            "leftover tokens after parsing {src:?}: {:?}",
            &parser.tokens[parser.pos..]
        );
        expr
    }

    fn parse_err(src: &str) {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        // Either a hard parse error, or leftover tokens (the parser
        // stopped before consuming the whole malformed input) both
        // count as "this was rejected" for these tests.
        match parser.parse_expr() {
            Err(_) => {}
            Ok(_) if !parser.is_at_eof() => {}
            Ok(expr) => panic!("expected {src:?} to be rejected, got {}", render(&expr)),
        }
    }

    #[test]
    fn literal_int() {
        assert_eq!(render(&parse("5")), "5");
    }

    #[test]
    fn literal_float() {
        assert_eq!(render(&parse("3.14")), "3.14");
    }

    #[test]
    fn literal_string() {
        assert_eq!(render(&parse("\"hi\"")), "\"hi\"");
    }

    #[test]
    fn literal_bools() {
        assert_eq!(render(&parse("true")), "true");
        assert_eq!(render(&parse("false")), "false");
    }

    #[test]
    fn identifier() {
        assert_eq!(render(&parse("x")), "x");
    }

    #[test]
    fn unary_negation() {
        assert_eq!(render(&parse("-5")), "(- 5)");
    }

    #[test]
    fn unary_not() {
        assert_eq!(render(&parse("!flag")), "(! flag)");
    }

    #[test]
    fn mul_binds_tighter_than_add() {
        assert_eq!(render(&parse("1 + 2 * 3")), "(+ 1 (* 2 3))");
    }

    #[test]
    fn subtraction_is_left_associative() {
        assert_eq!(render(&parse("1 - 2 - 3")), "(- (- 1 2) 3)");
    }

    #[test]
    fn division_is_left_associative() {
        assert_eq!(render(&parse("10 / 2 / 5")), "(/ (/ 10 2) 5)");
    }

    #[test]
    fn comparison_valid_single() {
        assert_eq!(render(&parse("a < b")), "(< a b)");
        assert_eq!(render(&parse("a == b")), "(== a b)");
    }

    #[test]
    fn comparison_does_not_chain() {
        parse_err("a < b < c");
        parse_err("a == b == c");
    }

    #[test]
    fn range_binds_looser_than_addition() {
        assert_eq!(render(&parse("0..n+1")), "(.. 0 (+ n 1))");
    }

    #[test]
    fn range_does_not_chain() {
        parse_err("0..1..2");
    }

    #[test]
    fn logical_and_binds_tighter_than_or() {
        assert_eq!(render(&parse("a && b || c")), "(|| (&& a b) c)");
    }

    #[test]
    fn pipe_is_lowest_precedence() {
        assert_eq!(render(&parse("a + b |> f")), "(|> (+ a b) f)");
    }

    #[test]
    fn pipe_is_left_associative() {
        assert_eq!(render(&parse("x |> f |> g")), "(|> (|> x f) g)");
    }

    #[test]
    fn parens_override_precedence() {
        assert_eq!(render(&parse("(1 + 2) * 3")), "(* (+ 1 2) 3)");
    }

    #[test]
    fn single_parenthesized_expr_is_not_a_tuple() {
        assert_eq!(render(&parse("(5)")), "5");
    }

    #[test]
    fn two_element_tuple() {
        assert_eq!(render(&parse("(1, 2)")), "(tuple 1 2)");
    }

    #[test]
    fn one_element_tuple_needs_trailing_comma() {
        assert_eq!(render(&parse("(5,)")), "(tuple 5)");
    }

    #[test]
    fn tuple_allows_trailing_comma_after_last_element() {
        assert_eq!(render(&parse("(1, 2,)")), "(tuple 1 2)");
    }

    #[test]
    fn field_access() {
        assert_eq!(render(&parse("p.x")), "(field p x)");
    }

    #[test]
    fn chained_field_access() {
        assert_eq!(render(&parse("a.b.c")), "(field (field a b) c)");
    }

    #[test]
    fn function_call() {
        assert_eq!(render(&parse("f(1, 2)")), "(call f 1 2)");
    }

    #[test]
    fn call_with_no_args() {
        assert_eq!(render(&parse("f()")), "(call f)");
    }

    #[test]
    fn method_call_chain() {
        assert_eq!(render(&parse("Ref.new(start)")), "(call (field Ref new) start)");
    }

    #[test]
    fn generic_instantiation_uses_capitalization_heuristic() {
        assert_eq!(render(&parse("Ref[Vec2]")), "(generic Ref Vec2)");
        assert_eq!(render(&parse("channel[Int]()")), "(call (generic channel Int))");
    }

    #[test]
    fn indexing_with_lowercase_is_not_generic_instantiation() {
        assert_eq!(render(&parse("arr[i]")), "(index arr i)");
    }

    #[test]
    fn nested_generic_type_argument() {
        assert_eq!(render(&parse("thing[Ref[Int]]")), "(generic thing (gt Ref Int))");
    }

    #[test]
    fn dotted_path_generic_type_argument() {
        assert_eq!(render(&parse("f[shapes.Circle]")), "(generic f shapes.Circle)");
    }

    #[test]
    fn combined_precedence_with_postfix() {
        assert_eq!(render(&parse("a.b + c.d")), "(+ (field a b) (field c d))");
    }

    #[test]
    fn unclosed_paren_is_an_error() {
        parse_err("(1 + 2");
    }

    #[test]
    fn trailing_operator_is_an_error() {
        parse_err("1 +");
    }

    // --- Pattern grammar ---

    fn parse_pattern(src: &str) -> Pattern {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let pattern = parser
            .parse_pattern()
            .unwrap_or_else(|e| panic!("pattern parse error for {src:?}: {e}"));
        assert!(
            parser.is_at_eof(),
            "leftover tokens after parsing pattern {src:?}: {:?}",
            &parser.tokens[parser.pos..]
        );
        pattern
    }

    fn pattern_parse_err(src: &str) {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        match parser.parse_pattern() {
            Err(_) => {}
            Ok(_) if !parser.is_at_eof() => {}
            Ok(p) => panic!("expected pattern {src:?} to be rejected, got {}", render_pattern(&p)),
        }
    }

    #[test]
    fn pattern_literals() {
        assert_eq!(render_pattern(&parse_pattern("5")), "5");
        assert_eq!(render_pattern(&parse_pattern("3.14")), "3.14");
        assert_eq!(render_pattern(&parse_pattern("\"hi\"")), "\"hi\"");
        assert_eq!(render_pattern(&parse_pattern("true")), "true");
    }

    #[test]
    fn pattern_wildcard() {
        assert_eq!(render_pattern(&parse_pattern("_")), "_");
    }

    #[test]
    fn pattern_plain_binding() {
        assert_eq!(render_pattern(&parse_pattern("x")), "x");
    }

    #[test]
    fn pattern_bare_zero_arg_variant() {
        // Capitalized bare identifier — a variant reference like `None`,
        // not a fresh binding. Same capitalization convention used
        // throughout (`.`, `[T]`).
        assert_eq!(render_pattern(&parse_pattern("None")), "(variant None)");
    }

    #[test]
    fn pattern_qualified_zero_arg_variant() {
        assert_eq!(
            render_pattern(&parse_pattern("Option.None")),
            "(variant Option.None)"
        );
    }

    #[test]
    fn pattern_variant_with_one_arg() {
        assert_eq!(
            render_pattern(&parse_pattern("Shape.Circle(r)")),
            "(variant Shape.Circle r)"
        );
    }

    #[test]
    fn pattern_variant_with_multiple_args() {
        assert_eq!(
            render_pattern(&parse_pattern("Shape.Rectangle(w, h)")),
            "(variant Shape.Rectangle w h)"
        );
    }

    #[test]
    fn pattern_variant_with_wildcard_args() {
        assert_eq!(
            render_pattern(&parse_pattern("Shape.Rectangle(_, _)")),
            "(variant Shape.Rectangle _ _)"
        );
    }

    #[test]
    fn pattern_nested_variant() {
        assert_eq!(
            render_pattern(&parse_pattern("Some(Shape.Circle(r))")),
            "(variant Some (variant Shape.Circle r))"
        );
    }

    #[test]
    fn pattern_tuple() {
        assert_eq!(render_pattern(&parse_pattern("(a, b)")), "(tuple a b)");
    }

    #[test]
    fn pattern_one_element_tuple_needs_trailing_comma() {
        assert_eq!(render_pattern(&parse_pattern("(a,)")), "(tuple a)");
    }

    #[test]
    fn pattern_single_parenthesized_is_not_a_tuple() {
        assert_eq!(render_pattern(&parse_pattern("(a)")), "a");
    }

    #[test]
    fn pattern_struct_shorthand() {
        assert_eq!(
            render_pattern(&parse_pattern("Point { x, y }")),
            "(struct Point x=x y=y)"
        );
    }

    #[test]
    fn pattern_struct_rename() {
        assert_eq!(
            render_pattern(&parse_pattern("Point { x: px, y: py }")),
            "(struct Point x=px y=py)"
        );
    }

    #[test]
    fn pattern_struct_with_rest() {
        assert_eq!(
            render_pattern(&parse_pattern("Point { x, .. }")),
            "(struct Point x=x ..)"
        );
    }

    #[test]
    fn pattern_struct_bare_rest() {
        assert_eq!(render_pattern(&parse_pattern("Point { .. }")), "(struct Point ..)");
    }

    #[test]
    fn pattern_or() {
        assert_eq!(
            render_pattern(&parse_pattern("Shape.Square(s) | Shape.Rectangle(s, s)")),
            "(or (variant Shape.Square s) (variant Shape.Rectangle s s))"
        );
    }

    #[test]
    fn pattern_or_from_is_flat_example() {
        // Straight out of examples/overview.plum's `is_flat` function.
        assert_eq!(
            render_pattern(&parse_pattern(
                "Shape.Rectangle(_, _) | Shape.Triangle(_, _, _)"
            )),
            "(or (variant Shape.Rectangle _ _) (variant Shape.Triangle _ _ _))"
        );
    }

    #[test]
    fn pattern_unclosed_variant_args_is_an_error() {
        pattern_parse_err("Shape.Circle(r");
    }

    // --- Block-level expression forms: blocks, if/match/for, closures,
    // unsafe, spawn, struct literals ---

    #[test]
    fn empty_block_is_unit() {
        assert_eq!(render(&parse("{}")), "(block)");
    }

    #[test]
    fn block_with_only_a_tail_expr() {
        assert_eq!(render(&parse("{ 5 }")), "(block 5)");
    }

    #[test]
    fn block_statement_rule_no_exemption_for_block_shaped_statements() {
        // Every non-tail item needs its own `;`, including `if`/`for`
        // used as statements — no Rust-style exemption. See DESIGN.md's
        // "Block statement/expression rule."
        assert_eq!(
            render(&parse("{ if true { 1 }; 2 }")),
            "(block (stmt (if true (block 1))) 2)"
        );
    }

    #[test]
    fn block_let_statement() {
        assert_eq!(
            render(&parse("{ let x = 5; x }")),
            "(block (let x 5) x)"
        );
    }

    #[test]
    fn block_let_mut_statement() {
        assert_eq!(
            render(&parse("{ let mut x = 5; x }")),
            "(block (let mut x 5) x)"
        );
    }

    #[test]
    fn block_trailing_semicolon_means_unit_value() {
        assert_eq!(render(&parse("{ 5; }")), "(block (stmt 5))");
    }

    #[test]
    fn nested_blocks() {
        assert_eq!(render(&parse("{ { 1 }; 2 }")), "(block (stmt (block 1)) 2)");
    }

    #[test]
    fn if_else() {
        assert_eq!(
            render(&parse("if true { 1 } else { 2 }")),
            "(if true (block 1) (block 2))"
        );
    }

    #[test]
    fn if_no_else() {
        assert_eq!(render(&parse("if true { 1 }")), "(if true (block 1))");
    }

    #[test]
    fn if_else_if_chain() {
        assert_eq!(
            render(&parse("if a { 1 } else if b { 2 } else { 3 }")),
            "(if a (block 1) (if b (block 2) (block 3)))"
        );
    }

    #[test]
    fn struct_literal_disallowed_bare_in_if_condition() {
        // GRAMMAR.md's flagged ambiguity: `Point { .. }` right after
        // `if` must NOT be parsed as a struct literal condition — the
        // `{` belongs to the if's body block instead. `Point` alone
        // (nonsensical as a Bool condition, but that's a type error,
        // not a parse error) is the condition.
        assert_eq!(
            render(&parse("if Point { 1 } else { 2 }")),
            "(if Point (block 1) (block 2))"
        );
    }

    #[test]
    fn struct_literal_allowed_when_parenthesized_in_condition() {
        assert_eq!(
            render(&parse("if (Point { x: 1.0 }) { 1 }")),
            "(if (struct-lit Point x=1) (block 1))"
        );
    }

    #[test]
    fn struct_literal_allowed_inside_call_args_within_condition() {
        // The restriction resets inside any nested bracket — call args
        // are unambiguous even while the outer condition disallows a
        // bare struct literal.
        assert_eq!(
            render(&parse("if f(Point { x: 1.0 }) { 1 }")),
            "(if (call f (struct-lit Point x=1)) (block 1))"
        );
    }

    #[test]
    fn match_basic() {
        assert_eq!(
            render(&parse("match shape { Shape.Circle(r) => r, _ => 0 }")),
            "(match shape (arm (variant Shape.Circle r) r) (arm _ 0))"
        );
    }

    #[test]
    fn match_with_guard() {
        assert_eq!(
            render(&parse("match n { x if x > 0 => 1, _ => 0 }")),
            "(match n (arm x if=(> x 0) 1) (arm _ 0))"
        );
    }

    #[test]
    fn match_is_flat_example_from_overview_plum() {
        assert_eq!(
            render(&parse(
                "match shape { Shape.Rectangle(w, h) if w == h => true, Shape.Circle(r) if r == 0.0 => true, Shape.Rectangle(_, _) | Shape.Triangle(_, _, _) => false, _ => false }"
            )),
            "(match shape \
             (arm (variant Shape.Rectangle w h) if=(== w h) true) \
             (arm (variant Shape.Circle r) if=(== r 0) true) \
             (arm (or (variant Shape.Rectangle _ _) (variant Shape.Triangle _ _ _)) false) \
             (arm _ false))"
        );
    }

    #[test]
    fn for_loop() {
        assert_eq!(
            render(&parse("for i in 0..n { total = total + i; }")),
            "(for i (.. 0 n) (block (= total (+ total i))))"
        );
    }

    #[test]
    fn closure_single_param_no_annotation() {
        assert_eq!(render(&parse("|p| p")), "(closure (p) p)");
    }

    #[test]
    fn closure_multiple_params() {
        assert_eq!(render(&parse("|a, b| a")), "(closure (a b) a)");
    }

    #[test]
    fn closure_zero_params() {
        // `||` lexes as a single OrOr token — must be treated as an
        // empty parameter list, not a syntax error or logical-or.
        assert_eq!(render(&parse("|| 5")), "(closure () 5)");
    }

    #[test]
    fn closure_annotated_param() {
        assert_eq!(render(&parse("|p: Point| p")), "(closure (p) p)");
    }

    #[test]
    fn underscore_placeholder_method_call_argument() {
        // `numbers.map(_.toString)` desugars to `numbers.map(|_| _.toString)`
        // — equivalent in meaning to `numbers.map(|n| n.toString())`, just
        // rendered with the synthetic param's real name, "_".
        assert_eq!(
            render(&parse("numbers.map(_.toString())")),
            "(call (field numbers map) (closure (_) (call (field _ toString))))"
        );
    }

    #[test]
    fn underscore_placeholder_field_access_argument() {
        assert_eq!(
            render(&parse("points.map(_.x)")),
            "(call (field points map) (closure (_) (field _ x)))"
        );
    }

    #[test]
    fn underscore_placeholder_bare_is_identity() {
        // No chain at all — the zero-length case of the same rule,
        // equivalent to `xs.map(|x| x)`.
        assert_eq!(render(&parse("xs.map(_)")), "(call (field xs map) (closure (_) _))");
    }

    #[test]
    fn underscore_placeholder_chained_access() {
        assert_eq!(
            render(&parse("things.each(_.a.b())")),
            "(call (field things each) (closure (_) (call (field (field _ a) b))))"
        );
    }

    #[test]
    fn underscore_placeholder_alongside_other_arguments() {
        // Only the argument that STARTS with `_` gets the sugar — a
        // sibling argument is parsed normally, unaffected.
        assert_eq!(
            render(&parse("pairs.fold(0, _.value)")),
            "(call (field pairs fold) 0 (closure (_) (field _ value)))"
        );
    }

    #[test]
    fn underscore_placeholder_not_a_general_expression() {
        // Deliberately narrow: `_` is only sugar as the receiver of a
        // postfix chain that IS the whole argument, not usable
        // anywhere in an expression like Scala's placeholder syntax.
        parse_err("xs.map(_ + 1)");
        // And outside argument position, `_` is still just the plain
        // Underscore token — not a general expression.
        parse_err("let x = _;");
    }

    #[test]
    fn unsafe_block() {
        assert_eq!(render(&parse("unsafe { sqrt(4.0) }")), "(unsafe (block (call sqrt 4)))");
    }

    #[test]
    fn spawn_block() {
        assert_eq!(render(&parse("spawn { producer(tx) }")), "(spawn (block (call producer tx)))");
    }

    #[test]
    fn select_basic() {
        assert_eq!(
            render(&parse("select { v = rx1.recv() => v, w = rx2.recv() => w }")),
            "(select (arm v = (call (field rx1 recv)) v) (arm w = (call (field rx2 recv)) w))"
        );
    }

    #[test]
    fn select_with_wildcard_arm() {
        assert_eq!(
            render(&parse("select { v = rx1.recv() => v, _ = rx2.recv() => 0 }")),
            "(select (arm v = (call (field rx1 recv)) v) (arm _ = (call (field rx2 recv)) 0))"
        );
    }

    #[test]
    fn select_trailing_comma() {
        assert_eq!(
            render(&parse("select { v = rx.recv() => v, }")),
            "(select (arm v = (call (field rx recv)) v))"
        );
    }

    // --- Arrays ---

    #[test]
    fn array_literal_basic() {
        assert_eq!(render(&parse("[1, 2, 3]")), "(array 1 2 3)");
    }

    #[test]
    fn array_literal_empty() {
        assert_eq!(render(&parse("[]")), "(array)");
    }

    #[test]
    fn array_literal_trailing_comma() {
        assert_eq!(render(&parse("[1, 2,]")), "(array 1 2)");
    }

    #[test]
    fn array_index_lowercase_disambiguates_from_generic_instantiation() {
        assert_eq!(render(&parse("arr[i]")), "(index arr i)");
    }

    #[test]
    fn array_index_on_a_literal() {
        assert_eq!(render(&parse("[1, 2, 3][0]")), "(index (array 1 2 3) 0)");
    }

    #[test]
    fn struct_literal_shorthand_and_explicit() {
        assert_eq!(
            render(&parse("Point { x: 1.0, y }")),
            "(struct-lit Point x=1 y=y)"
        );
    }

    #[test]
    fn struct_literal_with_spread() {
        assert_eq!(
            render(&parse("Point { x: p.x + dx, ..p }")),
            "(struct-lit Point x=(+ (field p x) dx) ..p)"
        );
    }

    #[test]
    fn struct_literal_qualified_path() {
        assert_eq!(
            render(&parse("shapes.Circle { radius: 2.0 }")),
            "(struct-lit shapes.Circle radius=2)"
        );
    }

    #[test]
    fn struct_literal_nested_field_update_path() {
        // Parsed only — `plumc::nested_struct_update` is what actually
        // expands this into real nested `StructLiteral`s; the parser's
        // only job is capturing the dotted path onto `FieldInit`.
        assert_eq!(
            render(&parse("Game { ship.position.x: nx, score: s, ..g }")),
            "(struct-lit Game ship.position.x=nx score=s ..g)"
        );
    }

    #[test]
    fn struct_literal_nested_field_update_path_requires_an_explicit_value() {
        parse_err("Game { ship.position }");
    }

    // --- String interpolation: desugars entirely at parse time into
    // ordinary `.concat()`/`.to_string()` calls — no new `Expr` variant,
    // so `render`'s existing arms show it exactly as if it had been
    // hand-written that way.

    #[test]
    fn interpolated_string_desugars_to_concat_and_to_string_calls() {
        assert_eq!(
            render(&parse("\"hello, ${name}!\"")),
            "(call (field (call (field \"hello, \" concat) (call (field name to_string))) concat) \"!\")"
        );
    }

    #[test]
    fn an_interpolation_with_no_surrounding_literal_text_skips_the_empty_concat() {
        // `"${x}"` should desugar straight to `x.to_string()`, not
        // `"".concat(x.to_string()).concat("")`.
        assert_eq!(render(&parse("\"${x}\"")), "(call (field x to_string))");
    }

    #[test]
    fn multiple_interpolations_chain_left_to_right() {
        assert_eq!(
            render(&parse("\"a${x}b${y}c\"")),
            "(call (field (call (field (call (field (call (field \"a\" concat) (call (field x to_string))) concat) \"b\") concat) \
             (call (field y to_string))) concat) \"c\")"
        );
    }

    #[test]
    fn an_interpolated_expression_can_be_an_arbitrary_arithmetic_or_call_expression() {
        assert_eq!(
            render(&parse("\"${1 + f(2, 3)}\"")),
            "(call (field (+ 1 (call f 2 3)) to_string))"
        );
    }

    #[test]
    fn a_plain_string_with_no_interpolation_is_unaffected() {
        assert_eq!(render(&parse("\"just plain text\"")), "\"just plain text\"");
    }

    #[test]
    fn a_block_expression_inside_interpolation_is_a_clear_parse_error() {
        parse_err("\"${if x { 1 } else { 2 }}\"");
        let tokens = Lexer::new("\"${if x { 1 } else { 2 }}\"").tokenize();
        let err = Parser::new(tokens).parse_expr().expect_err("expected a parse error");
        assert!(err.to_string().contains("hint"), "expected the block-expression hint, got: {err}");
    }

    #[test]
    fn full_tick_function_body_from_overview_plum() {
        assert_eq!(
            render(&parse(
                "entity.position.update(|p| Point { x: p.x + entity.velocity.x * dt, ..p })"
            )),
            "(call (field (field entity position) update) \
             (closure (p) (struct-lit Point x=(+ (field p x) (* (field (field entity velocity) x) dt)) ..p)))"
        );
    }

    // --- Item grammar: let defs, struct/enum decls, extern blocks,
    // use decls ---

    fn parse_program(src: &str) -> Program {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("program parse error for {src:?}: {e}"));
        assert!(
            parser.is_at_eof(),
            "leftover tokens after parsing {src:?}: {:?}",
            &parser.tokens[parser.pos..]
        );
        program
    }

    fn program_parse_err(src: &str) {
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        match parser.parse_program() {
            Err(_) => {}
            Ok(_) if !parser.is_at_eof() => {}
            Ok(p) => panic!("expected {src:?} to be rejected, got {}", render_program(&p)),
        }
    }

    #[test]
    fn item_let_value_binding() {
        assert_eq!(render_program(&parse_program("let x = 5")), "((let x () 5))");
    }

    #[test]
    fn item_let_function_no_annotations() {
        // NOTE: `sum (n - 1) (acc + n)` (juxtaposed parenthesized
        // arguments, no comma) would ALSO parse under this grammar —
        // but as two chained single-arg calls (`sum(n - 1)` and then
        // calling *that result* with `(acc + n)`), not as one two-arg
        // call. That's a real semantic footgun since Plum has no
        // currying, caught by writing this test against the real
        // examples/overview.plum source and noticing the rendered
        // shape didn't match the intended two-argument call — the
        // example was fixed to explicit comma-call syntax instead.
        assert_eq!(
            render_program(&parse_program(
                "let sum n acc = if n == 0 { acc } else { sum(n - 1, acc + n) }"
            )),
            "((let sum (n acc) (if (== n 0) (block acc) (block (call sum (- n 1) (+ acc n))))))"
        );
    }

    #[test]
    fn item_let_with_annotations() {
        assert_eq!(
            render_program(&parse_program("let double (n: Int): Int = n * 2")),
            "((let double ((n:Int)) ->Int (* n 2)))"
        );
    }

    #[test]
    fn item_let_with_generics_and_bound() {
        assert_eq!(
            render_program(&parse_program("let sum_list[T: Num] (list: T): T = list")),
            "((let sum_list [T:Num] ((list:T)) ->T list))"
        );
    }

    #[test]
    fn item_let_associated_function_combines_the_dotted_name_into_one_string() {
        // `let Type.func (...) = ...` — a real, per-type associated
        // function declaration (`plumc::assoc_fns`'s own doc comment
        // has the full design rationale). `LetDef.name` ends up
        // literally `"Point.add"`, one string, exactly mirroring
        // `plumc::modules::qualify`'s own module-qualification trick —
        // every downstream consumer already treats `LetDef.name` as an
        // opaque string key, so this needs no new AST field.
        assert_eq!(
            render_program(&parse_program("let Point.add (a: Point) (b: Point): Point = a")),
            "((let Point.add ((a:Point) (b:Point)) ->Point a))"
        );
    }

    #[test]
    fn item_let_associated_function_with_generics_still_works() {
        assert_eq!(
            render_program(&parse_program("let Option.map[T, U] (o: T) (f: U): T = o")),
            "((let Option.map [T,U] ((o:T) (f:U)) ->T o))"
        );
    }

    // --- Contract clauses (`require`/`ensure`) — see DESIGN.md's
    // "Contracts" section. These desugar entirely at parse time, so
    // every test here asserts on the REWRITTEN body, not on any
    // separate AST representation of the clauses (there isn't one). ---

    #[test]
    fn item_let_with_require_only_prepends_a_contract_check_and_leaves_the_tail_alone() {
        assert_eq!(
            render_program(&parse_program("let divide (a: Int) (b: Int): Int require b != 0 = a")),
            "((let divide ((a:Int) (b:Int)) ->Int \
             (block (stmt (call __contract_require (!= b 0) \"precondition failed\")) a)))"
        );
    }

    #[test]
    fn item_let_with_require_message_appends_it_to_the_base_message() {
        assert_eq!(
            render_program(&parse_program(
                "let divide (a: Int) (b: Int): Int require b != 0 : \"b must be non-zero\" = a"
            )),
            "((let divide ((a:Int) (b:Int)) ->Int \
             (block (stmt (call __contract_require (!= b 0) \"precondition failed: b must be non-zero\")) a)))"
        );
    }

    #[test]
    fn item_let_with_ensure_only_binds_result_and_yields_it() {
        assert_eq!(
            render_program(&parse_program("let f (a: Int): Int ensure result >= 0 = a")),
            "((let f ((a:Int)) ->Int \
             (block (let result a) \
             (stmt (call __contract_ensure (>= result 0) \"postcondition failed\")) result)))"
        );
    }

    #[test]
    fn item_let_with_both_require_and_ensure_orders_requires_before_the_result_binding() {
        assert_eq!(
            render_program(&parse_program(
                "let divide (a: Int) (b: Int): Int require b != 0 ensure result >= 0 = a"
            )),
            "((let divide ((a:Int) (b:Int)) ->Int \
             (block (stmt (call __contract_require (!= b 0) \"precondition failed\")) \
             (let result a) \
             (stmt (call __contract_ensure (>= result 0) \"postcondition failed\")) result)))"
        );
    }

    #[test]
    fn item_let_with_multiple_require_clauses_checks_each_one() {
        assert_eq!(
            render_program(&parse_program(
                "let f (a: Int) (b: Int): Int require a > 0 require b > 0 = a"
            )),
            "((let f ((a:Int) (b:Int)) ->Int \
             (block (stmt (call __contract_require (> a 0) \"precondition failed\")) \
             (stmt (call __contract_require (> b 0) \"precondition failed\")) a)))"
        );
    }

    #[test]
    fn item_let_with_no_contract_clauses_is_unchanged_from_before_contracts_existed() {
        // `require`/`ensure` are contextual keywords, only recognized in
        // this one grammar slot — an ordinary function body is untouched
        // by the desugar entirely (`desugar_contracts` short-circuits).
        assert_eq!(
            render_program(&parse_program("let double (n: Int): Int = n * 2")),
            "((let double ((n:Int)) ->Int (* n 2)))"
        );
    }

    #[test]
    fn require_and_ensure_stay_usable_as_ordinary_identifiers_elsewhere() {
        assert_eq!(
            render_program(&parse_program("let require = 5")),
            "((let require () 5))"
        );
        assert_eq!(render_program(&parse_program("let ensure = 6")), "((let ensure () 6))");
    }

    #[test]
    fn ensure_after_require_reappears_is_a_clear_parse_error() {
        program_parse_err("let f (a: Int): Int ensure a >= 0 require a > 0 = a");
    }

    #[test]
    fn ensure_clause_rejects_a_parameter_literally_named_result() {
        program_parse_err("let f (result: Int): Int ensure result > 0 = result");
    }

    #[test]
    fn require_clause_alone_does_not_reject_a_parameter_named_result() {
        // Only `ensure` needs the `result` name — a `require`-only
        // function is free to use it as an ordinary parameter.
        assert_eq!(
            render_program(&parse_program("let f (result: Int): Int require result > 0 = result")),
            "((let f ((result:Int)) ->Int \
             (block (stmt (call __contract_require (> result 0) \"precondition failed\")) result)))"
        );
    }

    #[test]
    fn item_let_with_multiple_bounds() {
        assert_eq!(
            render_program(&parse_program("let f[T: Num + Eq] (x: T): T = x")),
            "((let f [T:Num+Eq] ((x:T)) ->T x))"
        );
    }

    #[test]
    fn item_pub_let() {
        assert_eq!(
            render_program(&parse_program("pub let area c = 3.14159 * c.radius * c.radius")),
            "((let pub area (c) (* (* 3.14159 (field c radius)) (field c radius))))"
        );
    }

    #[test]
    fn item_let_with_struct_destructuring_param() {
        assert_eq!(
            render_program(&parse_program(
                "let distance_from_origin (Point { x, y }) = sqrt(x * x + y * y)"
            )),
            "((let distance_from_origin (((struct Point x=x y=y))) (call sqrt (+ (* x x) (* y y)))))"
        );
    }

    #[test]
    fn item_let_with_tuple_destructuring_param() {
        // From examples/overview.plum: `let swap (a, b) = (b, a)` — a
        // single-paren tuple-destructuring param, not the double-paren
        // form GRAMMAR.md's literal grammar would suggest. Resolves the
        // ambiguity flagged in GRAMMAR.md's "Known ambiguities" note.
        assert_eq!(
            render_program(&parse_program("let swap (a, b) = (b, a)")),
            "((let swap (((tuple a b))) (tuple b a)))"
        );
    }

    #[test]
    fn item_struct_decl() {
        assert_eq!(
            render_program(&parse_program("struct Point { x: Float, y: Float }")),
            "((struct Point x:Float y:Float))"
        );
    }

    #[test]
    fn item_struct_decl_with_pub_field() {
        assert_eq!(
            render_program(&parse_program("struct Circle { pub radius: Float }")),
            "((struct Circle pub radius:Float))"
        );
    }

    #[test]
    fn item_struct_decl_with_generics() {
        assert_eq!(
            render_program(&parse_program("struct Pair[T] { first: T, second: T }")),
            "((struct Pair [T] first:T second:T))"
        );
    }

    #[test]
    fn item_enum_decl() {
        assert_eq!(
            render_program(&parse_program(
                "enum Shape { Circle(Float), Rectangle(Float, Float), Triangle(Float, Float, Float) }"
            )),
            "((enum Shape Circle(Float) Rectangle(Float,Float) Triangle(Float,Float,Float)))"
        );
    }

    #[test]
    fn item_enum_decl_with_unit_variant() {
        assert_eq!(
            render_program(&parse_program("enum Option[T] { Some(T), None }")),
            "((enum Option [T] Some(T) None))"
        );
    }

    #[test]
    fn item_extern_block() {
        assert_eq!(
            render_program(&parse_program("extern \"C\" { fn sqrt(x: Float) -> Float; }")),
            "((extern \"C\" fn sqrt(x:Float)->Float))"
        );
    }

    #[test]
    fn item_extern_block_multiple_fns() {
        assert_eq!(
            render_program(&parse_program(
                "extern \"C\" { fn sqrt(x: Float) -> Float; fn abs(x: Float) -> Float; }"
            )),
            "((extern \"C\" fn sqrt(x:Float)->Float fn abs(x:Float)->Float))"
        );
    }

    #[test]
    fn function_type_in_extern_param() {
        assert_eq!(
            render_program(&parse_program("extern \"C\" { fn foo(cmp: (Int, Int) -> Int) -> Int; }")),
            "((extern \"C\" fn foo(cmp:(fn (Int Int) -> Int))->Int))"
        );
    }

    #[test]
    fn zero_param_function_type() {
        assert_eq!(
            render_program(&parse_program("extern \"C\" { fn foo(cmp: () -> Int) -> Int; }")),
            "((extern \"C\" fn foo(cmp:(fn () -> Int))->Int))"
        );
    }

    #[test]
    fn parenthesized_type_is_just_grouping() {
        assert_eq!(
            render_program(&parse_program("extern \"C\" { fn foo(x: (Int)) -> Int; }")),
            "((extern \"C\" fn foo(x:Int)->Int))"
        );
    }

    #[test]
    fn multiple_types_in_parens_without_arrow_is_an_error() {
        let tokens = Lexer::new("extern \"C\" { fn foo(x: (Int, Float)) -> Int; }").tokenize();
        assert!(Parser::new(tokens).parse_program().is_err());
    }

    #[test]
    fn item_use_decl() {
        assert_eq!(render_program(&parse_program("use shapes;")), "((use shapes))");
    }

    #[test]
    fn item_use_decl_dotted() {
        assert_eq!(
            render_program(&parse_program("use shapes.Circle;")),
            "((use shapes.Circle))"
        );
    }

    #[test]
    fn item_pub_use_decl() {
        assert_eq!(
            render_program(&parse_program("pub use shapes.Circle;")),
            "((pub use shapes.Circle))"
        );
    }

    #[test]
    fn program_multiple_items_no_separator_needed() {
        assert_eq!(
            render_program(&parse_program(
                "struct Point { x: Float, y: Float } let origin = Point { x: 0.0, y: 0.0 }"
            )),
            "((struct Point x:Float y:Float) (let origin () (struct-lit Point x=0 y=0)))"
        );
    }

    #[test]
    fn unknown_item_start_is_an_error() {
        program_parse_err("5 + 5");
    }

    #[test]
    fn full_overview_plum_example_file_parses() {
        // End-to-end sanity check against the real, hand-written
        // decided-syntax sketch, not just synthetic snippets.
        let src = include_str!("../../../examples/overview.plum");
        let tokens = Lexer::new(src).tokenize();
        let mut parser = Parser::new(tokens);
        let program = parser
            .parse_program()
            .unwrap_or_else(|e| panic!("examples/overview.plum failed to parse: {e}"));
        assert!(
            parser.is_at_eof(),
            "leftover tokens after parsing examples/overview.plum: {:?}",
            &parser.tokens[parser.pos..]
        );
        assert!(!program.items.is_empty());
    }
}
