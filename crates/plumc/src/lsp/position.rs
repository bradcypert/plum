//! Byte offset -> LSP `Position` conversion.
//!
//! Every `Span` in this codebase is a pair of BYTE offsets into UTF-8
//! source text (see `plum_syntax::span::Span`'s own doc comment) and
//! `diagnostics::ModuleSources::line_col` already turns one into a
//! human-readable, CHARACTER-counted `(line, col)` for CLI error text.
//! The LSP spec's own `Position` is different from BOTH of those: line
//! is 0-based (not 1-based), and — the part that actually needs new
//! code, not just a re-count — `character` is a UTF-16 CODE-UNIT
//! offset into the line, not a Unicode scalar value ("character")
//! count. Those three counts (bytes, chars, UTF-16 units) coincide for
//! plain ASCII source, which is why this distinction is easy to miss
//! entirely until a non-ASCII string literal or identifier shows up —
//! see this module's own tests for exactly where they diverge.

use tower_lsp::lsp_types::Position;

/// Converts a byte offset that is already LOCAL to `src` (i.e. `span.
/// start - <that module's own base offset>` — see `ModuleSources::
/// locate_offset`) into an LSP `Position`.
///
/// `byte_offset` past `src.len()` is clamped to the end of `src` rather
/// than panicking — a defensive fallback for a span that (through a
/// bug elsewhere) points past the end of its own source, matching
/// `ModuleSources::render`'s own "never panic on a bad offset" stance.
pub(crate) fn byte_offset_to_position(src: &str, byte_offset: usize) -> Position {
    let byte_offset = byte_offset.min(src.len());

    let mut line = 0u32;
    let mut line_start_byte = 0usize;
    for (i, ch) in src.char_indices() {
        if i >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start_byte = i + 1;
        }
    }

    let character = src[line_start_byte..byte_offset].encode_utf16().count() as u32;
    Position::new(line, character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_of_file_is_line_zero_column_zero() {
        assert_eq!(byte_offset_to_position("let x = 1", 0), Position::new(0, 0));
    }

    #[test]
    fn counts_lines_zero_based() {
        let src = "let a = 1\nlet b = 2\nlet c = 3";
        // Start of "let b" is byte 10.
        assert_eq!(byte_offset_to_position(src, 10), Position::new(1, 0));
        // Start of "let c" is byte 20.
        assert_eq!(byte_offset_to_position(src, 20), Position::new(2, 0));
    }

    #[test]
    fn ascii_column_matches_byte_and_char_count() {
        let src = "let xyz = 1";
        // 'x' is at byte 4.
        assert_eq!(byte_offset_to_position(src, 4), Position::new(0, 4));
    }

    #[test]
    fn a_multibyte_but_single_utf16_unit_character_counts_as_one_column() {
        // '£' is 2 UTF-8 bytes but exactly 1 UTF-16 code unit — the
        // byte count and the UTF-16 count diverge here, which is
        // exactly the gap this whole module exists to close.
        let src = "\"£\" + 1"; // £ starts at byte 1, ends at byte 3
        // The byte offset right after the closing quote of "£" is 4.
        assert_eq!(byte_offset_to_position(src, 4), Position::new(0, 3));
    }

    #[test]
    fn a_character_outside_the_basic_multilingual_plane_counts_as_two_utf16_units() {
        // '🦀' is 4 UTF-8 bytes and, being outside the BMP, encodes as
        // a UTF-16 SURROGATE PAIR — 2 code units, not 1. LSP's
        // `Position.character` counts surrogate pairs as 2, matching
        // how every mainstream editor's own internal UTF-16 buffer
        // (VSCode, JetBrains) already counts it.
        let src = "\"🦀\" + 1"; // 🦀 starts at byte 1, ends at byte 5
        assert_eq!(byte_offset_to_position(src, 6), Position::new(0, 4));
    }

    #[test]
    fn an_offset_past_the_end_of_source_clamps_instead_of_panicking() {
        let src = "let x = 1";
        assert_eq!(byte_offset_to_position(src, 999), byte_offset_to_position(src, src.len()));
    }
}
