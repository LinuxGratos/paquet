//! `KEY=value` configuration files. They are parsed, never executed.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Reads the settings of a configuration file, in order. Comments (`#`),
/// blank lines and an `export ` prefix are allowed; a value may be quoted
/// with `"` or `'`. Invalid lines are reported and skipped.
pub fn read(path: &Path) -> Result<Vec<(String, String)>> {
    let content = fs::read(path).with_context(|| format!("unable to read {}", path.display()))?;
    Ok(parse(&String::from_utf8_lossy(&content), path))
}

pub fn parse(content: &str, path: &Path) -> Vec<(String, String)> {
    let mut settings = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = parse_line(trimmed) else {
            eprintln!("Warning: ignoring invalid line in {}: {line}", path.display());
            continue;
        };
        settings.push((key.to_string(), value.to_string()));
    }
    settings
}

fn parse_line(line: &str) -> Option<(&str, &str)> {
    let line = match line.strip_prefix("export") {
        Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
        _ => line,
    };
    let (key, value) = line.split_once('=')?;
    let mut chars = key.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let value = value.trim_end();
    let value = if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    };
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings() {
        let content = "# comment\n\nMIRROR=\"https://example.org/pq\"\n  export BRANCH='testing'  \nbad line\nX=a b\n";
        let settings = parse(content, Path::new("test.conf"));
        assert_eq!(
            settings,
            vec![
                ("MIRROR".into(), "https://example.org/pq".into()),
                ("BRANCH".into(), "testing".into()),
                ("X".into(), "a b".into()),
            ]
        );
    }
}
