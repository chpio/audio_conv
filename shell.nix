let
    nixpkgs = import <nixpkgs> {};
in
    with nixpkgs;
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
    }
