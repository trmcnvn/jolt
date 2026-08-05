//! Classification of Markdown link destinations.
//!
//! Markdown itself does not distinguish a web URL from a local path. Keeping
//! that distinction here prevents platform URL openers from receiving bare
//! filesystem paths (which macOS rejects as scheme-less URLs) and preserves
//! source locations for a future editor-specific opener.

use std::path::{Path, PathBuf};

use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Web(String),
    File {
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
    },
}

impl LinkTarget {
    /// Classify a Markdown destination. Relative file links are only treated
    /// as local when a chat working directory is available; resolving them
    /// against Jolt's process directory would make the same chat behave
    /// differently depending on how the app was launched.
    pub fn parse(raw: &str, cwd: Option<&Path>) -> Self {
        let raw = raw.trim();

        if let Ok(url) = Url::parse(raw) {
            if url.scheme() != "file" {
                return Self::Web(raw.to_owned());
            }
            if let Ok(path) = url.to_file_path() {
                let (path, line, column) = split_path_location(path);
                return Self::File {
                    path,
                    line: line.or_else(|| fragment_line(url.fragment())),
                    column,
                };
            }
            return Self::Web(raw.to_owned());
        }

        let (path_text, fragment_line) = split_fragment_location(raw);
        let (path, line, column) = split_path_location(PathBuf::from(path_text));
        if path.is_absolute() {
            return Self::File {
                path,
                line: line.or(fragment_line),
                column,
            };
        }

        if let Some(cwd) = cwd
            && !path_text.is_empty()
            && !path_text.starts_with('#')
        {
            return Self::File {
                path: cwd.join(path),
                line: line.or(fragment_line),
                column,
            };
        }

        Self::Web(raw.to_owned())
    }

    /// URL suitable for GPUI's platform opener. File source locations are
    /// deliberately omitted: Launch Services understands file URLs, not
    /// editor line/column suffixes. An editor adapter can use the retained
    /// fields later.
    pub fn open_url(&self) -> Option<String> {
        match self {
            Self::Web(url) => Some(url.clone()),
            Self::File { path, .. } => Url::from_file_path(path).ok().map(Into::into),
        }
    }
}

fn split_path_location(path: PathBuf) -> (PathBuf, Option<u32>, Option<u32>) {
    let Some(raw) = path.to_str() else {
        return (path, None, None);
    };
    let Some((without_last, last)) = raw.rsplit_once(':') else {
        return (path, None, None);
    };
    let Ok(last) = last.parse::<u32>() else {
        return (path, None, None);
    };
    if last == 0 {
        return (path, None, None);
    }

    if let Some((without_line, line)) = without_last.rsplit_once(':')
        && let Ok(line) = line.parse::<u32>()
        && line > 0
    {
        return (PathBuf::from(without_line), Some(line), Some(last));
    }

    (PathBuf::from(without_last), Some(last), None)
}

fn split_fragment_location(raw: &str) -> (&str, Option<u32>) {
    let Some((path, fragment)) = raw.rsplit_once('#') else {
        return (raw, None);
    };
    match fragment_line(Some(fragment)) {
        Some(line) => (path, Some(line)),
        None => (raw, None),
    }
}

fn fragment_line(fragment: Option<&str>) -> Option<u32> {
    let line = fragment?.strip_prefix('L')?.parse::<u32>().ok()?;
    (line > 0).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_urls_remain_unchanged() {
        let raw = "https://example.com/docs?q=one#two";
        let target = LinkTarget::parse(raw, Some(Path::new("/workspace")));
        assert_eq!(target, LinkTarget::Web(raw.into()));
        assert_eq!(target.open_url().as_deref(), Some(raw));
    }

    #[test]
    fn absolute_file_keeps_line_and_column_out_of_file_url() {
        let target = LinkTarget::parse("/workspace/src/main.rs:42:7", None);
        assert_eq!(
            target,
            LinkTarget::File {
                path: PathBuf::from("/workspace/src/main.rs"),
                line: Some(42),
                column: Some(7),
            }
        );
        assert_eq!(
            target.open_url().as_deref(),
            Some("file:///workspace/src/main.rs")
        );
    }

    #[test]
    fn relative_file_resolves_against_chat_cwd() {
        let target = LinkTarget::parse("src/main.rs:9", Some(Path::new("/workspace")));
        assert_eq!(
            target,
            LinkTarget::File {
                path: PathBuf::from("/workspace/src/main.rs"),
                line: Some(9),
                column: None,
            }
        );
    }

    #[test]
    fn relative_destination_without_cwd_is_not_bound_to_process_cwd() {
        assert_eq!(
            LinkTarget::parse("docs/readme.md", None),
            LinkTarget::Web("docs/readme.md".into())
        );
    }

    #[test]
    fn file_url_is_decoded_and_fragment_line_is_retained() {
        let target = LinkTarget::parse("file:///workspace/a%20file.rs#L18", None);
        assert_eq!(
            target,
            LinkTarget::File {
                path: PathBuf::from("/workspace/a file.rs"),
                line: Some(18),
                column: None,
            }
        );
        assert_eq!(
            target.open_url().as_deref(),
            Some("file:///workspace/a%20file.rs")
        );
    }
}
