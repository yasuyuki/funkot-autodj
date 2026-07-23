# funkot-autodj

[English](README.md) | [日本語](README_ja.md)

Auto-DJ playback for Indonesian Funkot dance music. Give it a playlist and it
analyzes each track’s intro/outro, then crossfades DJ-style for continuous BGM.

## Install

Download the latest release from
[GitHub Releases](https://github.com/yasuyuki/funkot-autodj/releases):

| Asset | Contents |
| --- | --- |
| `funkot-autodj-{os}.zip` | CLI binary |
| `funkot-autodj-sdk-{os}.zip` | C ABI library + `funkot.h` |

`{os}` is one of `linux-x64`, `windows-x64`, or `macos-arm64`.

Unpack the CLI zip and run `funkot-autodj` (or `funkot-autodj.exe` on Windows).
Building from source is covered in [README_ja.md](README_ja.md#開発).

## Quick start

```sh
# Files on the command line
funkot-autodj track1.flac track2.mp3 track3.m4a

# Playlist file (one path per line, # comments, m3u-compatible)
funkot-autodj -l playlist.txt

funkot-autodj -l playlist.txt \
    --rate 1.10        # playback rate (default 1.10 = 198 BPM)
    --pitch-shift      # raise pitch with rate (turntable-style)
    --fade-bars 4      # fade length in bars
    --random           # shuffle each loop
    --no-loop          # stop after one pass (default: loop forever)
    --cache-dir DIR    # analysis cache directory
    --render out.wav   # write WAV instead of live playback
```

Supported formats: MP3 / AAC(m4a) / ALAC(m4a) / FLAC / Ogg Vorbis / WAV.

## Funkot assumptions (short)

- Base BPM 180; slight mismatches are time-stretched to match
- Intro/outro are fixed machine-rhythm sections (8/16/32/64 bars); analysis
  uses only the start and end of each track
- Default playback is 10% faster (198 BPM), pitch-preserved unless
  `--pitch-shift` is set

Full transition rules, cache format, C ABI, and Docker-based development notes
are in [README_ja.md](README_ja.md).

## License

MIT. See [LICENSE](LICENSE).
