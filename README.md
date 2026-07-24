# funkot-autodj

English | [日本語](README_ja.md) | [Bahasa Indonesia](README_id.md)

Auto-DJ playback tool for Indonesian Funkot dance music.
Give it a playlist and it analyzes each track’s intro/outro, then crossfades
DJ-style for continuous BGM playback.

## Funkot assumptions

- Base BPM is 180. Tracks that drift slightly (e.g. 178 or 181) are time-stretched to match
- Intro/outro are fixed machine-rhythm sections for mixing (intro: 8/16/32/48/64/80/96 bars,
  outro: 8/16/32/64 bars). Mid-track BPM is irregular/variable, so analysis uses only the
  start and end of each track
- Playback defaults to 10% faster (198 BPM). Pitch is preserved by default; optionally
  raise pitch with rate (turntable-style). The rate itself is also configurable

## Transition procedure

The reference point is **the next track’s intro end (main start) T0**. Default transition
length is 16 bars (`2 × fade_bars + MAIN_GAP_BARS`). The audible overlap of both tracks
is only the 8-bar fade window.

1. T0−16 bars: start the next track (high-pass ~300 Hz, volume 0). Linear fade-in over 4 bars
2. T0−12 bars: when next-track fade-in finishes, flip the high-pass to the previous track.
   Start the previous track’s 4-bar fade-out
3. T0−8 bars: previous volume reaches 0 → immediately drop the previous deck (no further playback)
4. T0: next track’s main starts
5. Long intros enter mid-way with `skip = intro − 16`. The outro-side mix point is
   back-calculated 16 bars from the “full mid-band energy drop” (in practice roughly
   end−48 bars; 64 is too much, hitting the full drop at 32 is too little)

When `F` or the intro/outro is short, shorten the fade while keeping the same shape.

## Usage

```sh
# Files on the command line
funkot-autodj track1.flac track2.mp3 track3.m4a

# Playlist file (one path per line, # comments allowed, m3u-compatible)
funkot-autodj -l playlist.txt

# Main options
funkot-autodj -l playlist.txt \
    --rate 1.10        # playback rate (default 1.10 = 198 BPM)
    --pitch-shift      # raise pitch with rate (no pitch preservation)
    --fade-bars 4      # fade length in bars
    --highpass-hz 300  # high-pass frequency for MID/HIGH during transitions
    --random           # shuffle (re-shuffle each loop)
    --no-loop          # stop after one pass (default: loop forever)
    --no-gain          # disable RMS loudness normalization
    --cache-dir DIR    # analysis cache directory
    --purge-auto-cache # delete caches without manual flags (clear auto fields if manual)
    --fill-missing-cache # recompute only missing/`needs_reanalysis`, then exit
    --sample-rate HZ   # output sample rate (live: device default; with --render: 44100)
    --render out.wav   # write WAV instead of live playback (for listening tests; implies --no-loop)
    --wav-format f32   # offline WAV format: f32 (default) / s24 / s16
```

Default `--render` output is **32-bit float WAV** (internal f32 mix bus written without
clamping). Use `--wav-format s24` or `s16` only when integer PCM is required (both with
TPDF dither). On write completion, peak level and count of samples/frames with `|x|>1`
are printed (no limiter is applied).

With `--render`, pacing defaults to up to 10× so the loader (decode/analyze/stretch of the
next track) can keep up. For CI / batch, prep work can be parallelized:

```sh
# Fastest offline (analysis/stretch results match single-thread; only wall-clock shrinks)
./dev.sh cargo run -p funkot-cli --release -- \
  -l playlist.txt --render out.wav --wav-format f32 --ci-fast

# Equivalent explicit flags: --no-loop --render-speed 0 --jobs 0 (0 = all CPUs)
./dev.sh cargo run -p funkot-cli --release -- \
  -l playlist.txt --render out.wav --jobs 0 --render-speed 0 --no-loop
```

