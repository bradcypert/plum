use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    True,
    False,
    // `_` is its own token, not an Ident — GRAMMAR.md's pattern grammar
    // treats "_" and Identifier as separate alternatives, and unlike a
    // real identifier it binds nothing.
    Underscore,

    // Keywords — see GRAMMAR.md's "Lexical grammar" section
    Let,
    Mut,
    Fn,
    Struct,
    Enum,
    Match,
    If,
    Else,
    For,
    In,
    Pub,
    Use,
    Mod,
    Extern,
    Unsafe,
    Spawn,

    // Punctuation and operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Colon,
    Dot,
    DotDot,
    Eq,
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    Bang,
    Pipe,
    PipeGt,
    Arrow,
    FatArrow,

    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub struct Lexer<'a> {
    source: &'a str,
    pos: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Lexer { source, pos: 0 }
    }

    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let start = self.pos as u32;

            let Some(c) = self.peek_char() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(start, start),
                });
                break;
            };

            let kind = if c.is_ascii_digit() {
                self.lex_number()
            } else if c == '"' {
                self.lex_string()
            } else if is_ident_start(c) {
                self.lex_ident_or_keyword()
            } else {
                self.lex_operator()
            };

            let end = self.pos as u32;
            tokens.push(Token {
                kind,
                span: Span::new(start, end),
            });
        }
        tokens
    }

    fn peek_char(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn peek_char_at(&self, n: usize) -> Option<char> {
        self.source[self.pos..].chars().nth(n)
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn bump_if(&mut self, expected: char) -> bool {
        if self.peek_char() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek_char() {
                Some(c) if c.is_whitespace() => {
                    self.advance();
                }
                Some('/') if self.peek_char_at(1) == Some('/') => {
                    while let Some(c) = self.peek_char() {
                        if c == '\n' {
                            break;
                        }
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    fn lex_number(&mut self) -> TokenKind {
        let mut text = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() || c == '_' {
                text.push(c);
                self.advance();
            } else {
                break;
            }
        }

        let is_float = self.peek_char() == Some('.')
            && self.peek_char_at(1).is_some_and(|c| c.is_ascii_digit());

        if is_float {
            text.push('.');
            self.advance();
            while let Some(c) = self.peek_char() {
                if c.is_ascii_digit() || c == '_' {
                    text.push(c);
                    self.advance();
                } else {
                    break;
                }
            }
            let cleaned: String = text.chars().filter(|c| *c != '_').collect();
            TokenKind::Float(cleaned.parse().expect("lexer produced an invalid float literal"))
        } else {
            let cleaned: String = text.chars().filter(|c| *c != '_').collect();
            TokenKind::Int(cleaned.parse().expect("lexer produced an invalid int literal"))
        }
    }

    fn lex_string(&mut self) -> TokenKind {
        self.advance(); // opening quote
        let mut s = String::new();
        while let Some(c) = self.peek_char() {
            match c {
                '"' => {
                    self.advance();
                    break;
                }
                '\\' => {
                    self.advance();
                    match self.advance() {
                        Some('n') => s.push('\n'),
                        Some('t') => s.push('\t'),
                        Some('r') => s.push('\r'),
                        Some('\\') => s.push('\\'),
                        Some('"') => s.push('"'),
                        Some(other) => s.push(other),
                        None => break,
                    }
                }
                _ => {
                    s.push(c);
                    self.advance();
                }
            }
        }
        TokenKind::Str(s)
    }

    fn lex_ident_or_keyword(&mut self) -> TokenKind {
        let mut text = String::new();
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                text.push(c);
                self.advance();
            } else {
                break;
            }
        }
        match text.as_str() {
            "let" => TokenKind::Let,
            "mut" => TokenKind::Mut,
            "fn" => TokenKind::Fn,
            "struct" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "match" => TokenKind::Match,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "pub" => TokenKind::Pub,
            "use" => TokenKind::Use,
            "mod" => TokenKind::Mod,
            "extern" => TokenKind::Extern,
            "unsafe" => TokenKind::Unsafe,
            "spawn" => TokenKind::Spawn,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "_" => TokenKind::Underscore,
            _ => TokenKind::Ident(text),
        }
    }

    fn lex_operator(&mut self) -> TokenKind {
        let c = self.advance().expect("lex_operator called at end of input");
        match c {
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            ':' => TokenKind::Colon,
            '.' => {
                if self.bump_if('.') {
                    TokenKind::DotDot
                } else {
                    TokenKind::Dot
                }
            }
            '=' => {
                if self.bump_if('=') {
                    TokenKind::EqEq
                } else if self.bump_if('>') {
                    TokenKind::FatArrow
                } else {
                    TokenKind::Eq
                }
            }
            '!' => {
                if self.bump_if('=') {
                    TokenKind::NotEq
                } else {
                    TokenKind::Bang
                }
            }
            '<' => {
                if self.bump_if('=') {
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.bump_if('=') {
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            '+' => TokenKind::Plus,
            '-' => {
                if self.bump_if('>') {
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '&' => {
                if self.bump_if('&') {
                    TokenKind::AndAnd
                } else {
                    panic!("unexpected character '&' (Plum has no bitwise-and token)")
                }
            }
            '|' => {
                if self.bump_if('>') {
                    TokenKind::PipeGt
                } else if self.bump_if('|') {
                    TokenKind::OrOr
                } else {
                    TokenKind::Pipe
                }
            }
            other => panic!("unexpected character '{other}'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Strips the trailing Eof so test expectations can focus on content;
    // `tokenize_always_ends_with_eof` below checks that separately.
    fn kinds(src: &str) -> Vec<TokenKind> {
        Lexer::new(src)
            .tokenize()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| *k != TokenKind::Eof)
            .collect()
    }

    #[test]
    fn empty_source_yields_only_eof() {
        assert_eq!(kinds(""), vec![]);
    }

    #[test]
    fn tokenize_always_ends_with_eof() {
        let tokens = Lexer::new("let x = 5").tokenize();
        assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);
    }

    #[test]
    fn keywords() {
        use TokenKind::*;
        assert_eq!(
            kinds("let mut fn struct enum match if else for in pub use mod extern unsafe spawn"),
            vec![
                Let, Mut, Fn, Struct, Enum, Match, If, Else, For, In, Pub, Use, Mod, Extern,
                Unsafe, Spawn,
            ]
        );
    }

    #[test]
    fn identifiers_lowercase_and_uppercase() {
        // Capitalization is not distinguished at the lexer level — see
        // GRAMMAR.md, that disambiguation happens later, in the parser
        // or resolver.
        assert_eq!(
            kinds("foo Bar baz_qux Point2"),
            vec![
                TokenKind::Ident("foo".to_string()),
                TokenKind::Ident("Bar".to_string()),
                TokenKind::Ident("baz_qux".to_string()),
                TokenKind::Ident("Point2".to_string()),
            ]
        );
    }

    #[test]
    fn underscore_is_its_own_token() {
        assert_eq!(kinds("_"), vec![TokenKind::Underscore]);
        assert_eq!(kinds("_foo"), vec![TokenKind::Ident("_foo".to_string())]);
        assert_eq!(kinds("foo_bar"), vec![TokenKind::Ident("foo_bar".to_string())]);
    }

    #[test]
    fn integer_literals() {
        assert_eq!(
            kinds("0 42 1_000"),
            vec![TokenKind::Int(0), TokenKind::Int(42), TokenKind::Int(1000)]
        );
    }

    #[test]
    fn float_literals() {
        assert_eq!(
            kinds("3.14 0.5 1_000.25"),
            vec![
                TokenKind::Float(3.14),
                TokenKind::Float(0.5),
                TokenKind::Float(1000.25),
            ]
        );
    }

    #[test]
    fn negative_number_is_two_tokens() {
        // Negation is the unary `-` operator in the expression grammar,
        // not part of the numeric literal — the lexer must not special-
        // case a leading `-` onto Int/Float.
        assert_eq!(kinds("-5"), vec![TokenKind::Minus, TokenKind::Int(5)]);
        assert_eq!(kinds("-3.14"), vec![TokenKind::Minus, TokenKind::Float(3.14)]);
    }

    #[test]
    fn string_literal_basic() {
        assert_eq!(kinds("\"hello\""), vec![TokenKind::Str("hello".to_string())]);
    }

    #[test]
    fn string_literal_with_escapes() {
        assert_eq!(
            kinds("\"line1\\nline2\""),
            vec![TokenKind::Str("line1\nline2".to_string())]
        );
        assert_eq!(kinds("\"a\\tb\""), vec![TokenKind::Str("a\tb".to_string())]);
        assert_eq!(kinds("\"quote:\\\"\""), vec![TokenKind::Str("quote:\"".to_string())]);
    }

    #[test]
    fn bool_literals() {
        assert_eq!(kinds("true false"), vec![TokenKind::True, TokenKind::False]);
    }

    #[test]
    fn single_char_punctuation() {
        use TokenKind::*;
        assert_eq!(
            kinds("( ) { } [ ] , ; :"),
            vec![
                LParen, RParen, LBrace, RBrace, LBracket, RBracket, Comma, Semicolon, Colon,
            ]
        );
    }

    #[test]
    fn dot_vs_dotdot() {
        assert_eq!(kinds("."), vec![TokenKind::Dot]);
        assert_eq!(kinds(".."), vec![TokenKind::DotDot]);
        // Maximal munch: `0..5` is [Int, DotDot, Int], not
        // [Int, Dot, Dot, Int].
        assert_eq!(
            kinds("0..5"),
            vec![TokenKind::Int(0), TokenKind::DotDot, TokenKind::Int(5)]
        );
    }

    #[test]
    fn comparison_operators_maximal_munch() {
        use TokenKind::*;
        assert_eq!(
            kinds("== != < > <= >="),
            vec![EqEq, NotEq, Lt, Gt, LtEq, GtEq]
        );
        // Bare `=` must not be swallowed into `==` when there's nothing
        // to munch.
        assert_eq!(kinds("="), vec![TokenKind::Eq]);
    }

    #[test]
    fn arithmetic_operators() {
        use TokenKind::*;
        assert_eq!(kinds("+ - * / %"), vec![Plus, Minus, Star, Slash, Percent]);
    }

    #[test]
    fn logical_operators() {
        use TokenKind::*;
        assert_eq!(kinds("&& || !"), vec![AndAnd, OrOr, Bang]);
    }

    #[test]
    fn pipe_operator_vs_bare_pipe() {
        assert_eq!(kinds("|"), vec![TokenKind::Pipe]);
        assert_eq!(kinds("|>"), vec![TokenKind::PipeGt]);
        // Closure delimiters use bare `|`, not `|>` — must not be
        // confused even when a closure appears next to pipe usage.
        assert_eq!(
            kinds("|x| x"),
            vec![
                TokenKind::Pipe,
                TokenKind::Ident("x".to_string()),
                TokenKind::Pipe,
                TokenKind::Ident("x".to_string()),
            ]
        );
    }

    #[test]
    fn arrow_and_fat_arrow() {
        assert_eq!(kinds("->"), vec![TokenKind::Arrow]);
        assert_eq!(kinds("=>"), vec![TokenKind::FatArrow]);
        // `-` immediately followed by `>` must form Arrow, not
        // [Minus, Gt].
        assert_eq!(kinds("a -> b"), vec![
            TokenKind::Ident("a".to_string()),
            TokenKind::Arrow,
            TokenKind::Ident("b".to_string()),
        ]);
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            kinds("let x = 5 // this is a comment\nlet y = 6"),
            kinds("let x = 5\nlet y = 6")
        );
    }

    #[test]
    fn whitespace_is_insignificant() {
        assert_eq!(kinds("let  x=5"), kinds("let\n\tx  =\n5"));
    }

    #[test]
    fn full_let_binding_snippet() {
        use TokenKind::*;
        assert_eq!(
            kinds("let double n = n * 2"),
            vec![
                Let,
                Ident("double".to_string()),
                Ident("n".to_string()),
                Eq,
                Ident("n".to_string()),
                Star,
                Int(2),
            ]
        );
    }

    #[test]
    fn full_function_with_annotations_snippet() {
        use TokenKind::*;
        assert_eq!(
            kinds("let double (n: Int): Int = n * 2"),
            vec![
                Let,
                Ident("double".to_string()),
                LParen,
                Ident("n".to_string()),
                Colon,
                Ident("Int".to_string()),
                RParen,
                Colon,
                Ident("Int".to_string()),
                Eq,
                Ident("n".to_string()),
                Star,
                Int(2),
            ]
        );
    }

    #[test]
    fn struct_literal_with_spread_snippet() {
        use TokenKind::*;
        assert_eq!(
            kinds("Point { x: 1.0, ..p }"),
            vec![
                Ident("Point".to_string()),
                LBrace,
                Ident("x".to_string()),
                Colon,
                Float(1.0),
                Comma,
                DotDot,
                Ident("p".to_string()),
                RBrace,
            ]
        );
    }

    #[test]
    fn generic_brackets_snippet() {
        use TokenKind::*;
        assert_eq!(
            kinds("channel[Int]()"),
            vec![
                Ident("channel".to_string()),
                LBracket,
                Ident("Int".to_string()),
                RBracket,
                LParen,
                RParen,
            ]
        );
    }

    #[test]
    fn span_tracking_basic() {
        let tokens = Lexer::new("let").tokenize();
        assert_eq!(tokens[0].span, Span::new(0, 3));

        let tokens = Lexer::new("  let").tokenize();
        assert_eq!(tokens[0].span, Span::new(2, 5));
    }
}
