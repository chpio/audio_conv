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
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base # needed for opus, resample, ...
            gst_all_1.gst-plugins-good # needed for flac
        ];
    }
