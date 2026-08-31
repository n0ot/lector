use mlua::{Lua, Result, String as LuaString, Table, Value};
use std::{cmp::Ordering, collections::HashSet, os::raw::c_void};

const INDENT: &str = "  ";
const MAX_DEPTH: usize = 20;

pub fn install(lua: &Lua) -> Result<()> {
    let inspect = lua.create_function(|_, value: Value| format(value))?;
    let lector: Table = lua.globals().get("lector")?;
    lector.raw_set("inspect", inspect)
}

fn format(value: Value) -> Result<String> {
    Inspector::default().format_value(value, 0)
}

#[derive(Default)]
struct Inspector {
    active_tables: HashSet<*const c_void>,
}

impl Inspector {
    fn format_value(&mut self, value: Value, depth: usize) -> Result<String> {
        match value {
            Value::Nil => Ok("nil".to_owned()),
            Value::Boolean(value) => Ok(value.to_string()),
            Value::Integer(value) => Ok(value.to_string()),
            Value::Number(value) => Ok(value.to_string()),
            Value::String(value) => Ok(quote_string(&value)),
            Value::Table(table) => self.format_table(table, depth),
            Value::Function(_) => Ok("<function>".to_owned()),
            Value::Thread(_) => Ok("<thread>".to_owned()),
            Value::UserData(_) => Ok("<userdata>".to_owned()),
            Value::LightUserData(_) => Ok("<lightuserdata>".to_owned()),
            Value::Error(error) => Ok(format!("<error: {error}>")),
            _ => Ok("<value>".to_owned()),
        }
    }

    fn format_table(&mut self, table: Table, depth: usize) -> Result<String> {
        if depth >= MAX_DEPTH {
            return Ok("<max depth>".to_owned());
        }

        let pointer = table.to_pointer();
        if !self.active_tables.insert(pointer) {
            return Ok("<cycle>".to_owned());
        }

        let result = self.format_table_contents(&table, depth);
        self.active_tables.remove(&pointer);
        result
    }

    fn format_table_contents(&mut self, table: &Table, depth: usize) -> Result<String> {
        let mut array_len = 0;
        loop {
            let next: Value = table.raw_get(array_len + 1)?;
            if matches!(next, Value::Nil) {
                break;
            }
            array_len += 1;
        }

        let mut entries = Vec::new();
        for pair in table.clone().pairs::<Value, Value>() {
            let (key, value) = pair?;
            if !is_array_key(&key, array_len) {
                entries.push((key, value));
            }
        }
        entries.sort_by(|(left, _), (right, _)| compare_keys(left, right));

        if array_len == 0 && entries.is_empty() {
            return Ok("{}".to_owned());
        }

        let item_indent = INDENT.repeat(depth + 1);
        let mut items = Vec::with_capacity(array_len + entries.len());
        for index in 1..=array_len {
            let value = table.raw_get(index)?;
            items.push(format!(
                "{item_indent}{}",
                self.format_value(value, depth + 1)?
            ));
        }
        for (key, value) in entries {
            let key = self.format_key(key, depth + 1)?;
            let value = self.format_value(value, depth + 1)?;
            items.push(format!("{item_indent}{key} = {value}"));
        }

        Ok(format!(
            "{{\n{},\n{}}}",
            items.join(",\n"),
            INDENT.repeat(depth)
        ))
    }

    fn format_key(&mut self, key: Value, depth: usize) -> Result<String> {
        if let Value::String(value) = &key
            && let Ok(value) = value.to_str()
            && is_identifier(&value)
        {
            return Ok(value.to_owned());
        }
        Ok(format!("[{}]", self.format_value(key, depth)?))
    }
}

fn is_array_key(key: &Value, array_len: usize) -> bool {
    match key {
        Value::Integer(value) => (1..=array_len as i64).contains(value),
        Value::Number(value) => value.fract() == 0.0 && *value >= 1.0 && *value <= array_len as f64,
        _ => false,
    }
}

