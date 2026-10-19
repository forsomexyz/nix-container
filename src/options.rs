use anyhow::{bail, Context, Result};
use std::{collections::BTreeMap, path::Path};

// Presence is significant: an explicit empty list suppresses environment defaults.
#[derive(Clone, Debug, Default)]
pub struct Options(pub BTreeMap<String, Vec<String>>);

impl Options {
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.0.insert(key.into(), vec![value.into()]);
    }
    pub fn push(&mut self, key: &str, value: impl Into<String>) {
        self.0.entry(key.into()).or_default().push(value.into());
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.list(key).first().map(String::as_str)
    }
    pub fn list(&self, key: &str) -> &[String] {
        self.0.get(key).map(Vec::as_slice).unwrap_or_default()
    }
    pub fn overlay(&mut self, other: Self) {
        self.0.extend(other.0);
    }
}

pub struct Invocation {
    pub options: Options,
    pub command: Option<Vec<String>>,
    pub config: Option<String>,
    pub use_config: bool,
    pub help: bool,
    pub verbose: bool,
}

pub fn parse(args: &[String]) -> Result<Invocation> {
    let mut result = Invocation {
        options: Options::default(),
        command: None,
        config: None,
        use_config: true,
        help: false,
        verbose: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        match arg.as_str() {
            "-h" | "--help" => {
                result.help = true;
                return Ok(result);
            }
            "run" => {
                if i == args.len() {
                    bail!("run requires a command (e.g. nix-container run npm test)");
                }
                result.command = Some(args[i..].to_vec());
                break;
            }
            "--no-config" => {
                result.use_config = false;
                continue;
            }
            "-v" | "--verbose" => {
                result.verbose = true;
                continue;
            }
            _ => {}
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
        let (key, multiple, required) = match flag {
            "--with-env" => ("with-env", false, "a file argument"),
            "--secret" => ("secrets", true, "a secret reference"),
            "--with-shared-nix-exprs" => ("with-shared-nix-exprs", true, "a value (NAME=DIR)"),
            "--mount" => ("mounts", true, "a value (HOST[:CONTAINER][:ro|:rw])"),
            "--persist" => ("persistence", true, "a container path"),
            "-p" | "--port" => ("ports", true, "a port specification"),
            "-m" | "--memory" => ("memory", false, "a memory size"),
            "--network" => ("network", false, "a network name"),
            "--config" => ("config", false, "a file argument"),
            _ => bail!("unknown option: {arg}"),
        };
        let value = if let Some(value) = inline {
            value
        } else {
            let value = args.get(i).map(String::as_str).unwrap_or_default();
            i += 1;
            value
        };
        if value.is_empty() {
            bail!("{flag} requires {required}");
        }
        if key == "config" {
            result.config = Some(value.into());
            result.use_config = true;
        } else if multiple {
            result.options.push(key, value);
        } else {
            result.options.set(key, value);
        }
    }
    Ok(result)
}

pub fn valid_memory(value: &str) -> bool {
    let mut number = value;
    if number.ends_with(['b', 'B']) {
        number = &number[..number.len() - 1];
    }
    if number.ends_with(['k', 'K', 'm', 'M', 'g', 'G', 't', 'T', 'p', 'P']) {
        number = &number[..number.len() - 1];
    }
    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}

pub fn scalar(value: &str) -> Result<String> {
    let value = value.trim();
    if value.starts_with('#') {
        return Ok(String::new());
    }
    if let Some(quote @ ('\'' | '"')) = value.chars().next() {
        let rest = &value[1..];
        let end = rest.find(quote).context("malformed quoted scalar")?;
        let inner = &rest[..end];
        let tail = &rest[end + 1..];
        if (quote == '"' && inner.contains('\\'))
            || (!tail.is_empty()
                && (!tail.starts_with(char::is_whitespace)
                    || (!tail.trim().is_empty() && !tail.trim().starts_with('#'))))
        {
            bail!("malformed quoted scalar");
        }
        return Ok(inner.into());
    }
    let end = value
        .char_indices()
        .find_map(|(i, c)| (c == '#' && value[..i].ends_with(char::is_whitespace)).then_some(i))
        .unwrap_or(value.len());
    Ok(value[..end].trim().into())
}

fn list_key(key: &str) -> Option<&str> {
    match key {
        "port" | "ports" => Some("ports"),
        "mount" | "mounts" => Some("mounts"),
        "persist" | "persistence" => Some("persistence"),
        "with-shared-nix-exprs" => Some("with-shared-nix-exprs"),
        "secret" | "secrets" => Some("secrets"),
        _ => None,
    }
}

pub fn config(text: &str, file: &Path) -> Result<Options> {
    let mut options = Options::default();
    let mut list = None::<String>;
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line == "---" {
            continue;
        }
        let mut parse_line = || -> Result<()> {
            if line == "-" || line.starts_with("- ") {
                let key = list
                    .as_deref()
                    .context("list item outside of an option that takes a list")?;
                let value = scalar(&line[1..])?;
                if value.is_empty() {
                    bail!("empty list item for '{key}'");
                }
                options.push(key, value);
                return Ok(());
            }
            let (key, raw) = line.split_once(':').context("expected 'option: value'")?;
            let key = key.trim().replace('_', "-");
            list = None;
            if let Some(key) = list_key(&key) {
                list = Some(key.into());
                options.0.entry(key.into()).or_default();
                let value = scalar(raw)?;
                if value.starts_with('[') {
                    if !value.ends_with(']') {
                        bail!("malformed list for '{key}'");
                    }
                    let inner = &value[1..value.len() - 1];
                    if !inner.trim().is_empty() {
                        // Like the original flat YAML subset, quote escapes and embedded commas
                        // in flow-list items are deliberately unsupported.
                        for item in inner.split(',') {
                            let item = scalar(item)?;
                            if item.is_empty() {
                                bail!("empty list item for '{key}'");
                            }
                            options.push(key, item);
                        }
                    }
                } else if !value.is_empty() {
                    options.push(key, value);
                }
                return Ok(());
            }
            let value = scalar(raw)?;
            match key.as_str() {
                "memory" => {
                    if !valid_memory(&value) {
                        bail!("invalid memory size: {value}");
                    }
                    options.set(&key, value);
                }
                "network" | "with-env" => {
                    if value.is_empty() {
                        bail!("'{key}' expects a nonempty value");
                    }
                    options.set(&key, value);
                }
                _ => bail!("unknown option: {key}"),
            }
            Ok(())
        };
        parse_line().with_context(|| format!("{}:{}", file.display(), line_no + 1))?;
    }
    // Only host-side paths are relative to the config; persistence is container-side.
    let base = file.parent().unwrap_or(Path::new("."));
    for key in ["mounts", "with-env", "with-shared-nix-exprs"] {
        if let Some(values) = options.0.get_mut(key) {
            for value in values {
                let (prefix, path) = if key == "with-shared-nix-exprs" {
                    match value.split_once('=') {
                        Some((name, path)) => (format!("{name}="), path),
                        None => continue,
                    }
                } else {
                    (String::new(), value.as_str())
                };
                if !path.starts_with('/') && !path.starts_with('~') {
                    *value = format!("{prefix}{}", base.join(path).display());
                }
            }
        }
    }
    Ok(options)
}

