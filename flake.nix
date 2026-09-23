{
  description = "Rust firmware dla Cardputer ADV (ESP32-S3, Xtensa)";

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
            # rustup zarządza toolchainem bezpośrednio w $HOME.
            # ESP32-S3 to Xtensa — wymaga forkowanego toolchaina Rusta ("esp"),
            # którego nie da się uzyskać z nixpkgs/oxalica-overlay.
            rustup
            espup
            espflash
          ];

          shellHook = ''
            echo ""
            echo "=== ratputer-rs — Cardputer ADV (ESP32-S3) ==="
            echo ""

            # Shims rustup (cargo/rustc) na początku PATH
            export PATH="$HOME/.cargo/bin:$PATH"
            # Linker/toolchain GCC dla Xtensa (instaluje espup)
            [ -f "$HOME/export-esp.sh" ] && source "$HOME/export-esp.sh"

            if ! rustup toolchain list 2>/dev/null | grep -q '^esp'; then
              echo "⚠️  Brak toolchaina 'esp' (Xtensa fork). Zainstaluj raz:"
              echo "     espup install"
              echo ""
              echo "   Uwaga (NixOS): forkowany rustc z espup wymaga dynamicznego"
              echo "   linkera. Włącz w konfiguracji NixOS:"
              echo "     programs.nix-ld.enable = true;"
            fi

            echo "Build:   cargo build --release"
            echo "Flash:   cargo run --release   (espflash + monitoring)"
            echo ""
          '';
        };
      });
}
