use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const HOME: &str = "/home/nix";
pub const PERSIST_ROOT: &str = "/home/nix/.nix-container-persist";

pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))[..8].into()
}

pub fn project_hash() -> Result<String> {
    let file = ["shell.nix", "default.nix"]
        .into_iter()
        .find(|f| Path::new(f).is_file())
        .context("no shell.nix or default.nix in current directory")?;
    Ok(hash(fs::read(file)?))
}

pub fn host_path(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.into()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        value.into()
    }
}

fn container_path(path: &str) -> Result<String> {
    let path = path.trim_end_matches('/');
    if !path.starts_with('/') || path.is_empty() {
        bail!("container path must be absolute and cannot be '/': {path}");
    }
    if path.contains(':') || path[1..].split('/').any(|c| matches!(c, "" | "." | "..")) {
        bail!("container path cannot contain ':', repeated '/', '.' or '..' components: {path}");
    }
    Ok(path.into())
}

#[derive(Debug)]
pub struct Mount {
    pub source: PathBuf,
    pub target: String,
    pub mode: String,
}
impl Mount {
    pub fn spec(&self) -> String {
        format!("{}:{}:{}", self.source.display(), self.target, self.mode)
    }
}

pub fn mount(spec: &str, home: &Path) -> Result<Mount> {
    let parts: Vec<_> = spec.split(':').collect();
    let (source, target, mode) = match parts.as_slice() {
        [s] => (*s, "", "rw"),
        [s, m @ ("ro" | "rw")] => (*s, "", *m),
        [s, t] => (*s, *t, "rw"),
        [s, t, m] => (*s, *t, if m.is_empty() { "rw" } else { *m }),
        _ => bail!("--mount: invalid specification: {spec} (expected HOST[:CONTAINER][:ro|:rw])"),
    };
    if source.is_empty() {
        bail!("--mount: missing host path");
    }
    if !matches!(mode, "ro" | "rw") {
        bail!("--mount: invalid mode: {mode}");
    }
    let source = host_path(source, home)
        .canonicalize()
        .with_context(|| format!("--mount: no such file or directory: {source}"))?;
    if source.to_string_lossy().contains(':') {
        bail!("--mount: host path cannot contain ':'");
    }
    let target = if target.is_empty() {
        format!(
            "{HOME}/{}",
            source
                .file_name()
                .context("--mount: cannot derive container path; specify HOST:CONTAINER")?
                .to_string_lossy()
        )
    } else {
        target.into()
    };
    Ok(Mount {
        source,
        target: container_path(&target)?,
        mode: mode.into(),
    })
}

pub fn persistence(spec: &str, home: &Path) -> Result<String> {
    if spec.is_empty() {
        bail!("--persist: location cannot be empty");
    }
    let host_home = home.to_string_lossy();
    let path = if spec == host_home || spec == "~" {
        HOME.into()
    } else if let Some(rest) = spec
        .strip_prefix(&format!("{host_home}/"))
        .or_else(|| spec.strip_prefix("~/"))
    {
        format!("{HOME}/{rest}")
    } else if spec.starts_with('~') {
        bail!("--persist: named home directories are not supported");
    } else if spec.starts_with('/') {
        spec.into()
    } else {
        format!("{HOME}/{spec}")
    };
    container_path(&path).context("--persist: invalid location")
}

pub fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.starts_with(&format!("{b}/"))
        || b.starts_with(&format!("{a}/"))
        || a == "/"
        || b == "/"
}

pub fn staging(location: &str) -> Option<String> {
    location
        .strip_prefix("/home/nix/")
        .filter(|s| s.contains('/'))
        .map(|rest| format!("{PERSIST_ROOT}/{rest}"))
}

pub fn loopback(original: &str) -> Result<String> {
    let (spec, proto) = original
        .split_once('/')
        .map_or((original, String::new()), |(s, p)| (s, format!("/{p}")));
    let rest = if let Some(ipv6) = spec.strip_prefix('[') {
        let (address, rest) = ipv6
            .split_once("]:")
            .context("invalid port specification")?;
        if !address.contains(':') || rest.matches(':').count() != 1 {
            bail!("invalid port specification: {original}");
        }
        rest.to_owned()
    } else {
        match spec.split(':').collect::<Vec<_>>().as_slice() {
            [port] if !port.is_empty() => format!("{port}:{port}"),
            [host, container] if !container.is_empty() => format!("{host}:{container}"),
            [_, host, container] if !container.is_empty() => format!("{host}:{container}"),
            _ => bail!("invalid port specification: {original}"),
        }
    };
    if rest.ends_with(':') {
        bail!("invalid port specification: {original}");
    }
    Ok(format!("127.0.0.1:{rest}{proto}"))
}

pub fn network_mode(network: &str) -> bool {
    matches!(network, "bridge" | "host" | "none" | "default")
        || network.starts_with("container:")
        || network.starts_with("ns:")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
pub fn valid_env(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

// nix-shell --run takes shell source, not argv. POSIX single quoting preserves every
// argument (including newlines, empty arguments and shell metacharacters).
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_hashes() {
        assert_eq!(hash("abc"), "ba7816bf");
        assert_eq!(hash(""), "e3b0c442");
    }
    #[test]
    fn ports_always_loopback() {
        for (spec, expected) in [
            ("8080", "8080:8080"),
            ("80:8080", "80:8080"),
            ("0.0.0.0:80:8080/udp", "80:8080/udp"),
            ("[::]:80:8080", "80:8080"),
            ("[::1]::8080", ":8080"),
        ] {
            assert_eq!(loopback(spec).unwrap(), format!("127.0.0.1:{expected}"));
        }
        for bad in ["", "[::]:8080", "::1:80:8080", "[abc]:80:8080", "[::]:80:"] {
            assert!(loopback(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn persistence_normalization() {
        let home = Path::new("/Users/test");
        for value in [
            ".local/share/tool",
            "~/.local/share/tool",
            "/Users/test/.local/share/tool",
            "/home/nix/.local/share/tool/",
        ] {
            assert_eq!(
                persistence(value, home).unwrap(),
                "/home/nix/.local/share/tool"
            );
        }
        for value in ["", "/", "a/../b", "a/./b", "a//b", "a:b", "~other/x"] {
            assert!(persistence(value, home).is_err());
        }
        assert_eq!(
            staging("/home/nix/.local/share/tool").unwrap(),
            "/home/nix/.nix-container-persist/.local/share/tool"
        );
        assert!(staging("/home/nix/.npm").is_none());
        assert!(staging("/var/lib/db").is_none());
        assert!(overlaps("/nix", "/nix/store"));
        assert!(!overlaps("/nix", "/nixos"));
    }
    #[test]
    fn shell_quoting() {
        let values = ["", "a b", "'", "$(touch /tmp/no)", "a\nb", "\\"];
        let source = format!("printf '%s\\0' {}", values.map(quote).join(" "));
        let out = std::process::Command::new("sh")
            .args(["-c", &source])
            .output()
            .unwrap();
        assert_eq!(
            out.stdout,
            values
                .iter()
                .flat_map(|s| s.bytes().chain([0]))
                .collect::<Vec<_>>()
        );
    }
}
