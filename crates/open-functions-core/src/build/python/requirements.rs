//! `requirements.open-functions.txt` generation (T025):
//! `contracts/python-function-contract.md`'s "Dependency resolution and artifacts" step 2 --
//! read the source snapshot's `requirements.txt` (if any), and append the
//! configured `functions-framework` declaration only when the user hasn't
//! already declared it themselves (FR-104).

use std::path::{Path, PathBuf};

/// PEP 503 name normalization: lowercase, and any run of `-`/`_`/`.`
/// collapsed to a single `-` (e.g. `Functions_Framework` -> `functions-framework`).
fn normalize(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !last_was_sep {
                out.push('-');
                last_was_sep = true;
            }
        } else {
            out.push(c.to_ascii_lowercase());
            last_was_sep = false;
        }
    }
    out
}

/// Extracts the leading distribution-name token of a PEP 508 requirement
/// line, or `None` if the line is blank, a comment (`#`), or an option line
/// (`-r other.txt`, `--index-url ...`) -- none of which name a package.
fn requirement_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let end = trimmed
        .find(|c: char| c.is_whitespace() || "[]<>=!~;@".contains(c))
        .unwrap_or(trimmed.len());
    if end == 0 {
        None
    } else {
        Some(&trimmed[..end])
    }
}

/// Whether `content` (an existing `requirements.txt`) already declares
/// `functions-framework` on some requirement line, under any version
/// constraint or none.
fn declares_functions_framework(content: &str) -> bool {
    content
        .lines()
        .filter_map(requirement_name)
        .any(|name| normalize(name) == "functions-framework")
}

/// Builds the content of `requirements.open-functions.txt` from the user's
/// own `requirements.txt` (`existing`, `None` if the file doesn't exist) and
/// the configured `functions_framework_spec`. Every user line -- including
/// comments and option lines -- is preserved verbatim; `functions_framework_spec`
/// is appended as a new final line only when no line already declares
/// `functions-framework` (FR-104: an explicit user declaration, of any
/// version, is never rewritten). When `existing` is `None`, the result is
/// just the single appended line.
pub fn build_requirements_content(
    existing: Option<&str>,
    functions_framework_spec: &str,
) -> String {
    let mut out = existing.unwrap_or("").to_string();
    let already_declared = existing.is_some_and(declares_functions_framework);
    if !already_declared {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(functions_framework_spec);
        out.push('\n');
    }
    out
}

/// Reads `<artifact_dir>/src/requirements.txt` (if present) and writes
/// `<artifact_dir>/requirements.open-functions.txt`, returning its path.
pub async fn resolve_requirements(
    artifact_dir: &Path,
    functions_framework_spec: &str,
) -> std::io::Result<PathBuf> {
    let requirements_path = artifact_dir.join("src").join("requirements.txt");
    let existing = match tokio::fs::read_to_string(&requirements_path).await {
        Ok(content) => Some(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let content = build_requirements_content(existing.as_deref(), functions_framework_spec);
    let out_path = artifact_dir.join("requirements.open-functions.txt");
    tokio::fs::write(&out_path, content).await?;
    Ok(out_path)
}
