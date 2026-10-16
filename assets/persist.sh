set -ef
IFS=:
for target in $NIX_CONTAINER_PERSIST_LINKS; do
    staging="$NIX_CONTAINER_PERSIST_ROOT/${target#/home/nix/}"
    mkdir -p "${target%/*}"
    if [ -e "$target" ] || [ -L "$target" ]; then
        rm -rf "$target"
    fi
    ln -s "$staging" "$target"
done
unset IFS
set +f
unset NIX_CONTAINER_PERSIST_LINKS NIX_CONTAINER_PERSIST_ROOT target staging
exec nix-shell "$@"
