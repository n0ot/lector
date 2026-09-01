//! Coroutine invocation used by event-loop-backed Lua APIs.
//!
//! Lua code sees ordinary blocking-looking functions. Each such function
//! yields a tagged request; the terminal event loop resumes the coroutine only
//! when the requested evidence exists.

use mlua::{Function, Lua, Result, Thread};
use std::rc::Rc;

pub(crate) const REQUEST_FIELD: &str = "__lector_request";

pub(crate) struct Invocation {
    pub(crate) lua: Rc<Lua>,
    pub(crate) thread: Thread,
}

impl Invocation {
    pub(crate) fn new(lua: Rc<Lua>, function: Function) -> Result<Self> {
        let thread = lua.create_thread(function)?;
        Ok(Self { lua, thread })
    }
}

/// Parse Neovim-style key notation. Text outside angle brackets is literal;
/// `<lt>` is the unambiguous way to insert a literal opening bracket.
pub(crate) fn parse_keys(spec: &str) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(spec.len());
    let mut rest = spec;
    while let Some(start) = rest.find('<') {
        output.extend_from_slice(&rest.as_bytes()[..start]);
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else {
            anyhow::bail!("unterminated key token in {spec:?}");
        };
        let token = &rest[..end];
        rest = &rest[end + 1..];
        match token.to_ascii_lowercase().as_str() {
            "space" => output.push(b' '),
            "lt" => output.push(b'<'),
            "esc" => output.push(0x1b),
            "cr" | "enter" => output.push(b'\r'),
            "tab" => output.push(b'\t'),
            "bs" | "backspace" => output.push(0x7f),
            lower if lower.starts_with("c-") => {
                let value = token[2..].chars().collect::<Vec<_>>();
                if value.len() != 1 || !value[0].is_ascii() {
                    anyhow::bail!("invalid control key token <{token}>");
                }
                let byte = value[0] as u8;
                let control = match byte {
                    b'?' => 0x7f,
                    b'@'..=b'_' => byte & 0x1f,
                    b'a'..=b'z' => byte.to_ascii_uppercase() & 0x1f,
                    _ => anyhow::bail!("invalid control key token <{token}>"),
                };
                output.push(control);
            }
            _ => anyhow::bail!("unknown key token <{token}>"),
        }
    }
    output.extend_from_slice(rest.as_bytes());
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::parse_keys;

    #[test]
    fn key_notation_keeps_ordinary_text_literal() {
        assert_eq!(
            parse_keys("prefix<Space><C-f><lt>suffix").unwrap(),
            b"prefix \x06<suffix"
        );
    }

    #[test]
    fn malformed_and_unknown_tokens_are_errors() {
        assert!(parse_keys("<Space").is_err());
        assert!(parse_keys("<mystery>").is_err());
        assert!(parse_keys("<C-long>").is_err());
    }
}
