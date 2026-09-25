{
  description = "Rust firmware for the Cardputer ADV (ESP32-S3, Xtensa)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    esp-rs-nix = {
      url = "github:leighleighleigh/esp-rs-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      esp-rs-nix,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # Pin exact compiler versions instead of inheriting the community
        # flake's moving defaults.
        espToolchain = esp-rs-nix.packages.${system}.esp-rs.override {
          rustc-version = "1.98.0.0";
          crosstool-version = "16.1.0_20260609";
        };
        buildCommand = pkgs.writeShellApplication {
          name = "build";
          runtimeInputs = [
            pkgs.git
            espToolchain
            pkgs.espflash
          ];
          text = ''
            project_root="$(git rev-parse --show-toplevel)"
            cd "$project_root"
            cargo xtask build
          '';
        };
        flashCommand = pkgs.writeShellApplication {
          name = "flash";
          runtimeInputs = [
            pkgs.git
            espToolchain
            pkgs.espflash
          ];
          text = ''
            project_root="$(git rev-parse --show-toplevel)"
            cd "$project_root"
            cargo xtask build
            exec espflash write-bin 0x0 ratputer-adv.bin
          '';
        };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "ratputer-rs-devshell";

          packages = with pkgs; [
            # Keep the compiler and target-specific linker binaries on PATH;
            # rustup still provides the standard proxy behavior for Cargo.
            espToolchain
            rustup
            espflash
            python3 # tools/update-hadris-fat.sh
            curl # tools/update-hadris-fat.sh
            buildCommand
            flashCommand
          ];

          # The Xtensa Rust fork, rust-src, LLVM, and GCC are supplied by Nix.
          # This takes precedence over the "esp" channel in rust-toolchain.toml.
          RUSTUP_TOOLCHAIN = "${espToolchain}";

          shellHook = ''
            echo ""
            echo "=== ratputer-rs — Cardputer ADV (ESP32-S3) ==="
            echo ""

            echo "Toolchain: $(rustc --version)"
            echo "Build:     build (merged and verified image)"
            echo "Flash:     flash (build, flash, and verify)"
            echo "Console:   serial terminal on the CDC port (PuTTY / screen / picocom)"
            echo ""
          '';
        };
      }
    );
}
