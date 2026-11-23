use crate::engine::QueryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    LParen,
    RParen,
    Colon,
    Plus,
    Minus,
    Not,
    And,
    Or,
    To,
    RangeInclusive,
    RangeExclusive,
    RangeEndInclusive,
    RangeEndExclusive,
    Tilde,
    Boost,
    Phrase,
    Regex,
    Term,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub value: String,
    pub raw: String,
    /// 1-based byte position (matches Lua error messages).
    pub position: usize,
}

fn special_kind(b: u8) -> Option<TokenKind> {
    match b {
        b'(' => Some(TokenKind::LParen),
        b')' => Some(TokenKind::RParen),
        b':' => Some(TokenKind::Colon),
        b'+' => Some(TokenKind::Plus),
        b'-' => Some(TokenKind::Minus),
        b'!' => Some(TokenKind::Not),
        b'[' => Some(TokenKind::RangeInclusive),
        b'{' => Some(TokenKind::RangeExclusive),
        b']' => Some(TokenKind::RangeEndInclusive),
        b'}' => Some(TokenKind::RangeEndExclusive),
        b'~' => Some(TokenKind::Tilde),
        b'^' => Some(TokenKind::Boost),
        _ => None,
    }
}

fn failure(msg: &str, pos_1based: usize) -> QueryError {
    QueryError::InvalidSyntax(format!("{msg} at character {pos_1based}."))
}

/// Ported 1:1 from `resources/lib/query/lexer.lua`.
pub fn tokenize(query: &str) -> Result<Vec<Token>, QueryError> {
    let bytes = query.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
            tokens.push(Token {
                kind: TokenKind::And,
                value: "AND".to_string(),
                raw: "&&".to_string(),
                position: i + 1,
            });
            i += 2;
            continue;
        }
        if b == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            tokens.push(Token {
                kind: TokenKind::Or,
                value: "OR".to_string(),
                raw: "||".to_string(),
                position: i + 1,
            });
            i += 2;
            continue;
        }
        if b == b'&' || b == b'|' {
            return Err(failure("RuneQL expected && or ||", i + 1));
        }
        if let Some(kind) = special_kind(b) {
            let s = (b as char).to_string();
            tokens.push(Token {
                kind,
                value: s.clone(),
                raw: s,
                position: i + 1,
            });
            i += 1;
            continue;
        }

        if b == b'"' || b == b'/' {
            let closing = b;
            let is_regex = closing == b'/';
            let start = i;
            i += 1;
            let mut val = Vec::new();
            let mut closed = false;
            while i < bytes.len() {
                let c = bytes[i];
                if c == b'\\' {
                    if i + 1 >= bytes.len() {
                        let msg = if is_regex {
                            "RuneQL found an incomplete regular expression escape"
                        } else {
                            "RuneQL found an incomplete escape"
                        };
                        return Err(failure(msg, i + 1));
                    }
                    if is_regex {
                        val.push(b'\\');
                    }
                    val.push(bytes[i + 1]);
                    i += 2;
                } else if c == closing {
                    let raw = query[start..=i].to_string();
                    tokens.push(Token {
                        kind: if is_regex {
                            TokenKind::Regex
                        } else {
                            TokenKind::Phrase
                        },
                        value: String::from_utf8_lossy(&val).into_owned(),
                        raw,
                        position: start + 1,
                    });
                    i += 1;
                    closed = true;
                    break;
                } else {
                    val.push(c);
                    i += 1;
                }
            }
            if !closed {
                let msg = if is_regex {
                    "RuneQL found an unterminated regular expression"
                } else {
                    "RuneQL found an unterminated quoted phrase"
                };
                return Err(failure(msg, start + 1));
            }
            continue;
        }

        // Read a term token.
        let start = i;
        let mut val = Vec::new();
        let mut any_escaped = false;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_whitespace() || special_kind(c).is_some() || c == b'"' || c == b'/' {
                break;
            }
            if c == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                break;
            }
            if c == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                break;
            }
            if c == b'\\' {
                if i + 1 >= bytes.len() {
                    return Err(failure("RuneQL found an incomplete escape", i + 1));
                }
                val.push(bytes[i + 1]);
                any_escaped = true;
                i += 2;
            } else {
                val.push(c);
                i += 1;
            }
        }
        if val.is_empty() {
            return Err(failure("RuneQL could not read this query token", start + 1));
        }
        let decoded = String::from_utf8_lossy(&val).into_owned();
        let kind = if !any_escaped {
            match decoded.as_str() {
                "AND" => TokenKind::And,
                "OR" => TokenKind::Or,
                "NOT" => TokenKind::Not,
                "TO" => TokenKind::To,
                _ => TokenKind::Term,
            }
        } else {
            TokenKind::Term
        };
        tokens.push(Token {
            kind,
            value: decoded,
            raw: query[start..i].to_string(),
            position: start + 1,
        });
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        value: String::new(),
        raw: String::new(),
        position: bytes.len() + 1,
    });
    Ok(tokens)
}
