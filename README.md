# nix-container

Run a per-project Nix development shell inside a container, without installing
Nix on the host.

## Goals

- **No dependencies on the host.** The host only needs a container CLI (`docker` or
  Apple's `container`). All Nix tooling lives inside the image.
- **Per-project shells.** Drop a `shell.nix` (or `default.nix`) into any
  directory and start a container that auto-launches `nix-shell` against it.
- **Cache across runs.** The Nix store and CLI caches are persisted in named
  volumes so packages don't re-download every invocation.

## Requirements

- A docker-CLI-compatible container runtime in `PATH`: `docker` or `container`.
- For `--with-gh-token`: the `gh` CLI, authenticated.
- For `--with-aws`: the `aws` CLI (v2.13+ for `configure export-credentials`).

## Setup

Source the script from your shell rc (works in `bash` and `zsh`):

```sh
source /path/to/nix-container/nix-container.sh
```

Build the images (first time, and after any change to `Dockerfile*`):

```sh
nix-container-build
```

This produces three images:

- `nix-container-base-build:latest` — intermediate, used to produce the base tarball.
- `nix-container-base:latest` — base image with Nix, direnv, nix-direnv.
- `nix-container:latest` — the image used at runtime.

## Usage

From any directory containing a `shell.nix` or `default.nix`:

```sh
nix-container
```

The current directory is bind-mounted to `/home/nix/<dirname>` and set as the
working directory inside the container. The entrypoint is `nix-shell`, so the
container drops you into the project's resolved environment.

### Options

| Flag                       | Effect                                                                                          |
| -------------------------- | ----------------------------------------------------------------------------------------------- |
| `--with-gh-token`          | Run `gh auth token` on the host and forward the result as `GH_TOKEN`.                           |
| `--with-aws`               | Export resolved AWS credentials as env vars and read-only mount `~/.aws/config` and SSO cache.  |
| `--with-aws=<profile>`     | Same as `--with-aws`, but resolves credentials from the given profile (sets `AWS_PROFILE` too). |
| `-p`, `--port <spec>`      | Publish a container port. Same syntax as `docker run -p` (e.g. `8080`, `8080:8080`). Repeatable. The host side is always rewritten to `127.0.0.1` so published ports are only reachable from the local machine. |

### Picking a specific container CLI

Auto-detection prefers `docker`, then falls back to `container`. To force one:

```sh
NIX_CONTAINER_CLI=container nix-container
```

### Clearing caches

From a directory with a `shell.nix` or `default.nix`, clear only that
project's caches:

```sh
nix-container-clear-cache
```

Clear caches for *every* nix-container project:

```sh
nix-container-clear-cache --all
```

## How caching works

Caching is per-project. A short sha256 hash of the project's `shell.nix` (or
`default.nix`) is appended to the volume names, so each project gets its own
store and cache:

- `nix-container-store-<hash>` → `/nix` — the Nix store for this project.
- `nix-container-cache-<hash>` → `/home/nix/.cache` — Nix's eval and fetcher caches.

Docker seeds these from the image contents on first mount, so the bootstrap
Nix install survives. Repeated runs against the same `shell.nix` reuse what's
in the store.

Note: the hash is computed from the file *contents*, so any edit to
`shell.nix` produces a new cache (and the previous one is left orphaned until
you clear it with `--all`).

## AWS credentials

`--with-aws` runs the host's full AWS credential resolution chain
(`aws configure export-credentials`) and forwards the resolved credentials as
environment variables. It also mounts:

- `~/.aws/config` — so `aws --profile <name>` calls inside the container can
  read profile definitions.
- `~/.aws/sso/cache` — so SSO-based profiles can reuse the host's existing
  SSO session token instead of re-authenticating.
