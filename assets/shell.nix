let pkgs = import <nixpkgs> {};
in
pkgs.mkShellNoCC {
  packages = with pkgs; [ ];
  shellHook = ''
    # Sane defaults for unicode and truecolor terminal support.
    export LANG=C.UTF-8
    export LC_ALL=C.UTF-8
    export COLORTERM=truecolor
  '';
}
