{ pkgs, ... }:

let
  # rust-toolchain.toml is the single source of truth for the compiler, because
  # it is the only form every consumer understands: rustup honours it on
  # Windows, where Nix cannot reach, and devenv reads it here. Restating the
  # version in this file would create a second declaration to drift.
  toolchain = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain;

  # The crate advertises `rust-version = "1.85"` and `cargo install agenttender`
  # is a documented path, so that promise is a published contract. It was not
  # verified by anything until now: the code used let-chains, stable only from
  # 1.88, so 1.85 could not build it at all.
  msrvToolchain = pkgs.rust-bin.stable."1.85.0".minimal;
in
{
  packages = [
    pkgs.git
    pkgs.just
  ];

  # Pin an exact version in rust-toolchain.toml, never a channel. "stable"
  # floats, and a floating toolchain is how a workspace ends up building
  # against something nobody chose.
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  # A repository that also needs an OLDER compiler -- an MSRV gate -- declares
  # it as a script, not a package. A second toolchain in `packages` puts a
  # second `cargo` on PATH and which one wins is an accident of ordering.
  #
  # Set RUSTC explicitly. Prepending PATH is NOT sufficient: cargo resolves its
  # compiler independently of the PATH the wrapper sets, so a PATH-only wrapper
  # runs the OLD cargo against the NEW rustc -- an MSRV gate that always passes,
  # which is worse than no gate. Measured: with PATH alone a crate calling a
  # newer API compiled clean under the older wrapper (exit 0); with RUSTC set it
  # fails correctly (exit 101, E0658).
  #
  # An MSRV gate belongs in CI too. This wrapper is the local half; a Linux job
  # pinned to the same version is what makes the promise executable.
  #
  scripts.msrv-build.exec = ''
    export RUSTC="${msrvToolchain}/bin/rustc"
    export RUSTDOC="${msrvToolchain}/bin/rustdoc"
    exec "${msrvToolchain}/bin/cargo" "$@"
  '';

  # Same shape for a repository needing nightly for one job -- fuzzing,
  # `build-std` -- while building on stable otherwise. `nightly.latest` resolves
  # from the rust-overlay revision in devenv.lock, so it is deterministic.
  #
  # nightlyToolchain = pkgs.rust-bin.nightly.latest.default.override {
  #   extensions = [ "rust-src" ];
  # };
  #
  # scripts.cargo-nightly.exec = ''
  #   export RUSTC="${nightlyToolchain}/bin/rustc"
  #   export RUSTDOC="${nightlyToolchain}/bin/rustdoc"
  #   exec "${nightlyToolchain}/bin/cargo" "$@"
  # '';

  # `devenv test` runs this. Assert the resolved compiler is the one declared in
  # rust-toolchain.toml, so a silent resolution change -- or a devenv that stops
  # reading the file -- fails here rather than inside a build.
  enterTest = ''
    set -euo pipefail
    git --version
    just --version
    cargo clippy --version
    actual=$(rustc --version | awk '{print $2}')
    [ "$actual" = "${toolchain.channel}" ] || {
      echo "expected rustc ${toolchain.channel} from rust-toolchain.toml, got $actual" >&2
      exit 1
    }
  '';
}
