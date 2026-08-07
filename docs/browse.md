# ファセット階層ドリルダウン (`sagasu browse`)

- 対象: issue #5 / design.md §6「ファセット階層(クラスタリングの代替)」
- 実装: `sagasu-core::browse`(判断のすべて)+ `sagasu-cli::browse`(表示だけ)

「ディレクトリ構造ではなく意味から探索する」を CLI で成立させる部分。
選んだタグで絞り込み、残った集合に対して**次に見るべきタグ軸**を順位づけして提示し、
その集合を c-TF-IDF で機械的にラベル付けする。

タグ生成そのものは issue #4(design.md §6-1、`docs/tag_rules.md`)。
ここはその上に載る読み取り専用のレイヤーで、`sagasu tag` が作った索引だけを読む。

---

## 1. 使い方

```
sagasu browse [TAG...] [--db FILE] [--axes N] [--values N]
              [--label-terms N] [-n/--files N] [--no-fresh]
```

引数のタグは `namespace:value` で AND。**引数なしが探索の出発点**(索引全体)。
出力の最後に「次に打つコマンド」がそのまま出るので、値をコピーして足していけばよい。

| フラグ | 既定 | 意味 |
|---|---|---|
| `--axes` | 4 | 提示する軸の数 |
| `--values` | 8 | 軸ごとに見せる値の数。**同時にランキング式の表示予算 `m`**(§2)|
| `--label-terms` | 5 | ラベルの語数 |
| `-n` / `--files` | 5 | 集合の先頭を何件プレビューするか。0 で無し |
| `--no-fresh` | off | 差分照会を省く(省いたことを明示する) |

`--values` は「何行出すか」だけでなく**どの軸が上に来るか**を変える。
理由は §2 の最後。

## 2. 「情報量の大きいタグ軸」の定義

design.md §6 の「残りの集合の中で情報量の大きいタグ軸を次の階層として提示」を、
**表示できるバケツに対する期待ビット数**として定義した。

```
score(ns) = H(上位 m 個の値, tail, residue)   [bit]
```

絞り込み後の集合を `n` 件、名前空間 `ns` の値 `v` を持つファイルを `c_v` 件として、
バケツは次の 3 種類:

| バケツ | 重み | 意味 |
|---|---|---|
| 上位 `m` 個の値それぞれ | `c_v` | 画面に出る値 = 実際に選べる次の一手 |
| tail | `Σ c_v − Σ_上位 c_v` | 画面に入りきらなかった値 |
| residue | `max(0, n − Σ c_v)` | この軸が何も言えない部分 |

`p` を各バケツの重み比として `H = −Σ p·log2 p`。**掛け合わせる係数は無い。**

### なぜこの式か

ファセット軸の失敗はすべて「H が低い」に集約される、というのが選定理由。

- **集合のごく一部しか覆わない軸** → residue バケツが巨大になり H が潰れる。
  カバレッジは表示はするが(`covers N% of the group`)、**式には掛けない**。
  residue が既にそれを表現しているので、掛けると二重の罰になる
- **値がほぼ全部シングルトンの軸**(横に広いフラットなディレクトリでの `path:`)
  → 上位 `m` 個は各 1 件、tail が残り全部になり H はほぼ 0。
  値の生の分布のエントロピーは**最大**なのに、である。
  これが表示予算を式に入れた理由そのもの:
  値を全部見られない軸は「次の一手」になり得ない
- **1 つの値が支配的な軸**(99/1) → H ≈ 0.08
- **均等な `m` 分割** → H ≈ log2(m)、1 ステップで到達できる上限

`m` は 1 回の呼び出しの中で全軸共通なので、スコアはそのまま比較できて、
しかも単位が残る — **「この軸の表示値を読んで得られるビット数」**。

### 見落としやすい帰結(バグではない)

**小さい集合ではシングルトン軸が正しく勝つ。** 40 件の集合でシングルトン 40 値の
`path:` は 1.32 bit、きれいな 20/20 分割の `ext:` は 1.00 bit。
上位 8 個が集合の 1/5 を占めるので、当たれば 1 件に着地する — 勝って当然である。
シングルトン軸が崩れるのは、集合が表示予算に対して十分大きくなったとき、
つまり値を提示しても意味が無くなったとき。実測(`/usr`, 63,901 件)では
`path:` は 3,841 値を持ちながら 2.13 bit で、`ext:` の 2.73 bit に負ける。

