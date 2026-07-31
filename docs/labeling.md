# 区間ラベリング（`--label-sections`）の実行手順

正解ラベル `labels.tsv` を人間が付けるための CLI。
[section-analysis-redesign.md](section-analysis-redesign.md) の Stage 3 はこれ待ちで止まる。

Docker（`./dev.sh`）は音声デバイスを持たないので、**ラベリングだけホストで実行する**。

## 前提（一度だけ）

```sh
sudo apt install -y libclang-dev          # signalsmith-stretch の bindgen が要求
sudo apt install -y libasound2-plugins    # WSL2 に ALSA カードが無く cpal が落ちるため
# ~/.asoundrc に pcm.!default / ctl.!default { type pulse } を置く
# WSLg の PulseAudio は PULSE_SERVER=unix:/mnt/wslg/PulseServer で既に動いている
```

## ビルドと実行

```sh
# target/ は Docker 名前付きボリュームの root 所有マウント点なので別ディレクトリを使う
CARGO_TARGET_DIR=target-host \
PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
  cargo build -p funkot-cli --release

# キャッシュを事前に温める（CACHE_VERSION が上がると全再解析になる）
./dev.sh cargo run -p funkot-cli --release -- \
  -l testdata/file_list.txt --cache-dir funkot-cache --fill-missing-cache --jobs 0

# ラベリング
./target-host/release/funkot-autodj \
  --label-sections -l testdata/file_list.txt --labels testdata/labels.tsv \
  --cache-dir funkot-cache
```

`PKG_CONFIG_PATH` が要るのは PATH 先頭の Homebrew 版 pkg-config が
`/usr/lib/x86_64-linux-gnu/pkgconfig` を見ないため（ALSA 自体は入っている）。

## 操作

`y`/Enter 採用 · `←`/`→` 候補移動 · `+` ±8/±16小節切替 · `r` 再生し直し ·
`a` 曖昧（許容集合トグル）· `n` メモ · `s` スキップ · `q` 保存して終了。

1曲ごとに保存され、既ラベルの曲はハッシュでスキップされるので中断・再開できる。
クリックが聞き取りにくい／ダッキングが邪魔なら `--click-db`（既定 12.0）と
`--click-duck-db`（既定 18.0）で調整する。

**注意**: UI の `OUTRO_CANDIDATES {8,16,32,48,64}` は**構造境界**の集合としては妥当
（24 はトリガ側にしか現れない = 8+リード16）。ただし候補格子に載らない構造境界に
遭遇したら、無理に近い候補を選ばず **`n` でメモを残す**こと。数が溜まったら
候補集合の拡張を検討する。

クリックが小節頭から外れて聞こえた場合も `n` メモを残す
（[guide-clicks.md](guide-clicks.md) の「テンポ refit の限界」に該当しうる）。

## ラベルを捨ててやり直すとき

`labels.tsv` を消すだけでは元に戻らない。**ラベリングはキャッシュも書き換える**:
1曲終えるたびに `cache::set_manual_bars` が `intro_bars` をラベル値で上書きし
`intro_bars_manual: true` を立てる（アウトロ側は書かない。書くと mix リード +16 を
推測することになるため）。この上書きは**元に戻せない** — `set_manual_bars` の
`None` は「変更しない」であって「クリアする」ではなく、解析器が本来出した
`intro_bars` はキャッシュのどこにも残っていない。

放置すると、UI の初期候補も `eval_sections` の入力も旧ラベル由来の値に
引きずられる。やり直すときは該当エントリを**捨てて作り直す**:

```sh
rm -f testdata/labels.tsv
rm -f $(grep -lE '"(intro|outro)_bars_manual" *: *true' funkot-cache/*.json)
./dev.sh cargo run -p funkot-cli --release -- \
  -l testdata/file_list.txt --cache-dir funkot-cache --fill-missing-cache --jobs 0
grep -lE '"(intro|outro)_bars_manual" *: *true' funkot-cache/*.json | wc -l   # 0 を確認
```

解析は決定的なので再解析で元の推定値に戻る。**中断したセッションの残骸**が混じる点にも
注意（`labels.tsv` に行が無くてもキャッシュにだけ manual フラグが立っていることがある。
実際 2026-07-31 のやり直しでは 5行に対して 6件が該当した）。

ガイドクリックの位置を変える修正を入れたあとにやり直す場合は、**ホストの
`target-host/release/funkot-autodj` を再ビルドしてから**始めること。古いバイナリは
修正前のアンカーでクリップを作る。

## WSLg の PulseAudio は勝手に死ぬ

Windows 側の RDP オーディオエンドポイントが落ちると `/mnt/wslg/pulseaudio.log` に
`module-rdp-sink.c: data_send: send failed` が出て、以後サーバが応答しなくなる。
症状は2段階:

- **起動前に死んでいる** → `snd_pcm_open` が `Connection refused (111)` または
  30秒の `Timeout` で落ち、CLI が起動しない。復旧は `wsl.exe --shutdown`
  （＝WSL 全体の再起動）が確実。funkot 側では直せない
- **起動後に死ぬ** → ALSA pulse プラグインが以後ずっと `EIO` を返す。cpal の ALSA
  ワーカーはこの種のエラーをバックオフ無しで再試行するので、エラーコールバックが
  **実測 188万回/秒** 発火する。`d00884d` 以前は1コールバック1行を `eprintln!` して
  いたため 20秒放置で 720万行 / 648 MB が流れ、ターミナルが死んだ。
  `funkot-cli/src/stream_error.rs` で「2秒に1行へ集約 → 連発を検知したらストリームを
  破棄して黙る」に変更済み。今は3行出て止まり、`r` で再取得を試みる（10秒クールダウン）

音声デバイス無しで再現したいときは `snd_pcm_avail_delay` を `LD_PRELOAD` で `-EIO` に
差し替え、`~/.asoundrc` 相当を `pcm.!default { type null }` にすればよい
（実デバイス不要・実 libasound 経路のまま）。
