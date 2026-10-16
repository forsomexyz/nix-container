use crate::{
    backend::{self, Backend, BuildSpec, Identity, RemoveOutcome, BASE},
    paths,
};
use anyhow::{bail, Context, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

pub fn init(force: bool) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(force)
        .truncate(force)
        .create_new(!force)
        .open("shell.nix")
        .context("cannot create shell.nix (use --force to overwrite an existing file)")?;
    file.write_all(include_bytes!("../assets/shell.nix"))?;
    println!("nix-container: created shell.nix");
    Ok(())
}

fn cache_volumes(backend: &dyn Backend) -> Result<Vec<String>> {
    Ok(backend
        .list_volumes()?
        .into_iter()
        .filter(|name| {
            name.starts_with("nix-container-store-") || name.starts_with("nix-container-cache-")
        })
        .collect())
}

pub fn clear_cache(all: bool) -> Result<()> {
    let backend = backend::detect()?;
    let volumes = if all {
        cache_volumes(backend.as_ref())?
    } else {
        let hash = paths::project_hash().context("use --all to clear all caches")?;
        vec![
            format!("nix-container-store-{hash}"),
            format!("nix-container-cache-{hash}"),
        ]
    };
    if volumes.is_empty() {
        println!("nix-container: no nix-container caches found");
    }
    for volume in volumes {
        if backend.remove_volume(&volume)? == RemoveOutcome::Missing {
            println!("nix-container: volume {volume} does not exist, skipping");
        }
    }
    Ok(())
}

pub fn build(no_cache: bool) -> Result<()> {
    let backend = backend::detect()?;
    backend.preflight()?;
    let previous = backend.image_id(BASE)?;
    // Embed the build inputs so a cargo-installed binary needs no checkout nearby.
    let context = tempfile::tempdir()?;
    for (name, data) in [
        ("Dockerfile", include_str!("../Dockerfile")),
        (
            "Dockerfile.base-build",
            include_str!("../Dockerfile.base-build"),
        ),
        ("build-base.sh", include_str!("../build-base.sh")),
    ] {
        fs::write(context.path().join(name), data)?;
    }
    backend.build_images(&BuildSpec {
        context: context.path().into(),
        identity: Identity::host(),
        no_cache,
    })?;
    let new = backend.image_id(BASE)?;
    if new.is_some() && previous != new {
        for volume in cache_volumes(backend.as_ref())? {
            if let Err(error) = backend.remove_volume(&volume) {
                eprintln!("nix-container: {error:#} (still in use? stop running containers and run 'nix-container clear-cache --all')");
            }
        }
        println!("nix-container: {BASE} changed; invalidated unused cache volumes");
    }
    Ok(())
}

pub fn flag(args: &[String], name: &str, command: &str, description: &str) -> Result<Option<bool>> {
    let mut enabled = false;
    for arg in args {
        if matches!(arg.as_str(), "--help" | "-h") {
            println!("Usage: nix-container {command} [{name}]\n\n{description}\n\nOptions:\n  {name}\n  -h, --help");
            return Ok(None);
        }
        if arg == name {
            enabled = true;
        } else {
            bail!("{command}: unknown option: {arg}");
        }
    }
    Ok(Some(enabled))
}
