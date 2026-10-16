use crate::{
    paths,
    runtime::{Runtime, BASE},
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

pub fn clear_cache(all: bool) -> Result<()> {
    let runtime = Runtime::detect()?;
    let volumes = if all {
        runtime.cache_volumes()?
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
        if runtime.exists("volume", &volume) {
            runtime.checked(&["volume", "rm", &volume])?;
        } else {
            println!("nix-container: volume {volume} does not exist, skipping");
        }
    }
    Ok(())
}

pub fn build(no_cache: bool) -> Result<()> {
    let runtime = Runtime::detect()?;
    runtime.preflight()?;
    let previous = runtime.image_id();
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
    let build_image = |image: &str, dockerfile: &str| -> Result<()> {
        let file = context.path().join(dockerfile);
        let file = file.to_string_lossy();
        let dir = context.path().to_string_lossy();
        let mut args = vec!["build"];
        if no_cache {
            args.push("--no-cache");
        }
        args.extend(["-t", image, "-f", &file, &dir]);
        runtime.checked(&args)
    };
    build_image("nix-container-base-build:latest", "Dockerfile.base-build")?;
    let out = tempfile::tempdir()?;
    // SAFETY: getuid/getgid have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    runtime.checked(&[
        "run",
        "--rm",
        "-e",
        &format!("HOST_UID={uid}"),
        "-e",
        &format!("HOST_GID={gid}"),
        "-e",
        if runtime.apple {
            "IMAGE_FORMAT=oci"
        } else {
            "IMAGE_FORMAT=docker"
        },
        "-v",
        &format!("{}:/tmp/out", out.path().display()),
        "nix-container-base-build:latest",
    ])?;
    let suffix = if runtime.apple { "-oci.tar" } else { ".tar.gz" };
    let mut artifacts = fs::read_dir(out.path())?.collect::<std::io::Result<Vec<_>>>()?;
    artifacts.retain(|f| f.file_name().to_string_lossy().ends_with(suffix));
    artifacts.sort_by_key(|f| f.metadata().and_then(|m| m.modified()).ok());
    let artifact = artifacts.last().with_context(|| {
        format!(
            "no image artifact (*{suffix}) produced in {}",
            out.path().display()
        )
    })?;
    runtime.checked(&["image", "load", "-i", &artifact.path().to_string_lossy()])?;
    build_image("nix-container:latest", "Dockerfile")?;
    let new = runtime.image_id();
    if new.is_some() && previous != new {
        for volume in runtime.cache_volumes()? {
            if !runtime.quiet(&["volume", "rm", &volume])? {
                eprintln!("nix-container: could not remove {volume} (still in use? stop running containers and run 'nix-container clear-cache --all')");
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
