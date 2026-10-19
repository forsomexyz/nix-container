FROM nix-container-base:latest

USER nix
WORKDIR /home/nix

# Pre-create as `nix` so a fresh named volume mounted here inherits nix ownership.
RUN mkdir -p /home/nix/.cache

# Pre-create as `nix` so bind-mounts under ~/.aws (from aws:// secrets) don't cause
# docker to auto-create the parent dirs as root, which would block the aws CLI
# from writing its own cache (e.g. /home/nix/.aws/cli).
RUN mkdir -p /home/nix/.aws/sso

# Pre-create as `nix` so bind-mounts under ~/.nix-shared-exprs (from
# --with-shared-nix-exprs) don't cause the runtime to auto-create the parent
# directory as root.
RUN mkdir -p /home/nix/.nix-shared-exprs

# Staging root that volumes for nested --persist locations are mounted under.
# Pre-created as `nix` for the same reason: the runtime would otherwise leave a
# root-owned directory in the home. Only mount points live below it, which the
# runtime creates itself.
RUN mkdir -p /home/nix/.nix-container-persist

ENV LC_ALL=C.UTF-8

ENTRYPOINT ["nix-shell"]
