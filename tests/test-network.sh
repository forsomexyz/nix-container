#!/usr/bin/env bash
# Exercise config parsing and the final container CLI argv without a running runtime.
set -euo pipefail

repo_dir=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/project"
cat > "$tmp/bin/mock-container" <<'EOF'
#!/usr/bin/env bash
case "$1 $2" in
    'image inspect') exit 0 ;;
    'network inspect')
        printf 'inspect %s\n' "$3" >> "$MOCK_EVENTS"
        [ -f "$MOCK_NETWORKS/$3" ]
        exit $?
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
chmod +x "$tmp/bin/mock-container"
mkdir -p "$tmp/networks"
export PATH="$tmp/bin:$PATH" NIX_CONTAINER_CLI=mock-container MOCK_LOG="$tmp/argv"
export MOCK_NETWORKS="$tmp/networks" MOCK_EVENTS="$tmp/events"
cd "$tmp/project"
printf '{}\n' > shell.nix
# shellcheck source=../nix-container.sh
source "$repo_dir/nix-container.sh"

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

# Aliases follow --network in argv, in the order they were given.
assert_aliases() {
    local expected="$1" got
    got=$(awk 'previous == "--network-alias" { print } { previous = $0 }' "$MOCK_LOG")
    [ "$got" = "$expected" ] || {
        echo "expected aliases [$expected], got [$got]" >&2
        exit 1
    }
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
assert_aliases ''
! grep -q 'inspect\|create' "$MOCK_EVENTS"
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
! grep -q 'inspect\|create' "$MOCK_EVENTS"
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

# Aliases: both spellings, in order, after the network they belong to.
nix-container --no-config --network dev-network --network-alias api --network-alias=api.internal run true
assert_network dev-network
assert_aliases $'api\napi.internal'
nix-container --no-config --network dev-network run true
assert_aliases ''

# ... from the config file, in every list shape the parser accepts.
printf 'network: dev-network\nnetwork-aliases:\n  - api\n  - api.internal\n' > "$tmp/custom.yaml"
nix-container --config "$tmp/custom.yaml" run true
assert_network dev-network
assert_aliases $'api\napi.internal'
printf 'network: dev-network\nnetwork-aliases: [api, api.internal]\n' > "$tmp/custom.yaml"
nix-container --config "$tmp/custom.yaml" run true
assert_aliases $'api\napi.internal'
printf 'network: dev-network\nnetwork_alias: api # underscores and a comment\n' > "$tmp/custom.yaml"
nix-container --config "$tmp/custom.yaml" run true
assert_aliases api

# A flag replaces every alias from the file rather than adding to them.
printf 'network: dev-network\nnetwork-aliases: [api, api.internal]\n' > "$tmp/custom.yaml"
nix-container --config "$tmp/custom.yaml" --network-alias only run true
assert_aliases only
# ... and the flag network still carries the file's aliases.
nix-container --config "$tmp/custom.yaml" --network other run true
assert_network other
assert_aliases $'api\napi.internal'

# Aliases need a network to be resolvable on.
assert_rejected 'requires --network' --no-config --network-alias api
for mode in host none bridge default container:other ns:/var/run/netns/x; do
    assert_rejected 'is a network mode' --no-config --network "$mode" --network-alias api
done

# Malformed aliases are rejected before the network is created or a container runs.
: > "$MOCK_EVENTS"
for bad in -api api- .api api. 'api name' 'api/sub' 'api:1'; do
    assert_rejected 'invalid alias' --no-config --network alias-check --network-alias "$bad"
done
assert_rejected 'duplicate alias' --no-config --network alias-check --network-alias api --network-alias api
[ ! -s "$MOCK_EVENTS" ] || { echo 'invalid aliases reached the runtime' >&2; exit 1; }
if nix-container --no-config --network dev-network --network-alias= run true > "$tmp/out" 2>&1; then
    echo 'accepted empty --network-alias' >&2
    exit 1
fi
grep -q 'requires an alias' "$tmp/out"
printf 'network: dev-network\nnetwork-aliases:\n  -\n' > "$tmp/custom.yaml"
assert_rejected 'custom.yaml:3:' --config "$tmp/custom.yaml"

echo 'network tests passed'
