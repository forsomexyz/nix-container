use crate::{
    options::{self, Invocation, Options},
    paths::{self, HOME, PERSIST_ROOT},
    runtime::{self, Runtime, IMAGE},
};
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{self, IsTerminal},
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

fn pair(args: &mut Vec<String>, flag: &str, value: impl Into<String>) {
    args.push(flag.into());
    args.push(value.into());
}

struct Plan {
    args: Vec<String>,
    credentials: BTreeMap<String, String>,
    op_file: Option<String>,
    persistence: Vec<(String, String)>,
    store: String,
    cache: String,
    network: String,
}

impl Plan {
    fn new(options: &Options) -> Result<Self> {
        let hash = paths::project_hash()?;
        let project = env::current_dir()?.canonicalize()?;
        let name = project
            .file_name()
            .context("cannot run a project at the filesystem root")?
            .to_string_lossy();
        let project_dst = format!("{HOME}/{name}");
        let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
        if project.to_string_lossy().contains(':') {
            bail!("project path cannot contain ':'");
        }
        if ["/nix", "/home/nix/.cache", PERSIST_ROOT]
            .iter()
            .any(|p| paths::overlaps(p, &project_dst))
        {
            bail!("project directory conflicts with a managed mount: {project_dst}");
        }
        let mut args = vec!["--rm".into(), "--name".into(), name.into()];
        let mut occupied = vec![
            "/nix".into(),
            "/home/nix/.cache".into(),
            project_dst.clone(),
            PERSIST_ROOT.into(),
        ];
        let mut credentials = BTreeMap::new();
        let mut op_file = None;
        if let Some(memory) = options.get("memory") {
            if !options::valid_memory(memory) {
                bail!("--memory: invalid memory size: {memory}");
            }
            pair(&mut args, "--memory", memory);
        }
        let network = options.get("network").unwrap_or_default();
        if !network.is_empty() {
            pair(&mut args, "--network", network);
        }
        if !options.list("network-aliases").is_empty() {
            if network.is_empty() {
                bail!("--network-alias requires --network");
            }
            if paths::network_mode(network) {
                bail!("--network-alias: {network} is a network mode, not a named network that can carry aliases");
            }
            let mut seen = HashSet::new();
            for alias in options.list("network-aliases") {
                if !paths::valid_alias(alias) {
                    bail!("--network-alias: invalid alias: {alias}");
                }
                if !seen.insert(alias) {
                    bail!("--network-alias: duplicate alias: {alias}");
                }
                pair(&mut args, "--network-alias", alias);
            }
        }
        for port in options.list("ports") {
            pair(&mut args, "-p", paths::loopback(port)?);
        }
        let aws = options.enabled("with-aws");
        let mut optional_mount = |source: PathBuf, target: &str, readonly: bool| -> Result<()> {
            if source.exists() {
                if source.to_string_lossy().contains(':') {
                    bail!("host path cannot contain ':'");
                }
                if occupied.iter().any(|p| paths::overlaps(p, target)) {
                    bail!("optional mount overlaps another mount: {target}");
                }
                pair(
                    &mut args,
                    "-v",
                    format!(
                        "{}:{target}{}",
                        source.display(),
                        if readonly { ":ro" } else { "" }
                    ),
                );
                occupied.push(target.into());
            }
            Ok(())
        };
        if options.enabled("with-npmrc") {
            optional_mount(home.join(".npmrc"), "/home/nix/.npmrc", true)?;
        }
        if aws {
            optional_mount(home.join(".aws/config"), "/home/nix/.aws/config", true)?;
            optional_mount(
                home.join(".aws/sso/cache"),
                "/home/nix/.aws/sso/cache",
                false,
            )?;
        }
        let mut seen = HashSet::new();
        let mut nix_path = Vec::new();
        for spec in options.list("with-shared-nix-exprs") {
            let (name, dir) = spec
                .split_once('=')
                .context("--with-shared-nix-exprs expects NAME=DIR")?;
            if !paths::valid_name(name) || matches!(name, "." | "..") {
                bail!("--with-shared-nix-exprs: invalid name: {name}");
            }
            if !seen.insert(name) {
                bail!("--with-shared-nix-exprs: duplicate name: {name}");
            }
            let dir = paths::host_path(dir, &home);
            if !dir.is_dir() {
                bail!(
                    "--with-shared-nix-exprs: not a directory: {}",
                    dir.display()
                );
            }
            let dir = dir.canonicalize()?;
            if dir.to_string_lossy().contains(':') {
                bail!("shared expression path cannot contain ':'");
            }
            let target = format!("{HOME}/.nix-shared-exprs/{name}");
            if occupied.iter().any(|p| paths::overlaps(p, &target)) {
                bail!("shared expression mount overlaps another mount: {target}");
            }
            pair(&mut args, "-v", format!("{}:{target}:ro", dir.display()));
            nix_path.push(format!("{name}={target}"));
            occupied.push(target);
        }
        if !nix_path.is_empty() {
            pair(&mut args, "-e", format!("NIX_PATH={}:/nix/var/nix/profiles/per-user/nix/channels:/home/nix/.nix-defexpr/channels", nix_path.join(":")));
        }
        for spec in options.list("mounts") {
            let mount = paths::mount(spec, &home)?;
            if let Some(other) = occupied.iter().find(|p| paths::overlaps(p, &mount.target)) {
                bail!(
                    "--mount: {} overlaps an existing or reserved container path ({other})",
                    mount.target
                );
            }
            pair(&mut args, "-v", mount.spec());
            occupied.push(mount.target);
        }
        let project_hash = paths::hash(project.as_os_str().as_encoded_bytes());
        let mut persistence = Vec::new();
        let mut links = Vec::new();
        for spec in options.list("persistence") {
            let location = paths::persistence(spec, &home)?;
            if let Some(other) = occupied.iter().find(|p| paths::overlaps(p, &location)) {
                bail!("--persist: {location} overlaps another mount or a location managed by nix-container ({other})");
            }
            let volume = format!(
                "nix-container-persist-{project_hash}-{}",
                paths::hash(&location)
            );
            let target = paths::staging(&location).unwrap_or_else(|| location.clone());
            if target != location {
                links.push(location.clone());
            }
            pair(&mut args, "-v", format!("{volume}:{target}"));
            occupied.push(location.clone());
            persistence.push((volume, location));
        }
        // Resolve credentials only after validating the plan. Values are passed in the
        // child environment, never in argv and never exported into the caller's shell.
        if options.enabled("with-gh-token") {
            let token = runtime::text("gh", &["auth", "token"])
                .context("failed to get token from 'gh auth token'")?;
            if token.is_empty() {
                bail!("failed to get token from 'gh auth token'");
            }
            credentials.insert("GH_TOKEN".into(), token);
            pair(&mut args, "-e", "GH_TOKEN");
        }
        if aws {
            let profile = options.get("aws-profile").filter(|p| !p.is_empty());
            let mut cmd = Vec::new();
            if let Some(profile) = profile {
                cmd.extend(["--profile", profile]);
            }
            cmd.extend([
                "configure",
                "export-credentials",
                "--format",
                "env-no-export",
            ]);
            let creds =
                runtime::text("aws", &cmd).context("--with-aws: could not export credentials")?;
            if creds.is_empty() {
                bail!("--with-aws: returned no credentials");
            }
            for line in creds.lines().filter(|s| !s.is_empty()) {
                let (name, value) = line
                    .split_once('=')
                    .context("--with-aws: returned malformed credentials")?;
                if !paths::valid_env(name) {
                    bail!("--with-aws: invalid environment variable name");
                }
                credentials.insert(name.into(), value.into());
                pair(&mut args, "-e", name);
            }
            if let Some(profile) = profile {
                credentials.insert("AWS_PROFILE".into(), profile.into());
                pair(&mut args, "-e", "AWS_PROFILE");
            } else if env::var_os("AWS_PROFILE").is_some() {
                pair(&mut args, "-e", "AWS_PROFILE");
            }
        }
        if let Some(file) = options.get("with-env") {
            let file = paths::host_path(file, &home);
            let text = fs::read_to_string(&file)
                .with_context(|| format!("env file not found or unreadable: {}", file.display()))?;
            if text.contains("op://") {
                if runtime::executable("op").is_none() {
                    bail!("{} has 1Password secret references (op://) but the 'op' CLI is not in PATH", file.display());
                }
                for line in text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                {
                    let line = line.strip_prefix("export ").unwrap_or(line);
                    if let Some((name, _)) = line.split_once('=') {
                        let name = name.trim();
                        if !paths::valid_env(name) {
                            bail!("invalid environment variable name in {}", file.display());
                        }
                        pair(&mut args, "-e", name);
                    }
                }
                op_file = Some(file.to_string_lossy().into());
            } else {
                pair(&mut args, "--env-file", file.to_string_lossy());
            }
        }
        let store = format!("nix-container-store-{hash}");
        let cache = format!("nix-container-cache-{hash}");
        pair(&mut args, "-v", format!("{store}:/nix"));
        pair(&mut args, "-v", format!("{cache}:{HOME}/.cache"));
        pair(
            &mut args,
            "-v",
            format!("{}:{project_dst}", project.display()),
        );
        pair(&mut args, "-w", project_dst);
        if !links.is_empty() {
            pair(
                &mut args,
                "-e",
                format!("NIX_CONTAINER_PERSIST_ROOT={PERSIST_ROOT}"),
            );
            pair(
                &mut args,
                "-e",
                format!("NIX_CONTAINER_PERSIST_LINKS={}", links.join(":")),
            );
            pair(&mut args, "--entrypoint", "sh");
        }
        args.push(IMAGE.into());
        if !links.is_empty() {
            args.extend([
                "-c".into(),
                include_str!("../assets/persist.sh").into(),
                "nix-container".into(),
            ]);
        }
        args.extend(["--extra-experimental-features".into(), "flakes".into()]);
        Ok(Self {
            args,
            credentials,
            op_file,
            persistence,
            store,
            cache,
            network: network.into(),
        })
    }
    fn argv(&self, runtime: &Runtime, command: Option<&[String]>, tty: bool) -> Vec<String> {
        let mut args = vec!["run".into()];
        if tty {
            if command.is_none() {
                args.push("-it".into());
            } else {
                if io::stdin().is_terminal() {
                    args.push("-i".into());
                }
                if io::stdin().is_terminal() && io::stdout().is_terminal() {
                    args.push("-t".into());
                }
            }
        }
        if runtime.apple {
            args.extend([
                "--user".into(),
                runtime::owner(),
                "-e".into(),
                format!("HOME={HOME}"),
            ]);
        }
        args.extend(self.args.clone());
        if let Some(command) = command {
            pair(
                &mut args,
                "--run",
                command
                    .iter()
                    .map(|s| paths::quote(s))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        args
    }
}

pub fn run(invocation: Invocation) -> Result<i32> {
    let options = options::load(&invocation)?;
    let plan = Plan::new(&options)?;
    let runtime = Runtime::detect()?;
    runtime.preflight()?;
    if !runtime.exists("image", IMAGE) {
        bail!("image {IMAGE} not found. Run 'nix-container build' to build it.");
    }
    runtime.seed_cache(&plan.store, &plan.cache)?;
    for (volume, location) in &plan.persistence {
        runtime.seed_persistence(volume, location)?;
    }
    runtime.network(&plan.network)?;
    let args = plan.argv(&runtime, invocation.command.as_deref(), true);
    let start = Instant::now();
    let code =
        runtime::wait(&mut runtime.launch(&args, &plan.credentials, plan.op_file.as_deref()))?;
    if code != 0 && code < 128 && start.elapsed() < Duration::from_secs(3) {
        // Probe nix-shell only, never repeat the user's command during diagnosis.
        let probe = plan.argv(&runtime, Some(&["true".into()]), false);
        let output = runtime
            .launch(&probe, &plan.credentials, plan.op_file.as_deref())
            .stdin(Stdio::null())
            .output()?;
        if [output.stdout, output.stderr]
            .concat()
            .windows(b"cannot determine user's home directory".len())
            .any(|w| w == b"cannot determine user's home directory")
        {
            eprintln!(
                "nix-container: stale cache detected for this project, clearing and retrying..."
            );
            for volume in [&plan.store, &plan.cache] {
                if runtime.exists("volume", volume) && !runtime.quiet(&["volume", "rm", volume])? {
                    eprintln!("nix-container: cannot clear cache while other sessions are open; exit those sessions and re-run");
                    return Ok(code);
                }
            }
            runtime.seed_cache(&plan.store, &plan.cache)?;
            return runtime::wait(&mut runtime.launch(
                &args,
                &plan.credentials,
                plan.op_file.as_deref(),
            ));
        }
    }
    Ok(code)
}
