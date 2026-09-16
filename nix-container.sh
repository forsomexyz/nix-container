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
    local original="$1" spec="$1"
    local proto=""
    if [[ "$spec" == */* ]]; then
        proto="/${spec##*/}"
        spec="${spec%/*}"
    fi

    # Docker requires brackets around an IPv6 host address. Discard that
    # address just like an IPv4 one, but require exactly host:container after
    # the closing bracket so an ambiguous spec can never pass through unchanged.
    if [[ "$spec" == \[* ]]; then
        local address rest rest_colons
        case "$spec" in
            \[*\]:*)
                address="${spec%%]*}"
                address="${address#\[}"
                rest="${spec#*]}"
                rest="${rest#:}"
                rest_colons="${rest//[^:]/}"
                if [[ "$address" == *:* ]] && [ "${#rest_colons}" -eq 1 ]; then
                    printf '127.0.0.1:%s%s\n' "$rest" "$proto"
                    return 0
                fi
                ;;
        esac
        printf 'nix-container: invalid port specification: %s\n' "$original" >&2
        return 1
    fi

    local colons="${spec//[^:]/}"
    case "${#colons}" in
        0) printf '127.0.0.1:%s:%s%s\n' "$spec" "$spec" "$proto" ;;
        1) printf '127.0.0.1:%s%s\n' "$spec" "$proto" ;;
        2) printf '127.0.0.1:%s%s\n' "${spec#*:}" "$proto" ;;
        *)
            printf 'nix-container: invalid port specification: %s\n' "$original" >&2
            return 1
            ;;
    esac
}

# Validate a memory size accepted by the container CLIs' `--memory`: a number
# with an optional K/M/G/T/P suffix (case-insensitive, an optional trailing `b`
# is allowed too, so `2G` and `2GB` are both fine). The value is passed through
# to the CLI unchanged, so only the shape is checked here to catch typos before
# a container is started.
_nix_container_valid_memory() {
    local num="$1"
    case "$num" in
        *[bB]) num="${num%?}" ;;
    esac
    case "$num" in
        *[kKmMgGtTpP]) num="${num%?}" ;;
    esac
    [ -n "$num" ] && [ -z "${num//[0-9]/}" ]
}

# Trim leading and trailing whitespace from a string.
_nix_container_trim() {
    local s="$1"
    while :; do
        case "$s" in
            [[:space:]]*) s="${s#?}" ;;
            *) break ;;
        esac
    done
    while :; do
        case "$s" in
            *[[:space:]]) s="${s%?}" ;;
            *) break ;;
        esac
    done
    printf '%s\n' "$s"
}

# Unwrap a YAML scalar: strip surrounding quotes, or drop a trailing
# `# comment` from an unquoted value. Only covers the flat subset of YAML that
# nix-container.yml uses (no multi-line or folded scalars).
_nix_container_yaml_scalar() {
    local v
    v=$(_nix_container_trim "$1")
    case "$v" in
        '#'*) v="" ;;
        '"'*)
            v="${v#\"}"
            v="${v%%\"*}"
            ;;
        \'*)
            v="${v#\'}"
            v="${v%%\'*}"
            ;;
        *[[:space:]]'#'*)
            v=$(_nix_container_trim "${v%%[[:space:]]#*}")
            ;;
    esac
    printf '%s\n' "$v"
}

# Echo 1 or 0 for a YAML boolean, or return non-zero if the value isn't one.
_nix_container_yaml_bool() {
    case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
        true|yes|on) echo 1 ;;
        false|no|off) echo 0 ;;
        *) return 1 ;;
    esac
}

# Echo the project's config file (nix-container.yml or nix-container.yaml) in
# the current directory, or return non-zero if neither exists.
_nix_container_config_file() {
    if [ -f nix-container.yml ]; then
        echo nix-container.yml
    elif [ -f nix-container.yaml ]; then
        echo nix-container.yaml
    else
        return 1
    fi
}

