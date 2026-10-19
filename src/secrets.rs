//! Secret reference providers. A provider declares mounts before any credential
//! is fetched, and returns environment values only to the host launch process.
use crate::paths;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct FileDelivery {
    pub source: PathBuf,
    pub target: &'static str,
    pub readonly: bool,
}

pub trait SecretHelper {
    /// Providers sharing a slot would attempt to supply the same credential.
    fn slot(&self) -> &'static str;
    fn files(&self, home: &Path) -> Vec<FileDelivery>;
    fn inherited_env(&self) -> &'static [&'static str] {
        &[]
    }
    fn resolve(&self) -> Result<Vec<(String, String)>>;
}

struct Gh {
    host: String,
    account: String,
}
struct Npmrc;
struct Aws {
    profile: Option<String>,
}

pub fn parse(reference: &str) -> Result<Box<dyn SecretHelper>> {
    let (scheme, rest) = reference
        .split_once("://")
        .context("secret reference must be scheme://path")?;
    match scheme {
        "gh" => {
            let (host, account) = rest
                .split_once('/')
                .context("gh reference must be gh://<host>/<account>")?;
            if host.is_empty()
                || host
                    .split('.')
                    .any(|part| part.is_empty() || part.starts_with('-') || part.ends_with('-'))
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
                || !paths::valid_name(account)
                || account.starts_with('-')
                || matches!(account, "." | "..")
            {
                bail!("invalid gh://<host>/<account> reference");
            }
            Ok(Box::new(Gh {
                host: host.into(),
                account: account.into(),
            }))
        }
        "npmrc" if rest == "default" => Ok(Box::new(Npmrc)),
        "aws" if rest == "default" => Ok(Box::new(Aws { profile: None })),
        "aws" => {
            let profile = rest
                .strip_prefix("profile/")
                .context("aws reference must be aws://default or aws://profile/<name>")?;
            if profile.is_empty()
                || profile.starts_with('-')
                || !paths::valid_name(profile)
                || matches!(profile, "." | "..")
            {
                bail!("invalid AWS profile in secret reference");
            }
            Ok(Box::new(Aws {
                profile: Some(profile.into()),
            }))
        }
        _ => bail!("unsupported secret reference scheme: {scheme}"),
    }
}

impl SecretHelper for Gh {
    fn slot(&self) -> &'static str {
        "GH_TOKEN"
    }
    fn files(&self, _home: &Path) -> Vec<FileDelivery> {
        vec![]
    }
    fn resolve(&self) -> Result<Vec<(String, String)>> {
        let token = crate::runtime::text(
            "gh",
            &[
                "auth",
                "token",
                "--hostname",
                &self.host,
                "--user",
                &self.account,
            ],
        )
        .context("failed to get token from 'gh auth token'")?;
        if token.is_empty() {
            bail!("failed to get token from 'gh auth token'");
        }
        Ok(vec![("GH_TOKEN".into(), token)])
    }
}

impl SecretHelper for Npmrc {
    fn slot(&self) -> &'static str {
        "npmrc"
    }
    fn files(&self, home: &Path) -> Vec<FileDelivery> {
        vec![FileDelivery {
            source: home.join(".npmrc"),
            target: "/home/nix/.npmrc",
            readonly: true,
        }]
    }
    fn resolve(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![])
    }
}

impl SecretHelper for Aws {
    fn slot(&self) -> &'static str {
        "AWS credentials"
    }
    fn files(&self, home: &Path) -> Vec<FileDelivery> {
        vec![
            FileDelivery {
                source: home.join(".aws/config"),
                target: "/home/nix/.aws/config",
                readonly: true,
            },
            // The existing AWS integration keeps the SSO cache writable for token refresh.
            FileDelivery {
                source: home.join(".aws/sso/cache"),
                target: "/home/nix/.aws/sso/cache",
                readonly: false,
            },
        ]
    }
    fn inherited_env(&self) -> &'static [&'static str] {
        if self.profile.is_none() {
            &["AWS_PROFILE"]
        } else {
            &[]
        }
    }
    fn resolve(&self) -> Result<Vec<(String, String)>> {
        let mut cmd = Vec::new();
        if let Some(profile) = &self.profile {
            cmd.extend(["--profile", profile.as_str()]);
        }
        cmd.extend([
            "configure",
            "export-credentials",
            "--format",
            "env-no-export",
        ]);
        let creds = crate::runtime::text("aws", &cmd)
            .context("aws secret: could not export credentials")?;
        if creds.is_empty() {
            bail!("aws secret: returned no credentials");
        }
        let mut result = Vec::new();
        for line in creds.lines().filter(|s| !s.is_empty()) {
            let (name, value) = line
                .split_once('=')
                .context("aws secret: returned malformed credentials")?;
            if !paths::valid_env(name) {
                bail!("aws secret: invalid environment variable name");
            }
            if result
                .iter()
                .any(|(existing, _): &(String, String)| existing == name)
            {
                bail!("aws secret: duplicate environment variable: {name}");
            }
            result.push((name.into(), value.into()));
        }
        if let Some(profile) = &self.profile {
            if result.iter().any(|(name, _)| name == "AWS_PROFILE") {
                bail!("aws secret: duplicate environment variable: AWS_PROFILE");
            }
            result.push(("AWS_PROFILE".into(), profile.clone()));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_shapes() {
        for good in [
            "gh://github.com/alice",
            "npmrc://default",
            "aws://default",
            "aws://profile/dev",
        ] {
            assert!(parse(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "gh://github.com",
            "gh://github.com/alice/extra",
            "gh://-flag/a",
            "gh://github.com/-u",
            "npmrc://other",
            "aws://profile/",
            "aws://profile/-flag",
            "op://vault/item/field",
            "other://ref",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
