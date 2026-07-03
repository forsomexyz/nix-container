# This file is meant to be sourced from a bash or zsh shell rc, not executed.
# shellcheck shell=bash
if [ -n "${BASH_VERSION:-}" ]; then
    _nix_container_src="${BASH_SOURCE[0]}"
elif [ -n "${ZSH_VERSION:-}" ]; then
    eval '_nix_container_src="${(%):-%x}"'
else
    _nix_container_src="$0"
fi

_NIX_CONTAINER_SCRIPT_DIR=$(
    src="$_nix_container_src"
    while [ -L "$src" ]; do
        target=$(readlink "$src")
        [[ "$target" = /* ]] && src="$target" || src="$(cd -P "$(dirname "$src")" && pwd)/$target"
    done
    cd -P "$(dirname "$src")" && pwd
)
unset _nix_container_src

# Normalize a docker `-p` spec to always bind the host side to 127.0.0.1, so
# published ports are never reachable from other machines on the network.
# Handles: `<container>`, `<host>:<container>`, `<ip>:<host>:<container>`,
# all optionally suffixed with `/<proto>`.
_nix_container_force_loopback() {
    local spec="$1"
    local proto=""
    if [[ "$spec" == */* ]]; then
        proto="/${spec##*/}"
        spec="${spec%/*}"
    fi

    local colons="${spec//[^:]/}"
    case "${#colons}" in
        0) echo "127.0.0.1:${spec}:${spec}${proto}" ;;
        1) echo "127.0.0.1:${spec}${proto}" ;;
        2) echo "127.0.0.1:${spec#*:}${proto}" ;;
        *) echo "${spec}${proto}" ;;
    esac
}

# Echo the project's nix file (shell.nix or default.nix) in the current
# directory, or return non-zero if neither exists.
_nix_container_nix_file() {
    if [ -f shell.nix ]; then
        echo shell.nix
    elif [ -f default.nix ]; then
        echo default.nix
    else
        return 1
    fi
}

# Compute a short content hash of the current directory's shell.nix or
# default.nix. Used to namespace per-project cache volumes.
_nix_container_project_hash() {
    local file
    file=$(_nix_container_nix_file) || return 1

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | cut -c1-8
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | cut -c1-8
    else
        echo "nix-container: no sha256 tool found (need sha256sum or shasum)" >&2
        return 1
    fi
}

# Resolve which container CLI to use. Honors $NIX_CONTAINER_CLI as an override,
# otherwise auto-detects from a list of docker-CLI-compatible tools.
_nix_container_cli() {
    if [ -n "${NIX_CONTAINER_CLI:-}" ]; then
        if command -v "$NIX_CONTAINER_CLI" >/dev/null 2>&1; then
            echo "$NIX_CONTAINER_CLI"
            return 0
        fi
        echo "nix-container: NIX_CONTAINER_CLI=$NIX_CONTAINER_CLI not found in PATH" >&2
        return 1
    fi

    local candidate
    for candidate in docker container; do
        if command -v "$candidate" >/dev/null 2>&1; then
            echo "$candidate"
            return 0
        fi
    done

    echo "nix-container: no supported container CLI found (tried: docker, container). Set NIX_CONTAINER_CLI to override." >&2
    return 1
}

# Classify the resolved CLI. Apple's `container` needs different handling from
# docker-compatible CLIs in a few places (image inspection format, volume
# seeding, preflight checks); everything else is treated as docker-compatible.
_nix_container_cli_kind() {
    case "$(basename "$1")" in
        container) echo container ;;
        *) echo docker ;;
    esac
}

# Echo a stable image id for the given image, or empty if it doesn't exist.
# Used to detect whether the base image changed across a rebuild. docker
# supports Go-template formatting; `container` only emits JSON, so parse it.
_nix_container_image_id() {
    local cli="$1" image="$2"
    if [ "$(_nix_container_cli_kind "$cli")" = container ]; then
        "$cli" image inspect "$image" 2>/dev/null | jq -r '.[0].id // empty'
    else
        "$cli" image inspect --format '{{.Id}}' "$image" 2>/dev/null
    fi
}

