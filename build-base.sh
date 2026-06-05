#!/bin/bash
#
SCRIPT_DIR=$(dirname "$(readlink -f "$0")")

pushd $SCRIPT_DIR > /dev/null
nix-build -E '
let
  pkgs = import <nixpkgs> {};
  dockerNix = builtins.fetchurl "https://raw.githubusercontent.com/NixOS/nix/97990235454f0aa19b793c74c8f7fd8b7da3001b/docker.nix";
in
  import dockerNix {
    inherit pkgs;

    name = "nix-container-base";

    uname = "nix";
    gname = "nix";

    uid = 502;
    gid = 502;
  }
'
cp $(readlink -e result) /tmp/out/
popd > /dev/null
