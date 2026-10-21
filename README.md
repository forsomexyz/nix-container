# nix-container

Run a per-project Nix development shell inside a container, without installing
Nix on the host.

## Goals

- **No host Nix installation.** Install the Rust `nix-container` binary and a
  container CLI (`docker` or Apple's `container`). All Nix tooling lives inside
  the image; Rust is only needed to compile the binary.
- **Per-project shells.** Drop a `shell.nix` (or `default.nix`) into any
  directory and start a container that auto-launches `nix-shell` against it.
  Recurring options can live in a `nix-container.yml` next to it.
- **Cache across runs.** The Nix store and CLI caches are persisted in named
  volumes so packages don't re-download every invocation.
- **Persist tool state.** Opt-in container directories can use project-specific
  named volumes, keeping their contents across otherwise ephemeral runs.

## Requirements

- A container runtime in `PATH`: `docker` or Apple's `container`.
- Linux or macOS. Building the CLI requires Rust 1.85+ and a system linker.
- For the `container` backend: the service running (`container system start`)
  and a guest kernel configured (`container system kernel set --recommended`).
  JSON is parsed directly in Rust; `jq` is not required. See [Using Apple's
  `container`](#using-apples-container).
- For `gh://` secrets: the `gh` CLI, authenticated.
- For `aws://` secrets: the `aws` CLI (v2.13+ for `configure export-credentials`).
- For `--with-env` with 1Password references: the `op` CLI, signed in.

## Setup

Install from this checkout (ensure `~/.cargo/bin` is in `PATH`):

```sh
cargo install --path . --locked
```

No shell sourcing is required. Build the images on first use:

```sh
nix-container build
```

The Dockerfiles and container-side setup scripts are embedded in the binary,
so the installed CLI works without this checkout. After changing those files,
reinstall the binary before rebuilding the images. The build produces:

- `nix-container-base-build:latest` — intermediate, used to produce the base tarball.
- `nix-container-base:latest` — base image with Nix, direnv, nix-direnv.
- `nix-container:latest` — the image used at runtime.

### Migrating from the shell implementation

Most run flags, flat YAML configuration, environment variables and volume
names are retained; gh, npmrc and AWS options now use `--secret` references.
The management commands are now `nix-container init`,
`nix-container build` and `nix-container clear-cache`. To keep their old hyphenated
names, optionally source `nix-container.sh` from bash/zsh; it now contains only
three compatibility wrappers calling the installed binary. Restart your shell
after upgrading to discard any previously sourced implementation.

Two validation changes are intentional: extra mounts cannot overlap other mounts
(including managed paths), and container paths containing `.`/`..` or repeated
slashes are rejected. An explicit empty config list suppresses environment
fallbacks. Projects reached through symlinks use their physical directory name
for the container name and working directory.

## Getting started

To scaffold a bare-bones `shell.nix` (no packages, empty shell hook) in the
current directory:

```sh
nix-container init
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
container drops you into the project's resolved environment. On a terminal,
`nix-container` shows a five-line cyberpunk boot panel during startup, with neon
cyan/magenta hex data, glitch accents, and a sweeping uplink scanner. The scanner
loops rather than reporting build progress, and the panel clears before the
shell takes over. The splash stays up while dependencies are
resolved, so it ends when the shell is ready rather than when the container
starts. A cold cache stays behind the splash too, however long it takes; use
`--verbose` to watch it. Small terminals, redirected output, and
`nix-container run ...` stay quiet.

To resolve dependencies without opening a shell first, the splash path builds
the project's expression with an emptied `shellHook`. This keeps the hook from
running twice per session; it does not change which dependencies are built, so
anything the hook itself builds is still resolved later, in the real shell.

### Watching startup

When startup is slow or fails, `--verbose` (`-v`) replaces the splash with
everything it would have hidden — volume seeding, network creation, and the full
nix build log:

```sh
nix-container --verbose
```

This takes the same path as a normal start, so what you see is what a silent
startup is doing.
Other host directories can be added with [`--mount`](#mounting-extra-directories).

### Running a command and exiting

To run a command inside the project's environment instead of opening an
interactive shell, use `run`:

```sh
nix-container run npm test
```

Everything after `run` is treated as the command (and its arguments), so any
options must come before it:

```sh
nix-container --secret aws://default run aws s3 ls
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
| `--secret <ref>`           | Use a secret reference to provide environment variables or files (repeatable). See [Secret references](#secret-references). |
| `--with-shared-nix-exprs <name>=<dir>`  | Mount `<dir>` read-only and add it to `NIX_PATH` as `<name>`, so the project's nix files can `import <name>`. Repeatable. See [Sharing Nix expressions across projects](#sharing-nix-expressions-across-projects). |
| `--mount <spec>`           | Bind-mount an extra host path into the container. `<spec>` is `host[:container][:ro\|:rw]`. Repeatable. See [Mounting extra directories](#mounting-extra-directories). |
| `--persist <path>`         | Back a container directory with a project-specific named volume. Repeatable. See [Persisting container data](#persisting-container-data). |
| `--with-env <file>`        | Load environment variables from a dotenv-style file. If the file has 1Password references (`op://...`), they are resolved at run time via `op run`; otherwise the file is passed to the CLI's `--env-file`. See [Environment files & secrets](#environment-files--secrets). |
| `-p`, `--port <spec>`      | Publish a container port. Same syntax as `docker run -p` (e.g. `8080`, `8080:8080`). Repeatable. The host side is always rewritten to `127.0.0.1` so published ports are only reachable from the local machine. |
| `-m`, `--memory <size>`    | Amount of memory the container may use. See [Memory](#memory). |
| `--network <name>`        | Connect to a named container network (or a runtime-supported mode such as `host` or `none`). Defaults to the runtime's network when unset. |
| `--config <file>`          | Read options from `<file>` instead of `./nix-container.yml`. See [Configuration file](#configuration-file). |
| `--no-config`              | Ignore `./nix-container.yml` and use only the options given on the command line. |

### Configuration file

Anything from the table above (except `--config` itself) can be set once in a
`nix-container.yml` next to the project's `shell.nix`, so it doesn't have to be
retyped on every run:

```yaml
# nix-container.yml
secrets: [gh://github.com/alice, npmrc://default, aws://profile/dev]
with-env: .env           # relative to this file
with-shared-nix-exprs: common=~/nix-common
memory: 4G
network: dev-network    # an existing network (or a runtime-supported mode)
ports:
  - 8080
  - 3000:3000
mounts:
  - ~/datasets:ro
  - ../shared-lib:/home/nix/shared-lib
persistence:
  - .local/share/my-tool
  - /var/lib/postgres
```

With that file in place, a bare `nix-container` behaves like:

```sh
nix-container --secret gh://github.com/alice --secret npmrc://default \
  --secret aws://profile/dev --with-env .env \
  --with-shared-nix-exprs common=~/nix-common --memory 4G --network dev-network \
  -p 8080 -p 3000:3000 \
  --mount ~/datasets:ro --mount ../shared-lib:/home/nix/shared-lib \
  --persist .local/share/my-tool --persist /var/lib/postgres
```

See [Networks](#networks) for what `network` does.

`nix-container.yaml` is accepted as well (`.yml` wins if both exist), and keys
may be spelled with underscores (`with_shared_nix_exprs`) instead of dashes.
List options such as secrets, ports, mounts and persistence may also be written
inline (`ports: [8080, 3000:3000]`) or as a single value (`ports: 8080`). The file is parsed directly in Rust,
so no YAML tooling is needed on the host. Only this flat subset of YAML is
understood; flow-list items cannot contain commas. Quoted scalars must have a matching delimiter;
embedded quote escapes are not
supported. Unknown keys and malformed values are reported with a line number
and abort the run.

Precedence, highest first:

1. Command line flags (including `--network`, which overrides `network` in the file).
2. `nix-container.yml` (or the file given to `--config`).
3. Environment variables (`NIX_CONTAINER_MEMORY`, `NIX_CONTAINER_SHARED_NIX_EXPRS`).

A flag overrides the corresponding key entirely: passing any `--secret`,
`-p`, `--mount`, `--persist`, or `--with-shared-nix-exprs` replaces *all*
references, ports, mounts, persistence locations, or shared expression dirs
from the file rather than adding to them. To ignore the file for a single run:

```sh
nix-container --no-config
```

The file does not change how cache volumes are keyed: those still follow the
contents of `shell.nix`. Persistence volumes use the project directory and
container location instead, so they stay attached when `shell.nix` changes.

### Mounting extra directories

Only the project directory is mounted by default. To make another host path
visible inside the container, use `--mount`:

```sh
nix-container --mount ~/datasets
```

The spec is `host[:container][:ro|:rw]`:

| Spec                         | Effect                                                        |
| ---------------------------- | ------------------------------------------------------------- |
| `~/datasets`                 | Read-write at `/home/nix/datasets`.                            |
| `~/datasets:ro`              | Read-only at `/home/nix/datasets`.                             |
| `~/datasets:/srv/data`       | Read-write at `/srv/data`.                                     |
| `~/datasets:/srv/data:ro`    | Read-only at `/srv/data`.                                      |

In other words, the container path defaults to `/home/nix/` plus the last
component of the host path, and the mode defaults to `rw` — the same default
`docker run -v` uses. A second field that isn't `ro` or `rw` is read as the
container path, which must be absolute.

The flag is repeatable, takes files as well as directories, and expands `~`.
Relative host paths resolve against the current directory (or, in
[`nix-container.yml`](#configuration-file), against the config file's own
directory):

```sh
nix-container --mount ../shared-lib --mount ~/.gitconfig:/home/nix/.gitconfig:ro
```

A few things are rejected up front rather than at container startup: a host
path that doesn't exist (docker would otherwise create it as a new root-owned
directory that looks like an empty mount), two mounts landing on the same
container path, and container paths overlapping another mount or a path that nix-container
manages itself (`/nix`, `/home/nix/.cache`, `/home/nix/.nix-container-persist`
and the project directory). Host paths containing a `:`
aren't supported, the same limitation `docker run -v` has.

Writes through a read-write mount are real writes to the host directory, and
files created inside the container are owned by the host user (the image is
built with the host's uid/gid). Mounts are not part of the [cache
key](#how-caching-works) — only `shell.nix` is.

### Persisting container data

The container is removed after each run, so state written outside the
bind-mounted project directory normally disappears. `--persist` backs a
container directory with a named volume instead:

```sh
nix-container --persist .local/share/my-tool
```

Relative locations are placed under `/home/nix`; the example above therefore
mounts at `/home/nix/.local/share/my-tool`. Absolute paths are accepted too,
and `~` means the container user's home (an unquoted `~` expanded by the host
shell is handled the same way). The option is repeatable:

```sh
nix-container --persist ~/.config/my-tool --persist /var/lib/postgres
```

The volume name (`nix-container-persist-<project-hash>-<location-hash>`) is
derived from the physical project directory and normalized container path. As
a result, each project and location has separate state, and the volume remains
attached across subsequent `nix-container` runs, edits to
`shell.nix`, image rebuilds, and `nix-container clear-cache`. Moving the project
to a different host path intentionally gives it a fresh set of volumes.
Fresh volumes are initialized with any directory contents baked into the image
and made writable by the container's `nix` user.

A location nested inside the container's home — `~/.local/share/my-tool`, but
not `~/.npm` — is not mounted at that path directly. Container runtimes create
the directories leading up to a mount point themselves, as `root`, which would
leave `~/.local` and `~/.local/share` owned by `root` and unwritable for the
container user. Such a volume is mounted under
`/home/nix/.nix-container-persist/` (mirroring the path below the home)
instead, and the location becomes a symlink to it, created at startup by the
non-root container user along with its parent directories. The data and
everything around it in the home therefore stay writable; only the location
itself is a symlink rather than a real directory. Locations outside the home
(`/var/lib/postgres`) are mounted where they were asked for, next to the
root-owned system directories they belong to.

The same locations can be kept in `nix-container.yml` (the singular key
`persist` is accepted as an alias):

```yaml
persistence:
  - .local/share/my-tool
  - /var/lib/postgres
```

A persistence location cannot overlap `/nix`, `/home/nix/.cache`,
`/home/nix/.nix-container-persist`, the project mount, or another mount. The
project directory itself is already persistent on the host; use
[`--mount`](#mounting-extra-directories) instead when data should
be directly accessible as host files rather than managed by the container
runtime. Persistence volumes are deliberately not removed by
`nix-container clear-cache`, because they may contain user data.

### Sharing Nix expressions across projects

Copying the same `mkShell` boilerplate into every project's `shell.nix`
invites divergence. Instead, keep the shared parts in one directory on the
host and hand it to nix-container:

```sh
nix-container --with-shared-nix-exprs common=~/nix-common
```

The directory is bind-mounted read-only at `/home/nix/.nix-shared-exprs/common` inside
the container, and `common=/home/nix/.nix-shared-exprs/common` is prepended to
`NIX_PATH` (the image's own search path entries are kept, so `<nixpkgs>` still
resolves to the baked channel). Nix's angle-bracket lookup then resolves
`<common>` to the shared directory. Structure the shared code as a function so
each project overrides only what differs:

```nix
# ~/nix-common/default.nix
{ pkgs ? import <nixpkgs> {} }:
{
  inherit pkgs;
  mkProjectShell = { packages ? [], shellHook ? "", ... } @ args:
    pkgs.mkShellNoCC (args // {
      packages = (with pkgs; [ git jq ]) ++ packages;
      shellHook = ''
        export LANG=C.UTF-8
      '' + shellHook;
    });
}
```

```nix
# a project's shell.nix
let common = import <common> {};
in common.mkProjectShell {
  packages = with common.pkgs; [ nodejs ];
}
```

`--with-shared-nix-exprs` is repeatable (`--with-shared-nix-exprs common=...
--with-shared-nix-exprs ci=...`). To pin it per project, use [`nix-container.yml`](#configuration-file):

```yaml
with-shared-nix-exprs:
  - common=~/nix-common
```

To set a default for every project, export a colon-separated list from your
shell rc:

```sh
export NIX_CONTAINER_SHARED_NIX_EXPRS=common=$HOME/nix-common
```

Flags override the config file, which overrides the environment variable.
Relative directories given in `nix-container.yml` resolve against the config
file's own directory; `~` is expanded in all sources.

Two things to keep in mind:

- Edits to the shared directory take effect on the next run — `nix-shell`
  re-evaluates every time and the mount tracks the host directory. The cache
  volumes stay keyed on the project's `shell.nix` contents only, so a changed
  lib reuses the existing store volume (packages already realised stay
  cached).
- The lookup is only as reproducible as the directory's contents. If you need
  machine-independent pinning (e.g. CI), commit the shared directory to a repo
  and either check it out at a fixed revision on each machine, or skip
  `--with-shared-nix-exprs` and `import (builtins.fetchGit { url = ...; rev = ...; })`
  directly in `shell.nix`.

### Networks

By default the container joins whatever network the runtime hands out. To put
it on a specific one:

```sh
nix-container --network dev-network
```

The value is a single network name or mode passed to the container CLI's
`--network` option. On first use, a missing named network is created with the
runtime's default network settings; it is reused on subsequent runs and is not
removed with the container or by `nix-container clear-cache`. Runtime modes
such as `host`, `none`, `bridge` and `default` are not created. If omitted, the
runtime's default network is used. Available modes depend on the runtime.

On a named network, containers reach each other by name — a nix-container run
is named after its project directory, so another container on `dev-network`
reaches it at that name. There are no additional DNS aliases: Apple's
`container` CLI has no equivalent of docker's `--network-alias`, so the option
is not offered on either runtime.

To set the network for a single project, put it in its
[`nix-container.yml`](#configuration-file):

```yaml
network: dev-network
```

The flag takes precedence over `nix-container.yml`.

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
alternative to `docker`. Its backend handles differences from Docker
automatically, plus some one-time host setup:

One-time setup:

```sh
container system start                             # start the service
container system kernel set --recommended          # install a guest kernel
```

`nix-container` and `nix-container build` preflight both of these and print
the exact command to run if something is missing.

What nix-container does for you:

- **Image format.** `container` only loads OCI archives, while Nix's `docker.nix`
  produces a docker-archive. `nix-container build` converts the base image with
  `skopeo` (pulled in via Nix during the build) when building for `container`.
- **Store seeding.** Docker populates a fresh named volume from the image's
  contents on first mount; `container` mounts an empty filesystem instead. Since
  the entire Nix installation lives under `/nix`, `nix-container` seeds a new
  project's `/nix` store volume from the image on first run (a one-time copy),
  and assigns the cache volume to the host uid/gid so Nix can write to it.
- **Session identity.** The backend supplies a numeric uid/gid and
  `HOME=/home/nix` instead of relying on the image's named `USER nix`.
  Root seeding helpers and the image builder keep their own identities.

### Clearing caches

From a directory with a `shell.nix` or `default.nix`, clear only that
project's caches:

```sh
nix-container clear-cache
```

Clear caches for *every* nix-container project:

```sh
nix-container clear-cache --all
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

## Secret references

Use repeatable `--secret REF` options, or `secrets:` in `nix-container.yml`.
The scheme selects a built-in provider; references contain identifiers, not
secret values:

| Reference | Delivery |
| --- | --- |
| `gh://<host>/<account>` | Run `gh auth token --hostname <host> --user <account>` on the host; forward `GH_TOKEN` by name. Both host and account are required. |
| `npmrc://default` | Mount the host's `~/.npmrc` read-only at `/home/nix/.npmrc`, if present. |
| `aws://default` | Resolve AWS credentials using the host's default profile and mount AWS config and SSO cache. |
| `aws://profile/<name>` | As above, selecting the given AWS profile and setting `AWS_PROFILE`. |

For example: `nix-container --secret gh://github.com/alice --secret npmrc://default`.
A reference with an unknown scheme or invalid shape is rejected. Duplicate
file mounts or environment names fail rather than silently overwriting another
secret. Env values stay out of runtime arguments; file references mount host
files, so secrets remain accessible to processes inside the container.
`op://` references in `--with-env` files still use `op run` as described below.

## AWS credentials

The `aws://default` and `aws://profile/<name>` references run the host's full
AWS credential resolution chain (`aws configure export-credentials`) and forward
the resolved credentials as environment variables. They also mount:

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

## Development

```sh
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

`nix-shell` provides the Rust toolchain, linker and ShellCheck if Nix is available.
Tests use mock runtimes and credential helpers; they do not need Docker, Apple's
`container`, Nix, or real credentials. CI runs them on Linux and macOS. Real
image builds and interactive runtime behavior still require testing with an
installed container runtime.

The host implementation lives in `src/`: CLI/config parsing in `options.rs`,
path and volume naming in `paths.rs`, shared process execution in `runtime.rs`,
run planning in `run.rs`, and init/build/cache policy in `manage.rs`. Only the
small container-side build and startup scripts remain shell, because they run
inside the Nix image. `nix-container.sh` is optional compatibility glue.

### Backend interface

`src/backend/mod.rs` defines the application-specific `Backend` interface;
`docker.rs` and `apple.rs` implement it. Application code passes a structured
`RunPlan`, never runtime CLI flags or backend capability checks.

| Operation | Contract |
| --- | --- |
| `preflight` | Check prerequisites without starting services or installing kernels. |
| `image_id` | Return an opaque image identity, `None` for absence, or a runtime/inspection error. |
| `build_images` | Build the builder, produce/import the backend's base archive, then build the session image. |
| `prepare_run` | Prepare resources and return opaque `PreparedRun` state; never execute the user's command. |
| `command` | Describe a session or probe process without spawning it. |
| `list_volumes` | Enumerate names without deciding which contain disposable data. |
| `remove_volume` | Non-forcing removal, distinguishing missing volumes from failures. |

Apple owns service/kernel checks, explicit cache seeding, numeric session
identity, and OCI archive selection. Docker uses native cache population and
image identity. Private helpers in `backend/cli.rs` share CLI encoding, build
sequencing, network setup, and persistence initialization/staging. Persistence
helpers remain necessary on both backends, particularly when the seed source
and staged mount path differ.

Credentials, 1Password wrapping, terminal handling and signal forwarding stay
in the common execution layer. Cache invalidation and the bounded stale-cache
probe/retry remain application policy; recovery calls `prepare_run` again so
each backend can restore its resources. Persistence volumes are never selected
for automatic deletion.

Preparation preserves the existing volume-existence readiness check. It is not
transactional: a failed initialization can leave a volume that a later run
reuses. Failed persistence volumes are retained to avoid deleting user data.
Mock tests cover orchestration and argv; real runtime tests are still needed
for filesystem ownership, copy-up and interactive terminal behavior.
