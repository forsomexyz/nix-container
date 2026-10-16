use super::{
    cli::{self, Archive, Cli},
    Backend, BuildSpec, ImageId, PreparedRun, RemoveOutcome, RunInvocation, RunPlan,
};
use crate::runtime::CommandSpec;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub(super) struct DockerBackend(Cli);
impl DockerBackend {
    pub fn new(path: PathBuf) -> Self {
        Self(Cli(path))
    }
}

impl Backend for DockerBackend {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    fn image_id(&self, image: &str) -> Result<Option<ImageId>> {
        let Some(out) = self
            .0
            .inspect("image", &["image", "inspect", "--format", "{{.Id}}", image])?
        else {
            return Ok(None);
        };
        let id = String::from_utf8(out.stdout)?;
        let id = id.trim();
        anyhow::ensure!(
            !id.is_empty(),
            "image inspect returned an empty ID for {image}"
        );
        Ok(Some(ImageId(id.into())))
    }

    fn build_images(&self, spec: &BuildSpec) -> Result<()> {
        self.0.build_images(spec, Archive::Docker)
    }

    fn prepare_run(&self, plan: &RunPlan) -> Result<PreparedRun> {
        // Docker populates/owns fresh /nix and cache volumes from the image.
        // Persistence still needs explicit source selection and ownership when
        // its image location is missing or its mount is staged elsewhere.
        self.0.seed_persistence(plan)?;
        self.0.ensure_network(plan.network.as_deref())?;
        Ok(cli::prepare(plan))
    }

    fn command(&self, run: &PreparedRun, invocation: &RunInvocation) -> Result<CommandSpec> {
        Ok(self.0.command(run, invocation))
    }

    fn list_volumes(&self) -> Result<Vec<String>> {
        self.0
            .list_volumes()
            .context("Docker volume enumeration failed")
    }

    fn remove_volume(&self, name: &str) -> Result<RemoveOutcome> {
        self.0.remove_volume(name)
    }
}