Minimal fixture generation for analysis goldens (does not build a full mix):

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  --gen-test-fixtures funkot-core/tests/fixtures
./dev.sh cargo test -p funkot-core --release --test analysis_golden
```

During live playback, press Enter to pause/resume. Stop with Ctrl+C or kill.

Supported formats: MP3 / AAC(m4a) / ALAC(m4a) / FLAC / Ogg Vorbis / WAV

## Analysis cache

Before the first playback, only the start and end of each track are analyzed. Results are
stored as JSON under `--cache-dir`, keyed by a hash of the file contents. If automatic
intro/outro bar estimates are wrong, edit `intro_bars` / `outro_bars` in the JSON by hand
and set the matching `intro_bars_manual` / `outro_bars_manual` to `true`
(default is `false`. When a manual flag is set, those bar counts are kept across
`--purge-auto-cache` and reanalysis).
When confidence is low, estimation falls back to 64 bars and sets
`bars_estimated_low_confidence: true`. Per-side flags
`intro_bars_low_confidence` / `outro_bars_low_confidence` are also recorded.
If both sides are high-confidence, `intro < outro` is kept as-is (for short-intro tracks;
only the low-confidence side is corrected conservatively as before).
When changing only `outro_bars`, also update `outro_start` to
`total_frames − outro_bars × bar_len` (it is not recomputed on load).
When the cache format changes, `version` is bumped and old JSON is invalidated (currently v8).

Startup options:

- `--purge-auto-cache` — delete entries where both manual flags are `false`. Entries with
  at least one `true` keep the manual bar counts, clear auto-computed fields, and set
  `needs_reanalysis: true`
- `--fill-missing-cache` — reanalyze only tracks with a missing cache or
  `needs_reanalysis`, then exit (full hits are skipped). After reanalysis, bar counts on
  the manual-flag side are preserved via merge

```sh
funkot-autodj -l playlist.txt --purge-auto-cache --fill-missing-cache
```

## Layout

- `funkot-core` — analysis and mix engine library. Exposes a C ABI
  (staticlib/cdylib) for embedding in smartphone apps and similar.
  No audio output; the host pulls samples
- `funkot-cli` — thin CLI that plays in real time via cpal

## Embedding (C ABI)

Header: [`include/funkot.h`](include/funkot.h). Build artifacts:
`target/release/libfunkot_core.a` (staticlib) and
`target/release/libfunkot_core.so` (cdylib). The host owns audio I/O and
pulls interleaved stereo `f32` from the engine.

```c
FunkotOptions opt; funkot_options_default(&opt);
opt.loop_playlist = 0; opt.output_sample_rate = 48000;
FunkotEngine* e = funkot_engine_new(&opt, paths, n, err, sizeof err);
float buf[1024 * 2];
while (funkot_engine_render(e, buf, 1024) > 0) { /* play buf */ }
FunkotEvent ev; while (funkot_engine_poll_event(e, &ev)) { /* UI */ }
funkot_engine_free(e);
```

## Development

Build and test inside the Docker container.

```sh
./dev.sh cargo build --workspace
./dev.sh cargo test --workspace
./dev.sh cargo run -p funkot-cli -- -l testdata/playlist.txt --render /work/out.wav
```

Only checks that need real device audio output should use `cargo run` on the host
(on Linux, `libasound2-dev` and `pkg-config` are required).

Synthetic Funkot-style track generation and demo mix for listening tests:

```sh
./dev.sh sh -c "cargo run -p funkot-core --example gen_synth --features testutil --release -- testdata/synth"
./dev.sh sh -c "cargo run -p funkot-cli --release -- \
    testdata/synth/track_a_180_i16_o16.wav \
    testdata/synth/track_b_178_i32_o8.wav \
    testdata/synth/track_c_181_i64_o64.wav \
    --no-loop --render testdata/demo_mix.wav --cache-dir testdata/cache"
```

Diagnostic A/B for Signalsmith window length (production stays on the official
120 ms/30 ms `preset_default`):

```sh
./dev.sh sh -c "cargo run -p funkot-core --example stretch_compare --release -- \
    /path/to/track.flac /work/testdata/stretch_ab --seconds 90"
```
