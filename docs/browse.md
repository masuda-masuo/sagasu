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
出力の最後の `next :` 行が**そのまま貼り付けて実行できるコマンド**になっている。

| フラグ | 既定 | 意味 |
|---|---|---|
| `--axes` | 4 | 提示する軸の数 |
| `--values` | 8 | 軸ごとに見せる値の数。**同時にランキング式の表示予算 `m`**(§2)|
| `--label-terms` | 5 | ラベルの語数 |
| `-n` / `--files` | 5 | 集合の先頭を何件プレビューするか。0 で無し |
| `--no-fresh` | off | 差分照会を省く(省いたことを明示する) |
| `--delta-limit` | delta 既定 | 差分照会を打ち切る変更ファイル数 |

`--values` は「何行出すか」だけでなく**どの軸が上に来るか**を変える。
理由は §2 の最後。

### `next :` 行について

- 推薦する値は**軸の先頭行ではない**。選び方は §2-1
- **既定と違うフラグをそのまま引き継ぐ**。`--values` は軸のランキングを変え、
  `--no-fresh` は鮮度を確認するかを変えるので、落とすと「今見ている木とは
  別の木を見るコマンド」を渡すことになる
- **シェルクォートする**。`path:` の値はディレクトリ名そのもので、
  `TOKEN_SEPARATORS`(`crate::tags`)には空白が入っている。
  `2024 reports/` というフォルダは `path:2024 reports` というタグを作るので、
  クォートしなければ貼り直した瞬間に
  `error: tag "reports" is not in namespace:value form` になる —
  ツールが印字したコマンドをツール自身が拒否する。
  POSIX 単一引用符(`'…'`、内部の `'` は `'\''`)を使う。bash / zsh、
  およびアポストロフィを含まない値なら PowerShell もこれで通る。
  `cmd.exe` は二重引用符を要求するのでカバーしていない — ヒントは助言であって、
  OS からシェルを推測するのは当たるのと同じくらい外れる

### 終了コード

issue #49 で rg 式の 3 値契約になった(正本は docs/cli.md §6)。タグ層が無い索引に
対しては **2**(使えないセットアップであって「マッチなし」ではない)。選択に一致する
ファイルが 0 件なら **1**(正常実行で空の答え)。それ以外は 0。タグの解析エラー等は
`error:` を出して **2**。

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

CLI は該当する軸に
`(a file can carry several path: values, so these shares … sum past 100%)`
を 1 行足す。`covers 100%` の下にシェア合計 232% が並ぶのは矛盾ではないが、
何も書かなければ読み手の推測は「バグ」か「自分の読み違い」の 2 つしかない。

## 2-1. 「次に取るべき 1 手」は軸の順位とは別物

**軸の順位は「どの問いを読むか」であって「どの答えを選ぶか」ではない。**
値は count 降順で並ぶので、最良軸の先頭行は**最大のバケツ = 最も絞れない値**になる。

実測(`/usr`)でこれは本物の欠陥だった。先頭行を素直に辿ると

```
63,901 → 21,006 → 21,002 → 21,000 → …   (17 件に落ちるまで 11 手)
```

`path:go1` は集合の 99.98% が持つ — universal(厳密に 100%)ではないので除外を
すり抜け、毎回提示され続ける。

そこで `BrowseView::recommended` を別に計算する。軸のスコアと同じ枠組みで、
**結果が最も予測しづらい 1 手**、すなわち「自分のファイルはこのバケツにあるか?」の
二値エントロピー `H(p, 1−p)` が最大になる値 = シェアが 1/2 に最も近い値を採る。

- 候補は**画面に出ている値だけ**(`FacetAxis::values`)。見せていない値を
  推薦するのは、悪い一手を推薦するより悪い罠になる
- 同点(`p` と `1−p` は同じエントロピー)は**小さいバケツ**を採る。
  同じだけ情報を得られるなら、読む量が少なくなる側が良い一手
- そこも同点なら名前空間名 → タグ名。ここは絶対に同点にならない

