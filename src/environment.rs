use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct DotenvReport {
    pub path: PathBuf,
    pub loaded: usize,
    pub preserved: usize,
}

pub fn load(config_path: Option<&Path>) -> Result<Option<DotenvReport>> {
    let explicit = env::var_os("INFERNAUT_ENV_FILE").map(PathBuf::from);
    let path = explicit
        .clone()
        .unwrap_or_else(|| default_path(config_path));
    if !path.exists() {
        if explicit.is_some() {
            bail!(
                "INFERNAUT_ENV_FILE points to missing file '{}'",
                path.display()
            );
        }
        return Ok(None);
    }

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read environment file {}", path.display()))?;
    let entries = parse(&contents)
        .with_context(|| format!("failed to parse environment file {}", path.display()))?;
    let mut loaded = 0;
    let mut preserved = 0;
    for (key, value) in entries {
        if env::var_os(&key).is_some() {
            preserved += 1;
            continue;
        }
        // SAFETY: Infernaut calls this loader synchronously before creating the
        // Tokio runtime or any application threads. Existing process variables
        // are never mutated, and the environment is read-only after startup.
        unsafe {
            env::set_var(key, value);
        }
        loaded += 1;
    }
    Ok(Some(DotenvReport {
        path,
        loaded,
        preserved,
    }))
}

fn default_path(config_path: Option<&Path>) -> PathBuf {
    let directory = config_path
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    directory.join(".env")
}

fn parse(contents: &str) -> Result<Vec<(String, String)>> {
    contents
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match parse_line(line, index + 1) {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn parse_line(line: &str, line_number: usize) -> Result<Option<(String, String)>> {
    let line = line.trim().trim_start_matches('\u{feff}').trim_start();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let Some((key, raw_value)) = line.split_once('=') else {
        bail!("line {line_number} must use KEY=VALUE syntax");
    };
    let key = key.trim();
    if !valid_key(key) {
        bail!("line {line_number} has invalid environment name '{key}'");
    }
    let value = parse_value(raw_value.trim_start(), line_number)?;
    Ok(Some((key.to_owned(), value)))
}

fn valid_key(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn parse_value(value: &str, line_number: usize) -> Result<String> {
    if let Some(quoted) = value.strip_prefix('\'') {
        let Some(end) = quoted.find('\'') else {
            bail!("line {line_number} has an unterminated single-quoted value");
        };
        validate_trailing(&quoted[end + 1..], line_number)?;
        return Ok(quoted[..end].to_owned());
    }
    if let Some(quoted) = value.strip_prefix('"') {
        return parse_double_quoted(quoted, line_number);
    }

    let comment = value
        .char_indices()
        .find(|(index, character)| {
            *character == '#'
                && *index > 0
                && value[..*index]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
        })
        .map(|(index, _)| index);
    Ok(value[..comment.unwrap_or(value.len())]
        .trim_end()
        .to_owned())
}

fn parse_double_quoted(value: &str, line_number: usize) -> Result<String> {
    let mut parsed = String::new();
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            parsed.push(match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                other => other,
            });
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => {
                validate_trailing(&value[index + character.len_utf8()..], line_number)?;
                return Ok(parsed);
            }
            other => parsed.push(other),
        }
    }
    bail!("line {line_number} has an unterminated double-quoted value")
}

fn validate_trailing(trailing: &str, line_number: usize) -> Result<()> {
    let trailing = trailing.trim();
    if trailing.is_empty() || trailing.starts_with('#') {
        Ok(())
    } else {
        bail!("line {line_number} has unexpected text after a quoted value")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_dotenv_syntax_without_shell_evaluation() {
        let values = parse(
            "# credentials\n\
             export PLAIN=value\n\
             EMPTY=\n\
             SINGLE='literal # value'\n\
             DOUBLE=\"line\\nvalue\" # note\n\
             HASH=token#fragment\n\
             COMMENTED=value # note\n",
        )
        .unwrap();
        assert_eq!(
            values,
            [
                ("PLAIN".to_owned(), "value".to_owned()),
                ("EMPTY".to_owned(), String::new()),
                ("SINGLE".to_owned(), "literal # value".to_owned()),
                ("DOUBLE".to_owned(), "line\nvalue".to_owned()),
                ("HASH".to_owned(), "token#fragment".to_owned()),
                ("COMMENTED".to_owned(), "value".to_owned()),
            ]
        );
    }

    #[test]
    fn rejects_invalid_names_and_malformed_quotes() {
        assert!(parse("BAD-NAME=value").is_err());
        assert!(parse("GOOD='missing").is_err());
        assert!(parse("GOOD=\"missing").is_err());
        assert!(parse("GOOD=\"value\" trailing").is_err());
    }

    #[test]
    fn dotenv_defaults_next_to_the_configuration() {
        assert_eq!(
            default_path(Some(Path::new("/srv/infernaut/infernaut.toml"))),
            PathBuf::from("/srv/infernaut/.env")
        );
        assert_eq!(
            default_path(Some(Path::new("infernaut.toml"))),
            PathBuf::from("./.env")
        );
    }
}
