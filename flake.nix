{
  description = "Persistent remote Codex workspaces over SSH";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/6aefcda9401be8acc2b74244fb3b37520ea1f0a8";
  outputs = {nixpkgs, ...}: {
    packages =
      nixpkgs.lib.genAttrs ["x86_64-linux" "aarch64-darwin" "x86_64-darwin"]
      (system: {default = import ./default.nix {pkgs = import nixpkgs {inherit system;};};});
  };
}