**コア側に置いた**のは、M4 の UI が同じ推薦を使えるようにするため。
UI が自前でボタンを並べて先頭を勧めれば、上の「1 手で 4 件しか進まない」挙動を
そのまま再現する。CLI はこの値を `next :` 行に印字するだけ。

実測の改善は §7-4。

## 3. 決定性

design.md §6 が名指しで要求している「再索引で階層が壊れない」を 2 段で担保する。

1. タグ層自体が純関数(design.md §6-1、`crate::tags`)
2. この層の順序付けはすべて**全順序**

スコアは浮動小数点なので、比較前に `rank_key` で **1e-6 グリッドに丸める**。
同点はそこから整数(カバレッジ → 値の数)、最後に名前空間名/タグ名に落ちる —
これは絶対に同点にならない。

丸めを先に置いたのは、`log2` / `ln` の最終ビットがプラットフォームの libm で
1 ulp 違いうるため。テスト `the_rank_key_turns_a_last_bit_difference_into_a_tie`。

**ただしこれは「露出を抑える」であって「同一性の証明」ではない。**
真のスコアがグリッド境界をまたいでいれば、1 ulp の差でも丸め先が分かれて
順序が入れ替わりうる。丸めが買っているのは
「スコアが境界の 1e-6 以内にあるときだけ問題になる」であって、
「1 ulp 以内にあるときは常に問題になる」からの改善である。
**同一マシン・同一データベースでの決定性は厳密**で、CLI とテストが主張しているのは
そちらだけ。

軸の中の値の順は `count desc, tag asc` で、これは `tagindex::tag_counts`
(= `sagasu tags`)と同じ規約。同じものを見せる 2 つのコマンドが違う順序で
並べることがないようにしている。

**プレビュー行の並びだけは別の話。** `files_in_selection` は `file_id` 順で返し、
`file_id` は並列クロールが採番するので、**同じ木を独立に 2 回索引すれば番号が変わりうる**
(既存 DB への再クロールは file_id を保つので、そちらは安定)。
プレビューはランキングではなくページである。テストの `render()` はこの区別を確かめる
ために preview の `file_id` を含めており、`re_crawling_and_re_tagging_an_unchanged_tree_…`
が同一 DB への再クロールで不変であることを固定している。

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
    pub recommended: Option<NextStep>, // 次に取るべき 1 手(§2-1)
    pub preview: Vec<FileRow>,     // 実在確認は呼び出し側の責務。file_id 順 = ページ
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

計測日 2026-08-07、Linux / ext4、release ビルド。
`--no-fresh -n 0` はデータベースの作業と軸の提示だけを見るため。

**転記の規律**: §7-1 の各ブロックは、先頭の鮮度ブロック
(`tags :` / `snapshot:` / `delta :` とその後の空行)**だけ**を全ブロックから
落としている。全ステップで同一の 3 行で、内容は §5 で扱っているため。
それ以外は 1 行も落としていない。§7-2 は抜粋で、落とした箇所には `…` を置く。

参考までに、落としている 3 行は全ブロックでこれである:

```
tags    : 555664 rows over 63901 files, 4424 distinct, built at scan generation 1
snapshot: tags describe the corpus as of that scan. Files created or renamed since carry no tags and are not merged in here the way `sagasu find` merges them (issue #5); files deleted since are dropped from a listing by an existence check, and reported.
delta   : (not probed — --no-fresh)
```

```
$ sagasu index /usr --db usr.db      # 63,901 files, 3.4s
$ sagasu tag --db usr.db             # 555,664 rows, 4,424 distinct
```

`sagasu tag` の所要は**ページキャッシュ次第で 5.6s〜27s**。初回は `magic` 列が
空なので 63,784 ファイルの先頭 512 バイトを読みに行く。同一マシンで
コールドキャッシュ 26.9s、ウォーム 5.6s、`magic` が既に入った 2 回目 3.3s。
1 つの数字を引用すると環境の違いが実装の違いに見えるので、3 つとも出す。

### 7-1. `/usr` — 人が「覚えていること」から辿る 4 ステップ

問い: **「Go の標準ライブラリのどこかに、zip のテストデータとして使われている
小さな PNG があったはず。場所は覚えていない。」**

