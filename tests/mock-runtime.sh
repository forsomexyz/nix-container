#!/bin/sh
set -eu
root=$MOCK_ROOT
case "$1 $2" in
    'system status')
        if [ "${MOCK_STOPPED:-0}" = 1 ]; then printf '{"status":"stopped"}'; else printf '{"status":"running"}'; fi
        exit 0 ;;
    'system property') printf '{"kernel":{"binaryPath":"/kernel"}}'; exit 0 ;;
    'image inspect')
        id=old
        [ ! -f "$root/loaded" ] || id=new
        case "$*" in
            *--format*) printf '%s\n' "$id" ;;
            *) printf '[{"id":"%s"}]\n' "$id" ;;
        esac
        exit 0 ;;
    'image load') touch "$root/loaded"; exit 0 ;;
    'volume inspect') [ -f "$root/volumes/$3" ]; exit $? ;;
    'volume create') touch "$root/volumes/$3"; exit 0 ;;
    'volume rm') printf 'remove %s\n' "$3" >> "$root/events"; rm -f "$root/volumes/$3"; exit 0 ;;
    'volume ls') for f in "$root"/volumes/*; do [ ! -f "$f" ] || basename "$f"; done; exit 0 ;;
    'network inspect') [ -f "$root/networks/$3" ]; exit $? ;;
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
    case "$*" in
        *nix-container-base-build:latest*)
            printf '%s\n' "$*" >> "$root/events"
            for arg do
                case "$arg" in *:/tmp/out) out=${arg%:/tmp/out} ;; esac
            done
            touch "$out/image.tar.gz" "$out/image-oci.tar"
            exit 0 ;;
        *:/seed-persistence*) printf 'seed-persistence\n' >> "$root/events"; exit 0 ;;
        *:/seed-nix*) printf 'seed-cache\n' >> "$root/events"; exit 0 ;;
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
