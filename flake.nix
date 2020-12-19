{
    description = "Converts audio files";

    inputs = {
        nixpkgs.url = github:NixOS/nixpkgs;
        flake-utils.url = "github:numtide/flake-utils";
        import-cargo.url = github:edolstra/import-cargo;
    };

    outputs = { self, flake-utils, nixpkgs, import-cargo }:
        flake-utils.lib.eachDefaultSystem (system:
            let
                pkgs = import nixpkgs { inherit system; };

                buildtimeDeps = with pkgs; [
                    cargo
                    rustc
                    pkg-config
                ];

                runtimeDeps = with pkgs; [
                    gst_all_1.gstreamer

                    # needed for opus, resample, ...
                    gst_all_1.gst-plugins-base

                    # needed for flac
                    gst_all_1.gst-plugins-good
                ];

                inherit (import-cargo.builders) importCargo;
            in {
                defaultPackage = pkgs.stdenv.mkDerivation {
                    name = "audio-conv";
                    src = self;

                    nativeBuildInputs = [
                        # setupHook which makes sure that a CARGO_HOME with vendored dependencies
                        # exists
                        (importCargo { lockFile = ./Cargo.lock; inherit pkgs; }).cargoHome
                    ]
                        ++ buildtimeDeps;

                    buildInputs = runtimeDeps;

                    buildPhase = ''
                        cargo build --release --offline
                    '';

                    installPhase = ''
                        install -Dm775 ./target/release/audio-conv $out/bin/audio-conv
                    '';
                };

                devShell = pkgs.stdenv.mkDerivation {
                    name = "audio-conv";
                    buildInputs = [ pkgs.rustfmt pkgs.rust-analyzer ]
                        ++ buildtimeDeps
                        ++ runtimeDeps;
                };
            }
        );
}
