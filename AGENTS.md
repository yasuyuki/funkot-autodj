# 開発指針と確認手順

この文書を、変更時に必要な開発上の判断と検証の入口にする。製品仕様と利用方法は
`README.md`、公開履歴は `CHANGELOG.md`、設計判断と実測知見は `docs/` を正とし、
ローカルの `HANDOFF.md`、`testdata/`、キャッシュ、WAV、`dist/`、`target*` は
調査・聴感確認に使ってもコミットしない。

## HANDOFF.md と docs/ の運用

`HANDOFF.md` は gitignore 対象で、**次の作業に取り掛かるために必要な最小限**だけを持つ。
具体的には現在地（ブランチ、HEAD、何が済んで何が止まっているか）、次にやること、
この環境固有の事情（パス、マウント、機材）、そして `docs/` へのリンク。

**完了した作業の記録を HANDOFF.md に溜めない。** 将来の作業で再び踏みうる知見
（設計判断とその理由、試して失敗した方式、実測値、壊してはいけない不変条件）は
`docs/` 以下のコミット対象ファイルへ移し、HANDOFF.md からは削除してリンクだけ残す。
判断に迷ったら「この情報は3か月後の別ブランチでも意味を持つか」で決める。
持つなら `docs/`、持たないなら消す。

- `docs/` に書くときは公開リポジトリであることを前提にする。個人パス、ホスト名、
  IP、資格情報は書かない（それらが必要な話は HANDOFF.md 側に残す）
- 新しい知見は既存の `docs/*.md` に追記して統合する。似た文書を増やさない
- 古くなった記述は書き換えるか削除する。「いま何が正か」が読み取れなくなったら失敗である
- 役目を終えた文書は消す。履歴は git にある

現在の `docs/`:

| 文書 | 内容 |
|---|---|
| `docs/guide-clicks.md` | `--label-sections` のガイドクリック位置決定（4層方式、失敗した方式、実測） |
| `docs/labeling.md` | ラベリングのホスト実行手順と音声まわりの罠 |
| `docs/section-analysis-redesign.md` | 区間解析再設計の意図・Stage 構成・着手前チェック |
| `docs/transition-phase.md` | 遷移の位相・小節 identity と v6〜v17 の失敗履歴 |

## 開発指針

- ユーザー向け応答は日本語で書く。
- 変更したら毎回コミットする。作業ツリーを未コミットのまま放置しない。
- `~/.claude/plans/` の計画ファイルは編集しない。誤りを見つけたら `docs/` に記録する。

- Rust workspace は `funkot-core`（デコード、解析、キャッシュ、ストレッチ、
  ミックスエンジン、C ABI）と `funkot-cli`（プレイリスト、cpal 再生、キー操作、
  WAV 出力）から成る。通常のビルドとテストは、Rust 1.93 と必要なネイティブ依存を
  固定した Docker 経由の `./dev.sh cargo ...` で実行する。実デバイス再生だけは
  ホストの `cargo run` を使う。
- 変更前後に `git status --short --ignored` を確認する。既存の未コミット変更を
  上書きせず、自分が変更した管理対象ファイルだけをステージする。
- 解析は Funkot の先頭・末尾を対象とし、基準は 180 BPM / 4拍子である。
  遷移は次曲メイン開始 T0 から逆算し、既定では4小節フェード2本と8小節の
  次曲単独区間を作る。フェード端点、前デッキ停止、位相・小節頭の整合を崩さない。
  詳細な現行値は `funkot-core/src/lib.rs`、`analysis.rs`、`engine.rs` を正とする。
- 位相の微調整は半拍未満に保つ。整拍の小節 identity を選ぶ場合は
  kick + hat の groove スコアと明確な差を使い、kick-only の ±1/±2拍相関へ
  戻さない。経緯は `docs/transition-phase.md`、ラベリング用ガイドクリック側の
  同種の制約は `docs/guide-clicks.md` を参照する。
- 「ミッドハイパス」は高域を通すハイパス（低域カット）である。ローパスに戻さない。
- `TrackAnalysis` の互換性を壊す変更では `funkot-core/src/cache.rs` の
  `CACHE_VERSION` を上げ、README の現行版表記も更新する。手動
  `intro_bars` / `outro_bars` と対応する `*_manual` の保持を壊さない。
- C ABI を変える場合は `funkot-core/src/ffi.rs` と手書きの
  `include/funkot.h` を同時に更新する。音声コールバックと C ABI の
  `render` はブロックさせない。
- 解析ゴールデンを変える場合は下記コマンドで再生成し、差分を確認して
  `golden.json` だけを必要に応じてコミットする。生成 WAV は ignore 対象である。
