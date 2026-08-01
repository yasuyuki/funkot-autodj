# メモリ — 全曲展開の内訳と削れる場所

ローダは曲を丸ごと RAM に展開する。組み込み用途（Android プレイヤー）では
これがピークを決めるので、どこに何本のバッファが同時に生きているかを把握してから
触ること。**削れる箇所と削れない箇所がはっきり分かれる。**

## 何本が同時に生きるか

### 保持側 — 再生中ずっと

`PreparedTrack.samples` は**出力レートの f32 インターリーブ・ステレオで全曲**。
6分曲・48kHz なら 138MB。エンジンが同時に持つのは最大4本:

| | 用途 |
|---|---|
| `active` | 再生中のデッキ |
| `next_track` | 次曲（ローダが先読み） |
| `last_track` | 巻き戻し用の履歴 |
| `prev` | トランジション中の前デッキ（終了後 `last_track` へ移る） |

プレイリストが3曲なら全曲が同時に載る。**ここを減らせるのはストリーミング化だけ**で、
それは `PreparedTrack` の設計変更になる。

### 一時側 — `prepare_track` 実行中だけ

`finish_prepare` → `stretch::render_track_owned` の経路で、全曲バッファが最大3本:

1. デコード結果（**入力**レート）
2. `time_stretch` の出力（入力レート）
3. `resample_fixed_rates` の出力（**出力**レート）

1 は 2 が出来た時点で不要になる。`render_preserve` の `drop(samples)` はここで
入力を返し、3 を確保する前に本数を2本に落とす。**この drop を消すとピークが戻る。**
`render_track_owned`（所有権を受け取る入口）とセットでしか効かない点に注意 —
`render_track`（`&[f32]`）で呼ぶと `Cow::Borrowed` なので drop は何も解放しない。

**入力レート == 出力レートなら 3 は存在しない**（`render_preserve` が `stretched` を
そのまま返す）。CLI の `--render` は既定 44.1kHz で、素材も 44.1kHz なので
**この経路では resample が走らない**。3本同時に生きるのはプレイヤー（48kHz 出力）と
`--sample-rate 48000` を渡した場合だけである。メモリを測るときにここを外すと
「変わらない」という誤った結論が出る。

## 実測

### コンテナ（`./dev.sh`、6.5分の実曲1曲、`--ci-fast --render`）

VmHWM を host 側から採取。

| 出力レート | `drop` 導入前 | 導入後 |
|---|---|---|
| 44.1kHz（resample 無し） | 286 MB | 286 MB（変化なし。3本目が存在しない） |
| 48kHz（resample あり） | 334 MB | **284 MB** |

48kHz で −50MB（−15%）。残った 284MB はデコード結果と `time_stretch` 出力の2本で、
**全曲一括ストレッチである限りこれが床**になる。

3曲を `--jobs 0`（全CPU並列 prepare）で回すと 877MB → 876MB でほとんど動かない。
並列 prepare のピークは別の要因で決まるため、この変更の効果を見る計測には使えない。

### Android 実機（Pixel 8 Pro、実曲3曲、10分前面再生）

| | PSS | RSS | Native Heap |
|---|---|---|---|
| 開始 | 465 MB | 644 MB | 297 MB |
| ピーク | 903 MB | 1081 MB | 735 MB |

OOM はしないが RSS が 1GB を超える。曲の切り替わりに同期した鋸歯状で、1時間の
連続再生でも谷の水準は上がらない（リークではない）。

## 次に削るなら

保持側（上表の最大4本）が本丸で、これはストリーミング化—— デッキがブロック単位で
デコード+ストレッチする形 —— でしか減らない。`stretch.exact()` の一括呼び出しを
`process()` のループに変える必要があり、**出力のバイト一致を再検証しないと入れられない**。

一時側でさらに削るなら、`time_stretch` の出力を全曲分まとめて確保するのをやめる
（＝同じくブロック化）ことになる。こちらも同じ検証が要る。

いずれも「メモリの少ない機種で鳴らす」ためだけの投資であり、現行の対象機では
必要になっていない。

## 検証のしかた

出力が1バイトも変わらないことが前提の変更なら、`--render` の sha256 を前後で比較する。

```sh
./dev.sh cargo run -p funkot-cli --release -- --ci-fast --sample-rate 48000 \
  --render OUT.wav --cache-dir funkot-cache TRACK.flac
sha256sum OUT.wav
```

ピークは host から VmHWM を拾う（コンテナ内のプロセスも host の `/proc` に見える）。
`pgrep -f target/release/funkot-autodj` で実バイナリを掴むこと —
`cargo run` の docker / sh ラッパは cmdline に `funkot-cli` を含むので、
そちらを掴むと数MBの値が採れて計測が無意味になる。
