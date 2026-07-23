# funkot-autodj

[English](README.md) | [日本語](README_ja.md) | Bahasa Indonesia

Alat Auto-DJ khusus musik dansa Indonesia Funkot.
Berikan playlist, lalu setiap track dianalisis intro/outro-nya dan diputar
sebagai BGM tanpa putus dengan crossfade ala DJ.

## Asumsi Funkot

- BPM dasar 180. Track yang sedikit melenceng (mis. 178 atau 181) disesuaikan
  dengan time-stretch
- Intro/outro adalah ritme mesin tetap untuk transisi (8/16/32/64 bar). BPM di
  tengah lagu tidak tetap/bisa berubah, jadi analisis hanya memakai awal dan
  akhir track
- Playback default dipercepat 10% (198 BPM). Default mempertahankan pitch;
  opsi menaikkan pitch seiring rate (gaya turntable) juga tersedia. Rate bisa
  diubah

## Langkah transisi

1. Saat track sebelumnya masuk outro, track berikutnya mulai diputar dari
   intro (path MID/HIGH high-pass ~300 Hz, volume 0)
2. Fade-in linear selama 4 bar (`--fade-bars`) (frame pertama volume 0,
   frame terakhir 1)
3. Saat fade-in selesai, high-pass segera dialihkan ke track sebelumnya
4. Track sebelumnya di-fade-out linear selama 4 bar (frame terakhir volume 0).
   Selesainya fade-out dijadwalkan mundur agar jatuh "8 bar sebelum main
   track berikutnya" (setelah fade-out, intro track berikutnya masih menyisakan
   8 bar = `MAIN_GAP_BARS`). Segera setelah volume 0, deck sebelumnya dibuang
   dan tidak diputar lagi
5. Intro yang terlalu panjang dipotong dari tengah agar segmen intro saja
   maksimal 16 bar. Outro panjang terpotong alami oleh fade-out

## Cara pakai

```sh
# File langsung di command line
funkot-autodj track1.flac track2.mp3 track3.m4a

# File playlist (satu path per baris, # komentar, kompatibel m3u)
funkot-autodj -l playlist.txt

# Opsi utama
funkot-autodj -l playlist.txt \
    --rate 1.10        # laju playback (default 1.10 = 198 BPM)
    --pitch-shift      # naikkan pitch seiring rate (gaya turntable)
    --fade-bars 4      # panjang fade dalam bar
    --highpass-hz 300  # frekuensi high-pass path MID/HIGH untuk transisi
    --random           # acak setiap putaran
    --no-loop          # berhenti setelah satu putaran (default: loop terus)
    --no-gain          # nonaktifkan normalisasi volume RMS
    --cache-dir DIR    # direktori cache analisis
    --sample-rate HZ   # sample rate output (live = bawaan device, --render = 44100)
    --render out.wav   # tulis WAV alih-alih putar live (uji mendengar, implisit --no-loop)
    --wav-format f32   # format WAV offline: f32 (default) / s24 / s16
```

Output default `--render` adalah **32-bit float WAV** (bus mix internal f32
ditulis tanpa clamp). Pakai `--wav-format s24` atau `s16` hanya jika butuh
PCM integer (keduanya dengan TPDF dither). Saat selesai menulis, peak level
dan jumlah sample/frame `|x|>1` ditampilkan (tanpa limiter).

Saat `--render`, loader (decode/analisis/stretch track berikutnya) di-pace
hingga 10× secara default agar tetap mengejar. Di CI / batch, persiapan bisa
diparalelkan:

```sh
# Offline tercepat (hasil analisis/stretch sama single-thread; hanya wall clock lebih singkat)
./dev.sh cargo run -p funkot-cli --release -- \
  -l playlist.txt --render out.wav --wav-format f32 --ci-fast

# Setara eksplisit: --no-loop --render-speed 0 --jobs 0 (0 = semua CPU)
./dev.sh cargo run -p funkot-cli --release -- \
  -l playlist.txt --render out.wav --jobs 0 --render-speed 0 --no-loop
```

Generate fixture minimal untuk analysis golden (tanpa full mix):

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  --gen-test-fixtures funkot-core/tests/fixtures
./dev.sh cargo test -p funkot-core --release --test analysis_golden
```

Saat putar live, Enter untuk pause/resume. Keluar dengan Ctrl+C atau kill.

Format didukung: MP3 / AAC(m4a) / ALAC(m4a) / FLAC / Ogg Vorbis / WAV

## Cache analisis

Sebelum putar pertama, hanya awal dan akhir tiap track dianalisis; hasil
disimpan sebagai JSON dengan kunci hash isi file di `--cache-dir`. Jika
estimasi otomatis `intro_bars` / `outro_bars` meleset, edit JSON secara
manual untuk override (jika estimasi kurang yakin, fallback ke 64 bar dengan
`bars_estimated_low_confidence: true`. Flag per sisi
`intro_bars_low_confidence` / `outro_bars_low_confidence` juga dicatat.
Intro diselaraskan agar tidak lebih pendek dari outro: `intro >= outro`).

## Struktur

- `funkot-core` — library engine analisis/mix. Menyediakan C ABI
  (staticlib/cdylib), ditujukan untuk embedding ke app smartphone dll.
  Tidak punya output audio; host yang pull sample
- `funkot-cli` — CLI tipis yang memutar real-time lewat cpal

## Embedding (C ABI)

Header di [`include/funkot.h`](include/funkot.h). Artefak build:
`target/release/libfunkot_core.a` (staticlib) dan
`target/release/libfunkot_core.so` (cdylib). Host punya audio I/O dan
pull stereo interleaved `f32` dari engine.

```c
FunkotOptions opt; funkot_options_default(&opt);
opt.loop_playlist = 0; opt.output_sample_rate = 48000;
FunkotEngine* e = funkot_engine_new(&opt, paths, n, err, sizeof err);
float buf[1024 * 2];
while (funkot_engine_render(e, buf, 1024) > 0) { /* play buf */ }
FunkotEvent ev; while (funkot_engine_poll_event(e, &ev)) { /* UI */ }
funkot_engine_free(e);
```

## Pengembangan

Build dan test di dalam container Docker.

```sh
./dev.sh cargo build --workspace
./dev.sh cargo test --workspace
./dev.sh cargo run -p funkot-cli -- -l testdata/playlist.txt --render /work/out.wav
```

Verifikasi dengan output audio perangkat nyata hanya di host dengan
`cargo run` (di Linux butuh `libasound2-dev` dan `pkg-config`).

Generate track sintetis bergaya Funkot untuk uji mendengar dan demo mix:

```sh
./dev.sh sh -c "cargo run -p funkot-core --example gen_synth --features testutil --release -- testdata/synth"
./dev.sh sh -c "cargo run -p funkot-cli --release -- \
    testdata/synth/track_a_180_i16_o16.wav \
    testdata/synth/track_b_178_i32_o8.wav \
    testdata/synth/track_c_181_i64_o64.wav \
    --no-loop --render testdata/demo_mix.wav --cache-dir testdata/cache"
```

A/B diagnostik panjang jendela Signalsmith (produksi tetap
`preset_default` resmi 120 ms/30 ms):

```sh
./dev.sh sh -c "cargo run -p funkot-core --example stretch_compare --release -- \
    /path/to/track.flac /work/testdata/stretch_ab --seconds 90"
```

## Lisensi

MIT. Lihat [LICENSE](LICENSE).