したがって **`--values` を変えるのは「問いを変える」ことであって、
リストを長くすることではない**。テスト
`widening_the_display_budget_can_change_which_axis_wins` がこれを固定している。

### 集合全体が共有する値

集合の**全ファイル**が持つ値は次の一手ではない(選んでも同じ集合が返る)ので、
バケツからも提示リストからも除く。ただし**捨てない**: `BrowseView::universal`
(CLI では `shared :` 行)に集める。

これを軸ごとではなく**ビューに持たせた**のは実装中の実測による。
軸に持たせていた版では、絞り込みできる値が 1 つも無い軸はランキングから丸ごと
落ちるため、その軸に付いていた共有値も一緒に消えていた。
1 ディレクトリに 6 枚の PNG だけ、という集合が**自分について何も言わない**ビューを
返していた(テスト
`a_value_every_file_shares_is_named_as_a_property_not_offered_as_a_step`)。
本プロジェクトが繰り返し自分の出力から見つけている「黙って欠ける」そのもの。

### 重みは「割り当て」であって分割ではない

1 ファイルが同じ名前空間の値を複数持つ(`path:` は常にそう)場合、
そのファイルは複数のバケツに数えられる。だから `Σ c_v` は `n` を超えうるし、
そのとき residue は 0 になる。分割ではないことは明記して読む側に渡す。

## 3. 決定性

design.md §6 が名指しで要求している「再索引で階層が壊れない」を 2 段で担保する。

1. タグ層自体が純関数(design.md §6-1、`crate::tags`)
2. この層の順序付けはすべて**全順序**

スコアは浮動小数点なので、比較前に `rank_key` で **1e-6 グリッドに丸める**。
同点はそこから整数(カバレッジ → 値の数)、最後に名前空間名/タグ名に落ちる —
これは絶対に同点にならない。

丸めを先に置いたのは、`log2` / `ln` の最終ビットがプラットフォームの libm で
1 ulp 違いうるため。丸めれば真の同点になり、以降の整数・文字列のタイブレークが
どこでも同じ順序を返す。テスト `the_rank_key_turns_a_last_bit_difference_into_a_tie`。

軸の中の値の順は `count desc, tag asc` で、これは `tagindex::tag_counts`
(= `sagasu tags`)と同じ規約。同じものを見せる 2 つのコマンドが違う順序で
並べることがないようにしている。

## 4. c-TF-IDF ラベル

集合に名前を付ける。何を「文書」とし何を「クラス」とするかを決めれば
古典的な class-based TF-IDF がそのまま使える。

- **文書** = 1 ファイル。その語 = そのファイルのタグ。
  パストークンもここに含まれる(`crate::tags` がディレクトリ名から作った `path:` タグ)
- **クラス** = 現在の絞り込み集合 `S`。全ファイルのタグを連結した 1 つのメタ文書
- **背景** = live なコーパス全体

```
W(t) = (c_S(t) / |S|) · ln(1 + N / df(t))
```

| 記号 | 意味 |
|---|---|
| `c_S(t)` | 集合の中でタグ `t` を持つファイル数 |
| `\|S\|` | 集合の件数 |
| `df(t)` | 索引全体(live)で `t` を持つファイル数 |
| `N` | 索引全体の live ファイル数 |

左が集合メタ文書の term frequency(集合内シェア)、右が inverse document
frequency。「この集合中にも多く、コーパス全体にも多い」タグは沈み、
「この集合中に多く、他には無い」タグが浮く。
**ユーザーが選んだタグ自身は除外**する — それは問いであって答えの説明ではない。

降順、同点はタグの辞書順。丸めは §3 と同じ。

### ファイル名を再トークナイズせず、タグを読む理由

タグ語彙はすでに決定的で、小文字化済みで、上限(64 タグ/ファイル)まで
掛かった語彙である(design.md §6-1)。ここで別のトークナイザを持ち込むと、
**ラベルがファセット件数と矛盾したことを言える**ようになる
(「このグループは請求書」と書いてあるのに `sagasu tags` にその軸が無い、等)。
同じ語彙から作れば、ラベルは必ず数えられる。

