#!/bin/sh
set -eu
root=$MOCK_ROOT
missing() {
    case "$0" in
        */container) echo "Error: notFound: $1 not found: $2" >&2 ;;
        *) echo "Error response from daemon: No such $1: $2" >&2 ;;
    esac
    exit 1
}
case "$1 $2" in
    'system status')
        if [ "${MOCK_STOPPED:-0}" = 1 ]; then printf '{"status":"stopped"}'; else printf '{"status":"running"}'; fi
        exit 0 ;;
    'system property')
        if [ "${MOCK_NO_KERNEL:-0}" = 1 ]; then printf '{}'; else printf '{"kernel":{"binaryPath":"/kernel"}}'; fi
        exit 0 ;;
    'image inspect')
        case "${MOCK_IMAGE:-ok}" in
            missing) missing image nix-container:latest ;;
            error) echo 'cannot connect to runtime' >&2; exit 1 ;;
            service) echo 'Error: notFound: image service not found' >&2; exit 1 ;;
            malformed) printf '{}'; exit 0 ;;
        esac
        id=old
        [ ! -f "$root/loaded" ] || id=new
        case "$*" in
            *--format*) printf '%s\n' "$id" ;;
            *) printf '[{"id":"%s"}]\n' "$id" ;;
        esac
        exit 0 ;;
    'image load') printf 'load %s\n' "$4" >> "$root/events"; touch "$root/loaded"; exit 0 ;;
    'volume inspect')
        [ "${MOCK_VOLUME_ERROR:-0}" != 1 ] || { echo 'permission denied' >&2; exit 1; }
        [ ! -f "$root/volumes/$3" ] || exit 0
        missing volume "$3" ;;
    'volume create') touch "$root/volumes/$3"; exit 0 ;;
    'volume rm')
        [ "${MOCK_REMOVE_FAIL:-0}" != 1 ] || { echo 'volume is in use' >&2; exit 1; }
        printf 'remove %s\n' "$3" >> "$root/events"; rm -f "$root/volumes/$3"; exit 0 ;;
    'volume ls') for f in "$root"/volumes/*; do [ ! -f "$f" ] || basename "$f"; done; exit 0 ;;
    'network inspect')
        [ ! -f "$root/networks/$3" ] || exit 0
        missing network "$3" ;;
    'network create') touch "$root/networks/$3"; exit 0 ;;
esac
if [ "$1" = build ]; then
    printf '%s\n' "$*" >> "$root/events"
    for last do :; done
    test -f "$last/Dockerfile"
    test -f "$last/build-base.sh"
    exit 0
fi
if [ "$1" = run ]; then
    # Keep every launch, including helpers and probes, for identity/TTY checks.
    count=$(find "$root" -name 'run-argv-*' | wc -l | tr -d ' ')
    printf '%s\0' "$@" > "$root/run-argv-$count"
    case "$*" in
        *nix-container-base-build:latest*)
            printf '%s\n' "$*" >> "$root/events"
            for arg do
                case "$arg" in *:/tmp/out) out=${arg%:/tmp/out} ;; esac
            done
            touch "$out/image.tar.gz" "$out/image-oci.tar"
            exit 0 ;;
        *:/seed-persistence*) printf 'seed-persistence\n' >> "$root/events"; exit "${MOCK_SEED_FAIL:-0}" ;;
        *:/seed-nix*) printf 'seed-cache\n' >> "$root/events"; exit "${MOCK_SEED_FAIL:-0}" ;;
    esac
    printf 'launch\n' >> "$root/events"
    printf '%s\0' "$@" > "$root/argv"
    printf '%s|%s|%s|%s|%s' "${GH_TOKEN:-}" "${AWS_ACCESS_KEY_ID:-}" "${AWS_SECRET_ACCESS_KEY:-}" "${AWS_PROFILE:-}" "${PASSWORD:-}" > "$root/credentials"
    for arg do
        case "$arg" in nix-container-store-*:/nix|nix-container-cache-*:/home/nix/.cache) touch "$root/volumes/${arg%%:*}" ;; esac
    done
    if [ "${MOCK_STALE:-0}" = 1 ]; then
        if [ ! -f "$root/failed" ]; then touch "$root/failed"; exit 1; fi
        if [ ! -f "$root/probed" ]; then
            for last do :; done
            [ "$last" = "'true'" ] || exit 99
            touch "$root/probed"
            printf 'probe\n' >> "$root/events"
            echo "cannot determine user's home directory" >&2
            exit 1
        fi
    fi
    exit "${MOCK_EXIT:-0}"
fi
exit 1