覚えているのは「画像」「PNG」「テストデータ」の 3 つだけ。

```
$ sagasu browse --db usr.db --no-fresh -n 0

select  : (whole index — no tag chosen yet)
matched : 63901 of 63901 live files (100%)
          (an indexed count — an upper bound; only the previewed rows below are checked against the filesystem)
label   : format:text (48357/63901)  kind:code (33390/63901)  path:local (28692/63901)  path:go1 (28667/63901)  path:src (21721/63901)
          (c-TF-IDF over 4424 candidate tag(s): share of this group × ln(1 + live files / files carrying the tag))
axes    : 4 of 8 shown, ranked by expected bits over the top 8 value(s)
          (4 more axis/axes could narrow this group — raise --axes)

  ext     : 2.73 bits, 466 value(s), covers 93% of the group
        21006  ext:go                           (33% of the group)
         5615  ext:svg                          (9% of the group)
         4413  ext:h                            (7% of the group)
         3769  ext:py                           (6% of the group)
         2943  ext:txt                          (5% of the group)
         1986  ext:rst                          (3% of the group)
         1671  ext:vim                          (3% of the group)
         1256  ext:s                            (2% of the group)
              (8 of 466 values shown — 16666 file-tag(s) behind the rest; raise --values)

  path    : 2.13 bits, 3841 value(s), covers 100% of the group
        28692  path:local                       (45% of the group)
        28667  path:go1                         (45% of the group)
        21721  path:src                         (34% of the group)
        20664  path:share                       (32% of the group)
        14492  path:go1.25.1                    (23% of the group)
        14175  path:go1.24.7                    (22% of the group)
        11375  path:lib                         (18% of the group)
         8360  path:cmd                         (13% of the group)
              (8 of 3841 values shown — 221554 file-tag(s) behind the rest; raise --values)
              (a file can carry several path: values, so these shares count some files more than once and sum past 100%)

  kind    : 2.07 bits, 12 value(s), covers 96% of the group
        33390  kind:code                        (52% of the group)
        14334  kind:text                        (22% of the group)
         6203  kind:image                       (10% of the group)   ← ①
         2902  kind:executable                  (5% of the group)
         2132  kind:data                        (3% of the group)
         1483  kind:archive                     (2% of the group)
          486  kind:config                      (1% of the group)
           55  kind:font                        (<1% of the group)
              (8 of 12 values shown — 91 file-tag(s) behind the rest; raise --values)

  format  : 1.31 bits, 18 value(s), covers 100% of the group
        48357  format:text                      (76% of the group)
         7616  format:xml                       (12% of the group)
         2995  format:binary                    (5% of the group)
         2750  format:elf                       (4% of the group)
         1107  format:gzip                      (2% of the group)
          449  format:png                       (1% of the group)
          257  format:zip                       (<1% of the group)
           83  format:gif                       (<1% of the group)
              (8 of 18 values shown — 234 file-tag(s) behind the rest; raise --values)

files   : (not listed — pass -n/--files N)

next    : sagasu browse --db usr.db --files 0 --no-fresh kind:code
          (1.00 bits — the step whose outcome is least predictable, leaving 33390 of 63901 file(s))
```

`next :` は「画像を探している」という**こちらの事前知識を知らない**ので、
集合を最もよく二分する `kind:code` を勧める(§2-1)。人はそれを無視して
`kind:image` を採る。それが 2 つの経路が別物である理由でもある。

