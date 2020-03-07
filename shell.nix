let
    moz_overlay = import (builtins.fetchTarball https://github.com/mozilla/nixpkgs-mozilla/archive/master.tar.gz);
    nixpkgs = import <nixpkgs> { overlays = [ moz_overlay ]; };
    rustpkgs = nixpkgs.rustChannels.stable;
in
    with nixpkgs;
    stdenv.mkDerivation {
        name = "audio-conv";
        buildInputs = [
            rustpkgs.rust
            rustpkgs.cargo
            rustpkgs.rls-preview
            rustpkgs.rustfmt-preview
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base # needed for opus, resample, ...
            gst_all_1.gst-plugins-good # needed for flac
        ];
    }
