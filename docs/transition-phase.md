# 遷移の位相・小節 identity

2曲を重ねるとき「ダウンビートが合っている」だけでは足りず、**どの拍が小節頭か（mod-4）**
が両曲で一致していなければならない。ここは v6〜v17 で何度も壊しては直した領域なので、
触る前に「禁止事項」と失敗履歴を読むこと。

## 現行方式（v17）

`align_next_entry_with_phase_hypotheses`（`engine.rs`）。

- マーカーは**アウトロ2候補**を保持する（`prepare_output_markers` →
  `(fd, intro_grid, end_anchored)`）
  - `outro_start_out` = intro-propagated 格子（遷移トリガ・小節スケジュール）
  - `outro_end_anchored_out` = outro tail（末尾数小節）で groove refine → `outro_bars` 分だけ戻す
- **Stage 1（位相）**: 各仮説を **±0.5拍のみ** micro-align（`align_next_entry_scored`）
- **Stage 2（小節 identity）**: 各仮説の nominal から **整拍オフセット `{0,1,2,3}`** を試し、
  同じ kick+hat groove スコアで再 micro-align。`SCORE_EPS=0.02` で明確勝ちのみ採用
  （曖昧な 4-on-floor は grid+0 を維持）
- スコアは **kick + 0.4×hat**。kick/hat が ¼拍以上食い違うときは
  **kick（ダウンビート）優先**
- next がファイル先頭で早められないときは、同じ位相補正を
  **`prev_nudge`（≤½拍の prev スキップ）** で適用

イントロ側マーカーは 解析 `first_downbeat` → scale → ±半拍 **kick+hat groove** refine
（疎な intro head 対策）。手動で `outro_bars` を直すときは `outro_start` も合わせる
（ロード時は再計算しない）。

### ガイドクリックの位相補正はミックス側に要らない

`funkot-cli` の `outro_beat_phase_shift`（→ docs/guide-clicks.md）は、曲自身の小節が
`first_downbeat` 格子からずれる曲のために 0〜3拍の補正を入れる。**これはミックス側には
不要**。エンジンの `outro_start_out` は fd 伝播ではなくファイル終端から逆算し、
Stage 2 の mod-4 groove bar identity が同じずれを実音から解決するため。

`testdata/verify_mix_phase_transitions/` の4遷移を聴いて確認済み。統計上 +2 / +3 / +1拍
ずれる3曲のアウトロと、補正が効かない対照（Kimi to Semi Blue → Boom Boom Pow）の
いずれもダウンビート・mod-4 とも一致していた。よって `outro_beat_phase_shift` を
`funkot-core` / `TrackAnalysis` へ移す必要はない。

## 禁止事項（v10 の失敗）

**±1/±2拍の coarse kick-only xcorr は使わない。** 拍は合うが小節がずれる。
整拍の候補探索は groove スコア（kick + hat）の比較＋明確マージンでのみ許可し、
曖昧なら markers の grid+0 に留まること。

根因（1→2 遷移で最後まで残った問題）:

1. ダウンビート（kick 位相）が合っていても、KazuyaP アウトロと Totsumal イントロの
   **mod-4 の小節頭**が食い違っていた
2. kick は毎拍にあるため、±0.5拍 micro だけでは小節を解決できない。
   hat を含む groove の整拍候補比較が必要
3. v10 の kick-only coarse は 2→3 を壊した

## 診断

```sh
./dev.sh cargo run -p funkot-core --example transition_phase_diag --release -- \
  --sr 44100 --cache-dir testdata/real-cache-v4 --rate 1.10 \
  PREV.flac NEXT.flac
```

- `funkot-core/examples/marker_phase_diag.rs` — マーカー比較
- `funkot-core/examples/transition_phase_diag.rs` — 2曲の遷移位相
  （両仮説の score / prev_nudge / bar_off も表示）

単体テスト:

```sh
./dev.sh cargo test -p funkot-core --release --lib engine::tests::align_next_entry_micro_only
./dev.sh cargo test -p funkot-core --release --lib engine::tests::bar_identity_groove_corrects_two_beat_marker_error
```

## ミックス版履歴と聴感フィードバック

実音源5曲（`testdata/real_playlist.txt`）でのレンダー版。遷移ディレクトリは
`testdata/real_mix_vN_transitions/`（`01_…` = 1→2）。

| 版 | 要点 | ユーザー反応 |
|----|------|----------------|
| v6 | 線形フェード | フェード端点が0でない／前曲が残る |
| v7 | 端点0＋prev破棄 | フェードはOK。3→4でハット二重 |
| v8 | legacy格子をやめて mapped_outro | 3→4は未解消 |
| v9 | 位相ロック（±2拍含む） | 3→4ほか改善。**2→3が大幅ずれ**（entry→0） |
| v10 | 2段ロック＋端却下。2→3は+1拍 | **2→3: 拍は合うが小節ずれ**。他は正常 |
| v11 | 微調整±0.5拍のみ | 聴感確認待ち |
| v12 | アウトロ/小節は解析粗位置→±半拍 refine のみ | ダウンビート OK。**1→2 / 2→3 小節ずれ** |
| v13 | アウトロをイントロ小節格子に統一 | **1→2 / 3→4 ダウンビートずれ**。2→3 / 4→5 は正しい |
| v14 | 遷移時に intro格子 vs end-anchored を選択 | **1→2 小節ずれ**（他は正しい） |
| v15 | end は kick score 明確勝ちのみ（slack 廃止） | **1→2 ダウンビートずれ**。他は正しい |
| v16 | 端 groove refine + kick優先 + prev_nudge | **1→2 小節ずれ継続**（ダウンビートは全体正しい） |
| **v17** | **mod-4 groove bar identity**（kick+hat、明確勝ちのみ） | 聴感確認待ち |

`transition_phase_diag` の計測:

- v16: 1→2 は intro-grid + `prev_nudge≈3328f (~0.25拍)`。kick +0.019拍（ロック）だが
  hat ~−0.48 / mid ~−0.977（小節ずれの兆候）。2→3 / 3→4 / 4→5 は良好
- v17: 1→2 は grid+≈2拍（aligned≈31216）で kick/hat/mid ~0.00拍（小節ロック）。
  2→3 は intro-grid bar_off≈0、3→4 は end-anchored 相当、4→5 は intro-grid。いずれも ~0

## 既知の未解決（低優先）

- ミックスピークが +3 dBFS 程度になることがある（リミッター未実装）。
  `verify_mix_phase_transitions` 生成時は peak 1.6232（+4.21 dBFS）、`|x|>1` が
  3197サンプル。f32 出力なのでファイル上はクリップしない
- ハイパス重なり中は設計上ハットが両デッキから聞こえる（300Hz HPF）
