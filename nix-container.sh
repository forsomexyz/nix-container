# Optional compatibility aliases for existing bash/zsh shell rc files.
# Install the Rust binary first: cargo install --path . --locked
# No host-side implementation lives in this file.
nix-container-init() { command nix-container init "$@"; }
nix-container-build() { command nix-container build "$@"; }
nix-container-clear-cache() { command nix-container clear-cache "$@"; }
