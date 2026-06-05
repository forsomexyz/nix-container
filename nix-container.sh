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
        0) echo "127.0.0.1::${spec}${proto}" ;;
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

nix-container-build() {
    local dir="$_NIX_CONTAINER_SCRIPT_DIR"
    if [ -z "$dir" ]; then
        echo "nix-container-build: script directory not set" >&2
        return 1
    fi

    local -a build_args=()
    while [ $# -gt 0 ]; do
        case "$1" in
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

    local prev_base_id
    prev_base_id=$("$cli" image inspect --format '{{.Id}}' nix-container-base:latest 2>/dev/null)

    "$cli" build "${build_args[@]}" -t nix-container-base-build:latest -f "$dir/Dockerfile.base-build" "$dir" || return 1

    local out
    out=$(mktemp -d) || return 1
    "$cli" run --rm -v "$out:/tmp/out" nix-container-base-build:latest || { rm -rf "$out"; return 1; }

    local tarball
    tarball=$(ls "$out"/*.tar.gz 2>/dev/null | head -n 1)
    if [ -z "$tarball" ]; then
        echo "nix-container-build: no tarball produced in $out" >&2
        rm -rf "$out"
        return 1
    fi

    "$cli" load -i "$tarball" || { rm -rf "$out"; return 1; }
    "$cli" tag nix-container-base:latest nix-container-base:latest 2>/dev/null

    rm -rf "$out"

    "$cli" build "${build_args[@]}" -t nix-container:latest -f "$dir/Dockerfile" "$dir" || return 1

    local new_base_id
    new_base_id=$("$cli" image inspect --format '{{.Id}}' nix-container-base:latest 2>/dev/null)

    # The /nix store and ~/.cache named volumes are populated from the image on
    # first use only; if the base image changed, existing volumes hold stale
    # store paths that no longer exist in the new image. Drop them so the next
    # `nix-container` run repopulates from the fresh image.
    if [ -n "$new_base_id" ] && [ "$prev_base_id" != "$new_base_id" ]; then
        local -a stale_volumes=()
        local vol
        while IFS= read -r vol; do
            [ -n "$vol" ] && stale_volumes+=("$vol")
        done < <("$cli" volume ls -q 2>/dev/null | grep -E '^nix-container-(store|cache)-')
        if [ "${#stale_volumes[@]}" -gt 0 ]; then
            echo "nix-container-build: base image changed, invalidating ${#stale_volumes[@]} cache volume(s)"
            for vol in "${stale_volumes[@]}"; do
                "$cli" volume rm "$vol" >/dev/null || return 1
            done
        fi
    fi
}

nix-container-clear-cache() {
    local all=0
    while [ $# -gt 0 ]; do
        case "$1" in
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
        done < <("$cli" volume ls -q 2>/dev/null | grep -E '^nix-container-(store|cache)-')
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

    local nix_file
    nix_file=$(_nix_container_nix_file) || {
        echo "nix-container: no shell.nix or default.nix in current directory" >&2
        return 1
    }

    local cli
    cli=$(_nix_container_cli) || return 1

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

    local dir
    dir=$(basename "$PWD")
    "$cli" run -it --rm "${env_args[@]}" "${mount_args[@]}" "${port_args[@]}" \
        -v "nix-container-store-$hash:/nix" \
        -v "nix-container-cache-$hash:/home/nix/.cache" \
        -v "$PWD:/home/nix/$dir" \
        -w "/home/nix/$dir" \
        nix-container:latest \
        --extra-experimental-features 'flakes'
}
