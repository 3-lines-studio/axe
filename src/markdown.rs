//! Markdown-to-ANSI renderer, ported from vercel-labs/fx
//! (src/core/agent/presentation/*.zig). Byte-exact output.
//!
//! The renderer is a streaming line processor: feed it chunks with `push`,
//! then `finish`. Assistant code blocks, tables and thematic rules are
//! delivered through `on_block` so the TUI can show them as distinct
//! transcript entries; without a callback they render inline.

#![forbid(unsafe_code)]

pub mod ansi {
    pub const BOLD_OPEN: &str = "\x1b[1m";
    pub const BOLD_CLOSE: &str = "\x1b[22m";
    pub const ITALIC_OPEN: &str = "\x1b[3m";
    pub const ITALIC_CLOSE: &str = "\x1b[23m";
    pub const DIM_OPEN: &str = "\x1b[2m";
    pub const DIM_CLOSE: &str = "\x1b[22m";
    pub const UNDERLINE_OPEN: &str = "\x1b[4m";
    pub const UNDERLINE_CLOSE: &str = "\x1b[24m";
    pub const TASK_COMPLETED_OPEN: &str = "\x1b[38;5;252m";
    pub const TASK_COMPLETED_CLOSE: &str = "\x1b[39m";
    pub const STRIKE_OPEN: &str = "\x1b[9m";
    pub const STRIKE_CLOSE: &str = "\x1b[29m";
    pub const INLINE_CODE_OPEN: &str = "\x1b[38;5;245m";
    pub const INLINE_CODE_CLOSE: &str = "\x1b[39m";
    pub const TABLE_COLUMN_SEP: &str = " \u{2502} ";
    pub const TABLE_HORIZ: &str = "\u{2500}";
    pub const TABLE_JUNCTION: &str = "\u{2500}\u{253c}\u{2500}";
    pub const VERTICAL_RULE_PREFIX: &str = "\u{2502} ";
    pub const BULLET_MARKER: &str = "\u{2022} ";
    pub const TASK_PENDING_MARKER: &str = "\u{2610}";
    pub const TASK_COMPLETED_MARKER: &str = "\u{2713}";
    pub const MAX_PIPE_BUFFER_BYTES: usize = 32 * 1024;
    pub const HORIZONTAL_RULE_WIDTH: usize = 60;
    pub const MAX_LINK_URL_BYTES: usize = 2083;

    pub fn write_dim(out: &mut String, bytes: &[u8]) {
        out.push_str(DIM_OPEN);
        out.push_str(std::str::from_utf8(bytes).unwrap_or(""));
        out.push_str(DIM_CLOSE);
    }

    pub fn write_horizontal_rule(out: &mut String) {
        out.push_str(DIM_OPEN);
        for _ in 0..HORIZONTAL_RULE_WIDTH {
            out.push_str(TABLE_HORIZ);
        }
        out.push_str(DIM_CLOSE);
    }
}

mod text_util {
    pub fn left_trim(line: &[u8]) -> &[u8] {
        let mut i = 0;
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        &line[i..]
    }

    pub fn is_space(c: u8) -> bool {
        c == b' ' || c == b'\t'
    }

    pub fn is_blank_markdown_line(line: &[u8]) -> bool {
        line.iter().all(|&c| c == b' ' || c == b'\t')
    }

    pub fn is_ascii_alpha(c: u8) -> bool {
        c.is_ascii_alphabetic()
    }

    pub fn is_ascii_alphanumeric(c: u8) -> bool {
        c.is_ascii_alphanumeric()
    }

