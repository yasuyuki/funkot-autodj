# gitignore 対象データの分類と再生成

このリポジトリの ignore 対象は、消えたときのコストで5つに分かれる。**保護するのは
「再生成不可」だけで、残りは1バイトも守らない。** 迷ったらこの表に戻る。

`testdata/` の現物の目録は `testdata/README.md`（これ自体も ignore 対象）にある。
本書は分類と再生成手順の正で、`testdata/README.md` はいまローカルに何があるかの索引。

| クラス | 対象 | 扱い |
|---|---|---|
| A 再生成可能 | `target*/`、`dist/`、`funkot-core/tests/fixtures/*.wav` | 消してよい。下のコマンドで戻る |
| B 外部から再取得 | 原盤（実音源） | **リポジトリに置かない。** `FUNKOT_TESTDATA_DIR` で音楽ライブラリを指す |
| C 高コストな派生 | `testdata/phase_ab/`、`testdata/click_*/`、`testdata/opus_s1/`、`testdata/synth/`、`whitelabel2022fall-b_transitions/` | 消してよい。再生成には原盤が要る |
| D キャッシュ | `funkot-cache/`、`testdata/cache/`、`testdata/real-cache-v*/` | 消してよい。温め直す |
| E **再生成不可** | `testdata/labels.tsv`、`testdata/survey.tsv`、`testdata/relabel_*.txt`、実耳評価の `*.md`、サーベイ生出力、`testdata/rekey_result.tsv`、`HANDOFF.md` | **公開リポジトリの外の private store が正。** 所在は `HANDOFF.md` |

## E がなぜ ignore なのか、どう守るか

`labels.tsv` と `survey.tsv` は**人が実耳で作った唯一のデータ**で、再解析では戻らない。
公開リポジトリなので追跡できず、かつ ignore されているため、Git を経由するどの
バックアップにも入らない（bundle は tracked 履歴しか含まない）。

そのため、これらの実体はリポジトリ外の private store に置き、作業ツリーへは
そこから戻す。新しい checkout を作ったら、まずそれを実行すること。手順は `HANDOFF.md`。

`testdata/README.md` の「消してよいもの」節は E を含まない。消す前に本書の表で確認する。

## 原盤の置き場所（`FUNKOT_TESTDATA_DIR`）

**原盤はこのリポジトリに1つも無い。** 音楽ライブラリが唯一の所在で、
実音源を使う省略可能なテストは `funkot_core::testdata` 経由でそこを探す。

- 解決順は `FUNKOT_TESTDATA_DIR` → `<repo>/testdata`
- 拡張子は問わない（`.flac` / `.m4a` / `.alac` の順に探す）。同じマスターなら
  ロスレス同士でどれでもよい
- ディレクトリの形も問わない。直下に無ければ再帰的に探す（音楽ライブラリは
  アルバム別に切ってある）。走査はプロセスにつき1回で、結果は使い回す
- 見つからなければテストは**失敗ではなく skip** する。ライブラリの見えない環境が
  普通だという前提
- `dev.sh` は設定されていれば `FUNKOT_TESTDATA_DIR` をコンテナへ渡す。リポジトリ外を
  指すなら `DEV_BIND_SRC` も要る（渡さないとコンテナ内にそのパスが存在しない）

テストが**書く**もの（解析キャッシュ、試聴用クリップ）は `FUNKOT_TESTDATA_DIR` の
下には置かない。音楽ライブラリは読み取り専用マウントのことがあるため、
`testdata::local_dir()`（この checkout の `testdata/`）へ書く。

### 原盤の入れ物を変えるとき

**`labels.tsv` と `funkot-cache` は全滅する。** `cache::content_hash` は
ファイルのバイト列（長さ＋先頭 64 KiB＋末尾 64 KiB）を見るので、同じ曲でも
FLAC と ALAC では別のキーになる。**両方が揃っている間に**再キーすること。
順序を逆にすると実耳ラベルが宙に浮く。手順は `examples/master_rekey.rs`:

```sh
# old_path<TAB>new_path を1行ずつ書いた MAP.tsv を用意して
DEV_BIND_SRC=<music-dir> ./dev.sh \
  cargo run -p funkot-core --example master_rekey --release -- MAP.tsv > rekey.tsv
```

デコード後の PCM がサンプル単位で一致した行だけ `same` になり、旧→新 `content_hash`
が出る。1行でも `same` でなければ非ゼロ終了する。あとは `labels.tsv` / `survey.tsv` の
1列目と2列目をこの対応で置き換え、**再キー前後で `eval_sections` の出力が
（ファイル名の拡張子を除いて）一致することを確認**してから旧ファイルを捨てる。

対応表そのものは旧ファイルを消すと二度と作れないので、再キー前のスナップショットと
一緒に private store へ残すこと（2026-08-11 の移行では `rekey_result.tsv` と
`*.bak-0811-preflac`）。

## 再生成

原盤を要するものは、音楽ライブラリをマウントしてコンテナへ渡す。

```sh
DEV_BIND_SRC=<music-dir> ./dev.sh sh /work/tools/<script>.sh
```

| 対象 | 手順 |
|---|---|
| `funkot-core/tests/fixtures/*.wav` | `./dev.sh cargo run -p funkot-cli --release -- --gen-test-fixtures funkot-core/tests/fixtures`（`funkot-core/tests/fixtures/README.md`）。テストは `golden.json` からメモリ上で合成するので、WAV は聴くとき以外は不要 |
| `testdata/synth/` | `cargo run -p funkot-core --example gen_synth --features testutil --release -- testdata/synth` |
| `testdata/phase_ab/` | `tools/render_ab.sh`（`file_list.txt` の絶対パス行が対象、両側 bars 16） |
| 小節位相サーベイ | `tools/survey.sh` |
| 裏拍・クリック位相サーベイ | `tools/click_survey.sh`、`tools/offbeat_survey.sh` |
| `funkot-cache/` | 上記いずれかを `--cache-dir funkot-cache` 付きで走らせれば温まる |
| `target*/`、`dist/` | 通常のビルド |
| `testdata/work_playlist.txt` | `./work.sh <music-dir>` が作り直す |

`eval_baseline.md` を取り直すときは **`--cache-dir` を付けない**（`testdata/README.md`）。

## 新しく ignore を足すとき

置き場所で決める。**手で書いたコードと文書は `testdata/` へ置かない。**
`testdata/` は「実音源と計測の生データ」だけにして、スクリプトは `tools/`、
残す知見は `docs/` へ置く。そうすれば ignore の粒度を細かくする必要がない。