### `shared :` との重複について

ラベルには集合全体が共有するタグも入りうるので、`shared :` 行と重複する。
これは意図的:`shared :` は**列挙**(辞書順・全部)、ラベルは**順位づけ**である。
末端まで降りて全タグが共有になった集合では、ラベルだけが
「このグループは `path:sagasu-cli` である」と**どれが効いているか**を言える。

## 5. 鮮度 — タグ層はスナップショットである

`sagasu browse` は `sagasu tags` とまったく同じ鮮度ブロックを出す。
同じ関数(`sagasu-cli::output::print_tag_freshness`)から出ている。
2 つ書いてあれば片方が古くなる、という理由でそこに置いた。

- タグ層が索引時点のスナップショットであることを**常に**明示する。
  `(current)` とは言わない(design.md §6-1 の経緯)
- 差分源に照会して「索引時点以降に変わったファイル数」を出す。
  0 でなければ stderr に警告(**照会はするがマージはしない**)
- 削除は差分源から来ないので、**プレビュー行は実在確認する**。
  消えている行は `dropped :` にパスごと出して stderr に警告
- `matched` とファセット件数はすべて索引側の数、すなわち**上限値**。
  実在確認されたのはプレビュー行だけ、と毎回書く

`BrowseView::snapshot` が `tag_scan_generation` と現在の `scan_generation` を
持って返るので、**M4 の UI はこの事実を受け取らずにビューを描画できない**。
表示規律をコア API の型で運んでいる。

### マージしない、という判断は #5 でも変わっていない

design.md §6-2 は「本格的なドリルダウンは issue #5 の範囲なのでそこで一緒に設計する」
としていた。設計した結果、**マージしない**を維持する。理由:

ファセット件数は SQL の集約である。索引後に作られたファイル(タグを持たない)を
そこに混ぜるには、変更ファイル全件にタグを生成した上でそれを集約の中に
差し込む必要があり、それは「1 クエリ 1 集約」ではなく
「差分集合をメモリに載せて再集約する 2 本目の経路」になる。
`sagasu tags` の絞り込みは行の列挙なのでまだ想像できたが、
**件数の集約はライブ側と索引側で足し合わせる意味が保証できない**
(同じファイルが両側に出れば二重に数える。それを避けるには差分集合全件の
file_id を集約から除外する必要があり、除外は SQL 側の集約に持ち込めない)。

正直な表示に留め、`delta : N changed` と警告で「今どれだけ古いか」を必ず出す。
増分タグ更新(design.md §6-2 の残論点)が入れば、この論点自体が消える。

## 6. コア API(M4 の Tauri がそのまま叩く形)

判断は全部 `sagasu-core::browse` にある。CLI はその印刷機。

```rust
pub fn browse(store: &Store, query: &BrowseQuery) -> Result<BrowseView>;

pub struct BrowseQuery {
    pub selected: Vec<Tag>,   // 空 = 索引全体(探索の根)
    pub max_axes: usize,
    pub max_values: usize,    // ランキング式の m
    pub label_terms: usize,
    pub preview: usize,
}

pub struct BrowseView {
    pub selected: Vec<Tag>,
    pub matched: i64,              // 索引側の件数 = 上限値
    pub corpus: i64,
    pub label: Vec<LabelTerm>,     // c-TF-IDF 降順
    pub label_vocabulary: usize,   // 切り捨て前の候補語数
    pub universal: Vec<Tag>,       // 集合全体が共有する値
    pub axes: Vec<FacetAxis>,      // スコア降順
    pub axes_total: usize,         // 集合に存在する名前空間の数
    pub axes_refining: usize,      // うち絞り込める軸の数
    pub preview: Vec<FileRow>,     // 実在確認は呼び出し側の責務
    pub snapshot: TagLayerSnapshot,
}
```

**なぜ CLI に `--json` を付けなかったか。** M4 の UI は Tauri なので
`sagasu-core` に直接リンクする(design.md §3「コアが Rust ならそのまま接続」)。
つまり機械可読インターフェースは**この構造体そのもの**であり、
JSON を CLI に足すと同じ内容を記述する 2 本目の契約を手で同期することになる。
CLI 全体の機械可読出力は issue #6 の論点なので、答えを出すなら
1 サブコマンドから始めるのではなく全サブコマンドに一度に答えるべき。

