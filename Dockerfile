FROM nix-container-base:latest

USER nix
WORKDIR /home/nix

# Pre-create as `nix` so a fresh named volume mounted here inherits nix ownership.
RUN mkdir -p /home/nix/.cache

# Pre-create as `nix` so bind-mounts under ~/.aws (from --with-aws) don't cause
# docker to auto-create the parent dirs as root, which would block the aws CLI
# from writing its own cache (e.g. /home/nix/.aws/cli).
RUN mkdir -p /home/nix/.aws/sso

ENV LC_ALL=C.UTF-8

ENTRYPOINT ["nix-shell"]
