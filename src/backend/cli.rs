//! Private helpers shared by the two CLI backends. None of these raw CLI
//! operations are available to application orchestration.
use super::{
    BuildSpec, Environment, PreparedRun, RemoveOutcome, RunInvocation, RunPlan, BUILDER, IMAGE,
};
use crate::{
    paths::PERSIST_ROOT,
    runtime::{self, CommandSpec},
};
use anyhow::{bail, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::PathBuf,
    process::{Command, Output},
};

pub(super) struct Cli(pub PathBuf);

pub(super) enum Archive {
    Docker,
    Oci,
}
impl Archive {
    fn format(&self) -> &str {
        match self {
            Self::Docker => "docker",
            Self::Oci => "oci",
        }
    }
    fn suffix(&self) -> &str {
        match self {
            Self::Docker => ".tar.gz",
            Self::Oci => "-oci.tar",
        }
    }
}

impl Cli {
    pub fn process(&self) -> Command {
        Command::new(&self.0)
    }

    pub fn checked(&self, args: &[&str]) -> Result<()> {
        let code = runtime::wait(self.process().args(args))?;
        if code != 0 {
            bail!(
                "container runtime {} failed (exit {code})",
                args.first().unwrap_or(&"command")
            );
        }
        Ok(())
    }

    /// `checked`, but without letting the child draw on the terminal. Used for
    /// preparation steps that may run while the startup animation is on screen;
    /// their output is only interesting when they fail, or when the user asked
    /// to see the whole startup.
    pub fn quiet(&self, args: &[&str]) -> Result<()> {
        if runtime::verbose() {
            return self.checked(args);
        }
        let out = self.output(args)?;
        if !out.status.success() {
            bail!(
                "container runtime {} failed (exit {}): {}",
                args.first().unwrap_or(&"command"),
                runtime::code(out.status),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    pub fn output(&self, args: &[&str]) -> Result<Output> {
        runtime::output(&self.0, args)
    }

    pub fn json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let out = self.output(args)?;
        check_output(&out)?;
        Ok(serde_json::from_slice(&out.stdout)?)
    }

    pub fn inspect(&self, kind: &str, args: &[&str]) -> Result<Option<Output>> {
        let out = self.output(args)?;
        if !out.status.success() && missing(&out, kind, args.last().unwrap_or(&"")) {
            return Ok(None);
        }
        check_output(&out)?;
        Ok(Some(out))
    }

    pub fn volume_exists(&self, name: &str) -> Result<bool> {
        Ok(self
            .inspect("volume", &["volume", "inspect", name])?
            .is_some())
    }

    pub fn create_volume(&self, name: &str) -> Result<()> {
        let out = self.output(&["volume", "create", name])?;
        check_output(&out).with_context(|| format!("could not create volume: {name}"))
    }

    pub fn list_volumes(&self) -> Result<Vec<String>> {
        let out = self.output(&["volume", "ls", "--quiet"])?;
        check_output(&out).context("could not list volumes")?;
        Ok(String::from_utf8(out.stdout)?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    pub fn remove_volume(&self, name: &str) -> Result<RemoveOutcome> {
        if !self.volume_exists(name)? {
            return Ok(RemoveOutcome::Missing);
        }
        let out = self.output(&["volume", "rm", name])?;
        if !out.status.success() && missing(&out, "volume", name) {
            // Another invocation may have removed it after inspection.
            return Ok(RemoveOutcome::Missing);
        }
        check_output(&out).with_context(|| format!("could not remove volume: {name}"))?;
        Ok(RemoveOutcome::Removed)
    }

    pub fn ensure_network(&self, network: Option<&str>) -> Result<()> {
        let Some(network) = network.filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        // Preserve the accepted runtime modes. These are passed through to the
        // selected CLI, which validates support; they must never be created as
        // named networks. Network aliases deliberately aren't part of the API.
        if matches!(network, "bridge" | "host" | "none" | "default")
            || network.starts_with("container:")
            || network.starts_with("ns:")
        {
            return Ok(());
        }
        let exists = || -> Result<bool> {
            Ok(self
                .inspect("network", &["network", "inspect", network])?
                .is_some())
        };
        if exists()? {
            return Ok(());
        }
        let created = self.quiet(&["network", "create", network]).is_ok();
        if !created && !exists()? {
            bail!("could not create network: {network}");
        }
        Ok(())
    }

    pub fn seed_persistence(&self, plan: &RunPlan) -> Result<()> {
        for volume in &plan.persistence {
            if self.volume_exists(&volume.name)? {
                continue;
            }
            self.create_volume(&volume.name)?;
            // Never delete a failed seed: it may contain data or have been
            // created concurrently. Existence remains the legacy readiness
            // check; this is not a transactional initialization protocol.
            self.quiet(&[
                "run",
                "--rm",
                "--user",
                "0",
                "--entrypoint",
                "sh",
                "-v",
                &format!("{}:/seed-persistence", volume.name),
                &plan.image,
                "-c",
                include_str!("../../assets/seed-persistence.sh"),
                "_",
                &volume.location,
                &plan.identity.owner(),
            ])
            .with_context(|| {
                format!(
                    "failed to prepare persistence volume for {} (volume retained: {})",
                    volume.location, volume.name
                )
            })?;
        }
        Ok(())
    }

    pub fn command(&self, run: &PreparedRun, invocation: &RunInvocation) -> CommandSpec {
        let mut args: Vec<OsString> = vec!["run".into()];
        match (invocation.attach_stdin, invocation.tty) {
            (true, true) => args.push("-it".into()),
            (true, false) => args.push("-i".into()),
            (false, true) => args.push("-t".into()),
            (false, false) => {}
        }
        args.extend(run.args.iter().cloned());
        args.extend(run.startup_args.iter().map(OsString::from));
        args.extend(invocation.args.iter().map(OsString::from));
        CommandSpec {
            program: self.0.clone(),
            args,
        }
    }

    pub fn build_images(&self, spec: &BuildSpec, archive: Archive) -> Result<()> {
        let build = |image: &str, dockerfile: &str| -> Result<()> {
            let mut cmd = self.process();
            cmd.arg("build");
            if spec.no_cache {
                cmd.arg("--no-cache");
            }
            cmd.args(["-t", image, "-f"])
                .arg(spec.context.join(dockerfile))
                .arg(&spec.context);
            let code = runtime::wait(&mut cmd)?;
            if code != 0 {
                bail!("container runtime build failed (exit {code})");
            }
            Ok(())
        };
        build(BUILDER, "Dockerfile.base-build")?;
        let out = tempfile::tempdir()?;
        // The builder uses its image identity, not the development session's
        // user/HOME override. Only HOST_UID/GID configure the image it produces.
        self.checked(&[
            "run",
            "--rm",
            "-e",
            &format!("HOST_UID={}", spec.identity.uid),
            "-e",
            &format!("HOST_GID={}", spec.identity.gid),
            "-e",
            &format!("IMAGE_FORMAT={}", archive.format()),
            "-v",
            &format!("{}:/tmp/out", out.path().display()),
            BUILDER,
        ])?;
        let suffix = archive.suffix();
        let mut artifacts = fs::read_dir(out.path())?.collect::<std::io::Result<Vec<_>>>()?;
        artifacts.retain(|f| f.file_name().to_string_lossy().ends_with(suffix));
        artifacts.sort_by_key(|f| f.metadata().and_then(|m| m.modified()).ok());
        let artifact = artifacts.last().with_context(|| {
            format!(
                "no image artifact (*{suffix}) produced in {}",
                out.path().display()
            )
        })?;
        self.checked(&["image", "load", "-i", &artifact.path().to_string_lossy()])?;
        build(IMAGE, "Dockerfile")
    }
}

fn missing(out: &Output, kind: &str, name: &str) -> bool {
    let error = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    // Docker: "No such image/volume" or "network NAME not found".
    // Apple: "<kind> not found: ..." in a ContainerizationError.notFound.
    // Do not match a generic notFound: an unavailable image/volume service
    // must not be mistaken for an absent resource.
    error.contains(&format!("no such {kind}"))
        || error.contains(&format!("{kind} not found"))
        || error.contains(&format!("{kind} {} not found", name.to_ascii_lowercase()))
}

fn check_output(out: &Output) -> Result<()> {
    if !out.status.success() {
        bail!(
            "container runtime failed (exit {}): {}",
            runtime::code(out.status),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub(super) fn pair(args: &mut Vec<OsString>, flag: &str, value: impl AsRef<OsStr>) {
    args.push(flag.into());
    args.push(value.as_ref().to_owned());
}

/// Common CLI encoding and persistence staging. Backend-specific preparation
/// happens before this helper; backend-specific session overrides after it.
pub(super) fn prepare(plan: &RunPlan) -> PreparedRun {
    let mut args = vec!["--rm".into()];
    pair(&mut args, "--name", &plan.name);
    if let Some(memory) = &plan.memory {
        pair(&mut args, "--memory", memory);
    }
    if let Some(network) = &plan.network {
        pair(&mut args, "--network", network);
    }
    for port in &plan.ports {
        pair(&mut args, "-p", port);
    }
    for bind in &plan.binds {
        let mut mount = bind.source.as_os_str().to_owned();
        mount.push(format!(
            ":{}{}",
            bind.target,
            if bind.readonly { ":ro" } else { ":rw" }
        ));
        pair(&mut args, "-v", mount);
    }
    for env in &plan.env {
        match env {
            Environment::Value(name, value) => pair(&mut args, "-e", format!("{name}={value}")),
            Environment::Forward(name) => pair(&mut args, "-e", name),
        }
    }
    if let Some(file) = &plan.env_file {
        pair(&mut args, "--env-file", file);
    }
    pair(&mut args, "-v", format!("{}:/nix", plan.store));
    pair(
        &mut args,
        "-v",
        format!("{}:{}/.cache", plan.cache, plan.home),
    );
    pair(&mut args, "-w", &plan.workdir);
    let mut links = Vec::new();
    for volume in &plan.persistence {
        let target =
            crate::paths::staging(&volume.location).unwrap_or_else(|| volume.location.clone());
        if target != volume.location {
            links.push(volume.location.clone());
        }
        pair(&mut args, "-v", format!("{}:{target}", volume.name));
    }
    let mut startup_args = Vec::new();
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
        startup_args.extend([
            "-c".into(),
            include_str!("../../assets/persist.sh").into(),
            "nix-container".into(),
        ]);
    }
    args.push(plan.image.clone().into());
    PreparedRun { args, startup_args }
}