# Verify the `container` backend is usable before we try to run anything. No-op
# for docker-compatible CLIs. Checks: jq present (needed to parse JSON output),
# the container service is running, and a guest kernel is configured.
_nix_container_preflight() {
    local cli="$1"
    [ "$(_nix_container_cli_kind "$cli")" = container ] || return 0

    if ! command -v jq >/dev/null 2>&1; then
        echo "nix-container: the 'container' backend requires 'jq' in PATH" >&2
        return 1
    fi

    local svc_status
    svc_status=$("$cli" system status --format json 2>/dev/null | jq -r '.status // empty')
    if [ "$svc_status" != "running" ]; then
        echo "nix-container: container service not running — run 'container system start'" >&2
        return 1
    fi

    if ! "$cli" system property list --format json 2>/dev/null \
            | jq -e '.kernel.binaryPath // empty' >/dev/null; then
        echo "nix-container: no guest kernel configured — run 'container system kernel set --recommended'" >&2
        return 1
    fi
}

# Prepare a project's cache volumes for the `container` backend. Docker
# auto-populates a fresh named volume from the image's contents at the mount
# path and inherits the image's ownership; Apple's `container` mounts an empty,
# root-owned filesystem instead. Two problems follow:
#   - An empty /nix would hide the entire Nix installation and break startup, so
#     seed it from the image (mounted at a staging path where the image's own
#     /nix is still visible). Idempotent: only copies when the volume is empty.
#   - A root-owned ~/.cache can't be written by the (non-root) run user, so nix
#     can't write its binary-cache database and silently rebuilds everything
#     from source. chown it to the run user.
# Runs as root so it can write both volumes regardless of their current owner.
_nix_container_prepare_volumes() {
    local cli="$1" store_vol="$2" cache_vol="$3" owner="$4"
    "$cli" run --rm --user 0 --entrypoint sh \
        -v "$store_vol:/seed-nix" \
        -v "$cache_vol:/seed-cache" \
        nix-container:latest \
        -c "[ -e /seed-nix/store ] || cp -a /nix/. /seed-nix/; chown $owner /seed-cache"
}

nix-container-init() {
    local force=0
    while [ $# -gt 0 ]; do
        case "$1" in
            -h|--help)
                cat <<'EOF'
Usage: nix-container-init [options]

Create a bare-bones shell.nix in the current directory to get started.
The generated file has no packages and an empty shell hook.

Options:
  --force       Overwrite an existing shell.nix.
  -h, --help    Show this help message.
EOF
                return 0
                ;;
            --force)
                force=1
                shift
                ;;
            *)
                echo "nix-container-init: unknown option: $1" >&2
                return 1
                ;;
        esac
    done

    if [ -f shell.nix ] && [ "$force" -ne 1 ]; then
        echo "nix-container-init: shell.nix already exists (use --force to overwrite)" >&2
        return 1
    fi

    cat > shell.nix <<'EOF'
let pkgs = import <nixpkgs> {};
in
pkgs.mkShellNoCC {
  packages = with pkgs; [ ];
  shellHook = ''
    # Sane defaults for unicode and truecolor terminal support.
    export LANG=C.UTF-8        # use a UTF-8 locale for unicode handling
    export LC_ALL=C.UTF-8      # override any inherited locale categories
    export COLORTERM=truecolor # advertise 24-bit color support to programs
  '';
}
EOF

    echo "nix-container-init: created shell.nix"
}

