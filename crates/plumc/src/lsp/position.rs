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

/// The inverse of `byte_offset_to_position` — an LSP `Position`
/// (cursor location, as sent by a `textDocument/hover`/`definition`
/// request) back into a byte offset LOCAL to `src`, for looking a span
/// up in `resolve_node_types`/`definitions`. Same UTF-16-vs-byte
/// distinction as the forward direction, just walked in reverse.
///
/// Both a `line` past the end of `src` and a `character` past the end
/// of that line clamp (to `src.len()`/the line's own end respectively)
/// rather than panicking — same defensive stance `byte_offset_to_
/// position` itself takes, since a Position an editor sends can, in
/// principle, race slightly ahead of what THIS server's own last-
/// rechecked buffer content still has.
pub(crate) fn position_to_byte_offset(src: &str, position: Position) -> usize {
    let line_start_byte = if position.line == 0 {
        Some(0)
    } else {
        let mut current_line = 0u32;
        let mut found = None;
        for (i, ch) in src.char_indices() {
            if ch == '\n' {
                current_line += 1;
                if current_line == position.line {
                    found = Some(i + 1);
                    break;
                }
            }
        }
        found
    };
    let Some(line_start) = line_start_byte else {
        return src.len();
    };
    let line_end = src[line_start..].find('\n').map(|off| line_start + off).unwrap_or(src.len());

    let mut utf16_count = 0u32;
    for (i, ch) in src[line_start..line_end].char_indices() {
        if utf16_count >= position.character {
            return line_start + i;
        }
        utf16_count += ch.len_utf16() as u32;
    }
    line_end
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

    #[test]
    fn position_to_byte_offset_start_of_file_is_zero() {
        assert_eq!(position_to_byte_offset("let x = 1", Position::new(0, 0)), 0);
    }

    #[test]
    fn position_to_byte_offset_finds_the_start_of_a_later_line() {
        let src = "let a = 1\nlet b = 2\nlet c = 3";
        assert_eq!(position_to_byte_offset(src, Position::new(1, 0)), 10);
        assert_eq!(position_to_byte_offset(src, Position::new(2, 0)), 20);
    }

    #[test]
    fn position_to_byte_offset_is_the_true_inverse_of_byte_offset_to_position_for_ascii() {
        let src = "let a = 1\nlet bcd = 2 + 3\nlet e = 4";
        for byte_offset in 0..=src.len() {
            // Only true byte offsets that land on a char boundary are
            // meaningful round-trip inputs — every ASCII offset does.
            let pos = byte_offset_to_position(src, byte_offset);
            assert_eq!(position_to_byte_offset(src, pos), byte_offset, "round trip failed for byte offset {byte_offset}");
        }
    }

    #[test]
    fn position_to_byte_offset_round_trips_through_a_surrogate_pair_character() {
        // Mirrors `a_character_outside_the_basic_multilingual_plane_
        // counts_as_two_utf16_units` above, just in reverse: the
        // POSITION right after '🦀' (UTF-16 column 4) must map back to
        // the same byte offset (6) that produced it.
        let src = "\"🦀\" + 1";
        let pos = byte_offset_to_position(src, 6);
        assert_eq!(position_to_byte_offset(src, pos), 6);
    }

    #[test]
    fn position_to_byte_offset_clamps_a_character_past_the_end_of_its_line() {
        let src = "let x = 1\nlet y = 2";
        assert_eq!(position_to_byte_offset(src, Position::new(0, 999)), 9); // end of line 0
    }

    #[test]
    fn position_to_byte_offset_clamps_a_line_past_the_end_of_the_file() {
        let src = "let x = 1";
        assert_eq!(position_to_byte_offset(src, Position::new(999, 0)), src.len());
    }
}
