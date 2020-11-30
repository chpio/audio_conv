{
    description = "Converts audio files";

    inputs = {
        nixpkgs.url = github:NixOS/nixpkgs/nixos-20.09;
        import-cargo.url = github:edolstra/import-cargo;
    };

    outputs = { self, nixpkgs, import-cargo }:
        let
            inherit (import-cargo.builders) importCargo;
        in {
            defaultPackage.x86_64-linux =
                with import nixpkgs { system = "x86_64-linux"; };
                stdenv.mkDerivation {
                    name = "audio-conv";
                    src = self;

                    nativeBuildInputs = [
                        # setupHook which makes sure that a CARGO_HOME with vendored dependencies
                        # exists
                        (importCargo { lockFile = ./Cargo.lock; inherit pkgs; }).cargoHome

                        # Build-time dependencies
                        cargo
                        rustc
                        pkg-config
                    ];

                    buildInputs = [
                        gst_all_1.gstreamer

                        # needed for opus, resample, ...
                        gst_all_1.gst-plugins-base

                        # needed for flac
                        gst_all_1.gst-plugins-good
                    ];

                    buildPhase = ''
                        cargo build --release --offline
                    '';

                    installPhase = ''
                        install -Dm775 ./target/release/audio-conv $out/bin/audio-conv
                    '';
                };

            devShell.x86_64-linux =
                with import nixpkgs { system = "x86_64-linux"; };
                stdenv.mkDerivation {
                    name = "audio-conv";
                    buildInputs = [
                        cargo
                        rustc
                        rustfmt
                        rust-analyzer

                        pkg-config
                        gst_all_1.gstreamer

                        # needed for opus, resample, ...
                        gst_all_1.gst-plugins-base

                        # needed for flac
                        gst_all_1.gst-plugins-good
                    ];
                };
        };
}
