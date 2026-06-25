#!/bin/bash
#
SCRIPT_DIR=$(dirname "$(readlink -f "$0")")

: "${HOST_UID:?HOST_UID must be set (pass via docker run -e HOST_UID=\$(id -u))}"
: "${HOST_GID:?HOST_GID must be set (pass via docker run -e HOST_GID=\$(id -g))}"

pushd "$SCRIPT_DIR" > /dev/null || exit 1
nix-build -E '
{ uid, gid }:
let
  pkgs = import <nixpkgs> {};
  dockerNix = builtins.fetchurl "https://raw.githubusercontent.com/NixOS/nix/97990235454f0aa19b793c74c8f7fd8b7da3001b/docker.nix";
in
  import dockerNix {
    inherit pkgs;

    name = "nix-container-base";

    uname = "nix";
    gname = "nix";

    inherit uid gid;
  }
' --arg uid "$HOST_UID" --arg gid "$HOST_GID"
cp "$(readlink -e result)" /tmp/out/
popd > /dev/null || exit 1
