# funkot-autodj — チャット引き継ぎメモ

最終更新: 2026-07-22（v13: アウトロをイントロ小節格子に統一、聴感確認待ち）

関連会話: [Funkot自動DJ実装](9dbb7172-b71e-4cf1-98d7-b518a8087a0e)

---

## 1. プロジェクト概要

- **場所**: `/home/yasuyuki/Projects/funkot-autodj`
- **目的**: インドネシア Funkot 向け自動DJ（イントロ/アウトロ解析 → テンポ同期 → クロスフェード）
- **構成**:
  - `funkot-core` — 解析・ストレッチ・2デッキエンジン・C ABI
  - `funkot-cli` — cpal 再生 / `--render` WAV 書き出し
- **開発**: Docker + `./dev.sh cargo …`（ホスト cargo は使わない）
- **git**: 未初期化
- **計画ファイル**: 編集禁止（既存の plan があれば触らない）

---

## 2. 確定仕様（グリル結果の要約）

| 項目 | 内容 |
|------|------|
| 基準BPM | 180。再生目標 `180 × rate`（既定 rate=1.10 → 198） |
| ストレッチ | 曲全体一定レート。既定 Pitch Preserve（signalsmith）、オプション Shift（rubato） |
| 解析 | 先頭・末尾のみ（~110s）。JSONキャッシュ。曖昧時フォールバック **64小節** |
| 長さ制約 | `intro >= outro`（同一強制は廃止） |
| フェード | 既定 **4小節** 線形。端点は gain=0 / 1 をビット正確に保証 |
| MAIN_GAP | フェードアウト完了後、次曲メインまでイントロ単独 **8小節** |
| フィルタ | **ハイパス約300Hz**（ミッドハイパス）。ユーザー確認済み。CLI: `--highpass-hz`（alias `--lpf-hz`） |
| WAV | 既定 **f32**。`--wav-format f32\|s24\|s16` |
| 形式 | MP3/AAC/ALAC/FLAC/Ogg/WAV（symphonia） |

### 遷移手順

1. 前曲がアウトロ入り → 次曲開始（HPF ON・音量0）
2. 線形フェードイン（先頭フレーム0）
3. フェードイン完了で HPF を前曲側へ即時切替
4. 前曲を線形フェードアウト（最終フレーム0）→ 直後に `prev` デッキ破棄
5. 長いイントロは途中入り（単独区間上限 `MAX_SOLO_INTRO_BARS=16`）

---

## 3. 実装の要点（コード上の約束）

### フェード端点・前曲停止（対応済み）

- `fade_in_gain` / `fade_out_gain`: 線形、端点 0/1
- `gain==0` のフレームはバスに加算しない
- `fade_out_end` 到達直後に `drop_prev`

### アウトロマーカー（v13 更新）

- イントロ側: 解析 `first_downbeat` → scale → ±半拍 refine（従来どおり）
- **アウトロ側: 解析 `first_downbeat`→`outro_start` の小節数をイントロ格子へ propagate**（`legacy_intro_propagated_outro`）し、末尾 scale だけの独立格子は使わない
  - 理由: v12 では末尾 scale の outro がイントロ格子から **~0.9〜3.3拍**ずれ、1→2 / 2→3 で小節 identity が崩れた（キック相関 ±1拍）
- ±半拍 refine はアウトロには適用しない（中盤ドリフトで隣接キックへ吸われるため）
- 入口: `prepare_output_markers`（`engine.rs`）

### 遷移時位相ロック（現行方針 = v12）

- `align_next_entry_to_prev`（`engine.rs`）
- **±0.5拍の微調整のみ**（キック帯エネルギー相互相関）
- **±1/±2拍の coarse 探索は廃止**
  - 理由: Funkot は毎拍キックがあり、相関が mod-4 小節identityを取り違える
  - 2→3 で「拍は合うが小節がずれる」症状の直接原因だった
- 端クランプで `entry=0` になる調整は却下（v9 系の別バグ）

### 診断用 example

- `funkot-core/examples/marker_phase_diag.rs` — マーカー比較
- `funkot-core/examples/transition_phase_diag.rs` — 2曲の遷移位相（nominal vs aligned）

```sh
./dev.sh cargo run -p funkot-core --example transition_phase_diag --release -- \
  --sr 44100 --cache-dir testdata/real-cache-v4 --rate 1.10 \
  PREV.flac NEXT.flac
```

---

## 4. 実音源テストセット

`testdata/`（gitignore）。プレイリスト: `testdata/real_playlist.txt`（パスはプレイリスト相対）

| # | ファイル |
|---|----------|
| 1 | `03. KazuyaP - Monitoring Db.flac` |
| 2 | `05. Totsumal - Written With A Compass Needle Rmx.flac` |
| 3 | `Funkot Import Recordings - Gugun Single Collection - 04 Eternal Light.flac` |
| 4 | `Nicho - Gakumas no Remix - 03 Sekaiichi Kawaii Watashi.flac` |
| 5 | `Nicho - Gakumas no Remix 2 - 04 Boom Boom Pow.flac` |

