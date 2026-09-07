use sha2::{Digest, Sha256};

/// Normalize an existing project identifier supplied by a client.
///
/// Registration still uses [`sanitize_ident`] because it may receive a path or
/// repository URL. Existing project references are already identifiers, so they
/// are only trimmed and lowercased.
pub fn normalize_project_ident(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Sanitize an arbitrary project identity string into a valid Discord channel name.
///
/// Discord channel rules: lowercase letters, digits, hyphens, underscores;
/// no leading/trailing hyphens; max 100 characters.
pub fn sanitize_ident(raw: &str) -> String {
    // 1. Strip trailing ".git"
    let trimmed = raw.trim_end_matches(".git");

    // 2. Take the last path segment (handle both '/' and '\')
    let basename = trimmed
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(trimmed);

    // 3. Lowercase
    let lower = basename.to_lowercase();

    // 4. Replace any char that's not [a-z0-9_-] with '-'
    let replaced: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // 5. Collapse consecutive hyphens
    let mut collapsed = String::new();
    let mut prev_hyphen = false;
    for c in replaced.chars() {
        if c == '-' {
            if !prev_hyphen {
                collapsed.push(c);
            }
            prev_hyphen = true;
        } else {
            collapsed.push(c);
            prev_hyphen = false;
        }
    }

    // 6. Strip leading/trailing hyphens
    let stripped = collapsed.trim_matches('-').to_string();

    // 7. Truncate to 100 chars
    let truncated = if stripped.len() > 100 {
        stripped[..100].trim_matches('-').to_string()
    } else {
        stripped
    };

    // 8. Fallback for empty result
    if truncated.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let hash = hasher.finalize();
        format!("project-{}", hex::encode(&hash[..4]))
    } else {
        truncated
    }
}

/// A repository reference parsed from a git remote URL or canonical ident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    /// Provider slug: `github`, `gitlab`, `bitbucket`, or the bare host for
    /// self-hosted remotes (`git.example.com`).
    pub provider: String,
    /// Everything between the host and the repository name (`nitecon`,
    /// `group/subgroup`).
    pub namespace: String,
    /// Repository name without `.git`.
    pub name: String,
    /// Normalized `host/namespace/name` form.
    pub canonical: String,
}

