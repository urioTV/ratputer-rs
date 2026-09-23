{
  description = "Rust firmware for the Cardputer ADV (ESP32-S3, Xtensa)";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = nixpkgs.legacyPackages.${system}; in
      {
        devShells.default = pkgs.mkShell {
          name = "ratputer-rs-devshell";

          packages = with pkgs; [
            # rustup manages the toolchain directly in $HOME.
            # ESP32-S3 is Xtensa — it needs the forked Rust toolchain ("esp"),
            # not available from nixpkgs/oxalica-overlay.
            rustup
            espup
            espflash
          ];

          shellHook = ''
            echo ""
            echo "=== ratputer-rs — Cardputer ADV (ESP32-S3) ==="
            echo ""

            # rustup shims (cargo/rustc) first in PATH
            export PATH="$HOME/.cargo/bin:$PATH"
            # GCC linker/toolchain for Xtensa (installed by espup)
            [ -f "$HOME/export-esp.sh" ] && source "$HOME/export-esp.sh"

            if ! rustup toolchain list 2>/dev/null | grep -q '^esp'; then
              echo "⚠️  Missing the 'esp' toolchain (Xtensa fork). Install once:"
              echo "     espup install"
              echo ""
              echo "   Note (NixOS): the espup-forked rustc needs a dynamic linker."
              echo "   Enable it in your NixOS config:"
              echo "     programs.nix-ld.enable = true;"
            fi

            echo "Build:   cargo build --release"
            echo "Flash:   cargo run --release   (espflash + monitor)"
            echo ""
          '';
        };
      });
}
