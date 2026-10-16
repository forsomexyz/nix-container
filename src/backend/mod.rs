//! Semantic boundary between application policy and container-runtime mechanics.
//! Callers never construct runtime argv or branch on the selected backend.
mod apple;
mod cli;
mod docker;

use crate::runtime::{self, CommandSpec};
use anyhow::{Context, Result};
use std::{env, ffi::OsStr, path::PathBuf};

pub const IMAGE: &str = "nix-container:latest";
pub const BASE: &str = "nix-container-base:latest";
const BUILDER: &str = "nix-container-base-build:latest";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageId(String);

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    Missing,
}

#[derive(Clone, Copy, Debug)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}
impl Identity {
    pub fn host() -> Self {
        // SAFETY: getuid/getgid have no preconditions.
        unsafe {
            Self {
                uid: libc::getuid(),
                gid: libc::getgid(),
            }
        }
    }
    fn owner(self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }
}

#[derive(Clone, Debug)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: String,
    pub readonly: bool,
}

#[derive(Clone, Debug)]
pub struct PersistenceVolume {
    pub name: String,
    /// Original image location to seed from and expose to the session. The
    /// backend may mount elsewhere and arrange a startup symlink to this path.
    pub location: String,
}

#[derive(Clone, Debug)]
pub enum Environment {
    Value(String, String),
    /// Resolve from the CLI process environment, keeping secrets out of argv.
    Forward(String),
}

#[derive(Clone, Debug)]
pub struct RunPlan {
    pub image: String,
    pub name: String,
    pub identity: Identity,
    pub home: String,
    pub workdir: String,
    pub binds: Vec<BindMount>,
    pub store: String,
    pub cache: String,
    pub persistence: Vec<PersistenceVolume>,
    pub memory: Option<String>,
    pub network: Option<String>,
    pub ports: Vec<String>,
    pub env: Vec<Environment>,
    pub env_file: Option<PathBuf>,
}

/// Backend-owned launch state. May include rewritten mounts and an entrypoint
/// wrapper. Reusable for a session and diagnostic probes, but prepare again
/// after removing any volumes. Fields intentionally aren't public.
pub struct PreparedRun {
    args: Vec<std::ffi::OsString>,
    startup_args: Vec<String>,
}

pub struct RunInvocation {
    /// Arguments to the image entrypoint, not runtime CLI flags.
    pub args: Vec<String>,
    pub attach_stdin: bool,
    pub tty: bool,
}

pub struct BuildSpec {
    /// Embedded Dockerfiles and build-base.sh, materialized by the application.
    pub context: PathBuf,
    pub identity: Identity,
    pub no_cache: bool,
}

pub trait Backend {
    fn preflight(&self) -> Result<()>;
    /// None means absent, not an unavailable runtime or malformed inspect output.
    fn image_id(&self, image: &str) -> Result<Option<ImageId>>;
    /// Build the builder, produce/import the base, then build the session image.
    fn build_images(&self, spec: &BuildSpec) -> Result<()>;
    /// May run setup containers and create resources, but never the user's
    /// command. Existing persistence data must not be deleted on failure.
    fn prepare_run(&self, plan: &RunPlan) -> Result<PreparedRun>;
    /// Describe a process without spawning it, so the common executor can wrap
    /// it with credential injection and select inherited or captured streams.
    fn command(&self, run: &PreparedRun, invocation: &RunInvocation) -> Result<CommandSpec>;
    fn list_volumes(&self) -> Result<Vec<String>>;
    /// Non-forcing removal; in-use and other runtime failures remain errors.
    fn remove_volume(&self, name: &str) -> Result<RemoveOutcome>;
}

pub fn detect() -> Result<Box<dyn Backend>> {
    let cli = if let Some(name) = env::var("NIX_CONTAINER_CLI").ok().filter(|s| !s.is_empty()) {
        runtime::executable(&name)
            .with_context(|| format!("NIX_CONTAINER_CLI={name} not found in PATH"))?
    } else {
        ["docker", "container"].into_iter().find_map(runtime::executable)
            .context("no supported container CLI found (tried: docker, container). Set NIX_CONTAINER_CLI to override.")?
    };
    // Preserve support for Docker-compatible wrappers. An explicit path whose
    // basename is `container` selects Apple, just as before the backend split.
    Ok(if cli.file_name() == Some(OsStr::new("container")) {
        Box::new(apple::AppleBackend::new(cli))
    } else {
        Box::new(docker::DockerBackend::new(cli))
    })
}
