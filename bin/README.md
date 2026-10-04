# Linux helper for macOS clients

`rcodex-linux-x86_64` is a stripped, static musl build of this project's Rust
sources. macOS clients embed it and upload it over SSH; they never try to run
the macOS executable on Linux. Helpers are cached by content hash remotely.

Regenerate on x86-64 Linux whenever the remote protocol or implementation changes:

```sh
nix build
cp result/bin/rcodex bin/rcodex-linux-x86_64
```

The Linux Nix build excludes `bin/` from its source, so regeneration does not
depend on the old helper. Commit the rebuilt helper with the source changes.
