use super::{
    cli::{self, Archive, Cli},
    Backend, BuildSpec, ImageId, PreparedRun, RemoveOutcome, RunInvocation, RunPlan,
};
use crate::runtime::CommandSpec;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::PathBuf;

pub(super) struct AppleBackend(Cli);
impl AppleBackend {
    pub fn new(path: PathBuf) -> Self {
        Self(Cli(path))
    }

    fn seed_cache(&self, plan: &RunPlan) -> Result<()> {
        // Check both: a user may have removed only one cache. Existence is the
        // legacy readiness check, not a guarantee that a prior seed completed.
        if self.0.volume_exists(&plan.store)? && self.0.volume_exists(&plan.cache)? {
            return Ok(());
        }
        for volume in [&plan.store, &plan.cache] {
            self.0.create_volume(volume)?;
        }
        // Staging mounts leave the image's /nix visible as the copy source.
        // Run as root, preserving store metadata and owning only the cache root.
        self.0.checked(&[
            "run", "--rm", "--user", "0", "--entrypoint", "sh",
            "-v", &format!("{}:/seed-nix", plan.store),
            "-v", &format!("{}:/seed-cache", plan.cache), &plan.image, "-c",
            "set -e; if [ ! -e /seed-nix/store ]; then cp -a /nix/. /seed-nix/; fi; chown \"$1\" /seed-cache",
            "_", &plan.identity.owner(),
        ]).context("failed to prepare the /nix and cache volumes")
    }
}

impl Backend for AppleBackend {
    fn preflight(&self) -> Result<()> {
        let status = self
            .0
            .json(&["system", "status", "--format", "json"])
            .unwrap_or(Value::Null);
        if status["status"] != "running" {
            bail!("container service not running — run 'container system start'");
        }
        let props = self
            .0
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

    fn image_id(&self, image: &str) -> Result<Option<ImageId>> {
        let Some(out) = self.0.inspect("image", &["image", "inspect", image])? else {
            return Ok(None);
        };
        let value: Value = serde_json::from_slice(&out.stdout)?;
        let id = value[0]["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("image inspect returned no ID for {image}"))?;
        Ok(Some(ImageId(id.into())))
    }

    fn build_images(&self, spec: &BuildSpec) -> Result<()> {
        // Couples IMAGE_FORMAT=oci and artifact selection; conversion is done
        // inside the builder with skopeo, never by the application orchestrator.
        self.0.build_images(spec, Archive::Oci)
    }

    fn prepare_run(&self, plan: &RunPlan) -> Result<PreparedRun> {
        self.seed_cache(plan)?;
        self.0.seed_persistence(plan)?;
        self.0.ensure_network(plan.network.as_deref())?;
        let mut run = cli::prepare(plan);
        // Apple's named USER handling needs a numeric uid:gid and explicit HOME.
        // Apply only to sessions/probes, not root seeders or image builders.
        let mut args = Vec::new();
        cli::pair(&mut args, "--user", plan.identity.owner());
        cli::pair(&mut args, "-e", format!("HOME={}", plan.home));
        args.extend(run.args);
        run.args = args;
        Ok(run)
    }

    fn command(&self, run: &PreparedRun, invocation: &RunInvocation) -> Result<CommandSpec> {
        Ok(self.0.command(run, invocation))
    }

    fn list_volumes(&self) -> Result<Vec<String>> {
        self.0.list_volumes()
    }

    fn remove_volume(&self, name: &str) -> Result<RemoveOutcome> {
        self.0.remove_volume(name)
    }
}
