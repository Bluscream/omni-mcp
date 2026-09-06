//! `${VAR}` expansion, so secrets never have to be written into the config file.
//!
//! The previous `omni-mcp.toml` had a Home Assistant JWT and an API key
//! committed in plaintext. Config values now reference the environment instead:
//!
//! ```toml
//! bearer = "${HOME_ASSISTANT_TOKEN}"
//! url    = "${OMNI_ROUTE_URL:-http://127.0.0.1:20128/api/mcp}"
//! ```

use crate::error::ConfigError;

/// Expands `${VAR}` and `${VAR:-default}` against a lookup function.
///
/// `$$` is an escape for a literal `$`. An unset variable with no default is an
/// error rather than an empty string — silently sending an empty bearer token
/// produces a confusing 401 far from its cause.
pub fn expand(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 1..];

        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
            continue;
        }

        let Some(body) = after.strip_prefix('{') else {
            // A bare `$` not introducing a placeholder is literal text.
            out.push('$');
            rest = after;
            continue;
        };

        let Some(end) = body.find('}') else {
            out.push_str("${");
            rest = body;
            continue;
        };

        let (placeholder, tail) = body.split_at(end);
        rest = &tail[1..];

        let (name, default) = match placeholder.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (placeholder, None),
        };
        let name = name.trim();

        match lookup(name) {
            Some(value) => out.push_str(&value),
            None => match default {
                Some(default) => out.push_str(default),
                None => return Err(ConfigError::MissingEnv { var: name.to_string() }),
            },
        }
    }

    out.push_str(rest);
    Ok(out)
}

/// Expands against the process environment.
pub fn expand_env(input: &str) -> Result<String, ConfigError> {
    expand(input, &|name| std::env::var(name).ok())
}

/// Expands every string value in a parsed TOML document.
///
/// Operating on values rather than raw text means comments, keys and non-string
/// values are left alone — a `${VAR}` written in a comment is documentation,
/// not a lookup.
pub fn expand_toml(value: toml::Value) -> Result<toml::Value, ConfigError> {
    expand_toml_with(value, &|name| std::env::var(name).ok())
}

fn expand_toml_with(
    value: toml::Value,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<toml::Value, ConfigError> {
    Ok(match value {
        toml::Value::String(text) => toml::Value::String(expand(&text, lookup)?),
        toml::Value::Array(items) => toml::Value::Array(
            items
                .into_iter()
                .map(|item| expand_toml_with(item, lookup))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        toml::Value::Table(table) => toml::Value::Table(
            table
                .into_iter()
                .map(|(key, item)| Ok((key, expand_toml_with(item, lookup)?)))
                .collect::<Result<toml::map::Map<_, _>, ConfigError>>()?,
        ),
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        move |name| owned.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn substitutes_a_set_variable() {
        let lookup = env(&[("TOKEN", "s3cret")]);
        assert_eq!(expand("bearer = \"${TOKEN}\"", &lookup).unwrap(), "bearer = \"s3cret\"");
    }

    #[test]
    fn uses_the_default_when_unset() {
        let lookup = env(&[]);
        assert_eq!(expand("${HOST:-127.0.0.1}", &lookup).unwrap(), "127.0.0.1");
    }

    #[test]
    fn prefers_the_variable_over_its_default() {
        let lookup = env(&[("HOST", "0.0.0.0")]);
        assert_eq!(expand("${HOST:-127.0.0.1}", &lookup).unwrap(), "0.0.0.0");
    }

    #[test]
    fn unset_without_default_is_a_named_error() {
        let err = expand("${NOPE}", &env(&[])).unwrap_err();
        assert!(matches!(&err, ConfigError::MissingEnv { var } if var == "NOPE"));
    }

    #[test]
    fn empty_default_is_allowed_and_yields_empty() {
        assert_eq!(expand("a${X:-}b", &env(&[])).unwrap(), "ab");
    }

    #[test]
    fn handles_multiple_and_adjacent_placeholders() {
        let lookup = env(&[("A", "1"), ("B", "2")]);
        assert_eq!(expand("${A}${B}/${A}", &lookup).unwrap(), "12/1");
    }

    #[test]
    fn double_dollar_escapes_a_literal_dollar() {
        assert_eq!(expand("cost is $$5 ${X:-y}", &env(&[])).unwrap(), "cost is $5 y");
    }

    #[test]
    fn lone_dollar_and_unclosed_brace_stay_literal() {
        assert_eq!(expand("100$ off", &env(&[])).unwrap(), "100$ off");
        assert_eq!(expand("${UNCLOSED", &env(&[])).unwrap(), "${UNCLOSED");
    }

    #[test]
    fn value_containing_a_placeholder_is_not_re_expanded() {
        let lookup = env(&[("A", "${B}"), ("B", "boom")]);
        assert_eq!(expand("${A}", &lookup).unwrap(), "${B}");
    }

    #[test]
    fn text_without_placeholders_is_returned_unchanged() {
        assert_eq!(expand("plain text", &env(&[])).unwrap(), "plain text");
    }

    #[test]
    fn toml_string_values_are_expanded_but_comments_and_keys_are_not() {
        // A `${VAR}` in a comment is documentation. Expanding raw file text
        // turned the shipped config's own examples into startup failures.
        let document = r#"
            # see ${OMNI_MCP_TOKEN} in the environment
            bearer = "${TOKEN}"
            port = 8080
            hosts = ["${HOST}", "literal"]

            [nested]
            key = "${TOKEN}"
        "#;

        let parsed: toml::Value = toml::from_str(document).unwrap();
        let lookup = env(&[("TOKEN", "abc"), ("HOST", "example.test")]);
        let expanded = expand_toml_with(parsed, &lookup).unwrap();

        assert_eq!(expanded["bearer"].as_str(), Some("abc"));
        assert_eq!(expanded["port"].as_integer(), Some(8080));
        assert_eq!(expanded["hosts"][0].as_str(), Some("example.test"));
        assert_eq!(expanded["hosts"][1].as_str(), Some("literal"));
        assert_eq!(expanded["nested"]["key"].as_str(), Some("abc"));
    }

    #[test]
    fn an_unset_variable_in_a_value_still_fails_loudly() {
        let parsed: toml::Value = toml::from_str(r#"bearer = "${ABSENT}""#).unwrap();
        let err = expand_toml_with(parsed, &env(&[])).unwrap_err();
        assert!(matches!(&err, ConfigError::MissingEnv { var } if var == "ABSENT"));
    }
}
