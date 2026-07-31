# 区間解析の再設計（2026-07-26〜、進行中）

ブランチ `feat/track-source-and-android-build`。進捗と次の一手は `HANDOFF.md` を見ること。
本書は**設計の意図と、実装前に確認すべき事項**を残す。

## なぜやっているか

`analysis.rs` の区間長判定は手調整の if カスケードで、閾値が Shirube / IVY / Starmine /
Love & Joy / Sakura / Totsumal といった**個別曲に合わせて刻まれている**。10曲規模の過学習で、
分岐を1つ足すと別の曲が壊れる。

根本原因は特徴量が**すべて広帯域エネルギー**（RMS / mid / hf / midhigh_ratio /
ZCR centroid）で、Funkot のイントロ・アウトロを実際に定義している
「旋律・ボーカルが無く機械リズムが反復する」性質を測る特徴が1つも無いこと。
加えて**実音源の正解ラベルも精度指標も存在せず、改善したかを測れなかった**。

方針（ユーザー確認済み）:

1. 正解ラベルは半自動アノテーション CLI で作る（最良値＋許容集合。曖昧な曲があるため）
2. 判定は候補スコアリング＋オフライン学習した重み（実行時 ML 依存ゼロ、決定的）
3. `outro_bars` は「構造境界」と「mix リード」を分離する

## Stage 0〜2（完了）

| Stage | commit | 内容 |
|---|---|---|
| 0 評価基盤 | `54849ad` | `funkot-core/src/labels.rs`（labels.tsv 読み書き、content_hash キー）、`examples/eval_sections.rs`（exact/tolerant accuracy、混同行列、DJ重大度コスト、low_conf 適合率・再現率、格子健全性、k-fold、`--json`）。`TrackAnalysis::outro_structure_bars` を追加し `CACHE_VERSION` 8→9 |
| 1 アノテーション CLI | `ba00c80` | `--label-sections`。境界±8小節（約21秒）をクリック付きで再生。`label_session.rs` は cpal/crossterm から切り離した純粋な状態機械＋単体テスト。`--render-clips` で headless 検証 |
| 2 特徴量 | `4c3f7d0` | `features.rs`（7帯域 / クロマ12 / トーナルネス / 有声率 / リズムパターン16×2 / オンセット密度）と `structure.rs`（Foote checkerboard novelty、ループ性 lag 1/2/4、前置モデルからのマハラノビス距離）。`rustfft` 追加。**まだ `analyze()` は使っていない** |

ガイドクリックの位置ずれ（`7b86b18` / `b486a08` / `bbf684c` / `0460891` / `33aac27`）は
[guide-clicks.md](guide-clicks.md) にまとめた。

## Stage 3（学習スコアラ）— ラベル待ち

**ラベルが無いと着手できない。** ベースライン数値もまだ無い。

1. ホストでラベリング（[labeling.md](labeling.md)）
2. ラベルが数十曲たまったら `eval_sections` でベースラインを取り、
   `testdata/eval_baseline.md` に記録
3. **着手前チェック（レビュー由来。特徴量を学習に入れる前に実音源で確認する）**
   - `section_diag --new-features` で **band novelty の分布**を見る。`structure.rs` の
     `band_vectors` は生 dB ベクトルのコサイン類似なので、共通オフセットで類似度が 1 付近に
     張り付いていたら SSM 前に次元ごと z-score を入れる
   - `mahalanobis_from_prefix` の分散フロア `1e-6` は絶対値。ほぼ一定の dB 次元で
     1 dB の変化が距離を支配していないか確認し、必要なら次元スケール比例のフロアか
     事前標準化に変える（8小節分散はもともとノイジーなので、学習側の重みで
     吸収できる可能性もある）
   - `features.rs` の periodicity は零詰め無しの循環自己相関（Wiener–Khinchin）で、
     長ラグ側に上振れバイアスがある。voiced 二値判定（閾値 0.35）はこのままで良いが、
     **連続値として学習特徴に入れるなら零詰め版に直す**
   - いずれも「入れる前に確認」であり、判断は交差検証に委ねてよい
4. 本体: `section_model.rs`（候補 φ(c) + softmax）、`examples/fit_section_model.rs`
   （部分ラベル対数尤度・L2・k-fold）、`section_weights.rs` 生成。
   曲別閾値（`MIN_SHARP_*` / `MAINNESS_*` / `OUTRO_MID_PLATEAU_FRAC_*`）と
   `analysis_tests.rs` の曲別再現テストを削除する
