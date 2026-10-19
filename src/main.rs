#[cfg(not(unix))]
compile_error!("nix-container supports Unix hosts (Linux and macOS)");

mod backend;
mod manage;
mod options;
mod paths;
mod run;
mod runtime;
mod secrets;

use anyhow::Result;

const HELP: &str = "Usage: nix-container [options] [run COMMAND [ARG...]]
       nix-container init [--force]
       nix-container build [--no-cache]
       nix-container clear-cache [--all]

Run a containerized nix-shell for shell.nix or default.nix in the current directory.
Everything after 'run' is the command; put nix-container options before it.

Options (CLI > nix-container.yml/.yaml > environment):
  --with-env FILE                  Load dotenv; resolve op:// references via op run
  --secret REF                     Provide secret via gh://, npmrc:// or aws:// (repeatable)
  --with-shared-nix-exprs NAME=DIR  Mount shared Nix expressions read-only (repeatable)
  --mount HOST[:CONTAINER][:ro|:rw] Bind an extra host path (repeatable)
  --persist PATH                   Persist a container directory per project (repeatable)
  -p, --port SPEC                  Publish a port on 127.0.0.1 only (repeatable)
  -m, --memory SIZE                Runtime memory limit (e.g. 512M, 4G)
  --network NAME                   Join a named network or runtime network mode
  --config FILE                    Use this config instead of the discovered file
  --no-config                      Ignore the project config
  -h, --help                       Show help
  --version                        Show version

Environment: NIX_CONTAINER_CLI, NIX_CONTAINER_MEMORY,
             NIX_CONTAINER_SHARED_NIX_EXPRS (colon-separated NAME=DIR entries)";

fn execute() -> Result<i32> {
    runtime::install_signals()?;
    let mut args: Vec<String> = std::env::args().collect();
    let invoked = std::path::Path::new(&args.remove(0))
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    // Also support symlinks with the historical command names.
    let alias = match invoked.as_str() {
        "nix-container-init" => Some("init"),
        "nix-container-build" => Some("build"),
        "nix-container-clear-cache" => Some("clear-cache"),
        _ => None,
    };
    if let Some(alias) = alias {
        args.insert(0, alias.into());
    }
    match args.first().map(String::as_str) {
        Some("--version") => println!("nix-container {}", env!("CARGO_PKG_VERSION")),
        Some("init") => {
            if let Some(force) = manage::flag(
                &args[1..],
                "--force",
                "init",
                "Create a bare-bones shell.nix.",
            )? {
                manage::init(force)?;
            }
        }
        Some("build") => {
            if let Some(no_cache) = manage::flag(
                &args[1..],
                "--no-cache",
                "build",
                "Build the base and runtime images for the host uid/gid.",
            )? {
                manage::build(no_cache)?;
            }
        }
        Some("clear-cache") => {
            if let Some(all) = manage::flag(
                &args[1..],
                "--all",
                "clear-cache",
                "Remove project caches, never persistence volumes.",
            )? {
                manage::clear_cache(all)?;
            }
        }
        _ => {
            let invocation = options::parse(&args)?;
            if invocation.help {
                println!("{HELP}");
            } else {
                return run::run(invocation);
            }
        }
    }
    Ok(0)
}

fn main() {
    std::process::exit(match execute() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("nix-container: {error:#}");
            1
        }
    });
}