pub fn load(invocation: &Invocation) -> Result<Options> {
    let mut result = Options::default();
    if let Ok(memory) = std::env::var("NIX_CONTAINER_MEMORY") {
        if !memory.is_empty() {
            result.set("memory", memory);
        }
    }
    if let Ok(shared) = std::env::var("NIX_CONTAINER_SHARED_NIX_EXPRS") {
        for spec in shared.split(':').filter(|s| !s.is_empty()) {
            result.push("with-shared-nix-exprs", spec);
        }
    }
    if invocation.use_config {
        let file = invocation.config.as_deref().map(Path::new).or_else(|| {
            ["nix-container.yml", "nix-container.yaml"]
                .into_iter()
                .map(Path::new)
                .find(|p| p.is_file())
        });
        if let Some(file) = file {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("cannot read config {}", file.display()))?;
            result.overlay(config(&text, file)?);
        }
    }
    result.overlay(invocation.options.clone());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory() {
        for good in ["1", "512M", "4GB", "2kb", "0", "2B"] {
            assert!(valid_memory(good), "{good}");
        }
        for bad in ["", "G", "-1", "2GG", "1.5G", "2BB", "2KM"] {
            assert!(!valid_memory(bad), "{bad}");
        }
    }
    #[test]
    fn config_shapes_and_paths() {
        let o = config("secrets: [gh://github.com/alice, aws://profile/dev]\nports: [8080, '3000:3000']\nmounts:\n - ../data:ro\nnetwork: dev-network # hi\nwith-env: .env\npersist: .local/share/app", Path::new("/cfg/options.yml")).unwrap();
        assert_eq!(
            o.list("secrets"),
            ["gh://github.com/alice", "aws://profile/dev"]
        );
        assert_eq!(o.list("ports"), ["8080", "3000:3000"]);
        assert_eq!(o.get("network"), Some("dev-network")); // trailing comment stripped
        assert_eq!(o.get("mounts"), Some("/cfg/../data:ro"));
        assert_eq!(o.get("with-env"), Some("/cfg/.env"));
        assert_eq!(o.get("persistence"), Some(".local/share/app"));
    }
    #[test]
    fn malformed_has_line_number() {
        for text in [
            "network: \"oops",
            "unknown: true",
            "ports:\n -",
            "ports: [1,,2]",
            "with-npmrc: true",
        ] {
            let err = config(text, Path::new("config.yml")).unwrap_err();
            assert!(format!("{err:#}").contains("config.yml:"));
        }
    }
    #[test]
    fn secret_references_are_repeatable_and_override_config() {
        let mut options = config(
            "secrets: [gh://github.com/alice, npmrc://default]",
            Path::new("config.yml"),
        )
        .unwrap();
        assert_eq!(
            options.list("secrets"),
            ["gh://github.com/alice", "npmrc://default"]
        );
        let invocation = parse(&[
            "--secret=aws://default".into(),
            "--secret".into(),
            "npmrc://default".into(),
            "run".into(),
            "true".into(),
        ])
        .unwrap();
        options.overlay(invocation.options);
        assert_eq!(
            options.list("secrets"),
            ["aws://default", "npmrc://default"]
        );
        assert!(parse(&["--secret".into(), "".into()]).is_err());
        assert!(config(
            "secrets: [gh://github.com/alice,,npmrc://default]",
            Path::new("config.yml")
        )
        .is_err());
    }
    #[test]
    fn command_boundary_and_override() {
        let args = ["--network=dev", "-p", "8080", "run", "echo", "--help"].map(String::from);
        let i = parse(&args).unwrap();
        assert!(!i.help);
        assert_eq!(i.command.unwrap(), ["echo", "--help"]);
        let mut o = config("network: old\nports: [1111, 2222]", Path::new("x")).unwrap();
        o.overlay(i.options);
        // A flag replaces the config's whole list rather than appending to it.
        assert_eq!(o.list("ports"), ["8080"]);
        assert_eq!(o.get("network"), Some("dev"));
    }
}