    pub fn is_ascii_word_byte(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    pub fn is_ascii_whitespace(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
    }

    pub fn is_trailing_url_punctuation(c: u8) -> bool {
        matches!(c, b'.' | b',' | b';' | b':' | b'!' | b'?')
    }

    fn is_escapable_punctuation(byte: u8) -> bool {
        matches!(
            byte,
            b'!' | b'"'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b'-'
                | b'.'
                | b'/'
                | b':'
                | b';'
                | b'<'
                | b'='
                | b'>'
                | b'?'
                | b'@'
                | b'['
                | b'\\'
                | b']'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
        )
    }

    pub fn is_escaped_punctuation_at(text: &[u8], index: usize) -> bool {
        if index == 0 || index >= text.len() || !is_escapable_punctuation(text[index]) {
            return false;
        }
        let mut slash_count: usize = 0;
        let mut cursor = index;
        while cursor > 0 && text[cursor - 1] == b'\\' {
            slash_count += 1;
            cursor -= 1;
        }
        slash_count % 2 == 1
    }

    pub fn append_escaped_punctuation(out: &mut String, text: &[u8]) {
        let mut i = 0;
        while i < text.len() {
            if text[i] == b'\\' && i + 1 < text.len() && is_escaped_punctuation_at(text, i + 1) {
                out.push(text[i + 1] as char);
                i += 2;
                continue;
            }
            // Copy the UTF-8 sequence starting at i.
            let len = utf8_seq_len(text[i]);
            let end = (i + len).min(text.len());
            if let Ok(s) = std::str::from_utf8(&text[i..end]) {
                out.push_str(s);
            }
            i = end;
        }
    }

    pub fn utf8_seq_len(first: u8) -> usize {
        if first < 0x80 {
            1
        } else if first >> 5 == 0b110 {
            2
        } else if first >> 4 == 0b1110 {
            3
        } else if first >> 3 == 0b11110 {
            4
        } else {
            1
        }
    }

    pub fn without_terminal_hard_break_marker(line: &[u8], line_has_lf: bool) -> &[u8] {
        if !line_has_lf || line.is_empty() || line[line.len() - 1] != b'\\' {
            return line;
        }
        let mut slash_start = line.len();
        while slash_start > 0 && line[slash_start - 1] == b'\\' {
            slash_start -= 1;
        }
        if (line.len() - slash_start).is_multiple_of(2) {
            return line;
        }
        &line[..line.len() - 1]
    }

    fn is_exact_underscore_run(text: &[u8], index: usize, marker_len: usize) -> bool {
        if marker_len != 1 && marker_len != 2 {
            return false;
        }
        if index + marker_len > text.len() {
            return false;
        }
        if index > 0 && text[index - 1] == b'_' {
            return false;
        }
        if index + marker_len < text.len() && text[index + marker_len] == b'_' {
            return false;
        }
        text[index..index + marker_len].iter().all(|&c| c == b'_')
    }

    pub fn is_valid_underscore_open(text: &[u8], index: usize, marker_len: usize) -> bool {
        if !is_exact_underscore_run(text, index, marker_len) {
            return false;
        }
        let after = index + marker_len;
        if after >= text.len() || is_space(text[after]) {
            return false;
        }
        index == 0 || !is_ascii_word_byte(text[index - 1])
    }

    pub fn is_valid_underscore_close(text: &[u8], index: usize, marker_len: usize) -> bool {
        if !is_exact_underscore_run(text, index, marker_len) {
            return false;
        }
        if index == 0 || is_space(text[index - 1]) {
            return false;
        }
        let after = index + marker_len;
        after >= text.len() || !is_ascii_word_byte(text[after])
    }

    pub fn find_underscore_closer(text: &[u8], start: usize, marker_len: usize) -> Option<usize> {
        let mut i = start;
        let mut in_code = false;
        while i < text.len() {
            if text[i] == b'`' {
                in_code = !in_code;
                i += 1;
                continue;
            }
            if !in_code && is_valid_underscore_close(text, i, marker_len) {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    pub fn nth_line(buf: &[u8], n: usize) -> Option<&[u8]> {
        let mut idx: usize = 0;
        let mut start: usize = 0;
        while start < buf.len() {
            let end = buf[start..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| start + p)
                .unwrap_or(buf.len());
            if idx == n {
                return Some(&buf[start..end]);
            }
            idx += 1;
            start = end + 1;
        }
        None
    }
}

mod display_width {
    use super::text_util::utf8_seq_len;

    pub fn ansi_sequence_end(bytes: &[u8], start: usize) -> usize {
        if start >= bytes.len() || bytes[start] != 0x1b {
            return start;
        }
        let mut i = start + 1;
        if i < bytes.len() && bytes[i] == b'[' {
            i += 1;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            return (i + 1).min(bytes.len());
        }
        if i < bytes.len() && bytes[i] == b']' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    return i + 2;
                }
                i += 1;
            }
            return bytes.len();
        }
        start
    }

    fn is_combining(cp: u32) -> bool {
        matches!(cp,
            0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF..=0x05C7 |
            0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06ED | 0x0711 |
            0x0730..=0x074A | 0x07A6..=0x07B0 | 0x07EB..=0x07F3 | 0x0816..=0x0819 |
            0x081B..=0x0823 | 0x0825..=0x0827 | 0x0829..=0x082D | 0x0859..=0x085B |
            0x08D3..=0x08E1 | 0x08E3..=0x0902 | 0x093A | 0x093C | 0x0941..=0x0948 |
            0x094D | 0x0951..=0x0957 | 0x0962..=0x0963 | 0x0981 | 0x09BC | 0x09C1..=0x09C4 |
            0x09CD | 0x09E2..=0x09E3 | 0x0A01..=0x0A02 | 0x0A3C | 0x0A41..=0x0A42 |
            0x0A47..=0x0A48 | 0x0A4B..=0x0A4D | 0x0A51 | 0x0A70..=0x0A71 | 0x0A75 |
            0x0A81..=0x0A82 | 0x0ABC | 0x0AC1..=0x0AC5 | 0x0AC7..=0x0AC8 | 0x0ACD |
            0x0AE2..=0x0AE3 | 0x0B01 | 0x0B3C | 0x0B3F | 0x0B41..=0x0B44 | 0x0B4D |
            0x0B56 | 0x0B62..=0x0B63 | 0x0B82 | 0x0BC0 | 0x0BCD | 0x0C00 | 0x0C04 |
            0x0C3E..=0x0C40 | 0x0C46..=0x0C48 | 0x0C4A..=0x0C4D | 0x0C55..=0x0C56 |
            0x0C62..=0x0C63 | 0x0C81 | 0x0CBC | 0x0CBF | 0x0CC6 | 0x0CCC..=0x0CCD |
            0x0CE2..=0x0CE3 | 0x0D00..=0x0D01 | 0x0D3B..=0x0D3C | 0x0D41..=0x0D44 |
            0x0D4D | 0x0D62..=0x0D63 | 0x0DCA | 0x0DD2..=0x0DD4 | 0x0DD6 | 0x0E31 |
            0x0E34..=0x0E3A | 0x0E47..=0x0E4E | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECD |
            0x0F18..=0x0F19 | 0x0F35 | 0x0F37 | 0x0F39 | 0x0F71..=0x0F7E | 0x0F80..=0x0F84 |
            0x0F86..=0x0F87 | 0x0F8D..=0x0F97 | 0x0F99..=0x0FBC | 0x0FC6 | 0x102D..=0x1030 |
            0x1032..=0x1037 | 0x1039..=0x103A | 0x103D..=0x103E | 0x1058..=0x1059 |
            0x105E..=0x1060 | 0x1071..=0x1074 | 0x1082 | 0x1085..=0x1086 | 0x108D |
            0x109D | 0x135D..=0x135F | 0x1712..=0x1714 | 0x1732..=0x1734 | 0x1752..=0x1753 |
            0x1772..=0x1773 | 0x17B4..=0x17B5 | 0x17B7..=0x17BD | 0x17C6 | 0x17C9..=0x17D3 |
            0x17DD | 0x180B..=0x180D | 0x1885..=0x1886 | 0x18A9 | 0x1920..=0x1922 |
            0x1927..=0x1928 | 0x1932 | 0x1939..=0x193B | 0x1A17..=0x1A18 | 0x1A1B |
            0x1A56 | 0x1A58..=0x1A5E | 0x1A60 | 0x1A62 | 0x1A65..=0x1A6C | 0x1A73..=0x1A7C |
            0x1A7F | 0x1AB0..=0x1ABE | 0x1B00..=0x1B03 | 0x1B34 | 0x1B36..=0x1B3A |
            0x1B3C | 0x1B42 | 0x1B6B..=0x1B73 | 0x1B80..=0x1B81 | 0x1BA2..=0x1BA5 |
            0x1BA8..=0x1BA9 | 0x1BAB..=0x1BAD | 0x1BE6 | 0x1BE8..=0x1BE9 | 0x1BED |
            0x1BEF..=0x1BF1 | 0x1C2C..=0x1C33 | 0x1C36..=0x1C37 | 0x1CD0..=0x1CD2 |
            0x1CD4..=0x1CE0 | 0x1CE2..=0x1CE8 | 0x1CED | 0x1CF4 | 0x1CF8..=0x1CF9 |
            0x1DC0..=0x1DF9 | 0x1DFB..=0x1DFF | 0x200C..=0x200F | 0x20D0..=0x20F0 |
            0x2CEF..=0x2CF1 | 0x2D7F | 0x2DE0..=0x2DFF | 0x302A..=0x302D | 0x3099..=0x309A |
            0xA66F..=0xA672 | 0xA674..=0xA67D | 0xA69E..=0xA69F | 0xA6F0..=0xA6F1 |
            0xA802 | 0xA806 | 0xA80B | 0xA825..=0xA826 | 0xA8C4..=0xA8C5 | 0xA8E0..=0xA8F1 |
            0xA926..=0xA92D | 0xA947..=0xA951 | 0xA980..=0xA982 | 0xA9B3 | 0xA9B6..=0xA9B9 |
            0xA9BC | 0xA9E5 | 0xAA29..=0xAA2E | 0xAA31..=0xAA32 | 0xAA35..=0xAA36 |
            0xAA43 | 0xAA4C | 0xAA7C | 0xAAB0 | 0xAAB2..=0xAAB4 | 0xAAB7..=0xAAB8 |
            0xAABE..=0xAABF | 0xAAC1 | 0xAAEC..=0xAAED | 0xAAF6 | 0xABE5 | 0xABE8 |
            0xABED | 0xFB1E | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0x101FD | 0x102E0 |
            0x10376..=0x1037A | 0x10A01..=0x10A0F | 0x10A38..=0x10A3F | 0x10AE5..=0x10AE6 |
            0x11001 | 0x11038..=0x11046 | 0x1107F..=0x11081 | 0x110B3..=0x110B6 |
            0x110B9..=0x110BA | 0x11100..=0x11102 | 0x11127..=0x1112B | 0x1112D..=0x11134 |
            0x11173 | 0x11180..=0x11181 | 0x111B6..=0x111BE | 0x111C9..=0x111CC |
            0x1122F..=0x11231 | 0x11234 | 0x11236..=0x11237 | 0x1123E | 0x112DF |
            0x112E3..=0x112EA | 0x11300..=0x11301 | 0x1133C | 0x11340 | 0x11366..=0x1136C |
            0x11370..=0x11374 | 0x11438..=0x1143F | 0x11442..=0x11444 | 0x11446 |
            0x114B3..=0x114B8 | 0x114BA | 0x114BF..=0x114C0 | 0x114C2..=0x114C3 |
            0x115B2..=0x115B5 | 0x115BC..=0x115BD | 0x115BF..=0x115C0 | 0x115DC..=0x115DD |
            0x11633..=0x1163A | 0x1163D | 0x1163F..=0x11640 | 0x116AB | 0x116AD |
            0x116B0..=0x116B5 | 0x116B7 | 0x1171D..=0x1171F | 0x11722..=0x11725 |
            0x11727..=0x1172B | 0x1182F..=0x11837 | 0x11839..=0x1183A | 0x11A01..=0x11A0A |
            0x11A33..=0x11A38 | 0x11A3B..=0x11A3E | 0x11A47 | 0x11A51..=0x11A56 |
            0x11A59..=0x11A5B | 0x11A8A..=0x11A96 | 0x11A98..=0x11A99 | 0x11C30..=0x11C36 |
            0x11C38..=0x11C3D | 0x11C3F | 0x11C92..=0x11CA7 | 0x11CAA..=0x11CB0 |
            0x11CB2..=0x11CB3 | 0x11CB5..=0x11CB6 | 0x11D31..=0x11D36 | 0x11D3A |
            0x11D3C..=0x11D3D | 0x11D3F..=0x11D45 | 0x11D47 | 0x16AF0..=0x16AF4 |
            0x16B30..=0x16B36 | 0x16F8F..=0x16F92 | 0x1BC9D..=0x1BC9E | 0x1D167..=0x1D169 |
            0x1D17B..=0x1D182 | 0x1D185..=0x1D18B | 0x1D1AA..=0x1D1AD | 0x1D242..=0x1D244 |
            0x1DA00..=0x1DA36 | 0x1DA3B..=0x1DA6C | 0x1DA75 | 0x1DA84 | 0x1DA9B..=0x1DA9F |
            0x1DAA1..=0x1DAAF | 0x1E000..=0x1E006 | 0x1E008..=0x1E018 | 0x1E01B..=0x1E021 |
            0x1E023..=0x1E024 | 0x1E026..=0x1E02A | 0x1E8D0..=0x1E8D6 | 0x1E944..=0x1E94A |
            0xE0100..=0xE01EF)
    }

    fn is_wide(cp: u32) -> bool {
        matches!(cp,
            0x1100..=0x115F | 0x231A..=0x231B | 0x2329..=0x232A | 0x23E9..=0x23EC |
            0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653 |
            0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 |
            0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD |
            0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 |
            0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 |
            0x2B55 | 0x2E80..=0x2E99 | 0x2E9B..=0x2EF3 | 0x2F00..=0x2FD5 | 0x2FF0..=0x2FFB |
            0x3000..=0x303E | 0x3041..=0x3096 | 0x3099..=0x30FF | 0x3105..=0x312F |
            0x3131..=0x318E | 0x3190..=0x31BA | 0x31C0..=0x31E3 | 0x31F0..=0x321E |
            0x3220..=0x3247 | 0x3250..=0x4DBF | 0x4E00..=0xA48C | 0xA490..=0xA4C6 |
            0xA960..=0xA97C | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE10..=0xFE19 |
            0xFE30..=0xFE52 | 0xFE54..=0xFE66 | 0xFE68..=0xFE6B | 0xFF00..=0xFF60 |
            0xFFE0..=0xFFE6 | 0x16FE0..=0x16FE4 | 0x17000..=0x187F7 | 0x18800..=0x18CD5 |
            0x1B000..=0x1B2FB | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A |
            0x1F200..=0x1F202 | 0x1F210..=0x1F23B | 0x1F240..=0x1F248 | 0x1F250..=0x1F251 |
            0x1F260..=0x1F265 | 0x1F300..=0x1F320 | 0x1F32D..=0x1F335 | 0x1F337..=0x1F37C |
            0x1F37E..=0x1F393 | 0x1F3A0..=0x1F3CA | 0x1F3CF..=0x1F3D3 | 0x1F3E0..=0x1F3F0 |
            0x1F3F4 | 0x1F3F8..=0x1F43E | 0x1F440 | 0x1F442..=0x1F4FC | 0x1F4FF..=0x1F53D |
            0x1F54B..=0x1F54E | 0x1F550..=0x1F567 | 0x1F57A | 0x1F595..=0x1F596 |
            0x1F5A4 | 0x1F5FB..=0x1F64F | 0x1F680..=0x1F6C5 | 0x1F6CC | 0x1F6D0..=0x1F6D2 |
            0x1F6D5..=0x1F6D7 | 0x1F6DC..=0x1F6DF | 0x1F6EB..=0x1F6EC | 0x1F6F4..=0x1F6FC |
            0x1F7E0..=0x1F7EB | 0x1F7F0 | 0x1F90C..=0x1F93A | 0x1F93C..=0x1F945 |
            0x1F947..=0x1F9FF | 0x1FA70..=0x1FA7C | 0x1FA80..=0x1FA88 | 0x1FA90..=0x1FABD |
            0x1FABF..=0x1FAC5 | 0x1FACE..=0x1FADB | 0x1FAE0..=0x1FAE8 | 0x1FAF0..=0x1FAF8 |
            0x20000..=0x2FFFD | 0x30000..=0x3FFFD)
    }

    pub fn visible_width_ignoring_ansi(bytes: &[u8]) -> usize {
        let mut width = 0usize;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                let end = ansi_sequence_end(bytes, i);
                if end > i {
                    i = end;
                    continue;
                }
            }
            let b = bytes[i];
            if b < 0x80 {
                width += 1;
                i += 1;
                continue;
            }
            let len = utf8_seq_len(b);
            let end = (i + len).min(bytes.len());
            if end > i + 1 {
                let cp = decode_cp(&bytes[i..end]);
                if is_combining(cp) {
                    // zero width
                } else if is_wide(cp) {
                    width += 2;
                } else {
                    width += 1;
                }
            } else {
                width += 1;
            }
            i = end;
        }
        width
    }

    fn decode_cp(bytes: &[u8]) -> u32 {
        match bytes.len() {
            2 => ((bytes[0] as u32 & 0x1f) << 6) | (bytes[1] as u32 & 0x3f),
            3 => {
                ((bytes[0] as u32 & 0x0f) << 12)
                    | ((bytes[1] as u32 & 0x3f) << 6)
                    | (bytes[2] as u32 & 0x3f)
            }
            4 => {
                ((bytes[0] as u32 & 0x07) << 18)
                    | ((bytes[1] as u32 & 0x3f) << 12)
                    | ((bytes[2] as u32 & 0x3f) << 6)
                    | (bytes[3] as u32 & 0x3f)
            }
            _ => bytes[0] as u32,
        }
    }
}

mod block_parse {
    use super::text_util;

    pub fn code_fence_marker(line: &[u8]) -> Option<u8> {
        if line.len() < 3 {
            return None;
        }
        let marker = line[0];
        if marker != b'`' && marker != b'~' {
            return None;
        }
        if line[1] != marker || line[2] != marker {
            return None;
        }
        Some(marker)
    }

    pub fn code_fence_language(line: &[u8]) -> &[u8] {
        let info = trim(line[3..].as_ref());
        let end = info
            .iter()
            .position(|&c| c == b' ' || c == b'\t')
            .unwrap_or(info.len());
        &info[..end]
    }

    fn trim(mut line: &[u8]) -> &[u8] {
        while !line.is_empty() && (line[0] == b' ' || line[0] == b'\t') {
            line = &line[1..];
        }
        while !line.is_empty() && (line[line.len() - 1] == b' ' || line[line.len() - 1] == b'\t') {
            line = &line[..line.len() - 1];
        }
        line
    }

    pub fn has_indented_code_prefix(line: &[u8]) -> bool {
        !line.is_empty() && (line[0] == b'\t' || (line.len() >= 4 && &line[..4] == b"    "))
    }

    pub fn deindent_code_line(line: &[u8]) -> &[u8] {
        if line[0] == b'\t' {
            &line[1..]
        } else {
            &line[4..]
        }
    }

    pub fn parse_header(line: &[u8]) -> Option<(usize, &[u8])> {
        if line.is_empty() || line[0] != b'#' {
            return None;
        }
        let mut level = 0usize;
        while level < 6 && level < line.len() && line[level] == b'#' {
            level += 1;
        }
        if level == 0 || level >= line.len() {
            return None;
        }
        if line[level] != b' ' {
            return None;
        }
        Some((level, &line[level + 1..]))
    }

    pub fn parse_setext_underline(line: &[u8]) -> Option<usize> {
        let mut level: Option<usize> = None;
        for &byte in line {
            if byte == b' ' || byte == b'\t' {
                continue;
            }
            let next_level = match byte {
                b'=' => 1,
                b'-' => 2,
                _ => return None,
            };
            if let Some(existing) = level {
                if existing != next_level {
                    return None;
                }
            } else {
                level = Some(next_level);
            }
        }
        level
    }

    pub fn is_setext_candidate(line: &[u8]) -> bool {
        if line.is_empty() || line[0] == b' ' || line[0] == b'\t' || line[0] == b':' {
            return false;
        }
        if parse_header(line).is_some() || parse_blockquote(line).is_some() {
            return false;
        }
        if parse_unordered_list(line).is_some() || parse_ordered_list(line).is_some() {
            return false;
        }
        !is_code_fence(line)
            && !is_pipe_line(line)
            && !is_horizontal_rule(line)
            && parse_setext_underline(line).is_none()
    }

    pub fn definition_marker_body(line: &[u8]) -> Option<&[u8]> {
        if line.len() < 2 || line[0] != b':' {
            return None;
        }
        let mut body_start = 1usize;
        while body_start < line.len() && (line[body_start] == b' ' || line[body_start] == b'\t') {
            body_start += 1;
        }
        if body_start == 1 || body_start == line.len() {
            return None;
        }
        Some(&line[body_start..])
    }

