{pkgs}: let
  manifest = pkgs.lib.importTOML ./Cargo.toml;
in
  pkgs.pkgsStatic.rustPlatform.buildRustPackage {
    pname = manifest.package.name;
    version = manifest.package.version;
    src = pkgs.lib.fileset.toSource {
      root = ./.;
      fileset = pkgs.lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./src
        ./tests
      ];
    };
    cargoLock.lockFile = ./Cargo.lock;
    meta = {
      description = manifest.package.description;
      mainProgram = "rcodex";
      platforms = pkgs.lib.platforms.linux;
    };
  }