fn compare_keys(left: &Value, right: &Value) -> Ordering {
    let rank = |value: &Value| match value {
        Value::Integer(_) | Value::Number(_) => 0,
        Value::String(_) => 1,
        Value::Boolean(_) => 2,
        Value::Table(_) => 3,
        Value::Function(_) => 4,
        Value::Thread(_) => 5,
        Value::UserData(_) | Value::LightUserData(_) => 6,
        _ => 7,
    };

    match rank(left).cmp(&rank(right)) {
        Ordering::Equal => match (left, right) {
            (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
            (Value::Integer(left), Value::Number(right)) => (*left as f64).total_cmp(right),
            (Value::Number(left), Value::Integer(right)) => left.total_cmp(&(*right as f64)),
            (Value::Number(left), Value::Number(right)) => left.total_cmp(right),
            (Value::String(left), Value::String(right)) => left.as_bytes().cmp(&right.as_bytes()),
            (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
            _ => left.to_pointer().cmp(&right.to_pointer()),
        },
        ordering => ordering,
    }
}

fn is_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        return false;
    }
    !matches!(
        value,
        "and"
            | "break"
            | "do"
            | "else"
            | "elseif"
            | "end"
            | "false"
            | "for"
            | "function"
            | "goto"
            | "if"
            | "in"
            | "local"
            | "nil"
            | "not"
            | "or"
            | "repeat"
            | "return"
            | "then"
            | "true"
            | "until"
            | "while"
    )
}

fn quote_string(value: &LuaString) -> String {
    let bytes = value.as_bytes();
    let Ok(value) = std::str::from_utf8(&bytes) else {
        return quote_bytes(&bytes);
    };

    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                for byte in character.to_string().bytes() {
                    quoted.push_str(&format!("\\{byte:03}"));
                }
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn quote_bytes(bytes: &[u8]) -> String {
    let mut quoted = String::from("\"");
    for byte in bytes {
        match byte {
            b'\\' => quoted.push_str("\\\\"),
            b'"' => quoted.push_str("\\\""),
            b'\n' => quoted.push_str("\\n"),
            b'\r' => quoted.push_str("\\r"),
            b'\t' => quoted.push_str("\\t"),
            0x20..=0x7e => quoted.push(*byte as char),
            byte => quoted.push_str(&format!("\\{byte:03}")),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::format;
    use mlua::{Lua, Value};

    #[test]
    fn formats_array_and_named_entries_in_stable_order() {
        let lua = Lua::new();
        let value: Value = lua
            .load(r#"{ "first", z = false, nested = { answer = 42 }, a = "line\ntext" }"#)
            .eval()
            .unwrap();

        assert_eq!(
            format(value).unwrap(),
            concat!(
                "{\n",
                "  \"first\",\n",
                "  a = \"line\\ntext\",\n",
                "  nested = {\n",
                "    answer = 42,\n",
                "  },\n",
                "  z = false,\n",
                "}"
            )
        );
    }

    #[test]
    fn marks_only_active_cycles_and_expands_shared_tables() {
        let lua = Lua::new();
        let value: Value = lua
            .load(
                r#"
                    local shared = { value = 1 }
                    local root = { left = shared, right = shared }
                    root.self = root
                    return root
                "#,
            )
            .eval()
            .unwrap();

        let formatted = format(value).unwrap();
        assert_eq!(formatted.matches("value = 1").count(), 2);
        assert!(formatted.contains("self = <cycle>"));
    }

    #[test]
    fn caps_deep_acyclic_tables() {
        let lua = Lua::new();
        let value: Value = lua
            .load(
                r#"
                    local root = {}
                    local current = root
                    for _ = 1, 25 do
                        current.child = {}
                        current = current.child
                    end
                    return root
                "#,
            )
            .eval()
            .unwrap();

        assert!(format(value).unwrap().contains("child = <max depth>"));
    }

    #[test]
    fn quotes_non_identifier_keys_and_binary_strings() {
        let lua = Lua::new();
        let value: Value = lua
            .load(r#"{ ["not a name"] = "a\0\255" }"#)
            .eval()
            .unwrap();

        assert_eq!(
            format(value).unwrap(),
            "{\n  [\"not a name\"] = \"a\\000\\255\",\n}"
        );
    }
}
