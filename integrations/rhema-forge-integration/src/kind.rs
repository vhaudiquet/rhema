//! The forge kinds: the vocabulary every forge surface shares — the
//! watch table's rows, the account store's `forge` column, the HTTP
//! routes' spelling, the source names the dispatcher registers, the
//! resource keys (`github:acme/w#7`). One type, owned by the crate
//! that speaks the forges; the claw's config no longer carries it.

/// Which forge a watch polls. The wire spelling is the source name the
/// dispatcher registers and the resource key carries (`github:acme/w#7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    /// GitHub (api.github.com, or a GitHub Enterprise host).
    GitHub,
    /// Gitea — self-hosted; a watch must name its instance.
    Gitea,
    /// GitLab (gitlab.com, or a self-hosted instance).
    GitLab,
}

impl ForgeKind {
    /// Every kind the claw ships a source for, in registration order.
    /// A kind missing here has no watch poller, whatever the table
    /// holds.
    pub const ALL: [ForgeKind; 3] = [ForgeKind::GitHub, ForgeKind::Gitea, ForgeKind::GitLab];

    /// The source name this kind registers and dispatches under.
    pub fn name(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github",
            ForgeKind::Gitea => "gitea",
            ForgeKind::GitLab => "gitlab",
        }
    }

    /// The environment variable the token falls back to.
    pub fn token_env(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "GITHUB_TOKEN",
            ForgeKind::Gitea => "GITEA_TOKEN",
            ForgeKind::GitLab => "GITLAB_TOKEN",
        }
    }

    /// Parses a forge name as the config, the HTTP surface, and the
    /// watch tools spell it: the source name (`github`, `gitea`,
    /// `gitlab`). `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "github" => Some(ForgeKind::GitHub),
            "gitea" => Some(ForgeKind::Gitea),
            "gitlab" => Some(ForgeKind::GitLab),
            _ => None,
        }
    }

    /// The production base URL; `None` for the self-hosted-only forges.
    pub fn default_base(self) -> Option<&'static str> {
        match self {
            ForgeKind::GitHub => Some(crate::github::DEFAULT_BASE_URL),
            ForgeKind::Gitea => None,
            ForgeKind::GitLab => Some(crate::gitlab::DEFAULT_BASE_URL),
        }
    }
}

/// Pure token resolution: the stored value first, then the forge's env
/// variable; the first non-blank wins. Parameterized over the env
/// lookup so tests can hold the environment still.
pub fn resolve_token(
    get_env: impl Fn(&str) -> Result<String, std::env::VarError>,
    forge: ForgeKind,
    stored: Option<&str>,
) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            get_env(forge.token_env())
                .ok()
                .map(|token| token.trim().to_owned())
                .filter(|token| !token.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_names_parse_as_the_sources_spell_them() {
        assert_eq!(ForgeKind::parse("github"), Some(ForgeKind::GitHub));
        assert_eq!(ForgeKind::parse(" gitea "), Some(ForgeKind::Gitea));
        assert_eq!(ForgeKind::parse("gitlab"), Some(ForgeKind::GitLab));
        assert_eq!(ForgeKind::parse("sourcehut"), None);
        assert_eq!(ForgeKind::parse(""), None);
        for kind in ForgeKind::ALL {
            assert_eq!(ForgeKind::parse(kind.name()), Some(kind), "{}", kind.name());
        }
    }

    #[test]
    fn the_token_falls_back_to_the_env_and_blank_counts_as_unset() {
        let env = |name: &str| match name {
            "GITHUB_TOKEN" => Ok("from-env".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        assert_eq!(
            resolve_token(env, ForgeKind::GitHub, Some(" from-config ")),
            Some("from-config".to_owned()),
            "the stored token wins"
        );
        assert_eq!(
            resolve_token(env, ForgeKind::GitHub, None),
            Some("from-env".to_owned()),
            "the env token serves when no stored token"
        );
        assert_eq!(
            resolve_token(
                |_| Err(std::env::VarError::NotPresent),
                ForgeKind::GitLab,
                Some("  ")
            ),
            None,
            "a blank token counts as unset"
        );
    }
}