```
$ sagasu browse --db usr.db --no-fresh -n 0 kind:image        # ① 6,203 件

select  : kind:image
matched : 6203 of 63901 live files (10%)
          (an indexed count — an upper bound; only the previewed rows below are checked against the filesystem)
label   : path:icons (5804/6203)  ext:svg (5615/6203)  format:xml (5473/6203)  path:status (3222/6203)  path:share (6020/6203)
          (c-TF-IDF over 112 candidate tag(s): share of this group × ln(1 + live files / files carrying the tag))
axes    : 4 of 4 shown, ranked by expected bits over the top 8 value(s)
          (1 further namespace(s) are present in this group but cannot narrow it — every file shares the same value, or the only values left are the ones already selected)

  path    : 2.89 bits, 97 value(s), covers 100% of the group
         6020  path:share                       (97% of the group)
         5804  path:icons                       (94% of the group)
         3222  path:status                      (52% of the group)
         2589  path:mono                        (42% of the group)
         2589  path:ubuntu                      (42% of the group)
         2264  path:humanity                    (36% of the group)
         1564  path:dark                        (25% of the group)
         1427  path:light                       (23% of the group)
              (8 of 97 values shown — 9645 file-tag(s) behind the rest; raise --values)
              (a file can carry several path: values, so these shares count some files more than once and sum past 100%)

  format  : 0.70 bits, 6 value(s), covers 100% of the group
         5473  format:xml                       (88% of the group)
          449  format:png                       (7% of the group)   ← ②
          144  format:text                      (2% of the group)
           83  format:gif                       (1% of the group)
           52  format:jpg                       (1% of the group)
            2  format:binary                    (<1% of the group)

  ext     : 0.55 bits, 7 value(s), covers 100% of the group
         5615  ext:svg                          (91% of the group)
          448  ext:png                          (7% of the group)
           83  ext:gif                          (1% of the group)
           52  ext:jpg                          (1% of the group)
            2  ext:ico                          (<1% of the group)
            2  ext:raw                          (<1% of the group)
            1  ext:in                           (<1% of the group)

  version : 0.01 bits, 2 value(s), covers <1% of the group
            2  version:draft                    (<1% of the group)
            1  version:old                      (<1% of the group)

files   : (not listed — pass -n/--files N)

next    : sagasu browse --db usr.db --files 0 --no-fresh kind:image path:status
          (1.00 bits — the step whose outcome is least predictable, leaving 3222 of 6203 file(s))
```

```
$ sagasu browse --db usr.db --no-fresh -n 0 kind:image format:png   # ② 449 件

select  : kind:image AND format:png
matched : 449 of 63901 live files (1%)
          (an indexed count — an upper bound; only the previewed rows below are checked against the filesystem)
label   : ext:png (448/449)  path:hicolor (149/449)  path:icons (268/449)  path:mimetypes (107/449)  path:png (96/449)
          (c-TF-IDF over 72 candidate tag(s): share of this group × ln(1 + live files / files carrying the tag))
axes    : 2 of 2 shown, ranked by expected bits over the top 8 value(s)
          (2 further namespace(s) are present in this group but cannot narrow it — every file shares the same value, or the only values left are the ones already selected)

  path    : 2.49 bits, 70 value(s), covers 100% of the group
          335  path:share                       (75% of the group)
          268  path:icons                       (60% of the group)
          149  path:hicolor                     (33% of the group)
          112  path:go1                         (25% of the group)
          112  path:local                       (25% of the group)
          110  path:src                         (24% of the group)
          110  path:testdata                    (24% of the group)   ← ③
          108  path:image                       (24% of the group)
              (8 of 70 values shown — 1176 file-tag(s) behind the rest; raise --values)
              (a file can carry several path: values, so these shares count some files more than once and sum past 100%)

  ext     : 0.02 bits, 2 value(s), covers 100% of the group
          448  ext:png                          (100% of the group)
            1  ext:in                           (<1% of the group)

files   : (not listed — pass -n/--files N)

next    : sagasu browse --db usr.db --files 0 --no-fresh kind:image format:png path:icons
          (0.97 bits — the step whose outcome is least predictable, leaving 268 of 449 file(s))
```

