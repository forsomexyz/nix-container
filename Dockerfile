FROM nix-container-base:latest

USER nix
WORKDIR /home/nix

ENTRYPOINT ["nix-shell"]
