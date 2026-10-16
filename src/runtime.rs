use anyhow::{bail, Context, Result};
use serde_json::Value;
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::Mutex,
    time::Duration,
};

pub const IMAGE: &str = "nix-container:latest";
pub const BASE: &str = "nix-container-base:latest";

pub fn executable(name: &str) -> Option<PathBuf> {
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        return executable(Path::new(name)).then(|| name.into());
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(name))
        .find(|p| executable(p))
}

pub fn output(program: impl AsRef<OsStr>, args: &[&str]) -> Result<Output> {
    Command::new(program.as_ref())
        .args(args)
        .output()
        .with_context(|| format!("could not execute {}", program.as_ref().to_string_lossy()))
}

pub fn text(program: &str, args: &[&str]) -> Result<String> {
    let out = output(program, args)?;
    if !out.status.success() {
        bail!("{program} failed");
    }
    Ok(String::from_utf8(out.stdout)?
        .trim_end_matches(['\r', '\n'])
        .into())
}

pub fn code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

static ACTIVE_CHILD: Mutex<Option<libc::pid_t>> = Mutex::new(None);

// Keep the CLI in the foreground process group for terminal job control. Forward
// externally delivered termination signals too; the runtime handles container cleanup.
// Install once: removing a signal-hook handler would leave the signal ignored.
pub fn install_signals() -> Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    std::thread::spawn(move || {
        for signal in signals.forever() {
            let active = ACTIVE_CHILD.lock().unwrap();
            if let Some(pid) = *active {
                // SAFETY: the mutex prevents reaping this child while signalling it.
                unsafe {
                    libc::kill(pid, signal);
                }
            } else {
                std::process::exit(128 + signal);
            }
        }
    });
    Ok(())
}

