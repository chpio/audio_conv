with import <nixpkgs> {};

stdenv.mkDerivation {
    name = "audio_conv";
    buildInputs = [
        stdenv
        pkg-config
        ffmpeg_4
        clang
        cargo
        rustc
        rls
    ];
    LIBCLANG_PATH="${llvmPackages.libclang}/lib";
}