    pub fn parse_footnote_definition(line: &[u8]) -> Option<(&[u8], &[u8])> {
        if line.len() < 6 || line[0] != b'[' || line[1] != b'^' {
            return None;
        }
        let close = line[2..].iter().position(|&c| c == b']').map(|p| p + 2)?;
        if close == 2 || close + 1 >= line.len() || line[close + 1] != b':' {
            return None;
        }
        let mut body_start = close + 2;
        while body_start < line.len() && (line[body_start] == b' ' || line[body_start] == b'\t') {
            body_start += 1;
        }
        if body_start == line.len() {
            return None;
        }
        Some((&line[2..close], &line[body_start..]))
    }

    pub fn footnote_continuation_body(line: &[u8]) -> Option<&[u8]> {
        if !line.is_empty() && line[0] == b'\t' {
            return Some(&line[1..]);
        }
        if line.len() >= 2 && line[0] == b' ' && line[1] == b' ' {
            return Some(&line[2..]);
        }
        None
    }

    pub fn parse_blockquote(line: &[u8]) -> Option<(usize, usize, &[u8])> {
        let mut i = 0usize;
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        let indent_end = i;
        if i == line.len() || line[i] != b'>' {
            return None;
        }
        i += 1;
        if i == line.len() {
            return Some((indent_end, 1, &line[i..]));
        }
        if line[i] != b' ' {
            return None;
        }
        i += 1;
        let mut depth = 1usize;
        while i < line.len() && line[i] == b'>' {
            let after_marker = i + 1;
            if after_marker < line.len() && line[after_marker] != b' ' {
                break;
            }
            depth += 1;
            i = after_marker;
            if i == line.len() {
                break;
            }
            i += 1;
        }
        Some((indent_end, depth, &line[i..]))
    }

    pub fn is_blockquote_paragraph(line: &[u8]) -> bool {
        is_lazy_blockquote_continuation(line)
    }

    pub fn is_lazy_blockquote_continuation(line: &[u8]) -> bool {
        if text_util::left_trim(line).is_empty() {
            return false;
        }
        if parse_blockquote(line).is_some() || parse_header(line).is_some() {
            return false;
        }
        if parse_unordered_list(line).is_some() || parse_ordered_list(line).is_some() {
            return false;
        }
        code_fence_marker(text_util::left_trim(line)).is_none()
            && !is_pipe_line(line)
            && !is_horizontal_rule(line)
    }

    pub fn parse_unordered_list(line: &[u8]) -> Option<(usize, &[u8])> {
        let mut i = 0usize;
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i + 1 >= line.len() {
            return None;
        }
        if line[i] != b'-' && line[i] != b'*' {
            return None;
        }
        if line[i + 1] != b' ' {
            return None;
        }
        Some((i, &line[i + 2..]))
    }

    pub fn parse_ordered_list(line: &[u8]) -> Option<(usize, &[u8], &[u8])> {
        let mut i = 0usize;
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        let indent_end = i;
        while i < line.len() && line[i].is_ascii_digit() {
            i += 1;
        }
        if i == indent_end {
            return None;
        }
        if i + 1 >= line.len() {
            return None;
        }
        if line[i] != b'.' {
            return None;
        }
        if line[i + 1] != b' ' {
            return None;
        }
        Some((indent_end, &line[indent_end..i + 1], &line[i + 2..]))
    }

    pub fn parse_task_list_item(content: &[u8]) -> Option<(bool, bool, &[u8])> {
        if content.len() < 3 || content[0] != b'[' || content[2] != b']' {
            return None;
        }
        let completed = match content[1] {
            b' ' => false,
            b'x' | b'X' => true,
            _ => return None,
        };
        if content.len() == 3 {
            return Some((completed, false, &content[3..]));
        }
        if content[3] != b' ' {
            return None;
        }
        Some((completed, true, &content[4..]))
    }

    pub fn is_horizontal_rule(line: &[u8]) -> bool {
        let trimmed = text_util::left_trim(line);
        if trimmed.len() < 3 {
            return false;
        }
        let rule_char = trimmed[0];
        if rule_char != b'-' && rule_char != b'*' && rule_char != b'_' {
            return false;
        }
        let mut count = 0usize;
        for &c in trimmed {
            if c == rule_char {
                count += 1;
            } else if c != b' ' && c != b'\t' {
                return false;
            }
        }
        count >= 3
    }

    pub fn is_pipe_line(line: &[u8]) -> bool {
        let trimmed = text_util::left_trim(line);
        if trimmed.is_empty() {
            return false;
        }
        for (index, &byte) in trimmed.iter().enumerate() {
            if byte == b'|' && !text_util::is_escaped_punctuation_at(trimmed, index) {
                return true;
            }
        }
        false
    }

    fn is_separator_line(line: &[u8]) -> bool {
        let trimmed = text_util::left_trim(line);
        if trimmed.is_empty() {
            return false;
        }
        let mut seen_dash = false;
        let mut seen_pipe = false;
        for &c in trimmed {
            match c {
                b'|' => seen_pipe = true,
                b':' | b' ' | b'\t' => {}
                b'-' => seen_dash = true,
                _ => return false,
            }
        }
        seen_dash && seen_pipe
    }

    pub fn is_valid_table(buf: &[u8]) -> bool {
        let mut line_count = 0usize;
        let mut saw_separator = false;
        let mut start = 0usize;
        while start < buf.len() {
            let end = buf[start..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| start + p)
                .unwrap_or(buf.len());
            let line = &buf[start..end];
            if line_count == 1 && is_separator_line(line) {
                saw_separator = true;
            }
            line_count += 1;
            start = end + 1;
        }
        line_count >= 2 && saw_separator
    }

    fn is_code_fence(line: &[u8]) -> bool {
        code_fence_marker(line).is_some()
    }
}

mod inline_render {
    use super::ansi;
    use super::text_util;

    pub fn write_inline_no_bold(
        text: &[u8],
        out: &mut String,
        restore_underline_after_link: bool,
        link_id: &mut u32,
        footnotes: Option<&mut dyn FnMut(&str) -> usize>,
    ) {
        let mut stripped: Vec<u8> = Vec::with_capacity(text.len());
        let mut i = 0usize;
        let mut in_code = false;
        while i < text.len() {
            let c = text[i];
            if c == b'`' {
                in_code = !in_code;
                stripped.push(c);
                i += 1;
                continue;
            }
            if !in_code
                && c == b'\\'
                && i + 1 < text.len()
                && text_util::is_escaped_punctuation_at(text, i + 1)
            {
                stripped.extend_from_slice(&text[i..i + 2]);
                i += 2;
                continue;
            }
            if !in_code
                && c == b'_'
                && text_util::is_valid_underscore_open(text, i, 2)
                && let Some(closer) = text_util::find_underscore_closer(text, i + 2, 2)
            {
                stripped.extend_from_slice(&text[i + 2..closer]);
                i = closer + 2;
                continue;
            }
            if !in_code && i + 1 < text.len() && c == b'*' && text[i + 1] == b'*' {
                i += 2;
                continue;
            }
            stripped.push(c);
            i += 1;
        }
        write_inline(
            &stripped,
            out,
            restore_underline_after_link,
            link_id,
            footnotes,
        );
    }

    pub fn write_inline(
        text: &[u8],
        out: &mut String,
        restore_underline_after_link: bool,
        link_id: &mut u32,
        mut footnotes: Option<&mut dyn FnMut(&str) -> usize>,
    ) {
        let mut i = 0usize;
        let mut in_bold = false;
        let mut in_italic = false;
        let mut in_underscore_bold = false;
        let mut in_underscore_italic = false;
        let mut in_strike = false;
        let mut in_code = false;
        let mut link_admission_suppressed_until = 0usize;

        while i < text.len() {
            let c = text[i];

            if c == b'`' {
                if in_code {
                    out.push_str(ansi::INLINE_CODE_CLOSE);
                    in_code = false;
                } else {
                    out.push_str(ansi::INLINE_CODE_OPEN);
                    in_code = true;
                }
                i += 1;
                continue;
            }

            if in_code {
                let len = text_util::utf8_seq_len(text[i]);
                push_char(out, &text[i..(i + len).min(text.len())]);
                i += len;
                continue;
            }

            if c == b'\\' && i + 1 < text.len() && text_util::is_escaped_punctuation_at(text, i + 1)
            {
                if text[i + 1] == b'<' {
                    link_admission_suppressed_until = link_admission_suppressed_until
                        .max(angle_autolink_candidate_end(text, i + 1));
                }
                if text[i + 1] == b'!'
                    && i + 2 < text.len()
                    && text[i + 2] == b'['
                    && let Some(candidate_end) = malformed_inline_link_candidate_end(text, i + 2)
                {
                    push_slice(out, &text[i + 1..candidate_end]);
                    link_admission_suppressed_until =
                        link_admission_suppressed_until.max(candidate_end);
                    i = candidate_end;
                    continue;
                }
                if text[i + 1] == b'['
                    && let Some(candidate_end) = malformed_inline_link_candidate_end(text, i + 1)
                {
                    link_admission_suppressed_until =
                        link_admission_suppressed_until.max(candidate_end);
                }
                out.push(text[i + 1] as char);
                i += 2;
                continue;
            }

            if i >= link_admission_suppressed_until
                && c == b'!'
                && i + 1 < text.len()
                && text[i + 1] == b'['
            {
                if let Some(image) = parse_inline_image(text, i) {
                    emit_inline_link(
                        out,
                        &image,
                        restore_underline_after_link,
                        Some("\u{25a7} "),
                        link_id,
                    );
                    i = image.end;
                    continue;
                }
                if let Some(candidate_end) = malformed_inline_link_candidate_end(text, i + 1) {
                    link_admission_suppressed_until =
                        link_admission_suppressed_until.max(candidate_end);
                }
            }

            if c == b'['
                && let Some(sink) = footnotes.as_mut()
                && let Some(reference) = parse_footnote_reference(text, i)
            {
                let number = sink(std::str::from_utf8(reference.label).unwrap_or(""));
                write_footnote_marker(out, number);
                i = reference.end;
                continue;
            }

            if i >= link_admission_suppressed_until && c == b'[' {
                if let Some(link) = parse_inline_link(text, i) {
                    emit_inline_link(out, &link, restore_underline_after_link, None, link_id);
                    i = link.end;
                    continue;
                }
                if let Some(candidate_end) = malformed_inline_link_candidate_end(text, i) {
                    link_admission_suppressed_until =
                        link_admission_suppressed_until.max(candidate_end);
                }
            }

            if i >= link_admission_suppressed_until && c == b'<' {
                if let Some(link) = parse_angle_autolink(text, i) {
                    emit_inline_link(out, &link, restore_underline_after_link, None, link_id);
                    i = link.end;
                    continue;
                }
                link_admission_suppressed_until =
                    link_admission_suppressed_until.max(angle_autolink_candidate_end(text, i));
            }

            if i >= link_admission_suppressed_until
                && let Some(link) = parse_bare_url(
                    text,
                    i,
                    in_bold,
                    in_italic,
                    in_underscore_bold,
                    in_underscore_italic,
                    in_strike,
                )
            {
                emit_inline_link(out, &link, restore_underline_after_link, None, link_id);
                i = link.end;
                continue;
            }

            if c == b'~' && i + 1 < text.len() && text[i + 1] == b'~' {
                if in_strike {
                    if i == 0 || text_util::is_space(text[i - 1]) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::STRIKE_CLOSE);
                    in_strike = false;
                } else {
                    if i + 2 >= text.len() || text_util::is_space(text[i + 2]) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::STRIKE_OPEN);
                    in_strike = true;
                }
                i += 2;
                continue;
            }

