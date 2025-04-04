inputs@{ self, nixpkgs, rust-overlay, ... }:
system:
let
  # Vanilla nixpkgs with some minor overrides for tooling. This is useful
  # for using programs that we don't care to do rust overrides for. For
  # example, we override rustc versions in `pkgs`. This means that if we
  # care about some program that uses rust, we will end up building it
  # ourselves. In most cases, we don't really care though: we just want
  # the binary. Therefore, for any packages that don't want to do
  # overrides for, we just use this.
  pkgs = import nixpkgs {
    inherit system;
    overlays = [ rust-overlay.overlays.default ];
  };

  rust = let
    rustChannel =
      (pkgs.rust-bin.fromRustupToolchainFile ../rust-toolchain).override {
        extensions =
          [ "clippy" "rust-analysis" "rust-docs" "rust-src" "rustfmt" ];
      };
  in {
    rustc = rustChannel;
    cargo = rustChannel;
    rust-fmt = rustChannel;
    rust-std = rustChannel;
    clippy = rustChannel;
    rustPlatform = pkgs.makeRustPlatform {
      rustc = rustChannel;
      cargo = rustChannel;
    };
  };

in {
  devShell = pkgs.mkShell {
    buildInputs = [ rust.rustc ];
    # Required by test-suite and in general let's set a uniform one.
    LANG = "C.UTF-8";

    # We set RUSTUP_HOME to non-standard location to avoid mixing
    # nix-provided rustup tools with those that user may have from some
    # other source. This is necessary as the nix tools tend to have a
    # recent glibc while other source use some old one and mixing the two
    # ends up poorly.
    #
    # https://tsuru.slack.com/archives/CDW0NFX96/p1617763514001500
    shellHook = ''
      export RUSTUP_HOME=$HOME/.rustup-spot-gitlab-driver-nix
    '';
  };
}