# Parse a nix-container.yml into a normalized `key=value` stream on stdout (one
# setting per line, ports repeated as `port=<spec>`), or fail with a file:line
# diagnostic. Deliberately a small hand-rolled reader rather than a dependency
# on yq/python: the host is only supposed to need a container CLI. Understood
# syntax: `key: value` scalars with optional quotes and `#` comments, plus block
# (`- x`) and flow (`[x, y]`) sequences for the one list-valued option (ports).
_nix_container_parse_config() {
    local file="$1"
    local lineno=0 line trimmed key value bool item rest list_key=""

    while IFS= read -r line || [ -n "$line" ]; do
        lineno=$((lineno + 1))
        line="${line%$'\r'}"
        trimmed=$(_nix_container_trim "$line")

        # Blank lines, comments and a leading document marker carry no settings.
        case "$trimmed" in
            ''|'#'*|'---') continue ;;
        esac

        # A `- item` line extends the list opened by the most recent key.
        case "$trimmed" in
            -|'- '*)
                value=$(_nix_container_yaml_scalar "${trimmed#-}")
                if [ -z "$list_key" ]; then
                    echo "nix-container: $file:$lineno: list item outside of an option that takes a list" >&2
                    return 1
                fi
                if [ -z "$value" ]; then
                    echo "nix-container: $file:$lineno: empty list item for '$list_key'" >&2
                    return 1
                fi
                # ports is the only list-valued option.
                printf 'port=%s\n' "$value"
                continue
                ;;
        esac

        case "$trimmed" in
            *:*) ;;
            *)
                echo "nix-container: $file:$lineno: expected 'option: value', got: $trimmed" >&2
                return 1
                ;;
        esac

        key=$(_nix_container_trim "${trimmed%%:*}")
        value=$(_nix_container_yaml_scalar "${trimmed#*:}")
        # Accept underscores as an alternate spelling of the dashes the command
        # line flags use, so `with_aws` and `with-aws` both work.
        key=$(printf '%s' "$key" | tr '_' '-')
        # Any key other than a list-valued one closes an open list.
        list_key=""

        case "$key" in
            with-gh-token|with-npmrc)
                if ! bool=$(_nix_container_yaml_bool "$value"); then
                    echo "nix-container: $file:$lineno: '$key' expects true or false, got: ${value:-<empty>}" >&2
                    return 1
                fi
                printf '%s=%s\n' "$key" "$bool"
                ;;
            with-aws)
                # true/false toggle credential forwarding; any other value names
                # the profile to resolve credentials from.
                if bool=$(_nix_container_yaml_bool "$value"); then
                    printf 'with-aws=%s\n' "$bool"
                elif [ -n "$value" ]; then
                    printf 'with-aws=1\naws-profile=%s\n' "$value"
                else
                    echo "nix-container: $file:$lineno: 'with-aws' expects true, false or a profile name" >&2
                    return 1
                fi
                ;;
            with-env)
                if [ -z "$value" ]; then
                    echo "nix-container: $file:$lineno: 'with-env' expects a file path" >&2
                    return 1
                fi
                printf 'env-file=%s\n' "$value"
                ;;
            memory)
                if ! _nix_container_valid_memory "$value"; then
                    echo "nix-container: $file:$lineno: invalid memory size: ${value:-<empty>} (expected a number with an optional K/M/G/T/P suffix, e.g. 512M or 4G)" >&2
                    return 1
                fi
                printf 'memory=%s\n' "$value"
                ;;
            port|ports)
                # Either an inline `[a, b]` flow sequence, a single scalar, or a
                # `- spec` block on the lines that follow.
                list_key=ports
                case "$value" in
                    '') ;;
                    '['*']')
                        rest="${value#\[}"
                        rest="${rest%\]}"
                        while [ -n "$rest" ]; do
                            case "$rest" in
                                *,*)
                                    item="${rest%%,*}"
                                    rest="${rest#*,}"
                                    ;;
                                *)
                                    item="$rest"
                                    rest=""
                                    ;;
                            esac
                            item=$(_nix_container_yaml_scalar "$item")
                            if [ -z "$item" ]; then
                                echo "nix-container: $file:$lineno: empty list item for '$key'" >&2
                                return 1
                            fi
                            printf 'port=%s\n' "$item"
                        done
                        ;;
                    *) printf 'port=%s\n' "$value" ;;
                esac
                ;;
            *)
                echo "nix-container: $file:$lineno: unknown option: $key" >&2
                return 1
                ;;
        esac
    done < "$file"
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
    # Create the volumes explicitly first so the seeding run below reuses them
    # instead of auto-creating them, which prints a "named volume was
    # automatically created" note we don't want.
    "$cli" volume create "$store_vol" >/dev/null 2>&1
    "$cli" volume create "$cache_vol" >/dev/null 2>&1
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
    # Options start out unset (empty / 0 markers) so that, after the command
    # line is parsed, nix-container.yml can fill in whatever wasn't given
    # explicitly. cli_aws/cli_ports mark options where the config contributes
    # more than one value (profile, repeated ports) and a flag must suppress
    # all of it.
    local with_gh_token=""
    local with_aws=""
    local with_npmrc=""
    local env_file=""
    local aws_profile=""
    local memory=""
    local cli_aws=0
    local cli_ports=0
    local config_file=""
    local use_config=1
    local do_run=0
    local normalized_port=""
    local -a run_cmd=()
    local -a port_args=()
    while [ $# -gt 0 ]; do
        case "$1" in
            -h|--help)
                cat <<'EOF'