5. **採用基準**: tolerant accuracy と DJ重大度コストの**両方**が交差検証で改善すること

### DJ 重大度コストの符号

**`eval_sections.rs` の実装を正とする。** アウトロ**過小**＝ドロップ後に混ぜる＝重い 2.0 /
過大＝軽い 0.5、intro は逆向き。計画ファイルの説明は過大/過小が逆になっているので信用しない
（計画ファイルは編集禁止のためここに記録した）。

## Stage 4 — **完了（2026-07-31）**

`outro_bars = outro_structure_bars + OUTRO_LEAD_BARS` を**全曲で正とした**
（`analysis::outro_trigger_bars`、`CACHE_VERSION` 9→10）。

**なぜやったか**: 遷移は 16小節（2×フェード + `MAIN_GAP_BARS`）なので、
構造境界の16小節手前でトリガを引けば**つなぎ終えた時点がちょうどアウトロの開始点**になる。
リードが落ちるとトリガ＝構造境界になり、**遷移全体がアウトロの中で起きる** — 一番やっては
いけない場所。実測で作業キャッシュ34曲中3曲（構造境界64小節の曲）がこれに該当していた。

旧実装のリードが条件付きだった理由は「アウトロ解析窓がリード分をカバーしていること」
（`feats.len() >= bars + LEAD + AFTER_WIN`）と `FALLBACK_BARS` クランプだが、
**どちらもトリガの正しさとは無関係**。リードは構造境界に対する算術であって、
解析窓が覆っている必要はない。唯一の実制約は曲の長さで、
`outro_trigger_bars` は「イントロを食い込まない」「構造境界より手前にしない」だけを守る。

トリガ集合は **{24, 32, 48, 80}**（構造境界 {8,16,32,64} + 16）。README 3言語も更新済み。

残り: ラベリング時に `set_manual_bars` へアウトロも書けるようにする（現在はイントロのみ。
アウトロを書くと +16 を推測することになる、という制約が上のルール確定で消えた）。

## 判明している事実・注意点

- ~~**`outro_bars` から構造境界は算術で復元できない。**~~ → **cache v10 で復元可能になった**
  （`structure + 16`、曲が短くてクランプが効いた場合を除く）。それでも
  `outro_structure_bars` フィールドは残す: ラベルと eval が比較するのはこちらで、
  再導出より読み手に明確なため。不変条件 **`outro_structure_bars <= outro_bars`** は
  `outro_trigger_bars` が構造境界より小さい値を返さないので**構成上成立する**
  （v9 以前は reconcile 後のクランプで担保していた）。
  v9 以前のキャッシュでは `outro_bars=64` が「drop=48+リード16」と「drop=64（リード無し）」の
  両方を意味していた——これがフィールドを分けた元々の理由
- **ラベルのアウトロは「音楽的な構造境界」**であって mix トリガではない。
  `cache::set_manual_bars` はイントロだけ書いている（アウトロを書くと +16 を推測することになるため）
- **現行解析は手元12曲で `low_confidence` を1件も立てていない。** 曖昧な曲が必ずある母集団に
  対してこれは考えにくく、棄却判定が機能していない疑いがある。ラベルが付けば eval で確認できる
- **合成フィクスチャは音が薄すぎて実音源の問題を検出できない。** クリック不可聴（マスキング）も
  クリッピング（peak 1.71）も、合成音源のテストは全部通っていた。実音源に近い密で大音量の
  合成信号をテストに使うこと

## 維持する設計判断（2026-07-27 レビュー）

方針・実装とも妥当と確認済み（`./dev.sh cargo test --workspace --release` 全 green。
CI はスケジュール実行とタグのみで、このブランチでは回らない）。

- `outro_structure_bars` の導入・不変条件と、「算術復元は不可能」の理由の文書化
- tonality の「フレーム毎に平坦度を出してからエネルギー重み平均」方式は
  sparsity 交絡（無音区間が tonal に見える）を正しく除去し、回帰テストで固定済み
- `label_session.rs` の純粋状態機械分離と、クリック可聴性の**差分**検証
- `structure.rs` の「outro 側は `section_bar_starts` が後方順を返すので時間反転不要」の整理

README の outro 集合は `c5cc904` で構造境界とトリガ値を区別する形に修正し、
Stage 4（2026-07-31）でトリガ値を **{24,32,48,80}** に更新した
（旧: 条件付きリード + 8小節格子丸め + `FALLBACK_BARS` クランプで {8,16,24,32,48,64}）。
