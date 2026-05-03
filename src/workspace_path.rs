//! Workspace roots arrive as POSIX paths **or** as `file:` URIs (VS Code
//! `URI.toString(true)` sends `file:///…`). Passing the URI string straight
//! into `PathBuf::from(...)` yields a bogus path segment and tools cannot read.

use std::path::PathBuf;

/// Turn a trimmed JSON `workspace_root` into an absolute filesystem path string.
///
/// Accepts ordinary paths (`/tmp/project`) and `file:///…` /
/// `file://localhost/…` URIs emitted by editors.
pub fn normalize_workspace_wire(raw: &str) -> Option<String> {
    parse_workspace_root(raw).map(|p| p.display().to_string())
}

/// Parse workspace root suitable for filesystem operations.
///
/// [`None`] when `raw` is empty or whitespace.
pub fn parse_workspace_root(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path_part = trimmed
        .strip_prefix("file://localhost")
        .or_else(|| trimmed.strip_prefix("file://LOCALHOST"))
        .or_else(|| trimmed.strip_prefix("file://"))
        .unwrap_or(trimmed);
    Some(PathBuf::from(path_part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn parses_file_triple_slash_uri() {
        let p = parse_workspace_root("file:///tmp/dm_workspace").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/dm_workspace"));
    }

    #[test]
    fn passthrough_plain_path() {
        let p = parse_workspace_root("./rel").unwrap();
        assert!(p.ends_with("rel"));
    }

    #[test]
    #[cfg(unix)]
    fn normalizes_wire_to_fs_string() {
        assert_eq!(
            normalize_workspace_wire("file:///opt/proj").unwrap(),
            "/opt/proj"
        );
    }

    #[test]
    #[cfg(unix)]
    fn file_localhost_form() {
        let p =
            parse_workspace_root("file://localhost/tmp/x").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn empty_is_none() {
        assert!(parse_workspace_root("  ").is_none());
        assert!(normalize_workspace_wire("").is_none());
    }
}
