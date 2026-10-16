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

nix-container run true
assert_network ''
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
echo 'network tests passed'
