{
  description = "masil — tmux-compatible terminal workspace for running multiple AI coding agents side by side";

  # Single nixpkgs input keeps flake.lock reproducible so `nix run github:Ochichan/masil`
  # builds the exact same core + agent pair everywhere.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      # nixpkgs 26.11+ dropped x86_64-darwin (Intel macOS users can install via Homebrew or install.sh).
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      version = "0.1.0"; # keep in sync with agent/Cargo.toml, relay/Cargo.toml, crates/masil-wire/Cargo.toml
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (system: pkgs:
        let
          lib = pkgs.lib;

          # Include only the source trees needed to build core, agent, and relay so store
          # copies stay small and doc/CI/flake edits do not invalidate builds.
          repoSrc = lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                rel = lib.removePrefix (toString ./. + "/") (toString path);
                top = builtins.head (lib.splitString "/" rel);
                base = baseNameOf path;
              in
              builtins.elem top [ ".cargo" "agent" "core" "crates" "relay" ]
              && base != "target"
              && base != "result"
              && lib.cleanSourceFilter path type;
          };

          masilCore = pkgs.stdenv.mkDerivation {
            pname = "masil-core";
            inherit version;
            src = repoSrc;

            nativeBuildInputs = [
              pkgs.autoconf
              pkgs.automake
              pkgs.bison
              pkgs.m4
              pkgs.pkg-config
            ];

            buildInputs = [
              pkgs.libevent
              pkgs.ncurses
              pkgs.utf8proc
              pkgs.jemalloc
            ];

            preConfigure = ''
              cd core
              sh autogen.sh
            '';

            configureFlags = [
              "--disable-debug"
              "--enable-optimizations"
              "--enable-utf8proc"
              "--enable-jemalloc"
              "--sysconfdir=/etc"
            ];

            installPhase = ''
              runHook preInstall
              install -Dm755 tmux "$out/bin/masil"
              sed 's|@SYSCONFDIR@|/etc|g' tmux.1 > masil.1
              install -Dm644 masil.1 "$out/share/man/man1/masil.1"
              runHook postInstall
            '';

            meta = {
              description = "tmux-derived native terminal multiplexer core for masil";
              homepage = "https://github.com/Ochichan/masil";
              license = with lib.licenses; [ isc mit ];
              mainProgram = "masil";
              platforms = systems;
            };
          };

          masilAgent = pkgs.rustPlatform.buildRustPackage {
            pname = "masil-agent";
            inherit version;
            src = repoSrc;

            cargoRoot = "agent";
            buildAndTestSubdir = "agent";
            cargoLock.lockFile = ./agent/Cargo.lock;

            # masil-agent uses rusqlite's bundled SQLite 3.54.1 with the hardened
            # LIBSQLITE3_FLAGS in .cargo/config.toml; keep nixpkgs from forcing pkg-config.
            dontCargoUseSystemLibs = true;

            # Skip tests inside the Nix sandbox that spawn nested processes or Seatbelt;
            # full Rust + core suites run in CI before tagging.
            doCheck = false;

            meta = {
              description = "Agent observation, desk TUI, and opt-in E2EE remote gateway for masil";
              homepage = "https://github.com/Ochichan/masil";
              license = with lib.licenses; [ mit asl20 ];
              mainProgram = "masil-agent";
              platforms = systems;
            };
          };

          masilRelay = pkgs.rustPlatform.buildRustPackage {
            pname = "masil-relay";
            inherit version;
            src = repoSrc;

            cargoRoot = "relay";
            buildAndTestSubdir = "relay";
            cargoLock.lockFile = ./relay/Cargo.lock;

            meta = {
              description = "Self-hosted opaque E2EE envelope relay for masil mobile remote access";
              homepage = "https://github.com/Ochichan/masil";
              license = lib.licenses.mit;
              mainProgram = "masil-relay";
              platforms = systems;
            };
          };

          # Keep real sibling files under $out/bin so realpath(argv[0]) resolves both
          # `masil` and `masil-agent` in the same directory.
          masil = pkgs.stdenvNoCC.mkDerivation {
            pname = "masil";
            inherit version;
            dontUnpack = true;

            installPhase = ''
              runHook preInstall
              install -Dm755 ${masilCore}/bin/masil "$out/bin/masil"
              install -Dm755 ${masilAgent}/bin/masil-agent "$out/bin/masil-agent"
              mkdir -p "$out/share/man/man1"
              cp -r ${masilCore}/share/man/man1/. "$out/share/man/man1/"
              install -Dm644 ${./LICENSE} "$out/share/masil/LICENSE"
              install -Dm644 ${./NOTICE} "$out/share/masil/NOTICE"
              install -Dm644 ${./core/COPYING} "$out/share/masil/core/COPYING"
              install -Dm644 ${./core/UPSTREAM.json} "$out/share/masil/core/UPSTREAM.json"
              install -Dm644 ${./core/compat/yyjson/LICENSE} "$out/share/masil/core/compat/yyjson/LICENSE"
              install -Dm644 ${./agent/src/manifests/LICENSE} "$out/share/masil/agent/src/manifests/LICENSE"
              runHook postInstall
            '';

            meta = {
              description = "tmux-compatible terminal workspace for running multiple AI coding agents side by side";
              homepage = "https://github.com/Ochichan/masil";
              license = with lib.licenses; [ mit isc asl20 ];
              mainProgram = "masil";
              platforms = systems;
            };
          };
        in
        {
          default = masil;
          inherit masil;
          masil-core = masilCore;
          masil-agent = masilAgent;
          masil-relay = masilRelay;
        });

      apps = forAllSystems (system: _pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/masil";
        };
        masil = self.apps.${system}.default;
        masil-agent = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/masil-agent";
        };
        masil-relay = {
          type = "app";
          program = "${self.packages.${system}.masil-relay}/bin/masil-relay";
        };
      });

      devShells = forAllSystems (system: pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = [
            pkgs.autoconf
            pkgs.automake
            pkgs.bison
            pkgs.m4
            pkgs.pkg-config
            pkgs.python313
          ];
          buildInputs = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.libevent
            pkgs.ncurses
            pkgs.utf8proc
            pkgs.jemalloc
          ];
        };
      });

      formatter = forAllSystems (_system: pkgs: pkgs.nixpkgs-fmt);
    };
}
