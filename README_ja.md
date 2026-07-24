# funkot-autodj

[English](README.md) | 日本語 | [Bahasa Indonesia](README_id.md)

インドネシアのダンスミュージック Funkot 専用の自動DJ再生ツール。
プレイリストを渡すと、各曲のイントロ/アウトロを解析し、DJ式のクロスフェードで
途切れなくBGM再生する。

## Funkot 前提仕様

- 基準BPMは180。178や181など僅かにずれる曲はタイムストレッチで合わせる
- イントロ/アウトロはつなぎ用の固定マシンリズム(イントロは8/16/32/48/64/80/96小節、
  アウトロは8/16/32/64小節)。曲中のBPMは不定・可変のため、解析は曲の先頭と末尾だけを使う
- 再生はデフォルトで10%加速(198 BPM)。デフォルトは音程維持、オプションで
  ピッチ上昇(ターンテーブル式)も選択可。加速率も変更可

## つなぎの手順

基準は**次曲のイントロ終了（メイン開始）T0**。遷移全長は既定で16小節
（`2 × fade_bars + MAIN_GAP_BARS`）。両曲が聞こえる重なりはフェード区間の8小節のみ。

1. T0−16小節: 次曲を再生開始（ハイパス約300Hz、音量0）。4小節かけて線形フェードイン
2. T0−12小節: 次曲フェードイン完了と同時にハイパスを前曲へ切替。前曲の4小節フェードアウト開始
3. T0−8小節: 前曲音量0 → 直後に前デッキを破棄（以降再生しない）
4. T0: 次曲メイン開始
5. 長いイントロは `skip = intro − 16` で途中から入る。アウトロ側のつなぎ位置は
   「中域エネルギーの完全ドロップ」から16小節逆算（実曲では概ね末尾−48小節。
   64は過剰、完全ドロップ直撃の32は過少）

`F` やイントロ/アウトロが短いときはフェードを縮めて同じ形を維持する。

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
    --purge-auto-cache # 手動フラグなしのキャッシュを削除(手動ありは自動欄をクリア)
    --fill-missing-cache # 欠落/`needs_reanalysis`のみ再計算して終了
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

ライブ再生中は Enter で一時停止/再開。終了は Ctrl+C または kill。

対応形式: MP3 / AAC(m4a) / ALAC(m4a) / FLAC / Ogg Vorbis / WAV

## 解析キャッシュ

初回再生前に各曲の先頭・末尾のみを解析し、結果をファイル内容のハッシュを
キーにしたJSONとして `--cache-dir` に保存する。イントロ/アウトロ小節数の
自動推定が外れた曲は、JSONの `intro_bars` / `outro_bars` を手で書き換え、
対応する `intro_bars_manual` / `outro_bars_manual` を `true` にすること
（既定は `false`。手動フラグがあると `--purge-auto-cache` や再解析でも
その小節数は保持される）。
推定に自信がない場合は64小節にフォールバックし
`bars_estimated_low_confidence: true` が付く。側別フラグ
`intro_bars_low_confidence` / `outro_bars_low_confidence` も記録される。
両側が高信頼なら `intro < outro` もそのまま保持する（短いイントロ曲向け。
低信頼側だけ従来どおり保守的に補正する）。
`outro_bars` だけ変えるときは `outro_start` も
`total_frames − outro_bars × bar_len` に合わせて更新すること（読込時は再計算しない）。
キャッシュ形式が変わると `version` が上がり旧JSONは無効化される（現行は v8）。

起動オプション:

- `--purge-auto-cache` — 両手動フラグが `false` のエントリを削除。少なくとも
  一方が `true` のエントリは手動小節数を残し、自動算出欄をクリアして
  `needs_reanalysis: true` を立てる
- `--fill-missing-cache` — キャッシュ欠落または `needs_reanalysis` の曲だけ
  再解析して終了（完全ヒットはスキップ）。再解析後に手動フラグ側の小節数は
  マージで保持される

```sh
funkot-autodj -l playlist.txt --purge-auto-cache --fill-missing-cache
```

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
