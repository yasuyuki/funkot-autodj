# 遷移の位相・小節 identity

2曲を重ねるとき「ダウンビートが合っている」だけでは足りず、**どの拍が小節頭か（mod-4）**
が両曲で一致していなければならない。ここは v6〜v17 で何度も壊しては直した領域なので、
触る前に「禁止事項」と失敗履歴を読むこと。

## 手動ナビゲーションの局所同期

手動遷移は現在曲と実際の移動先に対する準備済みプランを使う。開始位置を未来へ取り、
その位置の位相と、補正後の重ね区間全体を検査する。音声処理側では指定サンプルで適用し、
間に合わなかった位置の補正を後から使わない。オフラインも同じプラン生成処理を使う。

拍の位相と小節・4小節の起点は別の情報である。既存の先頭マーカーを音源と照合し、
そこからの拍の連続性が成立するときだけ周期を引き継ぐ。途中のブレイクや変速で連続性を
失った場合、同じキックの反復から4小節の先頭を復元したとは扱わない。局所窓の最初の
キックや通常アウトロ用の位相仮説を、曲中の新しい構造起点として使わない。

通常の十分な素材では4小節周期を優先する。短い素材、明示されたフェード設定、固定の
待機期限で4小節周期を使えない場合は、根拠のある小節境界へ縮退する。期限は延長しない。
現在曲の過去のブレイクで構造を引き継げなくても、移動先の構造と、実際の重ね区間全体の
テンポ・相対位相が安全なら、`beat sync: structural identity unknown` として拍同期の
段階的フェードを使う。この開始位置やフェードの長さを小節・4小節の一致とは呼ばない。
実際のテンポ・位相や素材が不十分なら簡易クロスフェードへ縮退する。速度は動的に変えず、
両曲の局所テンポ差から重ね終わりまでのずれを評価する。採用理由と実際の開始・entry・
フェード境界はエンジンの診断で確認できる。

連続性の検査は毎拍のキックだけを前提にしない。裏拍を含むリズムでは、既存の8小節の
kick+hat相関で固定参照との位相を確認し、打音のない間隔が測定誤差を含めて半速周期に
達しないことを要求する。キック密度の変化そのものからブレイクを推定しない一方、疎な
区間は保守的に構造不明となる。相関でマーカーや音声を動かすことはなく、曲間の位相補正は
最後の実entryに対して一度だけ行う。

実際の重ね区間では8秒の重複窓ごとにテンポを調べる。全区間の固定参照による位相検査と
組み合わせる場合に限り、単一位相のcombが別のアクセント周期を優先しても、提案周期の
onset対の相関がcomb首位と半速の両方に勝つことを独立の根拠として利用する。
拍の間の細かな刻み自体は計数を否定する根拠にしない。同点や同じピークの微小な補間差は
根拠にしない。
位相・欠落は窓の継ぎ目を含む重ね区間全体で検査する。

局所判定の閾値と待機期限は実装を正本とする。合成回帰は正常テンポ、減速、半速、欠落、
区間途中の変化を検査するが、実音源の聴感と実デバイスでの負荷時受入を代替しない。

初回ライブ再生のpreviewと全曲bufferは別々にstretchされるため、同じ再生位置でも波形は
一致するとは限らない。Upgradeでは実際の交換時点で、既存のclick除去と同じ10 msを使って
両bufferのgain込みの音声をつなぐ。preview末尾もゼロへfadeし、全曲bufferの到着が遅れた
場合はゼロからつなぐ。再生位置と全曲の構造マーカーは維持し、不要bufferの破棄は音声処理外で行う。
今回の実測・受入状況は [Issue #13](https://github.com/yasuyuki/funkot-autodj/issues/13) を参照。

## 自動アウトロ遷移（v17）

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
