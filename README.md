# nix-container

Run a per-project Nix development shell inside a container, without installing
Nix on the host.

## Goals

- **No dependencies on the host.** The host only needs a container CLI (`docker` or
  Apple's `container`). All Nix tooling lives inside the image.
- **Per-project shells.** Drop a `shell.nix` (or `default.nix`) into any
  directory and start a container that auto-launches `nix-shell` against it.
  Recurring options can live in a `nix-container.yml` next to it.
- **Cache across runs.** The Nix store and CLI caches are persisted in named
  volumes so packages don't re-download every invocation.

## Requirements

- A container runtime in `PATH`: `docker` or Apple's `container`.
- For the `container` backend: `jq` in `PATH` (ships with recent macOS), the
  service running (`container system start`), and a guest kernel configured
  (`container system kernel set --recommended`). See [Using Apple's
  `container`](#using-apples-container).
- For `--with-gh-token`: the `gh` CLI, authenticated.
- For `--with-aws`: the `aws` CLI (v2.13+ for `configure export-credentials`).
- For `--with-env` with 1Password references: the `op` CLI, signed in.

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

## Getting started

To scaffold a bare-bones `shell.nix` (no packages, empty shell hook) in the
current directory:

```sh
nix-container-init
```

Edit the generated `shell.nix` to add packages, then run `nix-container`. To
also fix the options that run should use (forwarded credentials, published
ports, memory, ...), add a [`nix-container.yml`](#configuration-file).

## Usage

From any directory containing a `shell.nix` or `default.nix`:

```sh
nix-container
```

The current directory is bind-mounted to `/home/nix/<dirname>` and set as the
working directory inside the container. The entrypoint is `nix-shell`, so the
container drops you into the project's resolved environment.

### Running a command and exiting

To run a command inside the project's environment instead of opening an
interactive shell, use `run`:

```sh
nix-container run npm test
```

Everything after `run` is treated as the command (and its arguments), so any
options must come before it:

```sh
nix-container --with-aws run aws s3 ls
```

The command executes inside the resolved `nix-shell` environment, its exit
status is propagated, and the container exits when it finishes. A TTY is only
allocated when both stdin and stdout are terminals, so output stays pipeable
in scripts and CI:

```sh
nix-container run node -e 'console.log(1 + 1)' | cat
```

### Options

| Flag                       | Effect                                                                                          |
| -------------------------- | ----------------------------------------------------------------------------------------------- |
| `--with-gh-token`          | Run `gh auth token` on the host and forward the result as `GH_TOKEN`.                           |
| `--with-aws`               | Export resolved AWS credentials as env vars and read-only mount `~/.aws/config` and SSO cache.  |
| `--with-aws=<profile>`     | Same as `--with-aws`, but resolves credentials from the given profile (sets `AWS_PROFILE` too). |
| `--with-npmrc`             | Read-only mount `~/.npmrc` at `/home/nix/.npmrc` if it exists on the host.                      |
| `--with-env <file>`        | Load environment variables from a dotenv-style file. If the file has 1Password references (`op://...`), they are resolved at run time via `op run`; otherwise the file is passed to the CLI's `--env-file`. See [Environment files & secrets](#environment-files--secrets). |
| `-p`, `--port <spec>`      | Publish a container port. Same syntax as `docker run -p` (e.g. `8080`, `8080:8080`). Repeatable. The host side is always rewritten to `127.0.0.1` so published ports are only reachable from the local machine. |
| `-m`, `--memory <size>`    | Amount of memory the container may use. See [Memory](#memory). |
| `--config <file>`          | Read options from `<file>` instead of `./nix-container.yml`. See [Configuration file](#configuration-file). |
| `--no-config`              | Ignore `./nix-container.yml` and use only the options given on the command line. |

### Configuration file

Anything from the table above (except `--config` itself) can be set once in a
`nix-container.yml` next to the project's `shell.nix`, so it doesn't have to be
retyped on every run:

```yaml
# nix-container.yml
with-gh-token: true
with-npmrc: true
with-aws: dev            # true for the default profile, or a profile name
with-env: .env           # relative to this file
memory: 4G
ports:
  - 8080
  - 3000:3000
```

With that file in place, a bare `nix-container` behaves like:

```sh
nix-container --with-gh-token --with-npmrc --with-aws=dev --with-env .env \
  --memory 4G -p 8080 -p 3000:3000
```

`nix-container.yaml` is accepted as well (`.yml` wins if both exist), and keys
may be spelled with underscores (`with_gh_token`) instead of dashes. Booleans
accept `true`/`false`, `yes`/`no` and `on`/`off`. Ports may also be written
inline (`ports: [8080, 3000:3000]`) or as a single value (`ports: 8080`).
The file is parsed by the script itself, so no YAML tooling is needed on the
host — which also means only this flat subset of YAML is understood. Quoted
scalars must have a matching delimiter; embedded quote escapes are not
supported. Unknown keys and malformed values are reported with a line number
and abort the run.

Precedence, highest first:

1. Command line flags.
2. `nix-container.yml` (or the file given to `--config`).
3. Environment variables (`NIX_CONTAINER_MEMORY`).

A flag overrides the corresponding key entirely: `--with-aws` ignores a profile
set in the file, and passing any `-p` replaces *all* ports from the file rather
than adding to them. To ignore the file for a single run:

```sh
nix-container --no-config
```

The file only configures how the container is run; it has no effect on the
cache volumes, which are still keyed on the contents of `shell.nix`.

### Memory

By default the container gets whatever the runtime hands out (all of the host's
memory on docker, the `container` service's default VM size on Apple's
`container`). To override that for a run:

```sh
nix-container --memory 4G
```

`<size>` is a number with an optional `K`, `M`, `G`, `T` or `P` suffix, and is
passed straight through to the container CLI's `--memory` (a hard limit on
docker, the guest VM size on Apple's `container`). Malformed values are
rejected before a container is started.

To set a default for a single project, put it in its
[`nix-container.yml`](#configuration-file):

```yaml
memory: 4G
```

To set a default for every project, export it from your shell rc:

```sh
export NIX_CONTAINER_MEMORY=8G
```

The flag takes precedence over `nix-container.yml`, which takes precedence over
the environment variable.

### Picking a specific container CLI

Auto-detection prefers `docker`, then falls back to `container`. To force one:

```sh
NIX_CONTAINER_CLI=container nix-container
```

### Using Apple's `container`

The [`container`](https://github.com/apple/container) CLI is supported as an
alternative to `docker`. It behaves differently from Docker in two ways that
nix-container handles automatically, plus some one-time host setup:

One-time setup:

```sh
container system start                             # start the service
container system kernel set --recommended          # install a guest kernel
```

`nix-container` and `nix-container-build` preflight both of these (and the
presence of `jq`) and print the exact command to run if something is missing.

What nix-container does for you:

- **Image format.** `container` only loads OCI archives, while Nix's `docker.nix`
  produces a docker-archive. `nix-container-build` converts the base image with
  `skopeo` (pulled in via Nix during the build) when building for `container`.
- **Store seeding.** Docker populates a fresh named volume from the image's
  contents on first mount; `container` mounts an empty filesystem instead. Since
  the entire Nix installation lives under `/nix`, `nix-container` seeds a new
  project's `/nix` store volume from the image on first run (a one-time copy).

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

## Environment files & secrets

`--with-env <file>` (or `with-env:` in a
[`nix-container.yml`](#configuration-file)) loads environment variables from a
dotenv-style file (`KEY=value` lines, `#` comments and blank lines ignored). It
takes one of two paths depending on the file's contents:

- **Plain values** — the file is handed straight to the container CLI's
  `--env-file`, and no extra tooling is required.
- **1Password secret references** — if any value is a
  [secret reference](https://developer.1password.com/docs/cli/secret-references/)
  (`op://<vault>/<item>/<field>`), the whole run is wrapped in `op run`, which
  resolves the references at launch time. Each variable is then forwarded into
  the container *by name*, so resolved secret values never appear in the process
  argument list (e.g. `ps`). This requires the `op` CLI in `PATH` and signed in;
  `nix-container` errors out if references are present but `op` is missing.

Example `.env` file mixing literals and secret references:

```sh
LOG_LEVEL=debug
DATABASE_URL=op://Dev/postgres/connection_string
API_KEY=op://Dev/api/key
```

```sh
nix-container --with-env .env
```

The first time a secret reference is resolved, `op` may prompt you to unlock
(e.g. via biometrics), then reuses the session for the rest of the run.
