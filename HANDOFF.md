# funkot-autodj — チャット引き継ぎメモ

最終更新: 2026-07-22（v17: mod-4 kick+hat groove で 1→2 小節 identity 修正 + CI fixtures）

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

### 遷移時位相ロック（v17）

- `align_next_entry_with_phase_hypotheses`（`engine.rs`）
- **Stage 1（位相）**: intro格子 / end-anchored の各仮説を **±0.5拍のみ** micro-align（`align_next_entry_scored`）
- **Stage 2（小節 identity）**: 各仮説の nominal から **整拍オフセット `{0,1,2,3}`** を試し、同じ kick+hat groove スコアで再 micro-align。`SCORE_EPS=0.02` で明確勝ちのみ採用（曖昧な 4-on-floor は grid+0 を維持）
- スコアは **kick + 0.4×hat**。kick/hat が ¼拍以上食い違うときは **kick（ダウンビート）優先**
- next がファイル先頭で早められないとき、同じ位相補正を **`prev_nudge`（≤½拍の prev スキップ）** で適用
- **±1/±2拍の coarse kick-only xcorr は禁止**（v10: 拍は合うが小節ずれ）。整拍探索は groove スコア比較のみ

### 診断用 example

- `funkot-core/examples/marker_phase_diag.rs` — マーカー比較
- `funkot-core/examples/transition_phase_diag.rs` — 2曲の遷移位相（両仮説の score / prev_nudge / bar_off も表示）

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

- キャッシュ: `testdata/real-cache-v4`（全曲 intro/outro **64/64** 寄り）。v17 でも再生成不要
- 再レンダー例（CI最速）:

```sh
./dev.sh cargo run -p funkot-cli --release -- \
  -l testdata/real_playlist.txt \
  --cache-dir testdata/real-cache-v4 \
  --render testdata/real_mix_vXX.wav \
  --wav-format f32 \
  --ci-fast
```

- `--ci-fast` = `--no-loop` + `--render-speed 0` + `--jobs 0`（全CPUで並列 prepare）。**音・解析結果は変えない**（壁時計のみ短縮）
- `--jobs N`（1=従来の逐次ローダー、0=全CPU）。`prepare_tracks_parallel` → `Engine::from_prepared`
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
| v16 | `real_mix_v16_edge_groove_f32.wav` | 端 groove refine + kick優先 + prev_nudge | **1→2 小節ずれ継続**（ダウンビートは全体正しい）。他遷移の小節は正しい |
| **v17** | **`real_mix_v17_bar_groove_f32.wav`** | **mod-4 groove bar identity**（kick+hat、明確勝ちのみ） | **聴感確認待ち** |

遷移ディレクトリ: `testdata/real_mix_vN_transitions/`（`01_…` = 1→2）

### 計測メモ（transition_phase_diag）

v16（edge groove + kick優先 + prev_nudge）:

- **1→2**: chosen **intro-grid** + **prev_nudge≈3328f (~0.25拍)**。kick **+0.019拍**（ロック）。hat ~−0.48 / mid ~−0.977（小節ずれの兆候）
- **2→3 / 3→4 / 4→5**: 良好

v17（mod-4 bar groove）:

- **1→2**: chosen **grid+≈2拍**（aligned≈31216）。kick/hat/mid **~0.00拍**（小節ロック）
- **2→3**: intro-grid bar_off≈0。kick/hat/mid **~0**
- **3→4**: end-anchored 相当。kick/hat/mid **0.00**
- **4→5**: intro-grid。kick/hat/mid **0.00**

根因メモ（1→2）:

1. ダウンビート（kick位相）は合っていても、KazuyaP アウトロと Totsumal イントロの **どの拍が小節頭か（mod-4）** が食い違っていた
2. kick は毎拍にあるため ±0.5拍 micro だけでは小節を解決できない。hat+kick groove の整拍候補比較が必要
3. v10 の kick-only coarse は 2→3 を壊す → 整拍は groove スコア＋明確マージンのみ（曖昧時は markers の grid+0）

---

## 6. 次チャットでやること候補

1. **v17（`real_mix_v17_bar_groove_f32.wav` + `real_mix_v17_transitions/`）を聴感確認**  
   特に **1→2**（`01_KazuyaP_to_Totsumal`）。2→3 / 3→4 / 4→5 が崩れていないかも確認
2. 残課題（未着手・低優先）:
   - 解析側で intro/outro 格子不一致そのものを減らす（キャッシュ v5 候補）
   - ミックスピーク超過（ヘッドルーム／リミッター）
   - ハイパス重なり中は設計上ハットが両デッキから聞こえる（300Hz HPF）
   - コンテナ内 rustfmt/clippy が DNS 失敗することがある

---

## 7. テスト・ビルド・CI

```sh
./dev.sh cargo test -p funkot-core --release
./dev.sh cargo test -p funkot-core --release --test analysis_golden
# 位相ロック単体:
./dev.sh cargo test -p funkot-core --release --lib engine::tests::align_next_entry_micro_only
./dev.sh cargo test -p funkot-core --release --lib engine::tests::bar_identity_groove_corrects_two_beat_marker_error

# 最小フィクスチャ生成（フルミックスではない。WAVは gitignore、golden.json のみコミット）
./dev.sh cargo run -p funkot-cli --release -- \
  --gen-test-fixtures funkot-core/tests/fixtures

# CI最速レンダー（結果不変・並列 prepare）
./dev.sh cargo run -p funkot-cli --release -- \
  -l testdata/real_playlist.txt --cache-dir testdata/real-cache-v4 \
  --render out.wav --wav-format f32 --ci-fast
```

主要テスト: `engine::tests::*`, `analysis_golden`, `fade_curve`, `engine` integration, `ffi`, CLI e2e。

---

## 8. 触るときの注意

- ユーザー向け応答は **日本語**
- 計画ファイルは編集しない
- **変更後は毎回コミット**（本チャットで方針変更済み）
- `testdata/` の実音源・巨大 WAV は成果物確認用；リポジトリには載せない想定
- 「ミッドハイパス」= ハイパス（低域カット）。ローパスに戻さない
- 小節identityをキック相関の ±N拍で取らない（v10 の失敗を繰り返さない）。groove スコアの整拍候補比較は v17 で許可