- キャッシュ: `testdata/real-cache-v4`（全曲 intro/outro **64/64** 寄り）
- 再レンダー例:

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  -l testdata/real_playlist.txt \
  --cache-dir testdata/real-cache-v4 \
  --no-loop \
  --render testdata/real_mix_vXX.wav \
  --wav-format f32
```

- `--render` 既定は約10倍速ペース（ローダー追いつき用）。`--render-speed 0` は無制限だがアウトロ延長フォールバックのリスクあり
- ピークが +3 dBFS 程度になることあり（リミッター未実装）

### 遷移クリップ抽出（尺 ~2194s のとき）

経験的オフセット（90秒）:

| 遷移 | `-ss` |
|------|-------|
| 1→2 | 300 |
| 2→3 | 600 |
| 3→4 | 960 |
| 4→5 | 1440 |

※ v10 は尺が ~2304s に延びオフセットがずれた。v11 は再び ~2193.9s（v9 相当）。

---

## 5. ミックス版履歴（聴感フィードバック付き）

| 版 | ファイル | 要点 | ユーザー反応 |
|----|----------|------|----------------|
| v6 | `real_mix_v6_bar_linear_f32.wav` | 線形フェード | フェード端点が0でない／前曲が残る |
| v7 | `real_mix_v7_zero_fades_f32.wav` | 端点0＋prev破棄 | フェードはOK。3→4でハット二重 |
| v8 | `real_mix_v8_bar_identity_f32.wav` | legacy格子をやめて mapped_outro | 3→4は未解消 |
| v9 | `real_mix_v9_phase_lock_f32.wav` | 位相ロック（±2拍含む） | 3→4ほか改善。**2→3が大幅ずれ**（entry→0） |
| v10 | `real_mix_v10_phase_lock2_f32.wav` | 2段ロック＋端却下。2→3は+1拍 | **2→3: 拍は合うが小節ずれ**。他は正常 |
| v11 | `real_mix_v11_bar_preserve_f32.wav` | **微調整±0.5拍のみ** | **聴感確認待ち** |
| **v12** | **`real_mix_v12_simplified_strict_f32.wav`** | アウトロ/小節は解析粗位置→±半拍 refine のみ（bar identity 投影を廃止） | ダウンビート OK。**1→2 / 2→3 小節ずれ** |
| **v13** | **`real_mix_v13_intro_grid_f32.wav`** | **アウトロをイントロ小節格子に統一**（末尾 scale 格子を廃止） | レンダー完了（~2194s）。聴感確認待ち |

遷移ディレクトリ: `testdata/real_mix_vN_transitions/`（特に `02_…` が 2→3）

### 計測メモ（transition_phase_diag）

v12（末尾 scale outro）:

- **1→2**: kick xcorr **+1.0拍** → 小節ずれ（ユーザー確認）
- **2→3**: kick xcorr **-1.0拍** → 小節ずれ（ユーザー確認）

v13 修正後（イントロ格子 outro）:

- **1→2**: kick xcorr **+0.25拍**（+75ms）。大幅改善、要聴感確認
- **2→3**: aligned entry **-0.02拍**（-5.8ms）
- **3→4 / 4→5**: 未再計測（レンダー後確認）

---

## 6. 次チャットでやること候補

1. **v13（`real_mix_v13_intro_grid_f32.wav` + `real_mix_v13_transitions/`）を聴感確認**  
   v12 フィードバック: ダウンビート OK、**1→2 / 2→3 小節ずれ**。v13 で 2→3 は診断上ほぼ解消、1→2 は残差 ~0.25拍
2. 問題がなければ 1→2 / 4→5 も通しで再確認
3. 残課題（未着手・低優先）:
   - ミックスピーク超過（ヘッドルーム／リミッター）
   - ハイパス重なり中は設計上ハットが両デッキから聞こえる（300Hz HPF）— 位相が合えば一体に聞こえる想定
   - git 初期化・CI
   - コンテナ内 rustfmt/clippy が DNS 失敗することがある

---

## 7. テスト・ビルド

```sh
./dev.sh cargo test -p funkot-core --release
# 位相ロック単体:
./dev.sh cargo test -p funkot-core --release --lib engine::tests::align_next_entry_micro_only
```

主要テスト: `engine::tests::*`, `fade_curve`, `engine` integration, `ffi`, CLI e2e。

---

## 8. 触るときの注意

- ユーザー向け応答は **日本語**
- 計画ファイルは編集しない
- コミットは依頼があるまでしない（現状 git なし）
- `testdata/` の実音源・巨大 WAV は成果物確認用；リポジトリには載せない想定
- 「ミッドハイパス」= ハイパス（低域カット）。ローパスに戻さない
- 小節identityをキック相関の ±N拍で取らない（v10 の失敗を繰り返さない）