- 依存を追加・更新したら `./cross-build.sh android` が通ることを確認する。
  制約は下記「モバイル (Android) ビルド」を正とする。

## 確認手順

まず変更範囲にかかわらず実行する。

```sh
./dev.sh cargo test --workspace --release
./work.sh --self-check
```

コンテナの固定ツールチェーンには現在 `rustfmt` が含まれないため、
`./dev.sh cargo fmt` を必須手順にはしない。整形を行う場合も既存ファイルの
書式に合わせ、無関係な差分を混ぜない。

変更内容に応じて次を追加する。

- 解析・キャッシュ: `./dev.sh cargo test -p funkot-core --release --test analysis_golden`
- エンジン・遷移・フェード・ナビゲーション:
  `./dev.sh cargo test -p funkot-core --release --test engine --test fade_curve`
- CLI・プレイリスト・WAV: `./dev.sh cargo test -p funkot-cli --release --test cli`
- C ABI: `./dev.sh cargo test -p funkot-core --release --test ffi`
- `work.sh`: `./work.sh --self-check`
- Windows 配布・`work.ps1`: `./cross-build.sh` の後に
  `pwsh.exe -File ./work.ps1 --self-check`
- 依存の追加・更新: `./cross-build.sh android`

解析、マーカー、遷移、フェード、ストレッチ、ナビゲーションを変えた場合は、
単体テストだけで完了にしない。手元に ignore 対象の実音源がある環境で、
空のキャッシュディレクトリを指定して実プレイリストをレンダーする。

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  -l testdata/real_playlist_v20.txt \
  --cache-dir "testdata/verify-cache-$(date +%s)" \
  --render testdata/verify_mix.wav \
  --wav-format f32 \
  --ci-fast
```

`testdata/verify_mix_transitions/` の全クリップを聴き、各接続について
ダウンビート、mod-4 の小節頭、無音ドロップ、フェード後の前曲残留がないことを
確認する。解析結果の `intro_bars` / `outro_bars` / BPM と、出力された peak・
`|x|>1` 件数も記録する。実音源がない環境ではその事実を報告し、実音源確認済みとは
扱わない。ライブ再生、Enter、左右キーの multi-tap、実デバイス形式はホスト上で
別途確認する。

ゴールデン再生成:

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  --gen-test-fixtures funkot-core/tests/fixtures
./dev.sh cargo test -p funkot-core --release --test analysis_golden
```

最後に `git diff --check` と差分をレビューし、秘密情報、個人パス、実音源、
キャッシュ、WAV、ビルド成果物が含まれないことを確認する。

## モバイル (Android) ビルド

`funkot-core` は GUI アプリ (`funkot-player`) から Cargo 依存として使われる。
このリポジトリで Android ターゲットを持つのは、依存追加で NDK ビルドが壊れたときに
下流ではなくここで気付くための回帰ガードと、他ターゲットと同様の C ABI SDK 配布のため。

```sh
./cross-build.sh android   # → dist/android-arm64/
```

`funkot-cli` は cpal と端末キー操作に依存するため Android では作らない。

実機 (Pixel 10 Pro / Android 17) で検証済みの制約。**いずれも推測ではなく実測**である。

- **API レベルは 26 が下限。** `libaaudio.so` が NDK sysroot に現れるのが 26 からで、
  24 では `ld.lld: error: unable to find library -laaudio` になる。ホスト側アプリの
  `minSdkVersion` も 26 以上にすること
- **bindgen の libclang は NDK ではなくホストのものを使う。** NDK は clang ドライバと
  サニタイザランタイムしか同梱せず `libclang.so` を持たない。`--sysroot` だけ NDK を
  指す。`signalsmith-stretch` の `src/wrapper.h` は `stddef.h` / `stdbool.h` しか
  include しないため、ホスト clang のリソースディレクトリで足りる
- **`libc++_shared.so` を同梱する。** `cc` クレートが `cargo::rustc-link-lib=c++_shared`
  を出すため、`-static-libstdc++` では消せない。アプリが複数の `.so` を読む場合に
  静的 libc++ を各所に埋めるのは Google が明示的に避けるよう言っている構成なので、
  共有版を配ること
- **リンク時に 16KB ページアライメントを指定する。** Android 15 以降の要件。
  `-Wl,-z,max-page-size=16384` を入れないと、端末起動時に
  「16 KB アライメントではありません」の互換性ダイアログが出る
- **メモリ**: `prepare_track` は曲を丸ごとデコード＋ストレッチして `Arc<Vec<f32>>` に
  載せる。合成3曲の再生中で PSS 353MB / RSS 500MB（WebView 込み）を実測した。
  デッキ数やストリーミング化を検討する際はこの数字を基準にする