            if c == b'*' && i + 1 < text.len() && text[i + 1] == b'*' {
                if in_bold {
                    if i == 0 || text_util::is_space(text[i - 1]) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::BOLD_CLOSE);
                    in_bold = false;
                    if in_underscore_bold {
                        out.push_str(ansi::BOLD_OPEN);
                    }
                } else {
                    if i + 2 >= text.len() || text_util::is_space(text[i + 2]) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::BOLD_OPEN);
                    in_bold = true;
                }
                i += 2;
                continue;
            }

            if c == b'_' && i + 1 < text.len() && text[i + 1] == b'_' {
                if in_underscore_bold {
                    if !text_util::is_valid_underscore_close(text, i, 2) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::BOLD_CLOSE);
                    in_underscore_bold = false;
                    if in_bold {
                        out.push_str(ansi::BOLD_OPEN);
                    }
                } else {
                    if !text_util::is_valid_underscore_open(text, i, 2) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::BOLD_OPEN);
                    in_underscore_bold = true;
                }
                i += 2;
                continue;
            }

            if c == b'_' {
                if in_underscore_italic {
                    if !text_util::is_valid_underscore_close(text, i, 1) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::ITALIC_CLOSE);
                    in_underscore_italic = false;
                    if in_italic {
                        out.push_str(ansi::ITALIC_OPEN);
                    }
                } else {
                    if !text_util::is_valid_underscore_open(text, i, 1) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::ITALIC_OPEN);
                    in_underscore_italic = true;
                }
                i += 1;
                continue;
            }

            if c == b'*' {
                if in_italic {
                    if i == 0 || text_util::is_space(text[i - 1]) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::ITALIC_CLOSE);
                    in_italic = false;
                    if in_underscore_italic {
                        out.push_str(ansi::ITALIC_OPEN);
                    }
                } else {
                    if i + 1 >= text.len() || text_util::is_space(text[i + 1]) {
                        out.push(c as char);
                        i += 1;
                        continue;
                    }
                    out.push_str(ansi::ITALIC_OPEN);
                    in_italic = true;
                }
                i += 1;
                continue;
            }

            let len = text_util::utf8_seq_len(text[i]);
            push_char(out, &text[i..(i + len).min(text.len())]);
            i += len;
        }

        if in_bold {
            out.push_str(ansi::BOLD_CLOSE);
        }
        if in_italic {
            out.push_str(ansi::ITALIC_CLOSE);
        }
        if in_underscore_bold {
            out.push_str(ansi::BOLD_CLOSE);
        }
        if in_underscore_italic {
            out.push_str(ansi::ITALIC_CLOSE);
        }
        if in_strike {
            out.push_str(ansi::STRIKE_CLOSE);
        }
        if in_code {
            out.push_str(ansi::INLINE_CODE_CLOSE);
        }
    }

    pub fn push_slice(out: &mut String, bytes: &[u8]) {
        out.push_str(std::str::from_utf8(bytes).unwrap_or(""));
    }

    pub fn push_char(out: &mut String, bytes: &[u8]) {
        if let Ok(s) = std::str::from_utf8(bytes) {
            out.push_str(s);
        }
    }

    fn write_footnote_marker(out: &mut String, number: usize) {
        let marker = format!("[{number}]");
        ansi::write_dim(out, marker.as_bytes());
    }

    struct FootnoteReference<'a> {
        label: &'a [u8],
        end: usize,
    }

    fn parse_footnote_reference(text: &[u8], start: usize) -> Option<FootnoteReference<'_>> {
        if start + 4 > text.len() || text[start] != b'[' || text[start + 1] != b'^' {
            return None;
        }
        let close = text[start + 2..]
            .iter()
            .position(|&c| c == b']')
            .map(|p| p + start + 2)?;
        if close == start + 2 {
            return None;
        }
        if close + 1 < text.len() && text[close + 1] == b':' {
            return None;
        }
        Some(FootnoteReference {
            label: &text[start + 2..close],
            end: close + 1,
        })
    }

    struct InlineLink<'a> {
        text: &'a [u8],
        url: &'a [u8],
        end: usize,
        destination_prefix: &'static [u8],
        label_mode: LabelMode,
    }

    enum LabelMode {
        Escaped,
        Literal,
    }

    fn emit_inline_link(
        out: &mut String,
        link: &InlineLink,
        restore_underline_after_link: bool,
        visible_prefix: Option<&str>,
        link_id: &mut u32,
    ) {
        let id = *link_id;
        *link_id = link_id.wrapping_add(1);
        out.push_str(&format!("\x1b]8;id=fx-{id};"));
        out.push_str(std::str::from_utf8(link.destination_prefix).unwrap_or(""));
        out.push_str(std::str::from_utf8(link.url).unwrap_or(""));
        out.push_str("\x1b\\");
        out.push_str(ansi::UNDERLINE_OPEN);
        if let Some(prefix) = visible_prefix {
            out.push_str(prefix);
        }
        let visible_text = if link.text.is_empty() && visible_prefix.is_some() {
            "image".as_bytes()
        } else {
            link.text
        };
        match link.label_mode {
            LabelMode::Escaped => text_util::append_escaped_punctuation(out, visible_text),
            LabelMode::Literal => push_slice(out, visible_text),
        }
        out.push_str(ansi::UNDERLINE_CLOSE);
        out.push_str("\x1b]8;;\x1b\\");
        if restore_underline_after_link {
            out.push_str(ansi::UNDERLINE_OPEN);
        }
    }

    fn parse_inline_link(text: &[u8], start: usize) -> Option<InlineLink<'_>> {
        parse_inline_bracket_destination(text, start, false)
    }

    fn parse_inline_image(text: &[u8], start: usize) -> Option<InlineLink<'_>> {
        if start + 1 >= text.len() || text[start] != b'!' || text[start + 1] != b'[' {
            return None;
        }
        parse_inline_bracket_destination(text, start + 1, true)
    }

    fn parse_inline_bracket_destination(
        text: &[u8],
        start: usize,
        allow_empty_text: bool,
    ) -> Option<InlineLink<'_>> {
        if start >= text.len() || text[start] != b'[' {
            return None;
        }
        let mut j = start + 1;
        while j < text.len() && text[j] != b']' && text[j] != b'\n' {
            j += 1;
        }
        if j >= text.len() || text[j] != b']' {
            return None;
        }
        let text_end = j;
        if !allow_empty_text && text_end == start + 1 {
            return None;
        }
        if text_end + 1 >= text.len() || text[text_end + 1] != b'(' {
            return None;
        }
        let mut k = text_end + 2;
        while k < text.len() && text[k] != b')' && text[k] != b'\n' {
            k += 1;
        }
        if k >= text.len() || text[k] != b')' {
            return None;
        }
        let url = &text[text_end + 2..k];
        if !is_valid_link_url(url) {
            return None;
        }
        Some(InlineLink {
            text: &text[start + 1..text_end],
            url,
            end: k + 1,
            destination_prefix: b"",
            label_mode: LabelMode::Escaped,
        })
    }

    fn parse_angle_autolink(text: &[u8], start: usize) -> Option<InlineLink<'_>> {
        if start >= text.len() || text[start] != b'<' {
            return None;
        }
        let end = angle_autolink_candidate_end(text, start);
        if end <= start + 1 || end > text.len() || text[end - 1] != b'>' {
            return None;
        }
        let value = &text[start + 1..end - 1];
        if is_valid_angle_autolink_uri(value) && is_valid_link_url(value) {
            return Some(InlineLink {
                text: value,
                url: value,
                end,
                label_mode: LabelMode::Literal,
                ..Default::default()
            });
        }
        if is_valid_angle_autolink_email(value) && is_valid_link_url_with_prefix(b"mailto:", value)
        {
            return Some(InlineLink {
                text: value,
                url: value,
                end,
                destination_prefix: b"mailto:",
                label_mode: LabelMode::Literal,
            });
        }
        None
    }

    impl<'a> Default for InlineLink<'a> {
        fn default() -> Self {
            InlineLink {
                text: b"",
                url: b"",
                end: 0,
                destination_prefix: b"",
                label_mode: LabelMode::Escaped,
            }
        }
    }

    fn angle_autolink_candidate_end(text: &[u8], start: usize) -> usize {
        if start >= text.len() || text[start] != b'<' {
            return start;
        }
        let mut end = start + 1;
        while end < text.len() && text[end] != b'>' && text[end] != b'\n' {
            end += 1;
        }
        if end < text.len() && text[end] == b'>' {
            end + 1
        } else {
            end
        }
    }

    fn is_valid_angle_autolink_uri(value: &[u8]) -> bool {
        let mut colon = 0usize;
        while colon < value.len() && value[colon] != b':' {
            colon += 1;
        }
        if !(2..=32).contains(&colon)
            || colon == value.len()
            || !text_util::is_ascii_alpha(value[0])
        {
            return false;
        }
        for &byte in &value[1..colon] {
            if !text_util::is_ascii_alphanumeric(byte)
                && byte != b'+'
                && byte != b'-'
                && byte != b'.'
            {
                return false;
            }
        }
        for &byte in &value[colon + 1..] {
            if byte <= b' ' || byte == b'<' || byte == b'>' {
                return false;
            }
        }
        true
    }

    fn is_valid_angle_autolink_email(value: &[u8]) -> bool {
        let mut at_index: Option<usize> = None;
        for (index, &byte) in value.iter().enumerate() {
            if byte == b'@' {
                if at_index.is_some() {
                    return false;
                }
                at_index = Some(index);
            }
        }
        let at = match at_index {
            Some(at) => at,
            None => return false,
        };
        if at == 0 || at + 1 >= value.len() {
            return false;
        }
        for &byte in &value[..at] {
            if !is_angle_autolink_email_local_byte(byte) {
                return false;
            }
        }
        let mut label_start = at + 1;
        let mut index = label_start;
        while index <= value.len() {
            if index != value.len() && value[index] != b'.' {
                index += 1;
                continue;
            }
            if !is_valid_angle_autolink_email_domain_label(&value[label_start..index]) {
                return false;
            }
            label_start = index + 1;
            index += 1;
        }
        true
    }

    fn is_angle_autolink_email_local_byte(byte: u8) -> bool {
        text_util::is_ascii_alphanumeric(byte)
            || matches!(
                byte,
                b'.' | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'/'
                    | b'='
                    | b'?'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'{'
                    | b'|'
                    | b'}'
                    | b'~'
                    | b'-'
            )
    }

    fn is_valid_angle_autolink_email_domain_label(label: &[u8]) -> bool {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        for (index, &byte) in label.iter().enumerate() {
            if index == 0 || index + 1 == label.len() {
                if !text_util::is_ascii_alphanumeric(byte) {
                    return false;
                }
            } else if !text_util::is_ascii_alphanumeric(byte) && byte != b'-' {
                return false;
            }
        }
        true
    }

    fn parse_bare_url(
        text: &[u8],
        start: usize,
        in_bold: bool,
        in_italic: bool,
        in_underscore_bold: bool,
        in_underscore_italic: bool,
        in_strike: bool,
    ) -> Option<InlineLink<'_>> {
        if !is_bare_url_boundary(text, start, in_underscore_bold, in_underscore_italic) {
            return None;
        }
        let scheme_len = if text[start..].starts_with(b"https://") {
            8
        } else if text[start..].starts_with(b"http://") {
            7
        } else {
            return None;
        };
        let mut end = start + scheme_len;
        while end < text.len()
            && !is_bare_url_terminator(
                text,
                end,
                in_bold,
                in_italic,
                in_underscore_bold,
                in_underscore_italic,
                in_strike,
            )
        {
            end += 1;
        }
        while end > start + scheme_len && text_util::is_trailing_url_punctuation(text[end - 1]) {
            end -= 1;
        }
        let url = &text[start..end];
        if !is_valid_link_url(url) {
            return None;
        }
        Some(InlineLink {
            text: url,
            url,
            end,
            label_mode: LabelMode::Literal,
            ..Default::default()
        })
    }

    fn malformed_inline_link_candidate_end(text: &[u8], start: usize) -> Option<usize> {
        if start >= text.len() || text[start] != b'[' {
            return None;
        }
        let mut label_end = start + 1;
        while label_end < text.len() && text[label_end] != b']' && text[label_end] != b'\n' {
            label_end += 1;
        }
        if label_end >= text.len() || text[label_end] != b']' {
            return None;
        }
        if label_end + 1 >= text.len() || text[label_end + 1] != b'(' {
            return None;
        }
        let mut candidate_end = label_end + 2;
        while candidate_end < text.len()
            && text[candidate_end] != b')'
            && text[candidate_end] != b'\n'
        {
            candidate_end += 1;
        }
        if candidate_end < text.len() && text[candidate_end] == b')' {
            Some(candidate_end + 1)
        } else {
            Some(candidate_end)
        }
    }

    fn is_valid_link_url(url: &[u8]) -> bool {
        if url.is_empty() || url.len() > ansi::MAX_LINK_URL_BYTES {
            return false;
        }
        url.iter().all(|&b| b >= 0x20 && b != 0x7f)
    }

    fn is_valid_link_url_with_prefix(prefix: &[u8], url: &[u8]) -> bool {
        if prefix.len() + url.len() > ansi::MAX_LINK_URL_BYTES {
            return false;
        }
        if prefix.iter().any(|&byte| byte < 0x20 || byte == 0x7f) {
            return false;
        }
        is_valid_link_url(url)
    }

    fn is_bare_url_boundary(
        text: &[u8],
        start: usize,
        in_underscore_bold: bool,
        in_underscore_italic: bool,
    ) -> bool {
        if start == 0 {
            return true;
        }
        let previous = text[start - 1];
        if !text_util::is_ascii_word_byte(previous) && previous != b'<' {
            return true;
        }
        if in_underscore_italic
            && start >= 1
            && text_util::is_valid_underscore_open(text, start - 1, 1)
        {
            return true;
        }
        in_underscore_bold && start >= 2 && text_util::is_valid_underscore_open(text, start - 2, 2)
    }

    fn is_bare_url_terminator(
        text: &[u8],
        index: usize,
        in_bold: bool,
        in_italic: bool,
        in_underscore_bold: bool,
        in_underscore_italic: bool,
        in_strike: bool,
    ) -> bool {
        let c = text[index];
        if text_util::is_ascii_whitespace(c) || c == b')' || c == b']' || c == b'}' || c == b'>' {
            return true;
        }
        if in_strike && c == b'~' && index + 1 < text.len() && text[index + 1] == b'~' {
            return true;
        }
        if in_underscore_bold && text_util::is_valid_underscore_close(text, index, 2) {
            return true;
        }
        if in_underscore_italic && text_util::is_valid_underscore_close(text, index, 1) {
            return true;
        }
        if c != b'*' {
            return false;
        }
        if in_bold && index + 1 < text.len() && text[index + 1] == b'*' {
            return true;
        }
        in_italic
    }
}

