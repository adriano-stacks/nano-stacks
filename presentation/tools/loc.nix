let
  lock = builtins.fromJSON (builtins.readFile ../../flake.lock);
  nixpkgs = builtins.fetchTree lock.nodes.nixpkgs.locked;
  pkgs = import nixpkgs { system = builtins.currentSystem; };
in
pkgs.python313.withPackages (python: [
  python.tree-sitter
  python.tree-sitter-rust
])
