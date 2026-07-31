# ラベリング用ガイドクリックの位置決定

`--label-sections` は区間境界の候補位置にガイドクリックを重ねて再生する。
「クリックが音楽の小節頭で、かつ意図した小節数だけ手前で鳴る」ことは
ラベルの正しさの前提なので、ここに手を入れる前に本書を読むこと。

2026-07-28〜30 に3段階の位置ずれを潰した。**現行 HEAD は全14曲・全候補で
`lock_beat_phase` の移動量 ≤0.09拍**（＝ nominal がその場の実ビート格子にほぼ乗っている）。

| 症状 | 原因 | 対応 commit |
|---|---|---|
| クリックが小節の途中で鳴る | 候補を `total_frames` 生値から逆算（サブ拍位相が任意） | `b486a08` 位相ロック、`bbf684c` 小節格子アンカー |
| 曲によって拍が不安定 | fd 伝播格子のテンポドリフト（最大 0.49拍） | `0460891` テンポ refit |
| 残16小節のはずが残13小節 | **ファイル末尾 ≠ 音楽の終わり**（全曲に 0.64〜3.86小節のテール） | `33aac27` 音楽終端アンカー |

実耳確認済み（2026-07-30、クリック無し版と比較）: 旧アンカーは小節の3拍目、
新アンカー（`bbf684c` + refit）は小節頭で鳴る。

## 現行方式（4層）

責務を分けてあり、**層をまたいで補正しないこと**が設計の肝。

1. **音楽終端アンカー**（`label_session.rs::music_end_frame`、`33aac27`）
   アウトロ候補は「ファイル末尾」ではなく「最後に叩かれた音」から数える。
   テールの 50ms RMS 包絡で、自身の75パーセンタイルから −20 dB 以内かつ
   **直前の窓より 1.6倍以上大きい**最後の窓を最後の一撃とする（テールは減衰しか
   しないので、これで残響と分離できる）。アンカーはその直後の小節線 = **`ceil`**
   （`round` ではない。最後の一撃が2拍目でもその小節は音楽的に埋まっているため）。
   ダウンビートちょうどで終わる曲は検出が数十ms遅れるので
   `ANCHOR_ONSET_SLACK_BARS=0.15` で吸収する（実測分布は 0.08小節以内が5曲、
   次が 0.22小節でその間が空いている）。
2. **整数小節への丸め**（`label_session.rs::outro_anchor_frame`、`bbf684c`）
   アンカーを `first_downbeat + round((anchor - fd) / bar_frames) × bar_frames` に
   丸め、`first_downbeat` の小節格子上に載せる。**丸めるのは整数小節数だけ**。
3. **テンポ refit**（`label_session.rs::outro_bar_frames_refit`、`0460891`）
   伝播に使う `bar_frames` をアウトロ局所のテンポで精密化する（下記）。
4. **サブ拍位相ロック**（`analysis::lock_beat_phase` を再生窓そのものに適用、`b486a08`）
   広帯域スペクトルフラックスの全拍周期コム。`LOCK_CLEAR_WIN_RATIO=1.05` の
   明確勝ちのみ採用し、最後に `refine_kick_marker` でキック本体へスナップ。
   移動量は常に半拍未満なので、**この層が担当するのはサブ拍位相だけ**
   （AGENTS.md の「小節 identity をキック相関の ±N拍で取らない」に抵触しない）。
   イントロ側にも適用する（`first_downbeat` は 0.2拍ほどずれている曲がある）。

### テンポ refit の理屈と限界

`outro_bpm` の精度は約0.05%。577小節（Boom Boom Pow）伝播すると
0.04% × 2308拍 ≒ **半拍**になり、`lock_beat_phase` の探索半径を食い潰す。

