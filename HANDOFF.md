# funkot-autodj — チャット引き継ぎメモ

最終更新: 2026-07-22（v16: 端の kick+hat groove / prev nudge で 1→2 ダウンビートロック）

関連会話: [Funkot自動DJ実装](9dbb7172-b71e-4cf1-98d7-b518a8087a0e)

---

## 1. プロジェクト概要

- **場所**: `/home/yasuyuki/Projects/funkot-autodj`
- **目的**: インドネシア Funkot 向け自動DJ（イントロ/アウトロ解析 → テンポ同期 → クロスフェード）
- **構成**:
  - `funkot-core` — 解析・ストレッチ・2デッキエンジン・C ABI
  - `funkot-cli` — cpal 再生 / `--render` WAV 書き出し
- **開発**: Docker + `./dev.sh cargo …`（ホスト cargo は使わない）
- **git**: あり（`master`）。**変更後は毎回コミット**する方針
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

### アウトロマーカー（v14+ / v16 強化）

- イントロ側: 解析 `first_downbeat` → scale → ±半拍 **kick+hat groove** refine（疎な intro head）
- アウトロは **2候補**を保持:
  - `outro_start_out` = intro-propagated 格子（遷移トリガ・小節スケジュール）
  - `outro_end_anchored_out` = **outro tail**（末尾数小節）で groove refine → `outro_bars` 分だけ戻す
- 入口: `prepare_output_markers` → `(fd, intro_grid, end_anchored)`
- 解析キャッシュ形状は不変（`real-cache-v4` / `CACHE_VERSION=4` のまま）。出力ドメインの refine のみ変更

### 遷移時位相ロック（v16）

- `align_next_entry_with_phase_hypotheses`（`engine.rs`）
- 各仮説を **±0.5拍のみ**で micro-align（`align_next_entry_scored`）
- スコアは **kick + 0.4×hat**。kick/hat が ¼拍以上食い違うときは **kick（ダウンビート）優先**
- next がファイル先頭でこれ以上早められないとき、同じ位相補正を **`prev_nudge`（≤½拍の prev スキップ）** で適用（1→2: ~0.25拍）
- **end-anchored は kick/groove score が intro格子より明確に高いときだけ採用**（`SCORE_EPS=0.02`）
- **±1/±2拍の coarse 探索は禁止**（v10: 拍は合うが小節ずれ）

### 診断用 example

- `funkot-core/examples/marker_phase_diag.rs` — マーカー比較
- `funkot-core/examples/transition_phase_diag.rs` — 2曲の遷移位相（両仮説の score / prev_nudge も表示）

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

- キャッシュ: `testdata/real-cache-v4`（全曲 intro/outro **64/64** 寄り）。v16 でも再生成不要
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
- `--transition-clip-seconds`（既定90）で遷移WAVを同時出力
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
| v12 | `real_mix_v12_simplified_strict_f32.wav` | アウトロ/小節は解析粗位置→±半拍 refine のみ | ダウンビート OK。**1→2 / 2→3 小節ずれ** |
| v13 | `real_mix_v13_intro_grid_f32.wav` | アウトロをイントロ小節格子に統一 | **1→2 / 3→4 ダウンビートずれ**。2→3 / 4→5 は正しい |
| v14 | `real_mix_v14_phase_hyp_f32.wav` | 遷移時に intro格子 vs end-anchored を選択 | **1→2 小節ずれ**（他は正しい）。ダウンビート検出自体は正しい |
| v15 | `real_mix_v15_grid_prefer_f32.wav` | end は kick score 明確勝ちのみ（slack 廃止） | **1→2 ダウンビートずれ**。2→3 / 3→4 / 4→5 は正しい |
| **v16** | **`real_mix_v16_edge_groove_f32.wav`** | 端 groove refine + kick優先 + prev_nudge | **聴感確認待ち** |

遷移ディレクトリ: `testdata/real_mix_vN_transitions/`（`01_…` = 1→2）

### 計測メモ（transition_phase_diag）

v15（slack 廃止、end は明確勝ちのみ）:

- **1→2**: chosen **intro-grid**。kick **+0.25拍**（v14 の +1拍ずれを回避）。mid は -0.73拍のまま（帯域不一致）
- **3→4**: chosen **end-anchored** 維持（score 0.916 ≫ 0.549）。kick **0.00拍**
- **2→3 / 4→5**: intro-grid 維持

v16（edge groove + kick優先 + prev_nudge）:

- **1→2**: chosen **intro-grid** + **prev_nudge≈3328f (~0.25拍)**。kick **+0.019拍**（ロック）。hat は配置差で ~−0.48拍のまま（KazuyaP=offbeat hat / Totsumal=onbeat hat）
- **2→3**: intro-grid。kick/hat **~0**
- **3→4**: end-anchored 維持。kick/hat/mid **0.00**
- **4→5**: intro-grid。kick/hat **0.00**

根因メモ（1→2）:

1. KazuyaP の intro格子と end-anchored が ~0.72拍ずれる（ソース領域の小節数えと末尾位相の差）
2. 遷移区間で **kick は +¼拍、hat は −¼拍**と食い違う（アレンジ差）。合成スコアだと 0 に張り付く
3. Totsumal の `first_downbeat` がファイル先頭付近のため、kick が要求する「entry を早める」補正がクランプされ、v15 では残差 +0.25拍のまま
4. v16: kick 優先で位相を決め、クランプ時は同等の相対位相を **prev を ¼拍スキップ**して実現

---

## 6. 次チャットでやること候補

1. **v16（`real_mix_v16_edge_groove_f32.wav` + `real_mix_v16_transitions/`）を聴感確認**  
   特に **1→2**（`01_KazuyaP_to_Totsumal`）。2→3 / 3→4 / 4→5 が崩れていないかも確認
2. 残課題（未着手・低優先）:
   - 1→2 の hat 配置差（onbeat vs offbeat）自体はアレンジ差 — ダウンビート（kick）優先で許容
   - 解析側で intro/outro 格子不一致そのものを減らす（キャッシュ v5 候補）
   - ミックスピーク超過（ヘッドルーム／リミッター）
   - ハイパス重なり中は設計上ハットが両デッキから聞こえる（300Hz HPF）
   - CI
   - コンテナ内 rustfmt/clippy が DNS 失敗することがある

---

## 7. テスト・ビルド

```sh
./dev.sh cargo test -p funkot-core --release
# 位相ロック単体:
./dev.sh cargo test -p funkot-core --release --lib engine::tests::align_next_entry_micro_only
./dev.sh cargo test -p funkot-core --release --lib engine::tests::phase_hypotheses_prefer_grid_unless_end_clearly_wins
```

主要テスト: `engine::tests::*`, `fade_curve`, `engine` integration, `ffi`, CLI e2e。

---

## 8. 触るときの注意

- ユーザー向け応答は **日本語**
- 計画ファイルは編集しない
- **変更後は毎回コミット**（本チャットで方針変更済み）
- `testdata/` の実音源・巨大 WAV は成果物確認用；リポジトリには載せない想定
- 「ミッドハイパス」= ハイパス（低域カット）。ローパスに戻さない
- 小節identityをキック相関の ±N拍で取らない（v10 の失敗を繰り返さない）
