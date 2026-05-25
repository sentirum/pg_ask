//! Tiny SQL-flavoured tokenizer.
//!
//! Enough to tell apart identifiers, strings, comments, and punctuation —
//! not a full SQL parser. We need just enough to:
//!
//! * skip `--` line comments and `/* … */` block comments;
//! * skip `'…'` and `$tag$…$tag$` string literals;
//! * skip `"…"` quoted identifiers;
//! * emit a flat stream of tokens that downstream rules can match on
//!   (keyword check, banned-function check, multi-statement check).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token<'a> {
    /// Bareword identifier or keyword (case as-written).
    Word(&'a str),
    /// `'…'` or dollar-quoted string body (without delimiters).
    String,
    /// `"…"` quoted identifier (kept as written, without quotes).
    QuotedIdent(&'a str),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// Any other punctuation / operator char.
    Punct(char),
}

pub fn tokenize(src: &str) -> Vec<Token<'_>> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // Whitespace
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comment `-- …`
        if b == b'-' && peek(bytes, i + 1) == Some(b'-') {
            i = skip_until(bytes, i + 2, b'\n');
            continue;
        }

        // Block comment `/* … */` (non-nesting; SQL allows nesting but
        // matching for guard purposes does not need to)
        if b == b'/' && peek(bytes, i + 1) == Some(b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }

        // Single-quoted string `'…'` with `''` escape.
        if b == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if peek(bytes, i + 1) == Some(b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Token::String);
            continue;
        }

        // Dollar-quoted string `$tag$…$tag$`
        if b == b'$' {
            if let Some((tag_end, tag)) = read_dollar_tag(bytes, i) {
                // tag includes outer `$`s. Find the matching close.
                let close_pat = tag.as_bytes();
                let mut j = tag_end;
                while j + close_pat.len() <= bytes.len() {
                    if &bytes[j..j + close_pat.len()] == close_pat {
                        j += close_pat.len();
                        break;
                    }
                    j += 1;
                }
                i = j.min(bytes.len());
                out.push(Token::String);
                continue;
            }
        }

        // Quoted identifier `"…"` with `""` escape.
        if b == b'"' {
            let start = i + 1;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if peek(bytes, i + 1) == Some(b'"') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            // SAFETY: indices are byte positions in `src` and bounded.
            let slice = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
            out.push(Token::QuotedIdent(slice));
            if i < bytes.len() {
                i += 1; // skip closing "
            }
            continue;
        }

        // Bareword: identifier / keyword / number
        if is_word_start(b) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_word_cont(bytes[i]) {
                i += 1;
            }
            let slice = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
            out.push(Token::Word(slice));
            continue;
        }

        // Punctuation
        let tok = match b {
            b'(' => Token::LParen,
            b')' => Token::RParen,
            b',' => Token::Comma,
            b';' => Token::Semicolon,
            other => Token::Punct(other as char),
        };
        out.push(tok);
        i += 1;
    }

    out
}

fn peek(bytes: &[u8], i: usize) -> Option<u8> {
    bytes.get(i).copied()
}

fn skip_until(bytes: &[u8], start: usize, target: u8) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] != target {
        i += 1;
    }
    if i < bytes.len() {
        i + 1
    } else {
        i
    }
}

fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}
fn is_word_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'.'
}

/// Read a `$tag$` opener starting at `i`. Returns `(end_offset, full_tag)`
/// where `full_tag` is `"$tag$"` (matchable verbatim against the closer).
fn read_dollar_tag(bytes: &[u8], i: usize) -> Option<(usize, String)> {
    debug_assert_eq!(bytes[i], b'$');
    let mut j = i + 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'$' {
        return None;
    }
    let tag = std::str::from_utf8(&bytes[i..=j]).ok()?.to_string();
    Some((j + 1, tag))
}