アウトロ各候補を解析テンポでロックすると、局所的（ドリフト無し）かつ `fd` から
**整数拍**の位置が得られる。距離をその整数で割ればテンポが約0.001%精度で決まる。
5候補の**中央値**を採り、`REFIT_MIN_AGREEING_POINTS=3` 以上が残差 0.25拍以内で
一致し、かつ補正が `REFIT_MAX_REL_ADJUST=0.2%` 以内のときだけ採用（外れたら解析値のまま）。
小節 identity は触らない。

**限界**: 参照点は「ドリフトした nominal をロックして」得るので、**元のドリフトが
半拍未満のときだけ**真のテンポを回復できる。実測最大 0.49拍なので今の母集団では
成立しているが余裕は小さい。半拍を超えると参照点が隣の拍に吸着し、精密化された
テンポは「レバー長あたり1拍」長く出る。その結果は**内部的には一貫している**
（全候補が同じ格子に乗るので、1候補のクリックが正しいと確認できれば残りも信頼できる）が、
絶対的な拍 identity はこの証拠からは決められない。残差チェックでも判別不能。

## 測り方

`|shift|` = `lock_beat_phase` が nominal をどれだけ動かしたか。新アンカーの nominal は
定義上「格子上」にあるので、**shift は「伝播格子が局所の実ビートからどれだけ
ドリフトしたか」の直接測定**になる。アンカーやテンポ推定を触るときは
**14曲の `|shift|` を before/after で並べて回帰を見ること**。

```sh
# 補正量・アンカー・最後の一撃からの距離を出す
./dev.sh cargo run -p funkot-cli --release --example click_phase_diag -- \
  --cache-dir funkot-cache testdata/*.flac

# 4方式（local / prop / hybrid / refit）を並べる
./dev.sh cargo run -p funkot-cli --release --example outro_anchor_ab -- ...

# 素の音楽窓（クリック無し・ダック無し）を書き出す
./dev.sh cargo run -p funkot-cli --release -- \
  --label-sections -l LIST.txt --labels /dev/null --cache-dir funkot-cache \
  --render-clips OUT_DIR "--click-db=-200" --click-duck-db 0
```

**クリック位置は「クリック有り − クリック振幅ゼロ版」の差分で取る。**
合成後の信号だけを見て「クリックが埋もれている」「ここで鳴っている」と判断してはいけない。
クリック自身の減衰音とダック済み音楽を分離できないため
（`label_session.rs` の `clicks_stay_audible_over_dense_loud_material` がこの方式）。

## 試して失敗した方式（再挑戦しないこと）

1. **`first_downbeat + n × intro_bpm 拍` に丸めてサブ拍位相を決める** — 全長にわたる
   伝播で BPM 推定誤差が積もり、Boom Boom Pow では**半拍**ずれる。末尾が元々正しい曲を壊す。
   （層2の「整数小節への丸め」はこれとは別物。伝播格子を**整数小節の丸め**にしか使わず、
   累積誤差 最悪半拍 ≒ 0.125小節では丸め先の整数小節は揺るがない）
2. **末尾数小節（engine の `derive_outro_start_out` 相当）で groove refine** — Shirube /
   Sakura Photograph は末尾がフェード＝オンセットゼロで、ロック先が無い
3. **`refine_groove_phase` / `refine_periodic_phase`** — 探索が ±0.45拍に制限されており、
   IVY の +0.494拍（＝最悪の半拍あいまい点）には原理的に届かない
4. **低帯域（150Hz LPF）のオンセット novelty で全周期コム** — 密なミッドソング素材では
   同じ曲の窓ごとに互いに矛盾する位相を返す。**広帯域スペクトルフラックス**なら
   解析側の `first_downbeat` と数百分の1拍で一致する
5. **hybrid（サブ拍と拍 identity は局所アンカー、小節 identity だけ伝播格子から）**
   — あいまい点が lock の ±0.5拍から**小節スナップの ±2拍**に移るだけで、EOF 残差が
   半小節に近い曲（Shuki −0.415 / Love & Joy +0.495）で**丸1小節ずれる**。実測で Shuki は
   HEAD より 4.1拍手前の小節頭に落ちた。`examples/outro_anchor_ab.rs` に残してある
