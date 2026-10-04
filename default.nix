{pkgs}: let
  manifest = pkgs.lib.importTOML ./Cargo.toml;
  rustPlatform =
    if pkgs.stdenv.isDarwin
    then pkgs.rustPlatform
    else pkgs.pkgsStatic.rustPlatform;
in
  rustPlatform.buildRustPackage {
    pname = manifest.package.name;
    version = manifest.package.version;
    src = pkgs.lib.fileset.toSource {
      root = ./.;
      fileset = pkgs.lib.fileset.unions ([
          ./Cargo.toml
          ./Cargo.lock
          ./src
          ./tests
        ]
        ++ pkgs.lib.optional pkgs.stdenv.isDarwin ./bin);
    };
    cargoLock.lockFile = ./Cargo.lock;
    meta = {
      description = manifest.package.description;
      mainProgram = "rcodex";
      platforms = ["x86_64-linux" "aarch64-darwin" "x86_64-darwin"];
    };
  }