`preview` の実在確認をコアではなく呼び出し側に置いたのも同じ線引きで、
`browse()` は**データベース以外に触れない**(§3 の決定性が守れる)。

## 7. 受け入れ基準: 実データでの到達シナリオ

### 7-1. `/usr`(63,901 ファイル / 555,664 タグ行)— 4 ステップ

計測日 2026-08-07、Linux / ext4、release ビルド。
`--no-fresh` はデータベースの作業だけを見るため。

```
$ sagasu index /usr --db usr.db      # 63,901 files, 3.4s
$ sagasu tag --db usr.db             # 555,664 rows, 4,424 distinct, 27s
```

問い: **「Go の標準ライブラリのどこかに、zip のテストデータとして使われている
小さな PNG があったはず。場所は覚えていない。」**

```
$ sagasu browse --db usr.db
axes    : 4 of 8 shown, ranked by expected bits over the top 8 value(s)
  ext     : 2.73 bits, 466 value(s), covers 93% of the group
  path    : 2.13 bits, 3841 value(s), covers 100% of the group
  kind    : 2.07 bits, 12 value(s), covers 96% of the group
        33390  kind:code                        (52% of the group)
        14334  kind:text                        (22% of the group)
         6203  kind:image                       (10% of the group)   ← ①
  format  : 1.31 bits, 18 value(s), covers 100% of the group
```

```
$ sagasu browse --db usr.db kind:image                    # ① 6,203 件
label   : path:icons (5804/6203)  ext:svg (5615/6203)  format:xml (5473/6203) …
  path    : 2.89 bits, 97 value(s), covers 100% of the group
  format  : 0.70 bits, 6 value(s), covers 100% of the group
         5473  format:xml                       (88% of the group)
          449  format:png                       (7% of the group)    ← ②
```

```
$ sagasu browse --db usr.db kind:image format:png         # ② 449 件
label   : ext:png (448/449)  path:hicolor (149/449)  path:icons (268/449) …
  path    : 2.49 bits, 70 value(s), covers 100% of the group
          335  path:share                       (75% of the group)
          268  path:icons                       (60% of the group)
          149  path:hicolor                     (33% of the group)
          112  path:go1                         (25% of the group)
          110  path:testdata                    (24% of the group)   ← ③
```

```
$ sagasu browse --db usr.db kind:image format:png path:testdata   # ③ 110 件
label   : path:png (96/110)  ext:png (110/110)  path:image (108/110)  path:pngsuite (70/110)
shared  : all 110 file(s) in this group carry ext:png, path:go1, path:local, path:src
  path    : 2.34 bits, 7 value(s), covers 100% of the group
          108  path:image                       (98% of the group)
           96  path:png                         (87% of the group)
           70  path:pngsuite                    (64% of the group)
           55  path:go1.24.7                    (50% of the group)
           55  path:go1.25.1                    (50% of the group)
            2  path:archive                     (2% of the group)
            2  path:zip                         (2% of the group)    ← ④
```

```
$ sagasu browse --db usr.db kind:image format:png path:testdata path:zip
matched : 2 of 63901 live files (0.0%)
files   : 2 of 2 shown
   10874  /usr/local/go1.25.1/src/archive/zip/testdata/gophercolor16x16.png
   12184  /usr/local/go1.24.7/src/archive/zip/testdata/gophercolor16x16.png
```

**63,901 → 6,203 → 449 → 110 → 2 の 4 ステップ。**
`shared :` が「この 110 件はすべて `path:go1/local/src` の下」= Go のソースツリーだと
言い、`path:zip` が最後の 1 手として提示されている。
ディレクトリの位置(`src/archive/zip/testdata/`)は一度も知らなくてよい。

### 7-2. sagasu リポジトリ自身(74 ファイル)— 2 ステップ