pub struct CodeBlock {
    pub language: String,
    pub code: String,
}

pub enum Block {
    Code { language: String, code: String },
    Table(String),
    Rule,
}

type BlockHandler = Box<dyn FnMut(Block, &mut String)>;

pub struct Markdown {
    out: String,
    on_block: Option<BlockHandler>,
    line_buf: Vec<u8>,
    pending_top_level_line: Vec<u8>,
    pipe_buf: Vec<u8>,
    code_buf: Vec<u8>,
    code_language: Vec<u8>,
    in_code_block: bool,
    code_fence_marker: Option<u8>,
    in_pipe_block: bool,
    pipe_last_line_has_lf: bool,
    active_blockquote: Option<(usize, usize)>,
    active_definition: bool,
    active_footnote: Option<ActiveFootnote>,
    footnotes: Vec<Footnote>,
    next_footnote_number: usize,
    previous_line_was_blank: bool,
    link_id_counter: u32,
}

struct ActiveFootnote {
    index: usize,
    append_body: bool,
}

pub(crate) struct Footnote {
    label: String,
    body: String,
    number: Option<usize>,
    has_definition: bool,
}

macro_rules! reg {
    ($s:ident) => {
        |label: &str| {
            let index = if let Some(pos) = $s.footnotes.iter().position(|n| n.label == label) {
                pos
            } else {
                $s.footnotes.push(Footnote {
                    label: label.to_string(),
                    body: String::new(),
                    number: None,
                    has_definition: false,
                });
                $s.footnotes.len() - 1
            };
            if let Some(number) = $s.footnotes[index].number {
                return number;
            }
            $s.next_footnote_number += 1;
            $s.footnotes[index].number = Some($s.next_footnote_number);
            $s.next_footnote_number
        }
    };
}

impl Default for Markdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Markdown {
    pub fn new() -> Self {
        Markdown {
            out: String::new(),
            on_block: None,
            line_buf: Vec::new(),
            pending_top_level_line: Vec::new(),
            pipe_buf: Vec::new(),
            code_buf: Vec::new(),
            code_language: Vec::new(),
            in_code_block: false,
            code_fence_marker: None,
            in_pipe_block: false,
            pipe_last_line_has_lf: false,
            active_blockquote: None,
            active_definition: false,
            active_footnote: None,
            footnotes: Vec::new(),
            next_footnote_number: 0,
            previous_line_was_blank: true,
            link_id_counter: 0,
        }
    }

    pub fn set_on_block(&mut self, f: impl FnMut(Block, &mut String) + 'static) {
        self.on_block = Some(Box::new(f));
    }

    pub fn push(&mut self, input: &str) {
        for &byte in input.as_bytes() {
            if byte == b'\n' {
                let mut line_end = self.line_buf.len();
                if !self.line_buf.is_empty() && self.line_buf[self.line_buf.len() - 1] == b'\r' {
                    line_end -= 1;
                }
                let line = self.line_buf[..line_end].to_vec();
                self.handle_line(&line, true);
                self.line_buf.clear();
            } else {
                self.line_buf.push(byte);
            }
        }
    }

    pub fn flush(&mut self) {
        let had_partial_line = !self.line_buf.is_empty();
        let len_before = self.out.len();
        if !self.line_buf.is_empty() {
            let line = self.line_buf.clone();
            self.handle_line(&line, false);
            self.line_buf.clear();
        }
        self.flush_pending_top_level_line();
        if had_partial_line
            && self.out.len() > len_before
            && self.out.as_bytes()[self.out.len() - 1] == b'\n'
        {
            self.out.pop();
        }
        if self.in_pipe_block {
            self.finalize_pipe_block();
        }
        if self.in_code_block {
            self.finalize_code_block();
            self.in_code_block = false;
            self.code_fence_marker = None;
        }
        self.flush_footnotes();
        self.active_blockquote = None;
        self.active_definition = false;
        self.active_footnote = None;
    }

    pub fn current_text(&self) -> String {
        if self.line_buf.is_empty() {
            return self.out.clone();
        }
        let partial = String::from_utf8_lossy(&self.line_buf).into_owned();
        format!("{}{}", self.out, partial)
    }

    pub fn finish(&mut self) -> String {
        self.flush();
        std::mem::take(&mut self.out)
    }

    pub fn render(input: &str) -> String {
        let mut m = Markdown::new();
        m.push(input);
        m.finish()
    }