nix-container-build() {
    local dir="$_NIX_CONTAINER_SCRIPT_DIR"
    if [ -z "$dir" ]; then
        echo "nix-container-build: script directory not set" >&2
        return 1
    fi

    local -a build_args=()
    while [ $# -gt 0 ]; do
        case "$1" in
            -h|--help)
                cat <<'EOF'
Usage: nix-container-build [options]

Build the nix-container base and runtime images.

Options:
  --no-cache    Build images without using the container build cache.
  -h, --help    Show this help message.
EOF
                return 0
                ;;
            --no-cache)
                build_args+=(--no-cache)
                shift
                ;;
            *)
                echo "nix-container-build: unknown option: $1" >&2
                return 1
                ;;
        esac
    done

    local cli
    cli=$(_nix_container_cli) || return 1
    _nix_container_preflight "$cli" || return 1

    # `container` only loads OCI archives; docker loads the docker-archive that
    # docker.nix produces. build-base.sh emits the format we ask for here.
    local kind image_format artifact_glob
    kind=$(_nix_container_cli_kind "$cli")
    if [ "$kind" = container ]; then
        image_format=oci
        artifact_glob='*-oci.tar'
    else
        image_format=docker
        artifact_glob='*.tar.gz'
    fi

    local prev_base_id
    prev_base_id=$(_nix_container_image_id "$cli" nix-container-base:latest)

    "$cli" build "${build_args[@]}" -t nix-container-base-build:latest -f "$dir/Dockerfile.base-build" "$dir" || return 1

    local out
    out=$(mktemp -d) || return 1
    "$cli" run --rm \
        -e "HOST_UID=$(id -u)" \
        -e "HOST_GID=$(id -g)" \
        -e "IMAGE_FORMAT=$image_format" \
        -v "$out:/tmp/out" \
        nix-container-base-build:latest || { rm -rf "$out"; return 1; }

    local tarball=""
    local f
    while IFS= read -r f; do
        if [ -z "$tarball" ] || [ "$f" -nt "$tarball" ]; then
            tarball=$f
        fi
    done < <(find "$out" -maxdepth 1 -name "$artifact_glob" 2>/dev/null)
    if [ -z "$tarball" ]; then
        echo "nix-container-build: no image artifact ($artifact_glob) produced in $out" >&2
        rm -rf "$out"
        return 1
    fi

    "$cli" image load -i "$tarball" || { rm -rf "$out"; return 1; }

    rm -rf "$out"

    "$cli" build "${build_args[@]}" -t nix-container:latest -f "$dir/Dockerfile" "$dir" || return 1

    local new_base_id
    new_base_id=$(_nix_container_image_id "$cli" nix-container-base:latest)

    # The /nix store and ~/.cache named volumes are populated from the image on
    # first use only; if the base image changed, existing volumes hold stale
    # store paths that no longer exist in the new image. Drop them so the next
    # `nix-container` run repopulates from the fresh image.
    if [ -n "$new_base_id" ] && [ "$prev_base_id" != "$new_base_id" ]; then
        local -a stale_volumes=()
        local vol
        while IFS= read -r vol; do
            [ -n "$vol" ] && stale_volumes+=("$vol")
        done < <("$cli" volume ls --quiet 2>/dev/null | grep -E '^nix-container-(store|cache)-')
        if [ "${#stale_volumes[@]}" -gt 0 ]; then
            echo "nix-container-build: base image changed, invalidating ${#stale_volumes[@]} cache volume(s)"
            for vol in "${stale_volumes[@]}"; do
                if ! "$cli" volume rm "$vol" >/dev/null 2>&1; then
                    echo "nix-container-build: could not remove $vol (still in use? stop running containers and rerun nix-container-clear-cache --all)" >&2
                fi
            done
        fi
    fi
}

nix-container-clear-cache() {
    local all=0
    while [ $# -gt 0 ]; do
        case "$1" in
            -h|--help)
                cat <<'EOF'
Usage: nix-container-clear-cache [options]

Remove cache volumes used by nix-container. By default removes only the
volumes for the current project (based on shell.nix or default.nix).

Options:
  --all         Remove cache volumes for all nix-container projects.
  -h, --help    Show this help message.
EOF
                return 0
                ;;
            --all)
                all=1
                shift
                ;;
            *)
                echo "nix-container-clear-cache: unknown option: $1" >&2
                return 1
                ;;
        esac
    done

    local cli
    cli=$(_nix_container_cli) || return 1

    local -a volumes=()
    if [ "$all" -eq 1 ]; then
        local vol
        while IFS= read -r vol; do
            [ -n "$vol" ] && volumes+=("$vol")
        done < <("$cli" volume ls --quiet 2>/dev/null | grep -E '^nix-container-(store|cache)-')
        if [ "${#volumes[@]}" -eq 0 ]; then
            echo "nix-container-clear-cache: no nix-container caches found"
            return 0
        fi
    else
        local hash
        hash=$(_nix_container_project_hash) || {
            echo "nix-container-clear-cache: no shell.nix or default.nix in current directory (use --all to clear all caches)" >&2
            return 1
        }
        volumes=("nix-container-store-$hash" "nix-container-cache-$hash")
    fi

    local volume
    for volume in "${volumes[@]}"; do
        if "$cli" volume inspect "$volume" >/dev/null 2>&1; then
            "$cli" volume rm "$volume" || return 1
        else
            echo "nix-container-clear-cache: volume $volume does not exist, skipping"
        fi
    done
}

