#!/usr/bin/env bash
# Exercise config parsing and the final container CLI argv without a running runtime.
set -euo pipefail

repo_dir=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/project"
cli_name=${MOCK_CLI_NAME:-mock-container}
cat > "$tmp/bin/$cli_name" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
    'system status') printf '{"status":"running"}'; exit 0 ;;
    'system property') printf '{"kernel":{"binaryPath":"/kernel"}}'; exit 0 ;;
    'volume inspect') exit 0 ;; # caches already prepared
    'image inspect')
        case "$*" in
            *--format*) printf 'image-id\n' ;;
            *) printf '[{"id":"image-id"}]' ;;
        esac
        exit 0 ;;
    'network inspect')
        printf 'inspect %s\n' "$3" >> "$MOCK_EVENTS"
        [ ! -f "$MOCK_NETWORKS/$3" ] || exit 0
        echo 'No such network' >&2
        exit 1
        ;;
    'network create')
        printf 'create %s\n' "$3" >> "$MOCK_EVENTS"
        if [ "${MOCK_CREATE_FAIL:-0}" = race ]; then
            touch "$MOCK_NETWORKS/$3"
            exit 1
        fi
        [ "${MOCK_CREATE_FAIL:-0}" = 0 ] || exit 1
        touch "$MOCK_NETWORKS/$3"
        exit 0
        ;;
esac
if [ "$1" = run ]; then
    printf 'run\n' >> "$MOCK_EVENTS"
    printf '%s\n' "$@" > "$MOCK_LOG"
    exit 0
fi
exit 1
EOF
chmod +x "$tmp/bin/$cli_name"
mkdir -p "$tmp/networks"
export PATH="$tmp/bin:$PATH" NIX_CONTAINER_CLI="$cli_name" MOCK_LOG="$tmp/argv"
export MOCK_NETWORKS="$tmp/networks" MOCK_EVENTS="$tmp/events"
cd "$tmp/project"
printf '{}\n' > shell.nix
# Test the compiled Rust CLI, not a shell implementation.
NIX_CONTAINER_BIN=${NIX_CONTAINER_BIN:-$repo_dir/target/debug/nix-container}
nix-container() { "$NIX_CONTAINER_BIN" "$@"; }

assert_network() {
    local expected="$1" count
    count=$(grep -cx -- '--network' "$MOCK_LOG" || :)
    if [ -z "$expected" ]; then
        [ "$count" -eq 0 ] || { echo "unexpected --network" >&2; exit 1; }
    else
        [ "$count" -eq 1 ] || { echo "expected one --network" >&2; exit 1; }
        awk -v expected="$expected" 'previous == "--network" && $0 == expected { found = 1 } { previous = $0 } END { exit !found }' "$MOCK_LOG" || {
            echo "missing --network argument: $expected" >&2
            exit 1
        }
    fi
}

assert_rejected() {
    local message="$1"
    shift
    if nix-container "$@" run true > "$tmp/out" 2>&1; then
        echo "accepted: $*" >&2
        exit 1
    fi
    grep -q -- "$message" "$tmp/out" || {
        echo "missing diagnostic '$message' for: $*" >&2
        cat "$tmp/out" >&2
        exit 1
    }
}

nix-container run true
assert_network ''
if grep -q 'inspect\|create' "$MOCK_EVENTS"; then
    echo 'default network unexpectedly inspected or created' >&2
    exit 1
fi
printf 'network: dev-network # comment\n' > nix-container.yaml
: > "$MOCK_EVENTS"
nix-container run true
assert_network dev-network
[ "$(< "$MOCK_EVENTS")" = $'inspect dev-network\ncreate dev-network\nrun' ]
: > "$MOCK_EVENTS"
nix-container run true
assert_network dev-network
[ "$(< "$MOCK_EVENTS")" = $'inspect dev-network\nrun' ]
: > "$MOCK_EVENTS"
nix-container --network=host run true
assert_network host
nix-container --network none run true
assert_network none
nix-container --network bridge run true
assert_network bridge
nix-container --network default run true
assert_network default
if grep -q 'inspect\|create' "$MOCK_EVENTS"; then
    echo 'runtime network mode unexpectedly inspected or created' >&2
    exit 1
fi
nix-container --no-config run true
assert_network ''
printf 'network: "quoted network"\n' > nix-container.yml
nix-container run true
assert_network 'quoted network'  # .yml takes precedence over .yaml
printf 'network: custom\n' > "$tmp/custom.yaml"
nix-container --config "$tmp/custom.yaml" run true
assert_network custom

: > "$MOCK_EVENTS"
export MOCK_CREATE_FAIL=1
if nix-container --network failing run true > "$tmp/out" 2>&1; then
    echo 'accepted failed network creation' >&2
    exit 1
fi
grep -q 'could not create network: failing' "$tmp/out"
[ "$(< "$MOCK_EVENTS")" = $'inspect failing\ncreate failing\ninspect failing' ]
MOCK_CREATE_FAIL=race
: > "$MOCK_EVENTS"
nix-container --network raced run true
assert_network raced
[ "$(< "$MOCK_EVENTS")" = $'inspect raced\ncreate raced\ninspect raced\nrun' ]
unset MOCK_CREATE_FAIL

for bad in 'network:' 'network: # comment' 'network: "unterminated'; do
    printf '%s\n' "$bad" > "$tmp/custom.yaml"
    if nix-container --config "$tmp/custom.yaml" run true > "$tmp/out" 2>&1; then
        echo "accepted invalid network: $bad" >&2
        exit 1
    fi
    grep -q 'custom.yaml:1:' "$tmp/out"
done
if nix-container --network= run true > "$tmp/out" 2>&1; then
    echo 'accepted empty --network' >&2
    exit 1
fi
grep -q 'requires a network name' "$tmp/out"

# Network aliases were removed: Apple's container CLI has no --network-alias,
# so neither the flag nor the config keys are accepted any more.
: > "$MOCK_EVENTS"
assert_rejected 'unknown option: --network-alias' --no-config --network dev-network --network-alias api
printf 'network: dev-network\nnetwork-aliases: [api]\n' > "$tmp/custom.yaml"
assert_rejected 'unknown option: network-aliases' --config "$tmp/custom.yaml"
printf 'network: dev-network\nnetwork_alias: api\n' > "$tmp/custom.yaml"
assert_rejected 'unknown option: network-alias' --config "$tmp/custom.yaml"
[ ! -s "$MOCK_EVENTS" ] || { echo 'rejected aliases reached the runtime' >&2; exit 1; }

echo 'network tests passed'