pub fn wait(command: &mut Command) -> Result<i32> {
    let mut active = ACTIVE_CHILD.lock().unwrap();
    let mut child = command.spawn().context("could not start child process")?;
    *active = Some(child.id() as libc::pid_t);
    drop(active);
    loop {
        let mut active = ACTIVE_CHILD.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => {
                *active = None;
                return Ok(code(status));
            }
            Err(error) => {
                *active = None;
                return Err(error.into());
            }
            Ok(None) => {}
        }
        drop(active);
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn owner() -> String {
    // SAFETY: getuid/getgid have no preconditions.
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

pub struct Runtime {
    pub cli: PathBuf,
    pub apple: bool,
}
impl Runtime {
    pub fn detect() -> Result<Self> {
        let cli = if let Some(name) = env::var("NIX_CONTAINER_CLI").ok().filter(|s| !s.is_empty()) {
            executable(&name)
                .with_context(|| format!("NIX_CONTAINER_CLI={name} not found in PATH"))?
        } else {
            ["docker", "container"].into_iter().find_map(executable)
                .context("no supported container CLI found (tried: docker, container). Set NIX_CONTAINER_CLI to override.")?
        };
        let apple = cli.file_name() == Some(OsStr::new("container"));
        Ok(Self { cli, apple })
    }
    pub fn command(&self) -> Command {
        Command::new(&self.cli)
    }
    pub fn exists(&self, kind: &str, name: &str) -> bool {
        self.quiet(&[kind, "inspect", name]).unwrap_or(false)
    }
    pub fn quiet(&self, args: &[&str]) -> Result<bool> {
        Ok(self
            .command()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success())
    }
    pub fn checked(&self, args: &[&str]) -> Result<()> {
        let code = wait(self.command().args(args))?;
        if code != 0 {
            bail!(
                "container runtime {} failed (exit {code})",
                args.first().unwrap_or(&"command")
            );
        }
        Ok(())
    }
    pub fn json(&self, args: &[&str]) -> Result<Value> {
        let out = output(&self.cli, args)?;
        if !out.status.success() {
            bail!("container runtime failed");
        }
        Ok(serde_json::from_slice(&out.stdout)?)
    }
    pub fn preflight(&self) -> Result<()> {
        if !self.apple {
            return Ok(());
        }
        let status = self
            .json(&["system", "status", "--format", "json"])
            .unwrap_or(Value::Null);
        if status["status"] != "running" {
            bail!("container service not running — run 'container system start'");
        }
        let props = self
            .json(&["system", "property", "list", "--format", "json"])
            .unwrap_or(Value::Null);
        if props["kernel"]["binaryPath"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
        {
            bail!("no guest kernel configured — run 'container system kernel set --recommended'");
        }
        Ok(())
    }
    pub fn image_id(&self) -> Option<String> {
        if self.apple {
            self.json(&["image", "inspect", BASE]).ok()?[0]["id"]
                .as_str()
                .map(str::to_owned)
        } else {
            let out = output(
                &self.cli,
                &["image", "inspect", "--format", "{{.Id}}", BASE],
            )
            .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
        }
    }
    pub fn cache_volumes(&self) -> Result<Vec<String>> {
        let out = output(&self.cli, &["volume", "ls", "--quiet"])?;
        if !out.status.success() {
            bail!("could not list cache volumes");
        }
        Ok(String::from_utf8(out.stdout)?
            .lines()
            .filter(|s| {
                s.starts_with("nix-container-store-") || s.starts_with("nix-container-cache-")
            })
            .map(str::to_owned)
            .collect())
    }
    pub fn network(&self, network: &str) -> Result<()> {
        if network.is_empty()
            || crate::paths::network_mode(network)
            || self.exists("network", network)
        {
            return Ok(());
        }
        let created = self
            .command()
            .args(["network", "create", network])
            .stdout(Stdio::null())
            .status()?
            .success();
        if !created && !self.exists("network", network) {
            bail!("could not create network: {network}");
        }
        Ok(())
    }
    pub fn seed_cache(&self, store: &str, cache: &str) -> Result<()> {
        // Check both: a user may have removed only the cache volume.
        if !self.apple || (self.exists("volume", store) && self.exists("volume", cache)) {
            return Ok(());
        }
        for volume in [store, cache] {
            if !self.quiet(&["volume", "create", volume])? {
                bail!("could not create volume: {volume}");
            }
        }
        self.checked(&["run", "--rm", "--user", "0", "--entrypoint", "sh", "-v", &format!("{store}:/seed-nix"), "-v", &format!("{cache}:/seed-cache"), IMAGE, "-c",
            "set -e; if [ ! -e /seed-nix/store ]; then cp -a /nix/. /seed-nix/; fi; chown \"$1\" /seed-cache", "_", &owner()])
            .context("failed to prepare the /nix and cache volumes")
    }
    pub fn seed_persistence(&self, volume: &str, location: &str) -> Result<()> {
        if self.exists("volume", volume) {
            return Ok(());
        }
        if !self.quiet(&["volume", "create", volume])? {
            bail!("could not create persistence volume: {volume}");
        }
        // Never delete a failed seed volume: it may have been created concurrently
        // and already contain user data. Cache clearing also excludes persistence.
        self.checked(&[
            "run",
            "--rm",
            "--user",
            "0",
            "--entrypoint",
            "sh",
            "-v",
            &format!("{volume}:/seed-persistence"),
            IMAGE,
            "-c",
            include_str!("../assets/seed-persistence.sh"),
            "_",
            location,
            &owner(),
        ])
        .with_context(|| {
            format!(
                "failed to prepare persistence volume for {location} (volume retained: {volume})"
            )
        })
    }
    pub fn launch(
        &self,
        args: &[String],
        credentials: &BTreeMap<String, String>,
        op_file: Option<&str>,
    ) -> Command {
        let mut command = if let Some(file) = op_file {
            let mut cmd = Command::new("op");
            cmd.args(["run", "--no-masking", &format!("--env-file={file}"), "--"])
                .arg(&self.cli);
            cmd
        } else {
            self.command()
        };
        command.args(args).envs(credentials);
        command
    }
}
