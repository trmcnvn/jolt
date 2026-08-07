//! Provider-neutral remote code-review detection for local checkouts.
//!
//! The protocol and UI consume [`CheckoutReview`]; each forge adapter owns its
//! remote recognition, authentication, terminology, and lookup mechanism.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use jolt_proto::CheckoutReview;
use serde::Deserialize;

use crate::Repos;
use crate::vcs::{compose_command_path, resolve_auxiliary_executable};

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForgeRemote {
    host: String,
    repository: String,
}

#[async_trait]
trait ForgeAdapter: Sync {
    fn recognizes(&self, remote: &ForgeRemote) -> bool;

    async fn find_review(
        &self,
        checkout: &Path,
        remote: &ForgeRemote,
        reference: &str,
    ) -> Option<CheckoutReview>;
}

struct GitHubAdapter;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitHubReview {
    number: u64,
    title: String,
    url: String,
    state: String,
}

#[async_trait]
impl ForgeAdapter for GitHubAdapter {
    fn recognizes(&self, remote: &ForgeRemote) -> bool {
        remote.host == "github.com" && remote.repository.split('/').count() == 2
    }

    async fn find_review(
        &self,
        checkout: &Path,
        remote: &ForgeRemote,
        reference: &str,
    ) -> Option<CheckoutReview> {
        let executable = resolve_auxiliary_executable("gh", "JOLT_GH_EXECUTABLE")?;
        let mut command = tokio::process::Command::new(&executable);
        command
            .args([
                "pr",
                "list",
                "--repo",
                &remote.repository,
                "--head",
                reference,
                "--state",
                "open",
                "--limit",
                "1",
                "--json",
                "number,title,url,state",
            ])
            .current_dir(checkout)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        compose_command_path(&mut command, &executable);
        let output = tokio::time::timeout(LOOKUP_TIMEOUT, command.output())
            .await
            .ok()?
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let review = serde_json::from_slice::<Vec<GitHubReview>>(&output.stdout)
            .ok()?
            .into_iter()
            .next()?;
        if review.state != "OPEN" {
            return None;
        }
        Some(CheckoutReview {
            forge: "github".into(),
            number: review.number,
            title: review.title,
            url: review.url,
        })
    }
}

static GITHUB: GitHubAdapter = GitHubAdapter;
static ADAPTERS: [&dyn ForgeAdapter; 1] = [&GITHUB];

/// Find the first open remote review associated with this checkout. Unsupported
/// forges, unavailable provider CLIs, missing authentication, and branches with
/// no review are all ordinary absence: the composer simply omits the badge.
pub(crate) async fn detect(repos: &Repos, checkout: &Path) -> Option<CheckoutReview> {
    let references = match repos.review_references(checkout).await {
        Ok(references) => references,
        Err(error) => {
            tracing::debug!(path = %checkout.display(), %error, "review references unavailable");
            return None;
        }
    };
    if references.is_empty() {
        return None;
    }
    let remotes = match repos.remote_urls(checkout).await {
        Ok(remotes) => remotes,
        Err(error) => {
            tracing::debug!(path = %checkout.display(), %error, "review remotes unavailable");
            return None;
        }
    };
    for remote in remotes.iter().filter_map(|url| parse_remote_url(url)) {
        for adapter in ADAPTERS
            .iter()
            .filter(|adapter| adapter.recognizes(&remote))
        {
            for reference in &references {
                if let Some(review) = adapter.find_review(checkout, &remote, reference).await {
                    return Some(review);
                }
            }
        }
    }
    None
}

fn parse_remote_url(raw: &str) -> Option<ForgeRemote> {
    let raw = raw.trim();
    let (authority, path) = if let Some((_, rest)) = raw.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        (authority, path)
    } else {
        raw.split_once(':')?
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .split(':')
        .next()?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let repository = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_matches('/'))
        .to_string();
    (!host.is_empty() && !repository.is_empty()).then_some(ForgeRemote { host, repository })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_remote_urls_without_assuming_a_forge() {
        assert_eq!(
            parse_remote_url("git@github.com:owner/repo.git"),
            Some(ForgeRemote {
                host: "github.com".into(),
                repository: "owner/repo".into(),
            })
        );
        assert_eq!(
            parse_remote_url("https://gitlab.example.com/group/subgroup/repo.git"),
            Some(ForgeRemote {
                host: "gitlab.example.com".into(),
                repository: "group/subgroup/repo".into(),
            })
        );
        assert_eq!(
            parse_remote_url("ssh://git@github.com/owner/repo.git"),
            Some(ForgeRemote {
                host: "github.com".into(),
                repository: "owner/repo".into(),
            })
        );
        assert_eq!(parse_remote_url("../local/repo"), None);
    }

    #[test]
    fn parses_open_github_review_list() {
        let review = serde_json::from_str::<Vec<GitHubReview>>(
            r#"[{"number":42,"title":"Ship it","url":"https://github.com/o/r/pull/42","state":"OPEN"}]"#,
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
        assert_eq!(review.number, 42);
        assert_eq!(review.state, "OPEN");
    }

    #[test]
    fn github_adapter_recognizes_only_github_repository_slugs() {
        assert!(GITHUB.recognizes(&ForgeRemote {
            host: "github.com".into(),
            repository: "owner/repo".into(),
        }));
        assert!(!GITHUB.recognizes(&ForgeRemote {
            host: "gitlab.com".into(),
            repository: "owner/repo".into(),
        }));
        assert!(!GITHUB.recognizes(&ForgeRemote {
            host: "github.com".into(),
            repository: "group/subgroup/repo".into(),
        }));
    }
}
