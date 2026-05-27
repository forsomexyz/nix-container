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

    local cli
    cli=$(_nix_container_cli) || return 1

    "$cli" build -t nix-container-base-build:latest -f "$dir/Dockerfile.base-build" "$dir" || return 1

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

    "$cli" build -t nix-container:latest -f "$dir/Dockerfile" "$dir" || return 1
}

nix-container-clear-cache() {
    local cli
    cli=$(_nix_container_cli) || return 1

    local volume
    for volume in nix-container-store nix-container-cache; do
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
    local aws_profile=""
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
            *)
                echo "nix-container: unknown option: $1" >&2
                return 1
                ;;
        esac
    done

    if [ ! -f shell.nix ] && [ ! -f default.nix ]; then
        echo "nix-container: no shell.nix or default.nix in current directory" >&2
        return 1
    fi

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
            mount_args+=(-v "$HOME/.aws/sso/cache:/home/nix/.aws/sso/cache:ro")
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

    local dir
    dir=$(basename "$PWD")
    "$cli" run -it --rm "${env_args[@]}" "${mount_args[@]}" \
        -v nix-container-store:/nix \
        -v nix-container-cache:/home/nix/.cache \
        -v "$PWD:/home/nix/$dir" \
        -w "/home/nix/$dir" \
        nix-container:latest
}
