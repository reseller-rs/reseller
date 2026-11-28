//! Environment references in a settings file.
//!
//! String values (never keys) may reference environment variables, so the
//! same file works in every deployment and keeps credentials out of it:
//!
//! - `${NAME}` is the value of `NAME`. An unset variable is an error.
//! - `${NAME:-fallback}` is the value of `NAME`, or `fallback` when `NAME`
//!   is unset or empty (as in POSIX shells). The fallback is literal text.
//! - `$${` is a literal `${`. Any other `$` is literal.
//!
//! `NAME` is a letter or `_` followed by letters, digits, or `_`.
use anyhow::{Context, bail};

/// Expands every reference in the string values of `value`, recursively.
pub(super) fn expand_value(
    value: &mut toml::Value,
    var: &impl Fn(&str) -> Option<String>,
    path: &str,
) -> anyhow::Result<()> {
    match value {
        toml::Value::String(text) => {
            *text = expand(text, var).with_context(|| format!("{path}: invalid reference"))?;
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                expand_value(item, var, &format!("{path}[{index}]"))?;
            }
        }
        toml::Value::Table(table) => expand_table(table, var, path)?,
        _ => {}
    }
    Ok(())
}

/// [`expand_value`] for every value of a table.
pub(super) fn expand_table(
    table: &mut toml::Table,
    var: &impl Fn(&str) -> Option<String>,
    path: &str,
) -> anyhow::Result<()> {
    for (key, value) in table.iter_mut() {
        let path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        expand_value(value, var, &path)?;
    }
    Ok(())
}

/// Whether `text` holds at least one `${...}` reference.
pub(super) fn references(text: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        if !rest[..at].ends_with('$') {
            return true;
        }
        rest = &rest[at + 2..];
    }
    false
}

/// Expands the references of one string.
fn expand(text: &str, var: &impl Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let end = after.find('}').context("a ${ reference has no closing }")?;
            let (name, fallback) = match after[..end].split_once(":-") {
                Some((name, fallback)) => (name, Some(fallback)),
                None => (&after[..end], None),
            };
            if !valid_name(name) {
                bail!("{name:?} is not a variable name");
            }
            let value = var(name);
            match (value, fallback) {
                (Some(v), Some(f)) if v.is_empty() => out.push_str(f),
                (Some(v), _) => out.push_str(&v),
                (None, Some(f)) => out.push_str(f),
                (None, None) => bail!("environment variable {name} is not set"),
            }
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Escapes every `${` in the string values of `value` as `$${`, so a written
/// settings file reads back unchanged.
pub(super) fn escape_value(value: &mut toml::Value) {
    match value {
        toml::Value::String(text) => *text = text.replace("${", "$${"),
        toml::Value::Array(items) => items.iter_mut().for_each(escape_value),
        toml::Value::Table(table) => table.iter_mut().for_each(|(_, v)| escape_value(v)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "KEY" => Some("sk-1".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn references_expand_with_shell_fallbacks() {
        let x = |t: &str| expand(t, &env);
        assert_eq!(x("Bearer ${KEY}").unwrap(), "Bearer sk-1");
        assert_eq!(x("${MISSING:-}").unwrap(), "");
        assert_eq!(x("${EMPTY:-d}").unwrap(), "d");
        assert_eq!(x("${KEY:-d}").unwrap(), "sk-1");
        assert_eq!(x("a/${MISSING:-x/y}/b").unwrap(), "a/x/y/b");
        assert_eq!(x("$5 and $$ and $${KEY}").unwrap(), "$5 and $$ and ${KEY}");
        assert!(x("${MISSING}").unwrap_err().to_string().contains("MISSING"));
        assert!(x("${KEY").is_err());
        assert!(x("${1X}").is_err());
        assert!(x("${}").is_err());
    }

    #[test]
    fn escaping_round_trips_and_is_not_a_reference() {
        for text in ["${KEY}", "$${KEY}", "a$b${", "plain", "$$${x}"] {
            let mut value = toml::Value::String(text.into());
            escape_value(&mut value);
            let escaped = value.as_str().unwrap().to_owned();
            assert!(!references(&escaped), "{escaped}");
            assert_eq!(expand(&escaped, &env).unwrap(), text);
        }
        assert!(references("x ${KEY}"));
        assert!(!references("x $${KEY}"));
    }

    #[test]
    fn nested_values_expand_and_errors_name_the_setting() {
        let mut table: toml::Table =
            toml::from_str("[[providers]]\napi_key = \"${KEY}\"\nheaders = { X = \"${NOPE}\" }")
                .unwrap();
        let error = expand_table(&mut table, &env, "").unwrap_err();
        assert!(format!("{error:#}").contains("providers[0].headers.X"));
        let mut table: toml::Table =
            toml::from_str("list = [\"${KEY}\"]\n[t]\nv = \"${MISSING:-z}\"").unwrap();
        expand_table(&mut table, &env, "").unwrap();
        assert_eq!(table["list"][0].as_str(), Some("sk-1"));
        assert_eq!(table["t"]["v"].as_str(), Some("z"));
    }
}