```
$ sagasu browse --db self.db
  path    : 2.96 bits, 29 value(s), covers 95% of the group
  ext     : 2.05 bits, 8 value(s), covers 96% of the group
  kind    : 1.50 bits, 4 value(s), covers 100% of the group
           41  kind:code                        (55% of the group)
  pattern : 0.24 bits, 1 value(s), covers 4% of the group

$ sagasu browse --db self.db kind:code                    # 41 件
label   : ext:rs (40/41)  path:src (29/41)  path:crates (26/41)  path:sagasu (26/41)
shared  : all 41 file(s) in this group carry format:text
  path    : 3.00 bits, 22 value(s), covers 100% of the group
           …
            7  path:cli                         (17% of the group)

$ sagasu browse --db self.db kind:code path:cli           # 7 件 = 葉
label   : path:sagasu-cli (7/7)  path:crates (7/7)  path:sagasu (7/7)  path:src (7/7)  ext:rs (7/7)
shared  : all 7 file(s) in this group carry ext:rs, format:text, path:crates,
          path:sagasu, path:sagasu-cli, path:src — none of these narrows it
axes    : 0 of 0 shown …
          (nothing left to drill into — this group is a leaf)
files   : 7 of 7 shown
```

葉に着いたことが `axes : 0 of 0` と `(this group is a leaf)` で明示される。
ラベルは全語が 7/7 だが、**順位が付いている**のがラベルの仕事:
`path:sagasu-cli` が先頭に来る。

### 7-3. 所要時間

`/usr`(63,901 files / 555,664 rows)、release、`--no-fresh`:

| 操作 | 所要 |
|---|---|
| `browse`(索引全体) | 1.01s |
| `browse kind:image` | 0.93s |
| `browse` × 3 タグ | 0.87s |
| (参考) `sagasu tags`(名前空間一覧) | 1.22s |

支配的なのは絞り込み集合そのものの評価。
`tagindex::selection_subquery` は AND を `file_tags` の `GROUP BY file_id` で判定するので、
SQLite はこれを主キー順の全走査で答える — 集合が何件になろうと 1 パス掛かる。
browse は同じ集合を 3 回読むので、**1 回だけ評価して temp テーブルに落とす**
(`browse::materialize_selection`)。これで 1.27s → 0.93s。
残っているのはその 1 パスと、ラベルの `df(t)` 集約(約 0.3s)。

`sagasu tags` 自身も同じ全走査を 2 回やっているので、
これは browse が持ち込んだコストではない。集合評価そのものの索引化(部分索引・
タグ集合のビットマップ等)は別 issue の話。

## 8. 既知の限界 / 残した論点

- **カバレッジの低い名前空間は既定では軸に出ない。** `anomaly:`(`/usr` で 7 件)、
  `date:`(574 件)、`version:`(660 件)は residue バケツに潰されて上位 4 軸に入らない。
  これはランキングとしては正しい(その軸を読んでも集合はほとんど絞れない)が、
  「珍しいものを見せてほしい」という別の問いには答えない。
  **黙って消してはいない**:`(N more axis/axes could narrow this group — raise --axes)`
  が必ず出るし、`sagasu tags` は全名前空間を列挙する。
  「意外性で並べる軸」が要るなら別 issue
- **ラベルと `shared :` は重複する。** §4 の通り意図的(列挙 vs 順位づけ)だが、
  末端の集合では同じタグが 2 回出る。表示上の冗長さは残っている
- **`matched` は上限値のまま。** 実在確認はプレビュー行だけ。
  集合全体の確認はコーパス規模のコストになるのでやらない(`sagasu tags` と同じ線)
- **差分マージはしない**(§5)。増分タグ更新が入るまでの仕様
- **集合評価が毎回全走査**(§7-3)。索引化は別 issue
- **`tag.rs` の分割は見送った。** `sagasu tag`(生成)と `sagasu tags`(読み取り)は
  同居のまま(475 行)。`browse` を足すにあたって実際に共有が必要だったのは
  鮮度ブロックだけで、それは `output.rs`(「複数のサブコマンドが必要とする表示」が
  存在理由のモジュール)に移した。読み取り系をさらに切り出すのは、
  共有されない片方を動かすだけの差分になり、レビューの目を薄めるので採らなかった

## 9. 関連

- design.md §6 / §6-1 / §6-3
- `docs/tag_rules.md` — タグ生成規則とユーザー定義ルール
- `docs/schema_v0.md` — `tags` / `file_tags` と `meta` のタグ関連キー
- `crates/sagasu-core/src/browse.rs` — 式と決定性の根拠(rustdoc)
- `crates/sagasu-core/tests/browse_tests.rs` — 上記の性質を固定しているテスト