    fn handle_line(&mut self, line: &[u8], line_has_lf: bool) {
        if self.in_code_block {
            self.active_definition = false;
            if let Some(marker) = self.code_fence_marker {
                if block_parse::code_fence_marker(text_util::left_trim(line)) == Some(marker) {
                    self.finalize_code_block();
                    self.in_code_block = false;
                    self.code_fence_marker = None;
                    self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
                    return;
                }
            } else if !text_util::is_blank_markdown_line(line)
                && !block_parse::has_indented_code_prefix(line)
            {
                self.finalize_code_block();
                self.in_code_block = false;
                self.handle_line(line, line_has_lf);
                return;
            }
            let code_line =
                if self.code_fence_marker.is_none() && !text_util::is_blank_markdown_line(line) {
                    block_parse::deindent_code_line(line).to_vec()
                } else {
                    line.to_vec()
                };
            self.append_code_line(&code_line, line_has_lf);
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if self.in_pipe_block {
            self.active_definition = false;
            if block_parse::is_pipe_line(line)
                && self.pipe_buf.len() + line.len() < ansi::MAX_PIPE_BUFFER_BYTES
            {
                self.pipe_buf.extend_from_slice(line);
                self.pipe_buf.push(b'\n');
                self.pipe_last_line_has_lf = line_has_lf;
                self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
                return;
            }
            self.finalize_pipe_block();
        }

        if let Some(active) = self.active_footnote.take()
            && let Some(body) = block_parse::footnote_continuation_body(line)
        {
            if active.append_body {
                let note = &mut self.footnotes[active.index];
                note.body.push('\n');
                note.body.push_str(std::str::from_utf8(body).unwrap_or(""));
            }
            self.active_footnote = Some(active);
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if let Some((label, body)) = block_parse::parse_footnote_definition(line) {
            self.active_definition = false;
            self.flush_pending_top_level_line();
            self.begin_footnote_definition(label, body);
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if !self.pending_top_level_line.is_empty() {
            if let Some(level) = block_parse::parse_setext_underline(line) {
                self.active_definition = false;
                let content = text_util::without_terminal_hard_break_marker(
                    &self.pending_top_level_line,
                    true,
                );
                block_render::write_heading(
                    level,
                    content,
                    &mut self.out,
                    &mut self.footnotes,
                    &mut self.next_footnote_number,
                    &mut self.link_id_counter,
                );
                self.out.push('\n');
                self.pending_top_level_line.clear();
                self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
                return;
            }
            if let Some(body) = block_parse::definition_marker_body(line) {
                self.flush_pending_top_level_line();
                block_render::write_definition_line(
                    body,
                    line_has_lf,
                    &mut self.out,
                    &mut self.footnotes,
                    &mut self.next_footnote_number,
                    &mut self.link_id_counter,
                );
                self.active_definition = true;
                self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
                return;
            }
            self.active_definition = false;
            self.flush_pending_top_level_line();
        }

        if let Some(blockquote) = self.active_blockquote.take() {
            self.active_definition = false;
            if block_parse::is_lazy_blockquote_continuation(line) {
                block_render::write_blockquote_line(
                    blockquote,
                    line,
                    line_has_lf,
                    &mut self.out,
                    &mut self.footnotes,
                    &mut self.next_footnote_number,
                    &mut self.link_id_counter,
                );
                self.out.push('\n');
                self.active_blockquote = Some(blockquote);
                self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
                return;
            }
        }

        if block_parse::has_indented_code_prefix(line)
            && (block_parse::parse_unordered_list(line).is_some()
                || block_parse::parse_ordered_list(line).is_some())
        {
            self.active_definition = false;
            self.process_line(line, line_has_lf);
            self.out.push('\n');
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if self.previous_line_was_blank && block_parse::has_indented_code_prefix(line) {
            self.active_definition = false;
            self.in_code_block = true;
            self.code_fence_marker = None;
            self.code_language.clear();
            let deindented = block_parse::deindent_code_line(line).to_vec();
            self.append_code_line(&deindented, line_has_lf);
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if !self.in_code_block && block_parse::is_pipe_line(line) {
            self.active_definition = false;
            self.in_pipe_block = true;
            self.pipe_buf.extend_from_slice(line);
            self.pipe_buf.push(b'\n');
            self.pipe_last_line_has_lf = line_has_lf;
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if let Some(marker) = block_parse::code_fence_marker(text_util::left_trim(line)) {
            self.active_definition = false;
            self.in_code_block = true;
            self.code_fence_marker = Some(marker);
            self.code_language.clear();
            self.code_language
                .extend_from_slice(block_parse::code_fence_language(text_util::left_trim(line)));
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if self.active_definition
            && let Some(body) = block_parse::definition_marker_body(line)
        {
            block_render::write_definition_line(
                body,
                line_has_lf,
                &mut self.out,
                &mut self.footnotes,
                &mut self.next_footnote_number,
                &mut self.link_id_counter,
            );
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }
        self.active_definition = false;

        if line_has_lf && block_parse::is_setext_candidate(line) {
            self.pending_top_level_line.extend_from_slice(line);
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        if block_parse::is_horizontal_rule(line) {
            self.deliver_block(Block::Rule);
            self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
            return;
        }

        self.process_line(line, line_has_lf);
        self.out.push('\n');
        self.previous_line_was_blank = text_util::is_blank_markdown_line(line);
    }

    fn append_code_line(&mut self, line: &[u8], _line_has_lf: bool) {
        self.code_buf.extend_from_slice(line);
        self.code_buf.push(b'\n');
    }

    fn flush_pending_top_level_line(&mut self) {
        if self.pending_top_level_line.is_empty() {
            return;
        }
        let line = self.pending_top_level_line.clone();
        self.process_line(&line, true);
        self.out.push('\n');
        self.pending_top_level_line.clear();
    }

    fn process_line(&mut self, line: &[u8], line_has_lf: bool) {
        if self.in_code_block {
            ansi::write_dim(&mut self.out, ansi::VERTICAL_RULE_PREFIX.as_bytes());
            push_utf8(&mut self.out, line);
            return;
        }

        if block_parse::is_horizontal_rule(line) {
            ansi::write_horizontal_rule(&mut self.out);
            return;
        }

        if let Some((level, content)) = block_parse::parse_header(line) {
            let content = text_util::without_terminal_hard_break_marker(content, line_has_lf);
            block_render::write_heading(
                level,
                content,
                &mut self.out,
                &mut self.footnotes,
                &mut self.next_footnote_number,
                &mut self.link_id_counter,
            );
            return;
        }

        if let Some((indent, depth, content)) = block_parse::parse_blockquote(line) {
            let blockquote = (indent, depth);
            self.active_blockquote = if block_parse::is_blockquote_paragraph(content) {
                Some(blockquote)
            } else {
                None
            };
            block_render::write_blockquote_line(
                blockquote,
                content,
                line_has_lf,
                &mut self.out,
                &mut self.footnotes,
                &mut self.next_footnote_number,
                &mut self.link_id_counter,
            );
            return;
        }

        if let Some((indent, content)) = block_parse::parse_unordered_list(line) {
            push_spaces(&mut self.out, indent);
            if let Some(task) = block_parse::parse_task_list_item(content) {
                block_render::write_task_list_marker(&task, &mut self.out);
                let task_content =
                    text_util::without_terminal_hard_break_marker(task.2, line_has_lf);
                let mut register = reg!(self);
                inline_render::write_inline(
                    task_content,
                    &mut self.out,
                    false,
                    &mut self.link_id_counter,
                    Some(&mut register),
                );
                return;
            }
            ansi::write_dim(&mut self.out, ansi::BULLET_MARKER.as_bytes());
            let content = text_util::without_terminal_hard_break_marker(content, line_has_lf);
            let mut register = reg!(self);
            inline_render::write_inline(
                content,
                &mut self.out,
                false,
                &mut self.link_id_counter,
                Some(&mut register),
            );
            return;
        }

        if let Some((indent, marker, content)) = block_parse::parse_ordered_list(line) {
            push_spaces(&mut self.out, indent);
            ansi::write_dim(&mut self.out, marker);
            self.out.push(' ');
            if let Some(task) = block_parse::parse_task_list_item(content) {
                block_render::write_task_list_marker(&task, &mut self.out);
                let task_content =
                    text_util::without_terminal_hard_break_marker(task.2, line_has_lf);
                let mut register = reg!(self);
                inline_render::write_inline(
                    task_content,
                    &mut self.out,
                    false,
                    &mut self.link_id_counter,
                    Some(&mut register),
                );
                return;
            }
            let content = text_util::without_terminal_hard_break_marker(content, line_has_lf);
            let mut register = reg!(self);
            inline_render::write_inline(
                content,
                &mut self.out,
                false,
                &mut self.link_id_counter,
                Some(&mut register),
            );
            return;
        }

        let content = text_util::without_terminal_hard_break_marker(line, line_has_lf);
        let mut register = reg!(self);
        inline_render::write_inline(
            content,
            &mut self.out,
            false,
            &mut self.link_id_counter,
            Some(&mut register),
        );
    }

    fn finalize_pipe_block(&mut self) {
        let buf = std::mem::take(&mut self.pipe_buf);
        self.in_pipe_block = false;
        self.pipe_last_line_has_lf = false;
        if block_parse::is_valid_table(&buf) {
            let table = block_render::render_table(
                &buf,
                &mut self.footnotes,
                &mut self.next_footnote_number,
                &mut self.link_id_counter,
            );
            self.deliver_block(Block::Table(table));
            return;
        }
        let mut start = 0usize;
        while start < buf.len() {
            let end = buf[start..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| start + p)
                .unwrap_or(buf.len());
            let line_has_lf = if end + 1 == buf.len() {
                self.pipe_last_line_has_lf
            } else {
                true
            };
            self.process_line(&buf[start..end], line_has_lf);
            if line_has_lf {
                self.out.push('\n');
            }
            start = end + 1;
        }
    }

    fn finalize_code_block(&mut self) {
        let code = std::mem::take(&mut self.code_buf);
        let language = std::mem::take(&mut self.code_language);
        self.deliver_block(Block::Code {
            language: String::from_utf8_lossy(&language).into_owned(),
            code: String::from_utf8_lossy(&code).into_owned(),
        });
    }

    fn deliver_block(&mut self, block: Block) {
        if let Some(cb) = &mut self.on_block {
            let mut out = std::mem::take(&mut self.out);
            cb(block, &mut out);
            self.out = out;
        } else {
            match block {
                Block::Code { language, code } => {
                    let rendered = render_code_block(language.as_bytes(), code.as_bytes());
                    self.out.push_str(&rendered);
                }
                Block::Table(table) => self.out.push_str(&table),
                Block::Rule => {
                    ansi::write_horizontal_rule(&mut self.out);
                    self.out.push('\n');
                }
            }
        }
    }

    fn begin_footnote_definition(&mut self, label: &[u8], body: &[u8]) {
        let index = self.find_or_append_footnote(label);
        let has_definition = self.footnotes[index].has_definition;
        if has_definition {
            self.active_footnote = Some(ActiveFootnote {
                index,
                append_body: false,
            });
            return;
        }
        self.footnotes[index].body = String::from_utf8_lossy(body).into_owned();
        self.footnotes[index].has_definition = true;
        self.active_footnote = Some(ActiveFootnote {
            index,
            append_body: true,
        });
    }

    fn find_or_append_footnote(&mut self, label: &[u8]) -> usize {
        let label_str = String::from_utf8_lossy(label).into_owned();
        if let Some(pos) = self.footnotes.iter().position(|n| n.label == label_str) {
            return pos;
        }
        self.footnotes.push(Footnote {
            label: label_str,
            body: String::new(),
            number: None,
            has_definition: false,
        });
        self.footnotes.len() - 1
    }

    fn flush_footnotes(&mut self) {
        let has_note = self
            .footnotes
            .iter()
            .any(|n| n.number.is_some() && n.has_definition);
        if !has_note {
            self.footnotes.clear();
            self.next_footnote_number = 0;
            return;
        }
        while self.out.as_bytes().last() == Some(&b'\n') {
            self.out.pop();
        }
        if self.out.is_empty() {
            self.out.push('\n');
        } else {
            self.out.push_str("\n\n");
        }
        let mut number = 1usize;
        while number <= self.next_footnote_number {
            let mut body = None;
            for i in 0..self.footnotes.len() {
                if self.footnotes[i].number == Some(number) && self.footnotes[i].has_definition {
                    body = Some(self.footnotes[i].body.clone());
                    break;
                }
            }
            if let Some(body) = body {
                block_render::write_footnote_definition_marker(&mut self.out, number);
                block_render::write_footnote_body(
                    &body,
                    &mut self.out,
                    &mut self.footnotes,
                    &mut self.next_footnote_number,
                    number,
                    &mut self.link_id_counter,
                );
            }
            number += 1;
        }
        self.footnotes.clear();
        self.next_footnote_number = 0;
    }
}

pub(crate) fn register_fn<'a>(
    footnotes: &'a mut Vec<Footnote>,
    next: &'a mut usize,
) -> impl FnMut(&str) -> usize + 'a {
    move |label: &str| {
        let index = if let Some(pos) = footnotes.iter().position(|n| n.label == label) {
            pos
        } else {
            footnotes.push(Footnote {
                label: label.to_string(),
                body: String::new(),
                number: None,
                has_definition: false,
            });
            footnotes.len() - 1
        };
        if let Some(number) = footnotes[index].number {
            return number;
        }
        *next += 1;
        footnotes[index].number = Some(*next);
        *next
    }
}

pub fn push_utf8(out: &mut String, bytes: &[u8]) {
    out.push_str(std::str::from_utf8(bytes).unwrap_or(""));
}

fn push_spaces(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push(' ');
    }
}

mod block_render {
    use super::Footnote;
    use super::ansi;
    use super::inline_render;
    use super::text_util;

    pub fn write_heading(
        level: usize,
        content: &[u8],
        out: &mut String,
        footnotes: &mut Vec<Footnote>,
        next: &mut usize,
        link_id: &mut u32,
    ) {
        match level {
            1 => {
                out.push_str(ansi::BOLD_OPEN);
                out.push_str(ansi::UNDERLINE_OPEN);
            }
            2 => out.push_str(ansi::BOLD_OPEN),
            3 => out.push_str(ansi::UNDERLINE_OPEN),
            4 => {
                out.push_str(ansi::BOLD_OPEN);
                out.push_str(ansi::DIM_OPEN);
            }
            5 => {
                out.push_str(ansi::DIM_OPEN);
                out.push_str(ansi::UNDERLINE_OPEN);
            }
            6 => out.push_str(ansi::DIM_OPEN),
            _ => {}
        }
        let mut register = super::register_fn(footnotes, next);
        inline_render::write_inline_no_bold(
            content,
            out,
            level == 1 || level == 3 || level == 5,
            link_id,
            Some(&mut register),
        );
        match level {
            1 => {
                out.push_str(ansi::UNDERLINE_CLOSE);
                out.push_str(ansi::BOLD_CLOSE);
            }
            2 | 4 | 6 => out.push_str(ansi::BOLD_CLOSE),
            3 => out.push_str(ansi::UNDERLINE_CLOSE),
            5 => {
                out.push_str(ansi::UNDERLINE_CLOSE);
                out.push_str(ansi::DIM_CLOSE);
            }
            _ => {}
        }
    }

    pub fn write_blockquote_line(
        blockquote: (usize, usize),
        content: &[u8],
        line_has_lf: bool,
        out: &mut String,
        footnotes: &mut Vec<Footnote>,
        next: &mut usize,
        link_id: &mut u32,
    ) {
        let (indent, depth) = blockquote;
        push_spaces(out, indent);
        for _ in 0..depth {
            ansi::write_dim(out, ansi::VERTICAL_RULE_PREFIX.as_bytes());
        }
        let content = text_util::without_terminal_hard_break_marker(content, line_has_lf);
        let mut register = super::register_fn(footnotes, next);
        inline_render::write_inline(content, out, false, link_id, Some(&mut register));
    }

    pub fn write_definition_line(
        body: &[u8],
        line_has_lf: bool,
        out: &mut String,
        footnotes: &mut Vec<Footnote>,
        next: &mut usize,
        link_id: &mut u32,
    ) {
        ansi::write_dim(out, b"  ");
        let body = text_util::without_terminal_hard_break_marker(body, line_has_lf);
        let mut register = super::register_fn(footnotes, next);
        inline_render::write_inline(body, out, false, link_id, Some(&mut register));
        out.push('\n');
    }

    pub fn write_task_list_marker(task: &(bool, bool, &[u8]), out: &mut String) {
        let (completed, has_separator, _content) = task;
        if *completed {
            out.push_str(ansi::TASK_COMPLETED_OPEN);
            out.push_str(ansi::TASK_COMPLETED_MARKER);
            out.push_str(ansi::TASK_COMPLETED_CLOSE);
            if *has_separator {
                out.push(' ');
            }
            return;
        }
        out.push_str(ansi::DIM_OPEN);
        out.push_str(ansi::TASK_PENDING_MARKER);
        if *has_separator {
            out.push(' ');
        }
        out.push_str(ansi::DIM_CLOSE);
    }

    fn split_cells<'a>(line: &'a [u8], cells: &mut Vec<&'a [u8]>) {
        let mut rest = text_util::left_trim(line);
        if !rest.is_empty() && rest[0] == b'|' {
            rest = &rest[1..];
        }
        while !rest.is_empty() && (rest[rest.len() - 1] == b' ' || rest[rest.len() - 1] == b'\t') {
            rest = &rest[..rest.len() - 1];
        }
        if !rest.is_empty()
            && rest[rest.len() - 1] == b'|'
            && !text_util::is_escaped_punctuation_at(rest, rest.len() - 1)
        {
            rest = &rest[..rest.len() - 1];
        }
        let mut cell_start = 0usize;
        let mut i = 0usize;
        while i <= rest.len() {
            if i == rest.len()
                || (rest[i] == b'|' && !text_util::is_escaped_punctuation_at(rest, i))
            {
                let mut start = cell_start;
                let mut end = i;
                while start < end && (rest[start] == b' ' || rest[start] == b'\t') {
                    start += 1;
                }
                while end > start && (rest[end - 1] == b' ' || rest[end - 1] == b'\t') {
                    end -= 1;
                }
                cells.push(&rest[start..end]);
                cell_start = i + 1;
            }
            i += 1;
        }
    }

    struct RenderedCell {
        bytes: String,
        width: usize,
    }

    pub fn render_table(
        buf: &[u8],
        footnotes: &mut Vec<Footnote>,
        next: &mut usize,
        link_id: &mut u32,
    ) -> String {
        let mut rows: Vec<Vec<RenderedCell>> = Vec::new();
        let mut line_idx = 0usize;
        let mut start = 0usize;
        while start < buf.len() {
            let end = buf[start..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| start + p)
                .unwrap_or(buf.len());
            if line_idx != 1 {
                let mut raw: Vec<&[u8]> = Vec::new();
                split_cells(&buf[start..end], &mut raw);
                let mut row = Vec::new();
                for cell in raw {
                    let mut bytes = String::new();
                    let mut register = super::register_fn(footnotes, next);
                    inline_render::write_inline(
                        cell,
                        &mut bytes,
                        false,
                        link_id,
                        Some(&mut register),
                    );
                    let width = super::display_width::visible_width_ignoring_ansi(bytes.as_bytes());
                    row.push(RenderedCell { bytes, width });
                }
                rows.push(row);
            }
            line_idx += 1;
            start = end + 1;
        }

        let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if rows.is_empty() || col_count == 0 {
            return String::new();
        }
        let mut widths = vec![0usize; col_count];
        for row in &rows {
            for (col, cell) in row.iter().enumerate() {
                if cell.width > widths[col] {
                    widths[col] = cell.width;
                }
            }
        }
        let mut alignments = vec![Align::Left; col_count];
        if let Some(sep_line) = text_util::nth_line(buf, 1) {
            let mut cells: Vec<&[u8]> = Vec::new();
            split_cells(sep_line, &mut cells);
            for (col, cell) in cells.iter().enumerate() {
                if col >= alignments.len() {
                    break;
                }
                let trimmed = trim_bytes(cell);
                if trimmed.is_empty() {
                    continue;
                }
                let starts = trimmed[0] == b':';
                let ends = trimmed[trimmed.len() - 1] == b':';
                alignments[col] = if starts && ends {
                    Align::Center
                } else if ends {
                    Align::Right
                } else {
                    Align::Left
                };
            }
        }

        let mut out = String::new();
        for (row_idx, row) in rows.iter().enumerate() {
            let is_header = row_idx == 0;
            for col in 0..col_count {
                if col > 0 {
                    out.push_str(ansi::TABLE_COLUMN_SEP);
                }
                let col_align = if is_header {
                    Align::Left
                } else {
                    alignments[col]
                };
                if col < row.len() {
                    let cell = &row[col];
                    let pad = widths[col].saturating_sub(cell.width);
                    let left_pad = match col_align {
                        Align::Left => 0,
                        Align::Right => pad,
                        Align::Center => pad / 2,
                    };
                    let right_pad = pad - left_pad;
                    for _ in 0..left_pad {
                        out.push(' ');
                    }
                    if is_header {
                        write_table_header_cell(&mut out, &cell.bytes);
                    } else {
                        out.push_str(&cell.bytes);
                    }
                    for _ in 0..right_pad {
                        out.push(' ');
                    }
                } else {
                    for _ in 0..widths[col] {
                        out.push(' ');
                    }
                }
            }
            out.push('\n');
            if is_header {
                write_table_separator(&mut out, &widths);
            }
        }
        out
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Align {
        Left,
        Right,
        Center,
    }

    fn trim_bytes(mut bytes: &[u8]) -> &[u8] {
        while !bytes.is_empty() && (bytes[0] == b' ' || bytes[0] == b'\t') {
            bytes = &bytes[1..];
        }
        while !bytes.is_empty()
            && (bytes[bytes.len() - 1] == b' ' || bytes[bytes.len() - 1] == b'\t')
        {
            bytes = &bytes[..bytes.len() - 1];
        }
        bytes
    }

    fn write_table_header_cell(out: &mut String, cell: &str) {
        out.push_str(ansi::BOLD_OPEN);
        let mut remaining = cell;
        while let Some(close_index) = remaining.find(ansi::BOLD_CLOSE) {
            out.push_str(&remaining[..close_index]);
            out.push_str(ansi::BOLD_CLOSE);
            out.push_str(ansi::BOLD_OPEN);
            remaining = &remaining[close_index + ansi::BOLD_CLOSE.len()..];
        }
        out.push_str(remaining);
        out.push_str(ansi::BOLD_CLOSE);
    }

    fn write_table_separator(out: &mut String, widths: &[usize]) {
        for (col, col_width) in widths.iter().enumerate() {
            if col > 0 {
                out.push_str(ansi::TABLE_JUNCTION);
            }
            for _ in 0..*col_width {
                out.push_str(ansi::TABLE_HORIZ);
            }
        }
        out.push('\n');
    }

    pub fn write_footnote_definition_marker(out: &mut String, number: usize) {
        let marker = format!("[{number}] ");
        ansi::write_dim(out, marker.as_bytes());
    }

    pub fn write_footnote_body(
        body: &str,
        out: &mut String,
        footnotes: &mut Vec<Footnote>,
        next: &mut usize,
        number: usize,
        link_id: &mut u32,
    ) {
        let mut start = 0usize;
        let mut is_first = true;
        loop {
            let end = body[start..]
                .find('\n')
                .map(|p| start + p)
                .unwrap_or(body.len());
            if !is_first {
                out.push('\n');
                let marker = format!("[{number}] ");
                let spaces = " ".repeat(marker.len());
                ansi::write_dim(out, spaces.as_bytes());
            }
            let mut register = super::register_fn(footnotes, next);
            inline_render::write_inline(
                &body.as_bytes()[start..end],
                out,
                false,
                link_id,
                Some(&mut register),
            );
            if end == body.len() {
                break;
            }
            is_first = false;
            start = end + 1;
        }
        out.push('\n');
    }

    pub fn push_spaces(out: &mut String, n: usize) {
        for _ in 0..n {
            out.push(' ');
        }
    }
}

pub fn render_code_block(language: &[u8], code: &[u8]) -> String {
    let lang = std::str::from_utf8(language).unwrap_or("");
    let profile = highlight::resolve(lang);
    let mut out = String::new();
    let mut start = 0usize;
    while start < code.len() {
        let end = code[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map(|p| start + p)
            .unwrap_or(code.len());
        let line = &code[start..end];
        out.push_str(ansi::DIM_OPEN);
        out.push_str(ansi::VERTICAL_RULE_PREFIX);
        out.push_str(ansi::DIM_CLOSE);
        if let Some(profile) = profile {
            out.push_str(&highlight::highlight(line, profile));
        } else {
            out.push_str(std::str::from_utf8(line).unwrap_or(""));
        }
        out.push('\n');
        if end == code.len() {
            break;
        }
        start = end + 1;
    }
    out
}

pub mod highlight {
    pub struct Profile {
        pub line_comments: &'static [&'static str],
        pub block_comment: Option<(&'static str, &'static str)>,
        pub quotes: &'static [u8],
        pub keywords: &'static [&'static str],
        pub literals: &'static [&'static str],
        pub case_insensitive: bool,
        pub aliases: &'static [&'static str],
    }

    impl Profile {
        pub const fn empty() -> Profile {
            Profile {
                line_comments: &[],
                block_comment: None,
                quotes: b"",
                keywords: &[],
                literals: &[],
                case_insensitive: false,
                aliases: &[],
            }
        }
    }

    const DARK_KEYWORD: &str = "\x1b[38;5;252m";
    const DARK_STRING: &str = "\x1b[38;5;250m";
    const DARK_NUMBER: &str = "\x1b[38;5;250m";
    const DARK_COMMENT: &str = "\x1b[38;5;245m";
    const RESET: &str = "\x1b[39m";

    fn eq(a: &[u8], b: &str, insensitive: bool) -> bool {
        if a.len() != b.len() {
            return false;
        }
        if insensitive {
            a.iter()
                .zip(b.bytes())
                .all(|(x, y)| x.eq_ignore_ascii_case(&y))
        } else {
            a == b.as_bytes()
        }
    }

    pub fn resolve(label: &str) -> Option<&'static Profile> {
        for p in PROFILES {
            for alias in p.aliases {
                if label.eq_ignore_ascii_case(alias) {
                    return Some(p);
                }
            }
        }
        None
    }

    fn in_list(token: &[u8], list: &'static [&'static str], insensitive: bool) -> bool {
        list.iter().any(|k| eq(token, k, insensitive))
    }

