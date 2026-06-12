//! Byte-offset ↔ LSP Position (UTF-16) conversion.

use tower_lsp_server::ls_types::{Position, Range};

pub fn offset_to_position(text: &str, byte: usize) -> Position {
    let byte = byte.min(text.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if i >= byte {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let col_utf16: usize = text[line_start..byte].chars().map(|c| c.len_utf16()).sum();
    Position::new(line, col_utf16 as u32)
}

pub fn position_to_offset(text: &str, pos: Position) -> usize {
    let mut line_start = 0usize;
    for _ in 0..pos.line {
        match text[line_start..].find('\n') {
            Some(nl) => line_start += nl + 1,
            None => return text.len(),
        }
    }
    let line_end = text[line_start..]
        .find('\n')
        .map(|nl| line_start + nl)
        .unwrap_or(text.len());
    let mut utf16 = 0u32;
    for (i, c) in text[line_start..line_end].char_indices() {
        if utf16 >= pos.character {
            return line_start + i;
        }
        utf16 += c.len_utf16() as u32;
    }
    line_end
}

pub fn span_to_range(text: &str, span: &std::ops::Range<usize>) -> Range {
    Range::new(offset_to_position(text, span.start), offset_to_position(text, span.end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii() {
        let text = "abc\ndef\nghi";
        let pos = offset_to_position(text, 5); // 'e'
        assert_eq!(pos, Position::new(1, 1));
        assert_eq!(position_to_offset(text, pos), 5);
    }

    #[test]
    fn utf16_handling() {
        let text = "a😀b\nc";
        // 😀 is 4 bytes, 2 utf16 units; 'b' is at byte 5, utf16 col 3
        let pos = offset_to_position(text, 5);
        assert_eq!(pos, Position::new(0, 3));
        assert_eq!(position_to_offset(text, pos), 5);
    }

    #[test]
    fn past_end_clamps() {
        let text = "ab";
        assert_eq!(offset_to_position(text, 99), Position::new(0, 2));
        assert_eq!(position_to_offset(text, Position::new(5, 0)), 2);
    }
}