nix-container() {
    local with_gh_token=0
    local with_aws=0
    local with_npmrc=0
    local aws_profile=""
    local -a port_args=()
    while [ $# -gt 0 ]; do
        case "$1" in
            -h|--help)
                cat <<'EOF'
Usage: nix-container [options]

Run a containerized nix-shell for the shell.nix or default.nix in the
current directory.

Options:
  --with-gh-token       Forward a GitHub token from 'gh auth token' as GH_TOKEN.
  --with-aws            Forward AWS credentials from the host (uses default profile).
  --with-aws=PROFILE    Forward AWS credentials using the given profile.
  --with-npmrc          Mount ~/.npmrc into the container (read-only) if present.
  -p, --port SPEC       Publish a port. Host side is always bound to 127.0.0.1.
                        SPEC: <container>, <host>:<container>, or <ip>:<host>:<container>,
                        optionally suffixed with /<proto>.
  -h, --help            Show this help message.
EOF
                return 0
                ;;
            --with-gh-token)
                with_gh_token=1
                shift
                ;;
            --with-aws)
                with_aws=1
                shift
                ;;
            --with-aws=*)
                with_aws=1
                aws_profile="${1#--with-aws=}"
                shift
                ;;
            --with-npmrc)
                with_npmrc=1
                shift
                ;;
            -p|--port)
                if [ -z "${2:-}" ]; then
                    echo "nix-container: $1 requires a value (e.g. 8080 or 8080:8080)" >&2
                    return 1
                fi
                port_args+=(-p "$(_nix_container_force_loopback "$2")")
                shift 2
                ;;
            -p=*|--port=*)
                port_args+=(-p "$(_nix_container_force_loopback "${1#*=}")")
                shift
                ;;
            *)
                echo "nix-container: unknown option: $1" >&2
                return 1
                ;;
        esac
    done

    _nix_container_nix_file >/dev/null || {
        echo "nix-container: no shell.nix or default.nix in current directory" >&2
        return 1
    }

    local cli
    cli=$(_nix_container_cli) || return 1
    _nix_container_preflight "$cli" || return 1

    if ! "$cli" image inspect nix-container:latest >/dev/null 2>&1; then
        echo "nix-container: image nix-container:latest not found. Run 'nix-container-build' to build it." >&2
        return 1
    fi

    local -a env_args=()
    local -a mount_args=()
    if [ "$with_gh_token" -eq 1 ]; then
        local token
        token=$(gh auth token 2>/dev/null)
        if [ -z "$token" ]; then
            echo "nix-container: failed to get token from 'gh auth token'" >&2
            return 1
        fi
        env_args+=(-e "GH_TOKEN=$token")
    fi

    if [ "$with_npmrc" -eq 1 ] && [ -f "$HOME/.npmrc" ]; then
        mount_args+=(-v "$HOME/.npmrc:/home/nix/.npmrc:ro")
    fi

    if [ "$with_aws" -eq 1 ]; then
        if ! command -v aws >/dev/null 2>&1; then
            echo "nix-container: --with-aws requires the 'aws' CLI in PATH" >&2
            return 1
        fi
        if [ -f "$HOME/.aws/config" ]; then
            mount_args+=(-v "$HOME/.aws/config:/home/nix/.aws/config:ro")
        fi
        # Mount the SSO token cache so `aws --profile <sso-profile>` calls inside
        # the container can reuse the host's SSO session instead of re-authenticating.
        if [ -d "$HOME/.aws/sso/cache" ]; then
            mount_args+=(-v "$HOME/.aws/sso/cache:/home/nix/.aws/sso/cache")
        fi
        local -a aws_cmd=(aws)
        if [ -n "$aws_profile" ]; then
            aws_cmd+=(--profile "$aws_profile")
        fi
        aws_cmd+=(configure export-credentials --format env-no-export)
        local aws_creds
        aws_creds=$("${aws_cmd[@]}" 2>/dev/null)
        if [ -z "$aws_creds" ]; then
            echo "nix-container: --with-aws: '${aws_cmd[*]}' returned no credentials" >&2
            return 1
        fi
        local line
        while IFS= read -r line; do
            [ -n "$line" ] && env_args+=(-e "$line")
        done <<< "$aws_creds"

        # Forward AWS_PROFILE for tools that inspect it as metadata. The credential
        # env vars above take precedence in the resolution chain, so this won't
        # cause auth to fall back to a missing profile file inside the container.
        if [ -n "$aws_profile" ]; then
            env_args+=(-e "AWS_PROFILE=$aws_profile")
        elif [ -n "${AWS_PROFILE:-}" ]; then
            env_args+=(-e "AWS_PROFILE=$AWS_PROFILE")
        fi
    fi

    local hash
    hash=$(_nix_container_project_hash) || return 1

    local kind
    kind=$(_nix_container_cli_kind "$cli")

    # Apple's `container` needs extra run flags that docker doesn't:
    #   --user  the image's `USER nix` (a name) fails to start on `container`;
    #           a numeric uid:gid works, and since the base is built with the
    #           host uid/gid it maps exactly to the nix user (so files created
    #           in the bind-mounted project dir are owned by the host user too).
    #   HOME    ensure it's set when running by numeric uid.
    local -a cli_run_args=()
    if [ "$kind" = container ]; then
        cli_run_args=(--user "$(id -u):$(id -g)" -e "HOME=/home/nix")

        # Docker seeds/owns fresh volumes from the image automatically;
        # `container` does not, so prepare them the first time this project's
        # volumes are created (seed /nix, chown ~/.cache). Once the store volume
        # exists it's already prepared, so skip the (VM-starting) step after.
        local store_vol="nix-container-store-$hash"
        if ! "$cli" volume inspect "$store_vol" >/dev/null 2>&1; then
            _nix_container_prepare_volumes "$cli" "$store_vol" \
                "nix-container-cache-$hash" "$(id -u):$(id -g)" || {
                echo "nix-container: failed to prepare the /nix and cache volumes" >&2
                return 1
            }
        fi
    fi

    local dir
    dir=$(basename "$PWD")

    local -a run_args=(
        --rm
        --name "$dir"
        "${cli_run_args[@]}"
        "${env_args[@]}"
        "${mount_args[@]}"
        "${port_args[@]}"
        -v "nix-container-store-$hash:/nix"
        -v "nix-container-cache-$hash:/home/nix/.cache"
        -v "$PWD:/home/nix/$dir"
        -w "/home/nix/$dir"
        nix-container:latest
        --extra-experimental-features flakes
    )

    local start=$SECONDS
    "$cli" run -it "${run_args[@]}"
    local rc=$?

    # If the container exited quickly with an error, it likely failed during
    # nix-shell startup. The most common cause is a stale /nix volume that was
    # populated by a previous (now-rebuilt) base image. Probe non-interactively
    # to confirm, and if so, drop the project's cache volumes and retry once.
    if [ $rc -ne 0 ] && [ $((SECONDS - start)) -lt 3 ]; then
        local probe
        probe=$("$cli" run "${run_args[@]}" --run true 2>&1)
        if echo "$probe" | grep -q "cannot determine user's home directory"; then
            echo "nix-container: stale cache detected for this project, clearing and retrying..." >&2
            local stuck=0
            local vol
            for vol in "nix-container-store-$hash" "nix-container-cache-$hash"; do
                "$cli" volume inspect "$vol" >/dev/null 2>&1 || continue
                "$cli" volume rm "$vol" >/dev/null 2>&1 || stuck=1
            done
            if [ "$stuck" -eq 1 ]; then
                echo "nix-container: cannot clear cache while other nix-container sessions are open for this project. Exit those sessions and re-run." >&2
                return $rc
            fi
            "$cli" run -it "${run_args[@]}"
            return $?
        fi
    fi

    return $rc
}