    fn block_comment_end(source: &[u8], index: usize, spec: (&str, &str)) -> Option<usize> {
        let start = spec.0.as_bytes();
        if !source[index..].starts_with(start) {
            return None;
        }
        let content_start = index + start.len();
        let close = spec.1.as_bytes();
        let mut i = content_start;
        while i + close.len() <= source.len() {
            if &source[i..i + close.len()] == close {
                return Some(i + close.len());
            }
            i += 1;
        }
        Some(source.len())
    }

    fn line_comment_end(source: &[u8], index: usize, prefixes: &[&str]) -> Option<usize> {
        for prefix in prefixes {
            if source[index..].starts_with(prefix.as_bytes()) {
                return Some(
                    source[index..]
                        .iter()
                        .position(|&c| c == b'\n')
                        .map(|p| index + p)
                        .unwrap_or(source.len()),
                );
            }
        }
        None
    }

    fn quoted_end(source: &[u8], start: usize) -> usize {
        let quote = source[start];
        let mut index = start + 1;
        while index < source.len() {
            if source[index] == b'\n' {
                return index;
            }
            if source[index] == b'\\' && index + 1 < source.len() {
                index += 2;
                continue;
            }
            if source[index] == quote {
                return index + 1;
            }
            index += 1;
        }
        source.len()
    }

    fn is_number_start(source: &[u8], index: usize) -> bool {
        let c = source[index];
        c.is_ascii_digit()
            || ((c == b'-' || c == b'+')
                && index + 1 < source.len()
                && source[index + 1].is_ascii_digit())
            || (c == b'.' && index + 1 < source.len() && source[index + 1].is_ascii_digit())
    }

    fn number_end(source: &[u8], start: usize) -> usize {
        let mut i = start;
        while i < source.len()
            && (source[i].is_ascii_alphanumeric() || matches!(source[i], b'.' | b'_' | b'-' | b'+'))
        {
            i += 1;
        }
        i
    }

    fn is_identifier_start(c: u8) -> bool {
        c.is_ascii_alphabetic() || c == b'_'
    }

    fn identifier_end(source: &[u8], start: usize) -> usize {
        let mut i = start;
        while i < source.len() && (source[i].is_ascii_alphanumeric() || source[i] == b'_') {
            i += 1;
        }
        i
    }

    fn append_styled(out: &mut String, style: &str, text: &[u8]) {
        out.push_str(style);
        out.push_str(std::str::from_utf8(text).unwrap_or(""));
        out.push_str(RESET);
    }

    pub fn highlight(source: &[u8], profile: &Profile) -> String {
        let mut styled = String::new();
        let mut index = 0usize;
        while index < source.len() {
            if source[index] == b'\n' {
                styled.push('\n');
                index += 1;
                continue;
            }
            if let Some(comment) = profile.block_comment
                && let Some(end) = block_comment_end(source, index, comment)
            {
                append_styled(&mut styled, DARK_COMMENT, &source[index..end]);
                index = end;
                continue;
            }
            if !profile.line_comments.is_empty()
                && let Some(end) = line_comment_end(source, index, profile.line_comments)
            {
                append_styled(&mut styled, DARK_COMMENT, &source[index..end]);
                index = end;
                continue;
            }
            if profile.quotes.contains(&source[index]) {
                let end = quoted_end(source, index);
                append_styled(&mut styled, DARK_STRING, &source[index..end]);
                index = end;
                continue;
            }
            if is_number_start(source, index) {
                let end = number_end(source, index);
                append_styled(&mut styled, DARK_NUMBER, &source[index..end]);
                index = end;
                continue;
            }
            if is_identifier_start(source[index]) {
                let end = identifier_end(source, index);
                let token = &source[index..end];
                if in_list(token, profile.keywords, profile.case_insensitive) {
                    append_styled(&mut styled, DARK_KEYWORD, token);
                } else if in_list(token, profile.literals, profile.case_insensitive) {
                    append_styled(&mut styled, DARK_NUMBER, token);
                } else {
                    styled.push_str(std::str::from_utf8(token).unwrap_or(""));
                }
                index = end;
                continue;
            }
            let len = super::text_util::utf8_seq_len(source[index]);
            styled.push_str(
                std::str::from_utf8(&source[index..(index + len).min(source.len())]).unwrap_or(""),
            );
            index += len;
        }
        styled
    }

    macro_rules! p {
        ($( $field:ident = $val:expr ),+ $(,)?) => {
            Profile { $($field: $val,)+ ..Profile::empty() }
        };
    }

