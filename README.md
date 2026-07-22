# funkot-autodj

インドネシアのダンスミュージック Funkot 専用の自動DJ再生ツール。
プレイリストを渡すと、各曲のイントロ/アウトロを解析し、DJ式のクロスフェードで
途切れなくBGM再生する。

## Funkot 前提仕様

- 基準BPMは180。178や181など僅かにずれる曲はタイムストレッチで合わせる
- イントロ/アウトロはつなぎ用の固定マシンリズム(8/16/32/64小節)。曲中のBPMは
  不定・可変のため、解析は曲の先頭と末尾だけを使う
- 再生はデフォルトで10%加速(198 BPM)。デフォルトは音程維持、オプションで
  ピッチ上昇(ターンテーブル式)も選択可。加速率も変更可

## つなぎの手順

1. 前曲がアウトロに入った時点で、次曲をイントロから再生開始
   (ハイパス約300HzのMID/HIGHパス、音量0)
2. 4小節(`--fade-bars`)かけて線形フェードイン（先頭フレームは音量0、
   最終フレームで1）
3. フェードイン完了と同時にハイパスを前曲側へ即時切替
4. 前曲を4小節かけて線形フェードアウト（最終フレームで音量0）。
   フェードアウト完了は「次曲のメイン部開始の8小節前」になるよう逆算して
   スケジュール（フェードアウト後に次曲イントロが8小節残る = `MAIN_GAP_BARS`）。
   音量0の直後に前デッキを破棄し、以降は再生しない
5. 長すぎるイントロは途中から入ってイントロ単独区間を最大16小節に制限。
   長いアウトロはフェードアウトで自然に切り捨て

## 使い方

```sh
# ファイルを直接並べる
funkot-autodj track1.flac track2.mp3 track3.m4a

# プレイリストファイル(1行1パス、#コメント可、m3u互換)
funkot-autodj -l playlist.txt

# 主なオプション
funkot-autodj -l playlist.txt \
    --rate 1.10        # 加速率(デフォルト1.10 = 198BPM)
    --pitch-shift      # 音程維持せず加速分ピッチを上げる
    --fade-bars 4      # フェード長(小節)
    --highpass-hz 300  # つなぎ用MID/HIGHパスのハイパス周波数
    --random           # ランダム再生(周回ごとに再シャッフル)
    --no-loop          # 一巡したら終了(デフォルトは無限ループ)
    --no-gain          # RMS音量正規化を無効化
    --cache-dir DIR    # 解析キャッシュの保存先
    --sample-rate HZ   # 出力サンプリングレート(ライブはデバイス既定、--render時は44100)
    --render out.wav   # 再生せずWAVに書き出し(聴感テスト用、暗黙で--no-loop)
    --wav-format f32   # オフラインWAV形式: f32(既定) / s24 / s16
```

`--render` の既定出力は **32-bit float WAV**（内部 f32 ミックスバスをクランプせず書き出し）。
整数 PCM が必要なときだけ `--wav-format s24` または `s16`（いずれも TPDF dither 付き）。
書き出し完了時にピークレベルと `|x|>1` のサンプル/フレーム数を表示する（リミッタは掛けない）。

`--render` 時はローダー(次曲のデコード・解析・ストレッチ)が追いつけるよう
既定で最大10倍速にペースする。CI / バッチでは前準備を並列化できる:

```sh
# 最速オフライン（解析・ストレッチ結果は単スレッドと同じ。壁時計だけ短縮）
./dev.sh cargo run -p funkot-cli --release -- \
  -l playlist.txt --render out.wav --wav-format f32 --ci-fast

# 同等の明示指定: --no-loop --render-speed 0 --jobs 0（0 = 全CPU）
./dev.sh cargo run -p funkot-cli --release -- \
  -l playlist.txt --render out.wav --jobs 0 --render-speed 0 --no-loop
```

解析ゴールデン用の最小フィクスチャ生成（フルミックスは作らない）:

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  --gen-test-fixtures funkot-core/tests/fixtures
./dev.sh cargo test -p funkot-core --release --test analysis_golden
```

終了は Ctrl+C または kill。

対応形式: MP3 / AAC(m4a) / ALAC(m4a) / FLAC / Ogg Vorbis / WAV

## 解析キャッシュ

初回再生前に各曲の先頭・末尾のみを解析し、結果をファイル内容のハッシュを
キーにしたJSONとして `--cache-dir` に保存する。イントロ/アウトロ小節数の
自動推定が外れた曲は、JSONの `intro_bars` / `outro_bars` を手で書き換えれば
上書きできる(推定に自信がない場合は64小節にフォールバックし
`bars_estimated_low_confidence: true` が付く。側別フラグ
`intro_bars_low_confidence` / `outro_bars_low_confidence` も記録される。
イントロはアウトロ以上の長さになるよう両側を突き合わせる: `intro >= outro`)。

## 構成

- `funkot-core` — 解析・ミックスエンジンのライブラリ。C ABI
  (staticlib/cdylib)を持ち、スマートフォンアプリ等への組み込みを想定。
  音声出力は持たず、ホストがサンプルをpullする設計
- `funkot-cli` — cpalで実時間再生する薄いCLI

## 組み込み (C ABI)

ヘッダは [`include/funkot.h`](include/funkot.h)。ビルド成果物は
`target/release/libfunkot_core.a`（staticlib）と
`target/release/libfunkot_core.so`（cdylib）。ホスト側が音声 I/O を持ち、
エンジンから interleaved stereo `f32` を pull する。

```c
FunkotOptions opt; funkot_options_default(&opt);
opt.loop_playlist = 0; opt.output_sample_rate = 48000;
FunkotEngine* e = funkot_engine_new(&opt, paths, n, err, sizeof err);
float buf[1024 * 2];
while (funkot_engine_render(e, buf, 1024) > 0) { /* play buf */ }
FunkotEvent ev; while (funkot_engine_poll_event(e, &ev)) { /* UI */ }
funkot_engine_free(e);
```

## 開発

Dockerコンテナ内でビルド・テストする。

```sh
./dev.sh cargo build --workspace
./dev.sh cargo test --workspace
./dev.sh cargo run -p funkot-cli -- -l testdata/playlist.txt --render /work/out.wav
```

実機の音声出力を伴う確認だけはホスト側で `cargo run` する
(Linuxでは `libasound2-dev` と `pkg-config` が必要)。

聴感テスト用の合成Funkot風トラック生成とデモミックス:

```sh
./dev.sh sh -c "cargo run -p funkot-core --example gen_synth --features testutil --release -- testdata/synth"
./dev.sh sh -c "cargo run -p funkot-cli --release -- \
    testdata/synth/track_a_180_i16_o16.wav \
    testdata/synth/track_b_178_i32_o8.wav \
    testdata/synth/track_c_181_i64_o64.wav \
    --no-loop --render testdata/demo_mix.wav --cache-dir testdata/cache"
```

Signalsmith 窓長の診断用 A/B（本番は公式 120 ms/30 ms `preset_default` のまま）:

```sh
./dev.sh sh -c "cargo run -p funkot-core --example stretch_compare --release -- \
    /path/to/track.flac /work/testdata/stretch_ab --seconds 90"
```