```
$ sagasu browse --db usr.db --no-fresh -n 0 kind:image format:png path:testdata   # ③ 110 件

select  : kind:image AND format:png AND path:testdata
matched : 110 of 63901 live files (<1%)
          (an indexed count — an upper bound; only the previewed rows below are checked against the filesystem)
label   : path:png (96/110)  ext:png (110/110)  path:image (108/110)  path:pngsuite (70/110)  path:src (110/110)
          (c-TF-IDF over 11 candidate tag(s): share of this group × ln(1 + live files / files carrying the tag))
shared  : all 110 file(s) in this group carry ext:png, path:go1, path:local, path:src — none of these narrows it
axes    : 1 of 1 shown, ranked by expected bits over the top 8 value(s)
          (3 further namespace(s) are present in this group but cannot narrow it — every file shares the same value, or the only values left are the ones already selected)

  path    : 2.34 bits, 7 value(s), covers 100% of the group
          108  path:image                       (98% of the group)
           96  path:png                         (87% of the group)
           70  path:pngsuite                    (64% of the group)
           55  path:go1.24.7                    (50% of the group)
           55  path:go1.25.1                    (50% of the group)
            2  path:archive                     (2% of the group)
            2  path:zip                         (2% of the group)   ← ④
              (a file can carry several path: values, so these shares count some files more than once and sum past 100%)

files   : (not listed — pass -n/--files N)

next    : sagasu browse --db usr.db --files 0 --no-fresh kind:image format:png path:testdata path:go1.24.7
          (1.00 bits — the step whose outcome is least predictable, leaving 55 of 110 file(s))
```

```
$ sagasu browse --db usr.db --no-fresh kind:image format:png path:testdata path:zip   # ④ 2 件

select  : kind:image AND format:png AND path:testdata AND path:zip
matched : 2 of 63901 live files (<1%)
          (an indexed count — an upper bound; only the previewed rows below are checked against the filesystem)
label   : path:archive (2/2)  ext:png (2/2)  path:src (2/2)  path:go1 (2/2)  path:local (2/2)
          (c-TF-IDF over 7 candidate tag(s): share of this group × ln(1 + live files / files carrying the tag))
shared  : all 2 file(s) in this group carry ext:png, path:archive, path:go1, path:local, path:src — none of these narrows it
axes    : 1 of 1 shown, ranked by expected bits over the top 8 value(s)
          (3 further namespace(s) are present in this group but cannot narrow it — every file shares the same value, or the only values left are the ones already selected)

  path    : 1.00 bits, 2 value(s), covers 100% of the group
            1  path:go1.24.7                    (50% of the group)
            1  path:go1.25.1                    (50% of the group)

files   : 2 of 2 shown
   10874  /usr/local/go1.25.1/src/archive/zip/testdata/gophercolor16x16.png
   12184  /usr/local/go1.24.7/src/archive/zip/testdata/gophercolor16x16.png

next    : sagasu browse --db usr.db --no-fresh kind:image format:png path:testdata path:zip path:go1.24.7
          (1.00 bits — the step whose outcome is least predictable, leaving 1 of 2 file(s))
```

**左端の `file_id`(`10874` / `12184`)はこのデータベース固有の値**で、
再現しようとしても一致しない。`file_id` は並列クロールが採番するため、
同じ `/usr` をもう一度索引すれば別の番号が付く(実測: 同一ツリーの 2 本目の
索引では `30938` / `55720`)。**パスの方が答え**であり、番号はページの識別子に
すぎない — §3 の「プレビュー行の並びだけは別の話」を参照。
「正しい値」に更新するのではなくこの注記を置いたのは、正しい値が存在しないため。

**63,901 → 6,203 → 449 → 110 → 2 の 4 ステップ。**
`shared :` が「この 110 件はすべて `path:go1/local/src` の下」= Go のソースツリーだと
言い、`path:zip` が最後の 1 手として提示されている。
ディレクトリの位置(`src/archive/zip/testdata/`)は一度も知らなくてよい。

### 7-2. sagasu リポジトリ自身(74 ファイル)— 2 ステップ

事前知識ゼロで葉に着く例。以下は該当行の抜粋で、落とした箇所には `…` を置く。