impl RepoRef {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

fn provider_for_host(host: &str) -> String {
    let host = host.to_lowercase();
    if host == "github.com" || host.ends_with(".github.com") {
        "github".into()
    } else if host == "gitlab.com" || host.starts_with("gitlab.") || host.contains(".gitlab.") {
        "gitlab".into()
    } else if host == "bitbucket.org" || host.starts_with("bitbucket.") {
        "bitbucket".into()
    } else {
        host
    }
}

/// Parse a git remote into a [`RepoRef`].
///
/// Accepts the forms agents encounter in practice:
///
/// * `github.com/nitecon/agent-gateway.git` (agent-tools canonical ident)
/// * `https://github.com/nitecon/agent-gateway.git`
/// * `git@github.com:nitecon/agent-gateway.git`
/// * `ssh://git@gitlab.example.com/group/sub/repo.git`
///
/// Returns `None` for anything that is not host + at least two path segments,
/// which includes bare filesystem paths and plain project names.
pub fn parse_remote(raw: &str) -> Option<RepoRef> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }
    for proto in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(proto) {
            s = rest.to_string();
            break;
        }
    }
    // Filesystem paths are never remotes.
    if s.starts_with('/') || s.starts_with('~') || s.starts_with('.') || s.contains(":\\") {
        return None;
    }
    // scp-style `user@host:path` -> `host/path`.
    if let Some(at) = s.find('@') {
        let before_slash = s.find('/').map(|i| at < i).unwrap_or(true);
        if before_slash {
            s = s[at + 1..].to_string();
        }
    }
    if let Some(colon) = s.find(':') {
        let slash = s.find('/');
        if slash.map(|i| colon < i).unwrap_or(true) {
            // Drop an explicit port (`host:2222/path`) or turn scp `host:path`
            // into `host/path`.
            let tail = &s[colon + 1..];
            let port_len = tail.chars().take_while(|c| c.is_ascii_digit()).count();
            if port_len > 0 && tail[port_len..].starts_with('/') {
                s = format!("{}{}", &s[..colon], &tail[port_len..]);
            } else {
                s = format!("{}/{}", &s[..colon], tail.trim_start_matches('/'));
            }
        }
    }
    let s = s.trim_end_matches('/');
    let mut segments: Vec<&str> = s.split('/').filter(|seg| !seg.is_empty()).collect();
    if segments.len() < 3 {
        return None;
    }
    let host = segments.remove(0);
    if !host.contains('.') && host != "localhost" {
        return None;
    }
    let name = segments
        .pop()
        .map(|n| n.trim_end_matches(".git").to_lowercase())
        .filter(|n| !n.is_empty())?;
    let namespace = segments
        .iter()
        .map(|seg| seg.to_lowercase())
        .collect::<Vec<_>>()
        .join("/");
    if namespace.is_empty() {
        return None;
    }
    let host = host.to_lowercase();
    Some(RepoRef {
        provider: provider_for_host(&host),
        canonical: format!("{host}/{namespace}/{name}"),
        namespace,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_remote_forms() {
        let expected = RepoRef {
            provider: "github".into(),
            namespace: "nitecon".into(),
            name: "agent-gateway".into(),
            canonical: "github.com/nitecon/agent-gateway".into(),
        };
        for raw in [
            "github.com/nitecon/agent-gateway.git",
            "https://github.com/nitecon/agent-gateway.git",
            "https://github.com/Nitecon/Agent-Gateway",
            "git@github.com:nitecon/agent-gateway.git",
            "ssh://git@github.com/nitecon/agent-gateway.git",
            "ssh://git@github.com:22/nitecon/agent-gateway.git",
        ] {
            assert_eq!(parse_remote(raw).as_ref(), Some(&expected), "{raw}");
        }
    }

    #[test]
    fn parses_nested_gitlab_groups_and_self_hosted_hosts() {
        let gl = parse_remote("https://gitlab.com/group/sub/repo.git").unwrap();
        assert_eq!(gl.provider, "gitlab");
        assert_eq!(gl.namespace, "group/sub");
        assert_eq!(gl.full_name(), "group/sub/repo");

        let own = parse_remote("git@git.example.com:team/tool.git").unwrap();
        assert_eq!(own.provider, "git.example.com");
        assert_eq!(own.canonical, "git.example.com/team/tool");
    }

    #[test]
    fn rejects_paths_and_bare_names() {
        for raw in [
            "/home/user/projects/my-app",
            "C:\\Users\\nitec\\Documents\\Projects\\bruce",
            "my-app",
            "nitecon/bruce",
            "",
        ] {
            assert!(parse_remote(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn strips_git_and_takes_basename() {
        assert_eq!(sanitize_ident("github.com/nitecon/bruce.git"), "bruce");
        assert_eq!(sanitize_ident("/home/user/projects/my-app"), "my-app");
        assert_eq!(
            sanitize_ident("C:\\Users\\nitec\\Documents\\Projects\\bruce"),
            "bruce"
        );
    }

    #[test]
    fn replaces_invalid_chars() {
        // trailing '!' becomes '-', which is then stripped by step 6
        assert_eq!(sanitize_ident("My Project!"), "my-project");
        assert_eq!(sanitize_ident("hello world"), "hello-world");
    }

    #[test]
    fn normalizes_existing_project_ident_to_lowercase() {
        assert_eq!(normalize_project_ident("  My-Project_1  "), "my-project_1");
    }

    #[test]
    fn collapses_hyphens() {
        assert_eq!(sanitize_ident("foo---bar"), "foo-bar");
    }

    #[test]
    fn fallback_for_all_special() {
        let result = sanitize_ident("!!!!");
        assert!(result.starts_with("project-"));
    }
}
