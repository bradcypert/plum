use crate::ast::{
    BinaryOp, Block, ClosureParam, EnumDecl, EnumVariant, Expr, ExternBlock, ExternFn,
    ExternParam, FieldInit, FieldPattern, GenericParam, Item, ItemKind, LetDef, MatchArm, Param,
    ParamKind, Pattern, Program, SelectArm, Stmt, StructDecl, StructField, Type, UnaryOp, UseDecl,
};
use crate::lexer::{Token, TokenKind};
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

    pub fn parse_program(&mut self) -> Result<Program, String> {
        let mut items = Vec::new();
        while !self.is_at_eof() {
            items.push(self.parse_item()?);
        }
        Ok(Program { items })
    }

    fn parse_item(&mut self) -> Result<Item, String> {
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
                return Err(format!(
                    "expected an item (let/struct/enum/extern/use), found {other:?} at {:?}",
                    self.peek().span
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

    fn parse_generic_params(&mut self) -> Result<Vec<GenericParam>, String> {
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

    fn parse_generic_param(&mut self) -> Result<GenericParam, String> {
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

    fn parse_let_def(&mut self) -> Result<LetDef, String> {
        let start = self.expect(TokenKind::Let, "'let'")?.span;
        let name_tok = self.expect_ident("a name")?;
        let name = Self::ident_text(&name_tok);
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
        self.expect(TokenKind::Eq, "'='")?;
        let body = self.parse_expr()?;
        let span = start.to(body.span());
        Ok(LetDef {
            name,
            generics,
            params,
            ret_ty,
            body,
            span,
        })
    }

    fn parse_param(&mut self) -> Result<Param, String> {
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
            other => Err(format!("expected a parameter, found {other:?} at {:?}", tok.span)),
        }
    }

    fn parse_struct_decl(&mut self) -> Result<StructDecl, String> {
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

    fn parse_struct_field(&mut self) -> Result<StructField, String> {
        let is_pub = self.bump_if(&TokenKind::Pub);
        let name_tok = self.expect_ident("a field name")?;
        let name = Self::ident_text(&name_tok);
        self.expect(TokenKind::Colon, "':'")?;
        let ty = self.parse_type()?;
        let span = name_tok.span.to(ty.span());
        Ok(StructField { is_pub, name, ty, span })
    }

    fn parse_enum_decl(&mut self) -> Result<EnumDecl, String> {
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

    fn parse_enum_variant(&mut self) -> Result<EnumVariant, String> {
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

    fn parse_extern_block(&mut self) -> Result<ExternBlock, String> {
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

    fn parse_extern_fn(&mut self) -> Result<ExternFn, String> {
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

    fn parse_extern_param(&mut self) -> Result<ExternParam, String> {
        let name_tok = self.expect_ident("a parameter name")?;
        let name = Self::ident_text(&name_tok);
        self.expect(TokenKind::Colon, "':'")?;
        let ty = self.parse_type()?;
        let span = name_tok.span.to(ty.span());
        Ok(ExternParam { name, ty, span })
    }

    fn parse_use_decl(&mut self) -> Result<UseDecl, String> {
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

    pub fn parse_pattern(&mut self) -> Result<Pattern, String> {
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

    fn parse_primary_pattern(&mut self) -> Result<Pattern, String> {
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
            other => Err(format!("expected a pattern, found {other:?} at {:?}", tok.span)),
        }
    }

    fn parse_pattern_path(&mut self) -> Result<(Vec<String>, Span), String> {
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

    fn parse_path_shaped_pattern(&mut self) -> Result<Pattern, String> {
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

    fn parse_struct_pattern(&mut self, path: Vec<String>, path_span: Span) -> Result<Pattern, String> {
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

    fn parse_field_pattern(&mut self) -> Result<FieldPattern, String> {
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

    fn parse_tuple_pattern(&mut self) -> Result<Pattern, String> {
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

    pub fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_pipe()
    }

    fn parse_expr_no_struct_literal(&mut self) -> Result<Expr, String> {
        let saved = self.no_struct_literal;
        self.no_struct_literal = true;
        let result = self.parse_expr();
        self.no_struct_literal = saved;
        result
    }

    fn parse_expr_allowing_struct_literal(&mut self) -> Result<Expr, String> {
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

    fn bump_if(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<Token, String> {
        if self.check(&kind) {
            Ok(self.advance())
        } else {
            Err(format!(
                "expected {what}, found {:?} at {:?}",
                self.peek_kind(),
                self.peek().span
            ))
        }
    }

    fn expect_ident(&mut self, what: &str) -> Result<Token, String> {
        if matches!(self.peek_kind(), TokenKind::Ident(_)) {
            Ok(self.advance())
        } else {
            Err(format!(
                "expected {what}, found {:?} at {:?}",
                self.peek_kind(),
                self.peek().span
            ))
        }
    }

    fn ident_text(tok: &Token) -> String {
        match &tok.kind {
            TokenKind::Ident(s) => s.clone(),
            _ => unreachable!("ident_text called on a non-identifier token"),
        }
    }

    fn expect_str(&mut self, what: &str) -> Result<Token, String> {
        if matches!(self.peek_kind(), TokenKind::Str(_)) {
            Ok(self.advance())
        } else {
            Err(format!(
                "expected {what}, found {:?} at {:?}",
                self.peek_kind(),
                self.peek().span
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

    fn parse_pipe(&mut self) -> Result<Expr, String> {
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

    fn parse_or(&mut self) -> Result<Expr, String> {
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

    fn parse_and(&mut self) -> Result<Expr, String> {
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

    fn parse_compare(&mut self) -> Result<Expr, String> {
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
            return Err(format!(
                "comparison operators do not chain — add parentheses (found another comparison operator at {:?})",
                self.peek().span
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

    fn parse_range(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_add()?;
        if !self.bump_if(&TokenKind::DotDot) {
            return Ok(lhs);
        }
        let rhs = self.parse_add()?;
        if self.check(&TokenKind::DotDot) {
            return Err(format!(
                "ranges do not chain — add parentheses (found another '..' at {:?})",
                self.peek().span
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

    fn parse_add(&mut self) -> Result<Expr, String> {
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

    fn parse_mul(&mut self) -> Result<Expr, String> {
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

    fn parse_unary(&mut self) -> Result<Expr, String> {
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

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
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

    fn parse_arguments(&mut self) -> Result<(Vec<Expr>, Span), String> {
        self.expect(TokenKind::LParen, "'('")?;
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_expr_allowing_struct_literal()?);
            while self.bump_if(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                args.push(self.parse_expr_allowing_struct_literal()?);
            }
        }
        let close = self.expect(TokenKind::RParen, "')'")?;
        Ok((args, close.span))
    }

    // `[e1, e2, ...]` — mirrors `parse_arguments`'s comma-separated,
    // optional-trailing-comma shape exactly, just bracketed instead of
    // parenthesized and with no callee to attach to.
    fn parse_array_literal(&mut self) -> Result<Expr, String> {
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

    fn parse_generic_args(&mut self) -> Result<(Vec<Type>, Span), String> {
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

    fn parse_type(&mut self) -> Result<Type, String> {
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

    fn parse_primary(&mut self) -> Result<Expr, String> {
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
            other => Err(format!("expected an expression, found {other:?} at {:?}", tok.span)),
        }
    }

    fn parse_paren_or_tuple(&mut self) -> Result<Expr, String> {
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
    fn parse_expr_path(&mut self) -> Result<Vec<(String, Span)>, String> {
        let first = self.expect_ident("an identifier")?;
        let mut segments = vec![(Self::ident_text(&first), first.span)];
        while self.check(&TokenKind::Dot) {
            self.advance();
            let seg = self.expect_ident("a path segment")?;
            segments.push((Self::ident_text(&seg), seg.span));
        }
        Ok(segments)
    }

    fn parse_path_shaped_expr(&mut self) -> Result<Expr, String> {
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

    fn parse_struct_literal(&mut self, path: Vec<String>, path_span: Span) -> Result<Expr, String> {
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

    fn parse_field_init(&mut self) -> Result<FieldInit, String> {
        let name_tok = self.expect_ident("a field name")?;
        let name = Self::ident_text(&name_tok);
        if self.bump_if(&TokenKind::Colon) {
            let value = self.parse_expr()?;
            let span = name_tok.span.to(value.span());
            Ok(FieldInit { name, value, span })
        } else {
            Ok(FieldInit {
                value: Expr::Ident(name.clone(), name_tok.span),
                name,
                span: name_tok.span,
            })
        }
    }

    fn parse_if_expr(&mut self) -> Result<Expr, String> {
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

    fn parse_match_expr(&mut self) -> Result<Expr, String> {
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
    fn parse_select_expr(&mut self) -> Result<Expr, String> {
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

    fn parse_for_expr(&mut self) -> Result<Expr, String> {
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

    fn parse_closure_expr(&mut self) -> Result<Expr, String> {
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

    fn parse_closure_param(&mut self) -> Result<ClosureParam, String> {
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

    fn parse_block(&mut self) -> Result<Block, String> {
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
                        return Err(format!(
                            "invalid assignment target at {:?} — only a plain local variable may be assigned",
                            expr.span()
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

    fn parse_let_stmt(&mut self) -> Result<Stmt, String> {
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
    use crate::ast::{BinaryOp, Type, UnaryOp};
    use crate::lexer::Lexer;

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

    fn op_symbol(op: &BinaryOp) -> &'static str {
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

    fn render_type(ty: &Type) -> String {
        match ty {
            Type::Path(segments, _) => segments.join("."),
            Type::Generic { base, args, .. } => {
                let mut parts = vec!["gt".to_string(), base.join(".")];
                parts.extend(args.iter().map(render_type));
                format!("({})", parts.join(" "))
            }
        }
    }

    // Renders the AST as a Lisp-like s-expression, ignoring spans, so
    // tests can assert on shape without hand-writing span boilerplate.
    fn render(expr: &Expr) -> String {
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
            Expr::For {
                pattern,
                iter,
                body,
                ..
            } => format!(
                "(for {} {} {})",
                render_pattern(pattern),
                render(iter),
                render_block(body)
            ),
            Expr::Closure { params, body, .. } => {
                let mut parts = vec!["closure".to_string()];
                parts.push(format!(
                    "({})",
                    params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                parts.push(render(body));
                format!("({})", parts.join(" "))
            }
            Expr::Unsafe(block, _) => format!("(unsafe {})", render_block(block)),
            Expr::Spawn(block, _) => format!("(spawn {})", render_block(block)),
            Expr::StructLiteral {
                path,
                fields,
                spread,
                ..
            } => {
                let mut parts = vec!["struct-lit".to_string(), path.join(".")];
                parts.extend(fields.iter().map(|f| format!("{}={}", f.name, render(&f.value))));
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

    fn render_block(block: &crate::ast::Block) -> String {
        let mut parts = vec!["block".to_string()];
        parts.extend(block.stmts.iter().map(render_stmt));
        if let Some(tail) = &block.tail {
            parts.push(render(tail));
        }
        format!("({})", parts.join(" "))
    }

    fn render_stmt(stmt: &crate::ast::Stmt) -> String {
        match stmt {
            crate::ast::Stmt::Let {
                is_mut,
                pattern,
                value,
                ..
            } => {
                let mut_marker = if *is_mut { "mut " } else { "" };
                format!("(let {mut_marker}{} {})", render_pattern(pattern), render(value))
            }
            crate::ast::Stmt::Assign { name, value, .. } => format!("(= {name} {})", render(value)),
            crate::ast::Stmt::Expr(e) => format!("(stmt {})", render(e)),
        }
    }

    fn render_arm(arm: &crate::ast::MatchArm) -> String {
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

    fn render_select_arm(arm: &crate::ast::SelectArm) -> String {
        format!(
            "(arm {} = {} {})",
            render_pattern(&arm.pattern),
            render(&arm.expr),
            render(&arm.body)
        )
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

    fn render_field_pattern(fp: &FieldPattern) -> String {
        format!("{}={}", fp.name, render_pattern(&fp.pattern))
    }

    fn render_pattern(pattern: &Pattern) -> String {
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
            Pattern::Struct {
                path,
                fields,
                has_rest,
                ..
            } => {
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

    fn render_generic_params(params: &[GenericParam]) -> String {
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

    fn render_param(param: &Param) -> String {
        match &param.kind {
            ParamKind::Ident(name) => name.clone(),
            ParamKind::Pattern(pattern, Some(ty)) => {
                format!("({}:{})", render_pattern(pattern), render_type(ty))
            }
            ParamKind::Pattern(pattern, None) => format!("({})", render_pattern(pattern)),
        }
    }

    fn render_item(item: &Item) -> String {
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
                    let params: Vec<String> = f
                        .params
                        .iter()
                        .map(|p| format!("{}:{}", p.name, render_type(&p.ty)))
                        .collect();
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

    fn render_program(program: &Program) -> String {
        let items: Vec<String> = program.items.iter().map(render_item).collect();
        format!("({})", items.join(" "))
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