```
$ sagasu browse --db self.db --no-fresh
  path    : 2.96 bits, 29 value(s), covers 95% of the group
  ext     : 2.05 bits, 8 value(s), covers 96% of the group
  kind    : 1.50 bits, 4 value(s), covers 100% of the group
           41  kind:code                        (55% of the group)
           …
  pattern : 0.24 bits, 1 value(s), covers 4% of the group

$ sagasu browse --db self.db --no-fresh kind:code                    # 41 件
label   : ext:rs (40/41)  path:src (29/41)  path:crates (26/41)  path:sagasu (26/41)  path:core (20/41)
shared  : all 41 file(s) in this group carry format:text — none of these narrows it
  path    : 3.00 bits, 22 value(s), covers 100% of the group
           …
            7  path:cli                         (17% of the group)
              (8 of 22 values shown — 40 file-tag(s) behind the rest; raise --values)

$ sagasu browse --db self.db --no-fresh kind:code path:cli           # 7 件 = 葉
label   : path:sagasu-cli (7/7)  path:crates (7/7)  path:sagasu (7/7)  path:src (7/7)  ext:rs (7/7)
shared  : all 7 file(s) in this group carry ext:rs, format:text, path:crates,
          path:sagasu, path:sagasu-cli, path:src — none of these narrows it
axes    : 0 of 0 shown, ranked by expected bits over the top 8 value(s)
          (4 further namespace(s) are present in this group but cannot narrow it — …)
          (nothing left to drill into — this group is a leaf)
files   : 7 of 7 shown
   …
next    : (nothing to add — this group is a leaf; list it with `sagasu tags --db self.db kind:code path:cli`)
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

### 7-4. `next :` だけを追った場合(事前知識ゼロ)

§7-1 は人の記憶を使う経路なので、推薦ロジックそのものは検証していない。
そこで `/usr` で `next :` が言う通りにだけ進む経路も計測した(既定フラグ)。

| 手 | 集合 | 取った 1 手 | 削減 |
|---|---|---|---|
| 0 | 63,901 | `kind:code` | 47.7% |
| 1 | 33,390 | `path:src` | 55.4% |
| 2 | 14,900 | `path:go1.25.1` | 49.3% |
| 3 | 7,556 | `path:internal` | 56.6% |
| 4 | 3,283 | `path:cmd` | 44.3% |
| 5 | 1,828 | `path:compile` | 63.8% |
| 6 | 662 | `path:ssa` | 77.6% |
| 7 | 148 | `path:_gen` | 87.8% |
| 8 | 18 | `ext:bash` | 94.4% |
| 9 | 1 | (葉) | — |

**9 手で 63,901 → 1**、毎手 44% 以上を落とす。
軸の先頭行を採っていた旧実装は同じコーパスで
`63,901 → 21,006 → 21,002 → 21,000 → …`(11 手で 17 件)だった。
テスト `every_recommended_step_removes_at_least_a_fifth_of_the_group` が
「1 手あたり 20% 以上」を下限として固定している
(「狭まる」だけでは、上の 4 件ずつ進む挙動を通してしまう)。

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
- **ラベルは同一軸の排他値を同時に並べうる。** `path:renewal` と `path:photos` が
  1 つのラベルに並ぶことがある。c-TF-IDF は語ごとに独立にスコアするので
  「この集合の 6 割は renewal、4 割は photos」を「renewal かつ photos」とは
  区別しない。**直していない**: 排他性を判定するには
  「同じ名前空間の値の和が集合を超えるか」を見る必要があり、`path:` はそもそも
  多値なのでその判定自体が成り立たない。ラベルは各語に `(6/10)` を添えて出すので、
  100% でない語が並んでいれば「これは AND ではない」は読み取れる — が、
  暗黙の読解に頼っている
- **`matched` は上限値のまま。** 実在確認はプレビュー行だけ。
  集合全体の確認はコーパス規模のコストになるのでやらない(`sagasu tags` と同じ線)
- **差分マージはしない**(§5)。増分タグ更新が入るまでの仕様
- **集合評価が毎回全走査**(§7-3)。索引化は別 issue
- **`next :` のクォートは POSIX シェル前提**(§1)。`cmd.exe` では貼り直せない
- **推薦は貪欲で、1 手先しか見ない。** 「2 手で最短」になる経路は探索しない。
  `next :` を追った実測は §7-4(9 手)だが、これが最小手数だとは主張していない
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
