# 開発指針と確認手順

この文書を、変更時に必要な開発上の判断と検証の入口にする。製品仕様と利用方法は
`README.md`、公開履歴は `CHANGELOG.md` を正とし、ローカルの `HANDOFF.md`、
`testdata/`、キャッシュ、WAV、`dist/`、`target*` は調査・聴感確認に使っても
コミットしない。

## 開発指針

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
  戻さない。
- `TrackAnalysis` の互換性を壊す変更では `funkot-core/src/cache.rs` の
  `CACHE_VERSION` を上げ、README の現行版表記も更新する。手動
  `intro_bars` / `outro_bars` と対応する `*_manual` の保持を壊さない。
- C ABI を変える場合は `funkot-core/src/ffi.rs` と手書きの
  `include/funkot.h` を同時に更新する。音声コールバックと C ABI の
  `render` はブロックさせない。
- 解析ゴールデンを変える場合は下記コマンドで再生成し、差分を確認して
  `golden.json` だけを必要に応じてコミットする。生成 WAV は ignore 対象である。

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
