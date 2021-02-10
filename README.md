# audio-conv

Takes two paths, all audio files encountered in the first path are transcoded and stored in the
second path. The directory structure from the first path gets also copied to the second path.

## Dependencies

Requires *gstreamer* version 1.10 or higher with the *base* plugin.

The supported source audio formats (or even other media that is able to contain audio) depend on
the installed *gstreamer* plugins.

## Installation via nix flakes

*audio-conv* can be easily installed via *nix flakes*:

```bash
$ nix profile install gitlab:chpio/audio-conv/release
```

## Generate example config

*audio-conv* is able to write an example config to your current directory:

```bash
$ audio-conv init
```

Now you need to edit the generated *audio-conv.yaml* file. And let it convert your audio files
by running it:

```bash
$ audio-conv
```