    #[allow(clippy::needless_update)]
    pub static PROFILES: &[Profile] = &[
        p!(
            line_comments = &["//"],
            quotes = b"\"",
            keywords = &[
                "const", "var", "fn", "pub", "return", "if", "else", "while", "for", "struct",
                "enum", "union", "try", "catch", "comptime", "defer", "errdefer", "async", "await",
                "anytype", "void"
            ],
            aliases = &["zig"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'`",
            keywords = &[
                "const",
                "let",
                "var",
                "function",
                "class",
                "interface",
                "type",
                "export",
                "import",
                "from",
                "return",
                "if",
                "else",
                "for",
                "while",
                "async",
                "await",
                "new",
                "extends",
                "implements",
                "public",
                "private",
                "readonly"
            ],
            literals = &["true", "false", "null", "undefined"],
            aliases = &["js", "jsx", "javascript", "ts", "tsx", "typescript"]
        ),
        p!(
            quotes = b"\"",
            literals = &["true", "false", "null"],
            aliases = &["json"]
        ),
        p!(
            line_comments = &["#"],
            quotes = b"\"'`",
            keywords = &[
                "if", "then", "fi", "for", "do", "done", "in", "case", "esac", "function", "local",
                "export", "readonly", "return"
            ],
            literals = &["true", "false", "null"],
            aliases = &["sh", "bash", "zsh", "shell"]
        ),
        p!(
            line_comments = &["#"],
            quotes = b"\"'",
            keywords = &[
                "def", "class", "return", "if", "elif", "else", "for", "while", "in", "import",
                "from", "as", "try", "except", "with", "lambda", "async", "await", "pass", "raise",
                "yield", "match", "case"
            ],
            literals = &["True", "False", "None"],
            aliases = &["python", "py"]
        ),
        p!(
            line_comments = &["#"],
            quotes = b"\"'",
            literals = &["true", "false", "null", "yes", "no", "on", "off"],
            aliases = &["yaml", "yml"]
        ),
        p!(
            line_comments = &["#"],
            quotes = b"\"'",
            literals = &["true", "false"],
            aliases = &["toml"]
        ),
        p!(
            line_comments = &["--"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "select", "from", "where", "join", "left", "right", "inner", "outer", "on",
                "insert", "into", "values", "update", "set", "delete", "create", "alter", "drop",
                "table", "index", "group", "by", "order", "having", "limit", "as", "and", "or",
                "not", "distinct", "union"
            ],
            literals = &["true", "false", "null"],
            case_insensitive = true,
            aliases = &["sql"]
        ),
        p!(
            line_comments = &["#"],
            quotes = b"\"'",
            keywords = &[
                "from",
                "run",
                "cmd",
                "entrypoint",
                "copy",
                "add",
                "workdir",
                "env",
                "arg",
                "expose",
                "volume",
                "user",
                "label",
                "onbuild",
                "stopsignal",
                "healthcheck",
                "shell",
                "maintainer"
            ],
            case_insensitive = true,
            aliases = &["dockerfile", "docker"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "use", "mod",
                "crate", "return", "if", "else", "match", "for", "while", "loop", "async", "await",
                "move", "where", "self", "super"
            ],
            literals = &["true", "false", "None", "Some"],
            aliases = &["rust", "rs"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"`",
            keywords = &[
                "package",
                "import",
                "func",
                "var",
                "const",
                "type",
                "struct",
                "interface",
                "return",
                "if",
                "else",
                "for",
                "range",
                "switch",
                "case",
                "go",
                "defer",
                "select",
                "chan",
                "map"
            ],
            literals = &["true", "false", "nil"],
            aliases = &["go"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "auto", "break", "case", "char", "const", "continue", "default", "do", "double",
                "else", "enum", "extern", "float", "for", "goto", "if", "int", "long", "return",
                "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
                "unsigned", "void", "volatile", "while"
            ],
            literals = &["true", "false", "NULL"],
            aliases = &["c", "h"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "auto",
                "bool",
                "class",
                "const",
                "constexpr",
                "decltype",
                "delete",
                "enum",
                "explicit",
                "friend",
                "inline",
                "namespace",
                "new",
                "nullptr",
                "private",
                "protected",
                "public",
                "template",
                "this",
                "typename",
                "using",
                "virtual",
                "void"
            ],
            literals = &["true", "false", "nullptr", "NULL"],
            aliases = &["cpp", "c++", "cc", "cxx", "hpp"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "class",
                "namespace",
                "using",
                "public",
                "private",
                "protected",
                "internal",
                "static",
                "void",
                "string",
                "int",
                "var",
                "new",
                "return",
                "if",
                "else",
                "for",
                "foreach",
                "while",
                "async",
                "await",
                "interface",
                "record",
                "get",
                "set"
            ],
            literals = &["true", "false", "null"],
            aliases = &["csharp", "cs"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "class",
                "interface",
                "package",
                "import",
                "public",
                "private",
                "protected",
                "static",
                "final",
                "void",
                "new",
                "return",
                "if",
                "else",
                "for",
                "while",
                "try",
                "catch",
                "throws",
                "extends",
                "implements",
                "record",
                "var"
            ],
            literals = &["true", "false", "null"],
            aliases = &["java"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "function",
                "class",
                "public",
                "private",
                "protected",
                "namespace",
                "use",
                "return",
                "if",
                "else",
                "foreach",
                "for",
                "while",
                "try",
                "catch",
                "new",
                "static",
                "const",
                "echo",
                "yield"
            ],
            literals = &["true", "false", "null"],
            aliases = &["php"]
        ),
        p!(
            line_comments = &["#"],
            quotes = b"\"'",
            keywords = &[
                "def",
                "class",
                "module",
                "end",
                "return",
                "if",
                "elsif",
                "else",
                "unless",
                "case",
                "when",
                "do",
                "while",
                "for",
                "in",
                "begin",
                "rescue",
                "require",
                "attr_reader"
            ],
            literals = &["true", "false", "nil"],
            aliases = &["ruby", "rb"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "func",
                "let",
                "var",
                "class",
                "struct",
                "enum",
                "protocol",
                "extension",
                "import",
                "public",
                "private",
                "return",
                "if",
                "else",
                "guard",
                "for",
                "while",
                "switch",
                "case",
                "async",
                "await",
                "throws",
                "try"
            ],
            literals = &["true", "false", "nil"],
            aliases = &["swift"]
        ),
        p!(
            line_comments = &["#"],
            block_comment = Some(("<#", "#>")),
            quotes = b"\"'",
            keywords = &[
                "function", "param", "if", "else", "elseif", "foreach", "for", "while", "switch",
                "return", "throw", "try", "catch", "finally", "begin", "process", "end", "filter",
                "class", "enum"
            ],
            literals = &["true", "false", "null"],
            case_insensitive = true,
            aliases = &["powershell", "ps1", "pwsh", "ps"]
        ),
        p!(
            line_comments = &["--"],
            block_comment = Some(("--[[", "]]")),
            quotes = b"\"'",
            keywords = &[
                "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto",
                "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true",
                "until", "while"
            ],
            literals = &["true", "false", "nil"],
            aliases = &["lua"]
        ),
        p!(
            block_comment = Some(("<!--", "-->")),
            quotes = b"\"'",
            keywords = &[
                "html", "head", "body", "main", "header", "footer", "section", "article", "div",
                "span", "a", "p", "script", "style", "link", "meta", "title", "button", "input",
                "form", "img", "ul", "li"
            ],
            aliases = &["html", "htm"]
        ),
        p!(
            block_comment = Some(("<!--", "-->")),
            quotes = b"\"'",
            keywords = &["xml", "version", "encoding", "DOCTYPE", "CDATA"],
            aliases = &["xml"]
        ),
        p!(
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "color",
                "background",
                "display",
                "position",
                "margin",
                "padding",
                "border",
                "font",
                "width",
                "height",
                "flex",
                "grid",
                "align",
                "justify",
                "transition",
                "transform",
                "animation",
                "media"
            ],
            aliases = &["css"]
        ),
        p!(
            line_comments = &["#", "//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "resource",
                "module",
                "variable",
                "output",
                "provider",
                "terraform",
                "locals",
                "data",
                "dynamic",
                "for_each",
                "count"
            ],
            literals = &["true", "false", "null"],
            aliases = &["hcl", "terraform", "tf"]
        ),
        p!(
            line_comments = &["//"],
            block_comment = Some(("/*", "*/")),
            quotes = b"\"'",
            keywords = &[
                "fun",
                "val",
                "var",
                "class",
                "object",
                "interface",
                "package",
                "import",
                "public",
                "private",
                "return",
                "if",
                "else",
                "when",
                "for",
                "while",
                "try",
                "catch",
                "data",
                "sealed",
                "suspend"
            ],
            literals = &["true", "false", "null"],
            aliases = &["kotlin", "kt", "kts"]
        ),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(s: &str) -> String {
        Markdown::render(s)
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(render("hello world\n"), "hello world\n");
    }

    #[test]
    fn bold_italic_and_code() {
        assert_eq!(
            render("a **bold** *it* `code` b\n"),
            "a \x1b[1mbold\x1b[22m \x1b[3mit\x1b[23m \x1b[38;5;245mcode\x1b[39m b\n"
        );
    }

    #[test]
    fn heading_styles() {
        assert_eq!(render("# H1\n"), "\x1b[1m\x1b[4mH1\x1b[24m\x1b[22m\n");
        assert_eq!(render("## H2\n"), "\x1b[1mH2\x1b[22m\n");
        assert_eq!(render("### H3\n"), "\x1b[4mH3\x1b[24m\n");
        assert_eq!(render("###### H6\n"), "\x1b[2mH6\x1b[22m\n");
    }

    #[test]
    fn code_block_uses_rail() {
        assert_eq!(
            render("```rs\nlet x = 1;\n```\n"),
            "\x1b[2m│ \x1b[22m\x1b[38;5;252mlet\x1b[39m x = \x1b[38;5;250m1\x1b[39m;\n"
        );
    }

    #[test]
    fn blockquote_uses_rail() {
        assert_eq!(render("> hi\n"), "\x1b[2m│ \x1b[22mhi\n");
        assert_eq!(
            render("> > nested\n"),
            "\x1b[2m│ \x1b[22m\x1b[2m│ \x1b[22mnested\n"
        );
    }

    #[test]
    fn task_lists() {
        assert_eq!(render("- [ ] todo\n"), "\x1b[2m☐ \x1b[22mtodo\n");
        assert_eq!(render("- [x] done\n"), "\x1b[38;5;252m✓\x1b[39m done\n");
        assert_eq!(
            render("1. [ ] item\n"),
            "\x1b[2m1.\x1b[22m \x1b[2m☐ \x1b[22mitem\n"
        );
    }

    #[test]
    fn bare_url_becomes_osc8_link() {
        let out = render("see https://example.com now\n");
        assert!(out.contains("\x1b]8;id=fx-0;https://example.com\x1b\\"));
        assert!(out.contains("\x1b[4mhttps://example.com\x1b[24m"));
        assert!(out.contains("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn horizontal_rule() {
        let out = render("---\n");
        assert_eq!(
            out,
            "\x1b[2m".to_string() + &"\u{2500}".repeat(60) + "\x1b[22m\n"
        );
    }

    #[test]
    fn table_renders_aligned() {
        let md = "| a | b |\n|---|---:|\n| 1 | 22 |\n";
        let out = render(md);
        assert!(out.contains("\u{2502}"));
        assert!(out.contains("\u{2500}\u{253c}\u{2500}"));
        assert!(out.contains("\x1b[1m")); // header bold
    }

    #[test]
    fn streaming_across_chunks() {
        let mut m = Markdown::new();
        m.push("**bo");
        m.push("ld**\n");
        assert_eq!(m.finish(), "\x1b[1mbold\x1b[22m\n");
    }

    #[test]
    fn blocks_delivered_through_callback() {
        let mut m = Markdown::new();
        let blocks = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let text = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        {
            let blocks = blocks.clone();
            let text = text.clone();
            m.set_on_block(move |b: Block, out: &mut String| {
                text.borrow_mut().push_str(out);
                out.clear();
                blocks.borrow_mut().push(match b {
                    Block::Code { code, .. } => format!("CODE:{code}"),
                    Block::Table(t) => format!("TABLE:{t}"),
                    Block::Rule => "RULE".to_string(),
                });
            });
        }
        m.push("before\n```\ncode\n```\nafter\n");
        let tail = m.finish();
        text.borrow_mut().push_str(&tail);
        assert_eq!(*blocks.borrow(), vec!["CODE:code\n"]);
        assert_eq!(*text.borrow(), "before\nafter\n");
    }

    #[test]
    fn escaped_punctuation() {
        assert_eq!(render("\\*not italic\\*\n"), "*not italic*\n");
    }

    #[test]
    fn strikethrough() {
        assert_eq!(render("~~gone~~\n"), "\x1b[9mgone\x1b[29m\n");
    }
}
