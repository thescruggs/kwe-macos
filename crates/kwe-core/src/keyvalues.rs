// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeMap;

use thiserror::Error;

const MAX_VDF_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvValue {
    String(String),
    Object(BTreeMap<String, KvValue>),
}

impl KvValue {
    pub fn object(&self) -> Option<&BTreeMap<String, KvValue>> {
        match self {
            Self::Object(value) => Some(value),
            Self::String(_) => None,
        }
    }

    pub fn string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Object(_) => None,
        }
    }

    pub fn get_case_insensitive(&self, key: &str) -> Option<&KvValue> {
        self.object()?
            .iter()
            .find_map(|(candidate, value)| candidate.eq_ignore_ascii_case(key).then_some(value))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KvError {
    #[error("VDF input exceeds the {MAX_VDF_BYTES} byte safety limit")]
    TooLarge,
    #[error("unterminated quoted string at byte {0}")]
    UnterminatedString(usize),
    #[error("invalid escape at byte {0}")]
    InvalidEscape(usize),
    #[error("expected a value for key '{key}' at byte {offset}")]
    MissingValue { key: String, offset: usize },
    #[error("unexpected closing brace at byte {0}")]
    UnexpectedClose(usize),
    #[error("unclosed object")]
    UnclosedObject,
    #[error("VDF nesting exceeds {MAX_DEPTH} levels")]
    TooDeep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Text(String),
    Open,
    Close,
}

#[derive(Debug, Clone)]
struct SpannedToken {
    token: Token,
    offset: usize,
}

pub fn parse_key_values(input: &str) -> Result<KvValue, KvError> {
    if input.len() > MAX_VDF_BYTES {
        return Err(KvError::TooLarge);
    }
    let tokens = tokenize(input)?;
    let mut cursor = 0;
    let root = parse_object(&tokens, &mut cursor, 0, false)?;
    if let Some(token) = tokens.get(cursor) {
        return match token.token {
            Token::Close => Err(KvError::UnexpectedClose(token.offset)),
            _ => Err(KvError::MissingValue {
                key: "<root>".into(),
                offset: token.offset,
            }),
        };
    }
    Ok(KvValue::Object(root))
}

fn parse_object(
    tokens: &[SpannedToken],
    cursor: &mut usize,
    depth: usize,
    expect_close: bool,
) -> Result<BTreeMap<String, KvValue>, KvError> {
    if depth > MAX_DEPTH {
        return Err(KvError::TooDeep);
    }
    let mut result = BTreeMap::new();
    while let Some(current) = tokens.get(*cursor) {
        match &current.token {
            Token::Close if expect_close => {
                *cursor += 1;
                return Ok(result);
            }
            Token::Close => return Err(KvError::UnexpectedClose(current.offset)),
            Token::Open => {
                return Err(KvError::MissingValue {
                    key: "<object>".into(),
                    offset: current.offset,
                });
            }
            Token::Text(key) => {
                let key = key.clone();
                *cursor += 1;
                let Some(value_token) = tokens.get(*cursor) else {
                    return Err(KvError::MissingValue {
                        key,
                        offset: current.offset,
                    });
                };
                let value = match &value_token.token {
                    Token::Text(value) => {
                        *cursor += 1;
                        KvValue::String(value.clone())
                    }
                    Token::Open => {
                        *cursor += 1;
                        KvValue::Object(parse_object(tokens, cursor, depth + 1, true)?)
                    }
                    Token::Close => {
                        return Err(KvError::MissingValue {
                            key,
                            offset: value_token.offset,
                        });
                    }
                };
                result.insert(key, value);
            }
        }
    }
    if expect_close {
        Err(KvError::UnclosedObject)
    } else {
        Ok(result)
    }
}

fn tokenize(input: &str) -> Result<Vec<SpannedToken>, KvError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b' ' | b'\t' | b'\r' | b'\n' => offset += 1,
            b'/' if bytes.get(offset + 1) == Some(&b'/') => {
                offset += 2;
                while offset < bytes.len() && bytes[offset] != b'\n' {
                    offset += 1;
                }
            }
            b'{' => {
                tokens.push(SpannedToken {
                    token: Token::Open,
                    offset,
                });
                offset += 1;
            }
            b'}' => {
                tokens.push(SpannedToken {
                    token: Token::Close,
                    offset,
                });
                offset += 1;
            }
            b'"' => {
                let start = offset;
                offset += 1;
                let mut value = String::new();
                let mut terminated = false;
                while offset < bytes.len() {
                    match bytes[offset] {
                        b'"' => {
                            offset += 1;
                            terminated = true;
                            break;
                        }
                        b'\\' => {
                            offset += 1;
                            let Some(escaped) = bytes.get(offset) else {
                                return Err(KvError::InvalidEscape(offset));
                            };
                            match escaped {
                                b'"' => value.push('"'),
                                b'\\' => value.push('\\'),
                                b'n' => value.push('\n'),
                                b't' => value.push('\t'),
                                other => {
                                    value.push('\\');
                                    value.push(*other as char);
                                }
                            }
                            offset += 1;
                        }
                        byte => {
                            value.push(byte as char);
                            offset += 1;
                        }
                    }
                }
                if !terminated {
                    return Err(KvError::UnterminatedString(start));
                }
                tokens.push(SpannedToken {
                    token: Token::Text(value),
                    offset: start,
                });
            }
            _ => {
                let start = offset;
                while offset < bytes.len()
                    && !bytes[offset].is_ascii_whitespace()
                    && !matches!(bytes[offset], b'{' | b'}')
                {
                    offset += 1;
                }
                tokens.push(SpannedToken {
                    token: Token::Text(input[start..offset].to_string()),
                    offset: start,
                });
            }
        }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_library_folder() {
        let root = parse_key_values(r#""libraryfolders" { "0" { "path" "/games" } }"#).unwrap();
        assert_eq!(
            root.get_case_insensitive("libraryfolders")
                .and_then(|v| v.object())
                .and_then(|v| v.get("0"))
                .and_then(|v| v.get_case_insensitive("path"))
                .and_then(KvValue::string),
            Some("/games")
        );
    }

    #[test]
    fn rejects_excessive_nesting() {
        let input = format!(
            "{}{}",
            "\"x\" {".repeat(MAX_DEPTH + 2),
            "}".repeat(MAX_DEPTH + 2)
        );
        assert_eq!(parse_key_values(&input), Err(KvError::TooDeep));
    }

    #[test]
    fn ignores_line_comments() {
        let root = parse_key_values("// hi\n\"one\" \"two\"").unwrap();
        assert_eq!(
            root.get_case_insensitive("one").and_then(KvValue::string),
            Some("two")
        );
    }
}
