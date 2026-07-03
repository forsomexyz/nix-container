#!/bin/bash
#
SCRIPT_DIR=$(dirname "$(readlink -f "$0")")

: "${HOST_UID:?HOST_UID must be set (pass via docker run -e HOST_UID=\$(id -u))}"
: "${HOST_GID:?HOST_GID must be set (pass via docker run -e HOST_GID=\$(id -g))}"

pushd "$SCRIPT_DIR" > /dev/null || exit 1
# The single quotes are intentional: this is a Nix expression, not shell, and
# must not be expanded by the shell.
# shellcheck disable=SC2016
nix-build -E '
{ uid, gid }:
let
  dockerNix = builtins.fetchurl "https://raw.githubusercontent.com/NixOS/nix/97990235454f0aa19b793c74c8f7fd8b7da3001b/docker.nix";
  # Pin the baked channel to nixpkgs-unstable rather than inheriting the
  # frozen release snapshot that the nixos/nix build image ships. docker.nix
  # bakes the channel from `pkgs.path`, so importing unstable here is what makes
  # `<nixpkgs>` resolve to unstable inside the container.
  nixpkgs = builtins.fetchTarball "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz";
  pkgs = import nixpkgs {};
in
  import dockerNix {
    inherit pkgs;

    name = "nix-container-base";

    uname = "nix";
    gname = "nix";

    # channelURL/channelName only populate ~/.nix-channels for a future
    # `nix-channel --update`; they do not affect the baked <nixpkgs> (that comes
    # from `pkgs` above). Set explicitly so an in-container update stays on the
    # same channel and consistent with what we bake.
    channelName = "nixpkgs";
    channelURL = "https://channels.nixos.org/nixpkgs-unstable";

    inherit uid gid;
  }
' --arg uid "$HOST_UID" --arg gid "$HOST_GID"

built=$(readlink -e result)
if [ "${IMAGE_FORMAT:-docker}" = oci ]; then
    # Apple's `container` only loads OCI archives, not the docker-archive that
    # docker.nix produces. Convert with skopeo (pulled in via nix). skopeo's
    # docker-archive transport wants an uncompressed tar, so gunzip first, and
    # --insecure-policy avoids needing a signature policy.json in the image.
    cp "$built" /tmp/build/image.tar.gz
    gunzip -f /tmp/build/image.tar.gz
    nix-shell -p skopeo --run "skopeo --insecure-policy copy \
        docker-archive:/tmp/build/image.tar \
        oci-archive:/tmp/out/nix-container-base-oci.tar:nix-container-base:latest" || exit 1
else
    cp "$built" /tmp/out/
fi
popd > /dev/null || exit 1