6. **レベル閾値だけで音楽終端を取る** — 残響の途中で切るため Love & Joy で半小節遅れる。
   Shuki で合ったのは偶然
7. **オンセット + `round` で音楽終端の小節線を決める** — Shuki が 238小節になり実耳と1小節ずれる

## 実測データ（testdata 14曲）

### 末尾テール（最後の一撃から EOF まで、小節）

全曲にテールがある＝**全曲・全候補がテール分だけ遅れていた**。

| 曲 | テール | 曲 | テール |
|---|---|---|---|
| Sakura Photograph | **3.86** | Shuki Shuki Song | **2.55** |
| Kimi to Semi Blue | 2.03 | Totsumal | 1.84 |
| Boom Boom Pow | 1.76 | Moon Turtle | 1.16 |
| Monitoring Db / Love & Joy / Superlative | 0.94 | Eternal Light | 0.79 |
| Sekaiichi Kawaii Watashi | 0.75 | IVY / Shirube | 0.71 |
| Starmine | 0.64 | | |

Shuki Shuki Song の実例: キックが EOF の 2.59小節前で止まる（50ms 窓の低域 RMS が
−25 → −46 → −60 dB と落ち、最後は完全な digital silence）。候補16の境界は EOF の
15.58小節前なので、音楽の終わりからは 15.58 − 2.59 ≒ **13小節** — ユーザーの実耳報告と一致。
`33aac27` でアンカーは `fd + 242小節` → `fd + 239小節`（報告どおりぴったり3小節）。

**「末尾40秒の RMS が −10〜−11 dBFS だから無音は無い」は誤り**（40秒平均では
2.5小節のテールが見えない）。50ms 窓で見ること。

### `(total_frames - first_downbeat) / bar_frames` の小数部

0付近に集中せず ±0.5小節の全域に分布する。「ファイル末尾は小節境界」は
**小節単位でしか正しくない**。

| 曲 | 小数部 | 曲 | 小数部 |
|---|---|---|---|
| Love & Joy | **+0.495** | Shuki Shuki Song | **−0.415** |
| Totsumal | +0.256 | Monitoring Db | −0.250 |
| Eternal Light | −0.214 | Superlative | +0.187 |
| Boom Boom Pow | +0.141 | IVY | −0.128 |
| Kimi to Semi Blue | +0.093 | Shirube | −0.083 |
| Sakura Photograph | −0.078 | Sekaiichi Kawaii Watashi | −0.014 |
| Moon Turtle | +0.007 | Starmine | +0.005 |

### テンポ refit の効果（14曲 × 全5候補）

| | 修正前 | 修正後 |
|---|---|---|
| `\|shift\|` 最大 | **0.588拍**（Monitoring Db） | **0.166拍**（Sakura 48小節の1点のみ。他は全て ≤0.09） |
| Monitoring Db | −0.44〜−0.59 | −0.01〜−0.09 |
| Boom Boom Pow | +0.47〜+0.55 | +0.02〜+0.08 |
| Sakura / Shirube / Love & Joy | 0.20〜0.37 | ≤0.17 |
| クリック位置の移動量 | — | **最大 0.09拍**（＝聴感上は同じ位置） |

テンポ補正量は最大 0.036%（Monitoring Db）、Boom Boom Pow +0.023%。

## 残っている懸念

`lock_beat_phase` は**拍**にしか吸着せず、小節頭（mod-4）を見ない。アンカーが1拍以上
ずれた曲では、吸着先が小節頭でない拍になり得る。ここは AGENTS.md の
「小節 identity をキック相関の ±N拍で取らない」と正面から衝突する領域なので、
手を入れるなら [transition-phase.md](transition-phase.md) の v10 の失敗を読み直してから。