Usage: nix-container [options]
       nix-container [options] run COMMAND [ARG...]

Run a containerized nix-shell for the shell.nix or default.nix in the
current directory.

With `run`, COMMAND is executed inside the project's nix-shell environment
and the container exits, instead of opening an interactive shell. Everything
after `run` is treated as the command, so options must come before it.

Options not given on the command line default to the ones set in
./nix-container.yml (or ./nix-container.yaml), if that file exists.

Options:
  --with-gh-token       Forward a GitHub token from 'gh auth token' as GH_TOKEN.
  --with-aws            Forward AWS credentials from the host (uses default profile).
  --with-aws=PROFILE    Forward AWS credentials using the given profile.
  --with-npmrc          Mount ~/.npmrc into the container (read-only) if present.
  --with-env FILE       Load environment variables from FILE (dotenv format).
                        If FILE contains 1Password secret references (op://...),
                        they are resolved at run time with the 'op' CLI (which
                        must be in PATH); otherwise FILE is passed straight to
                        the container CLI's --env-file.
  -p, --port SPEC       Publish a port. Host side is always bound to 127.0.0.1.
                        SPEC: <container>, <host>:<container>, or <ip>:<host>:<container>,
                        optionally suffixed with /<proto>.
                        Overrides the ports set in nix-container.yml.
  -m, --memory SIZE     Amount of memory the container may use, passed to the
                        container CLI's --memory. SIZE is a number with an
                        optional K, M, G, T or P suffix (e.g. 512M, 4G).
                        Defaults to nix-container.yml, then $NIX_CONTAINER_MEMORY.
  --config FILE         Read options from FILE instead of ./nix-container.yml.
  --no-config           Ignore ./nix-container.yml.
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
                cli_aws=1
                shift
                ;;
            --with-aws=*)
                with_aws=1
                cli_aws=1
                aws_profile="${1#--with-aws=}"
                shift
                ;;
            --with-npmrc)
                with_npmrc=1
                shift
                ;;
            --with-env)
                if [ -z "${2:-}" ]; then
                    echo "nix-container: $1 requires a file argument" >&2
                    return 1
                fi
                env_file="$2"
                shift 2
                ;;
            --with-env=*)
                if [ -z "${1#--with-env=}" ]; then
                    echo "nix-container: --with-env requires a file argument" >&2
                    return 1
                fi
                env_file="${1#--with-env=}"
                shift
                ;;
            -p|--port)
                if [ -z "${2:-}" ]; then
                    echo "nix-container: $1 requires a value (e.g. 8080 or 8080:8080)" >&2
                    return 1
                fi
                cli_ports=1
                normalized_port=$(_nix_container_force_loopback "$2") || return 1
                port_args+=(-p "$normalized_port")
                shift 2
                ;;
            -p=*|--port=*)
                cli_ports=1
                normalized_port=$(_nix_container_force_loopback "${1#*=}") || return 1
                port_args+=(-p "$normalized_port")
                shift
                ;;
            -m|--memory)
                if [ -z "${2:-}" ]; then
                    echo "nix-container: $1 requires a value (e.g. 512M or 4G)" >&2
                    return 1
                fi
                memory="$2"
                shift 2
                ;;
            -m=*|--memory=*)
                memory="${1#*=}"
                shift
                ;;
            --config)
                if [ -z "${2:-}" ]; then
                    echo "nix-container: $1 requires a file argument" >&2
                    return 1
                fi
                config_file="$2"
                use_config=1
                shift 2
                ;;
            --config=*)
                if [ -z "${1#--config=}" ]; then
                    echo "nix-container: --config requires a file argument" >&2
                    return 1
                fi
                config_file="${1#--config=}"
                use_config=1
                shift
                ;;
            --no-config)
                use_config=0
                shift
                ;;
            run)
                do_run=1
                shift
                run_cmd=("$@")
                break
                ;;
            *)
                echo "nix-container: unknown option: $1" >&2
                return 1
                ;;
        esac
    done

    if [ "$do_run" -eq 1 ] && [ "${#run_cmd[@]}" -eq 0 ]; then
        echo "nix-container: run requires a command (e.g. nix-container run npm test)" >&2
        return 1
    fi

    _nix_container_nix_file >/dev/null || {
        echo "nix-container: no shell.nix or default.nix in current directory" >&2
        return 1
    }

    # Fill in anything the command line didn't set from the project's config
    # file, so recurring options don't have to be typed on every run. An
    # explicit --config must exist; a discovered one is optional.
    if [ "$use_config" -eq 1 ]; then
        if [ -z "$config_file" ]; then
            config_file=$(_nix_container_config_file) || config_file=""
        elif [ ! -f "$config_file" ]; then
            echo "nix-container: --config: file not found: $config_file" >&2
            return 1
        fi
    else
        config_file=""
    fi

    if [ -n "$config_file" ]; then
        local config_settings config_dir setting cfg_key cfg_value
        config_settings=$(_nix_container_parse_config "$config_file") || return 1
        config_dir=$(dirname "$config_file")
        # A here-string (not a pipe) so port_args survives the loop.
        while IFS= read -r setting; do
            [ -n "$setting" ] || continue
            cfg_key="${setting%%=*}"
            cfg_value="${setting#*=}"
            case "$cfg_key" in
                with-gh-token) [ -n "$with_gh_token" ] || with_gh_token="$cfg_value" ;;
                with-npmrc) [ -n "$with_npmrc" ] || with_npmrc="$cfg_value" ;;
                with-aws) [ "$cli_aws" -eq 1 ] || with_aws="$cfg_value" ;;
                aws-profile) [ "$cli_aws" -eq 1 ] || aws_profile="$cfg_value" ;;
                memory) [ -n "$memory" ] || memory="$cfg_value" ;;
                port)
                    if [ "$cli_ports" -ne 1 ]; then
                        normalized_port=$(_nix_container_force_loopback "$cfg_value") || return 1
                        port_args+=(-p "$normalized_port")
                    fi
                    ;;
                env-file)
                    # Relative paths resolve against the config file's own
                    # directory, which is the project directory unless --config
                    # pointed somewhere else.
                    if [ -z "$env_file" ]; then
                        case "$cfg_value" in
                            /*) env_file="$cfg_value" ;;
                            *)
                                if [ "$config_dir" = . ]; then
                                    env_file="$cfg_value"
                                else
                                    env_file="$config_dir/$cfg_value"
                                fi
                                ;;
                        esac
                    fi
                    ;;
            esac
        done <<< "$config_settings"
    fi

    # Settle the remaining defaults now that flags and config have been merged:
    # off for the toggles, and the environment for the memory limit.
    : "${with_gh_token:=0}"
    : "${with_aws:=0}"
    : "${with_npmrc:=0}"
    : "${memory:=${NIX_CONTAINER_MEMORY:-}}"

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

    # Wrap the container invocation in `op run` only when the env file actually
    # carries 1Password secret references; empty otherwise so the run is direct.
    local -a op_prefix=()
    if [ -n "$env_file" ]; then
        if [ ! -f "$env_file" ]; then
            echo "nix-container: env file not found: $env_file" >&2
            return 1
        fi
        # If the file contains 1Password secret references (op://...), resolve
        # them at run time by wrapping the container invocation in `op run`,
        # which sets the resolved values in the CLI's environment; each variable
        # is then forwarded into the container by name (-e NAME), so resolved
        # secrets never appear in the process argument list. A file without
        # references is handed to the CLI's own --env-file, no op required.
        if grep -q 'op://' "$env_file"; then
            if ! command -v op >/dev/null 2>&1; then
                echo "nix-container: $env_file has 1Password secret references (op://) but the 'op' CLI is not in PATH" >&2
                return 1
            fi
            local line name
            while IFS= read -r line; do
                case "$line" in
                    ''|\#*) continue ;;
                esac
                line="${line#export }"
                case "$line" in
                    *=*) name="${line%%=*}" ;;
                    *) continue ;;
                esac
                env_args+=(-e "$name")
            done < "$env_file"
            op_prefix=(op run --no-masking "--env-file=$env_file" --)
        else
            env_args+=(--env-file "$env_file")
        fi
    fi

    local hash
    hash=$(_nix_container_project_hash) || return 1

    local kind
    kind=$(_nix_container_cli_kind "$cli")

    # Memory limit: both docker and `container` take `--memory` with the same
    # suffixed-size syntax (on `container` it sizes the guest VM).
    local -a memory_args=()
    if [ -n "$memory" ]; then
        if ! _nix_container_valid_memory "$memory"; then
            echo "nix-container: --memory: invalid memory size: $memory (expected a number with an optional K/M/G/T/P suffix, e.g. 512M or 4G)" >&2
            return 1
        fi
        memory_args+=(--memory "$memory")
    fi

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
        "${memory_args[@]}"
        -v "nix-container-store-$hash:/nix"
        -v "nix-container-cache-$hash:/home/nix/.cache"
        -v "$PWD:/home/nix/$dir"
        -w "/home/nix/$dir"
        nix-container:latest
        --extra-experimental-features flakes
    )

    # Default is an interactive shell (-it). With `run`, execute the command
    # non-interactively via nix-shell's --run and exit; only allocate a TTY when
    # our own std streams are terminals so output stays pipeable in scripts/CI.
    local -a it_args=(-it)
    if [ "$do_run" -eq 1 ]; then
        local quoted_cmd="" arg
        for arg in "${run_cmd[@]}"; do
            quoted_cmd+="$(printf '%q ' "$arg")"
        done
        run_args+=(--run "$quoted_cmd")
        it_args=()
        [ -t 0 ] && it_args+=(-i)
        [ -t 1 ] && it_args+=(-t)
    fi

    local start=$SECONDS
    "${op_prefix[@]}" "$cli" run "${it_args[@]}" "${run_args[@]}"
    local rc=$?

    # If the container exited quickly with an error, it likely failed during
    # nix-shell startup. The most common cause is a stale /nix volume that was
    # populated by a previous (now-rebuilt) base image. Probe non-interactively
    # to confirm, and if so, drop the project's cache volumes and retry once.
    if [ $rc -ne 0 ] && [ $((SECONDS - start)) -lt 3 ]; then
        local probe
        probe=$("${op_prefix[@]}" "$cli" run "${run_args[@]}" --run true 2>&1)
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
            "${op_prefix[@]}" "$cli" run "${it_args[@]}" "${run_args[@]}"
            return $?
        fi
    fi

    return $rc
}
