# sagasu CLI インターフェース設計 (issue #6)

design.md §2 の「CLI ファースト」の具体化。サブコマンド体系・引数・出力形式・
設定ファイル・終了コードを 1 箇所に固める。

**この文書の射程**: M0〜M3 で実装済みの 9 サブコマンドの現状を棚卸しし、
(a) 機械可読出力 `--json`、(b) 設定ファイルの 1 本化、の 2 点を決める。
**両方とも実装済み**(PR #48 / #50、issue #6 はクローズ)。
**実装済みでない項目は §9 に隔離して「設計のみ」と明記する** — 設計書が実装を
先取りしていること自体は正しいが、どちらなのか読者に分からない状態は事故のもと。
§9-1 の `--check-journal` は **issue #60 で実装済み**で、§9 に残る未実装は §9-2
(色付き出力、`RIPGREP_CONFIG_PATH` 相当)のみ。

---

## 1. 原則: rg の UX 慣習をどこまで踏襲するか

rg (ripgrep) は「パイプで繋がる検索コマンド」の事実上の基準で、design.md §3 も
`CLI (rg 風 UX)` と書いている。ただし rg は**行を返す grep** で、sagasu は
**ファイルを返す索引**なので、慣習は形ではなく理由で選ぶ。

| rg の慣習 | sagasu | 理由 |
|---|---|---|
| 人間向けと機械向けを別フラグで出し分ける (`--json`) | **踏襲する** | §4 |
| 機械向けは JSON Lines (1 行 1 イベント・型フィールド付き) | **踏襲する** | §4-1 |
| stdout は結果だけ、診断は stderr | **踏襲する** | §3 |
| 終了コードで「見つからなかった」と「失敗した」を区別 | **踏襲する**(0/1/2 の 3 値) | §6 |
| 設定ファイルは 1 本 (`RIPGREP_CONFIG_PATH`) | **踏襲する**(`sagasu.toml`) | §5 |
| 祖先ディレクトリを遡って設定を探す | **採らない** | §5-2 |
| `--files` (マッチせずファイル一覧) | 採らない(名前が `browse --files` と衝突) | §7 |
| 色付き出力 / `--color` | 未実装。今回も入れない | §7 |

**バイナリ名は `sagasu` のまま。** issue #6 の論点だったが短縮形は採らない:
2〜3 文字のコマンド名は衝突しやすく(`sg` は ast-grep が使っている)、短くしたい
利用者はシェルの alias で足りる。`[[bin]] name = "sagasu"` を変えない。

---

## 2. サブコマンド体系

9 個。パイプラインの並びで書く。

```
sagasu index <ROOT>     メタデータ走査 → SQLite 索引         (書き)
sagasu hash             BLAKE3 ハッシュのバックフィル         (書き)
sagasu fulltext         本文抽出 → tantivy 全文索引           (書き)
sagasu tag              ルールベースのタグ層生成              (書き)

sagasu search <QUERY>   全文検索(スコア順・スニペット付き)     (読み)
sagasu find <QUERY>     パス部分一致検索                      (読み)
sagasu tags [TAG...]    タグの一覧・タグでの絞り込み・説明      (読み)
sagasu browse [TAG...]  ファセットドリルダウン                (読み)
sagasu status           索引の鮮度・規模のレポート             (読み)
```

依存の向きは `index → {hash, fulltext, tag}` で、`fulltext` と `tag` は
ファイルシステムを再走査せず索引の行を読む。読み側は全部 `--db` から入る。

### 2-1. 共通引数

| フラグ | 対象 | 既定 |
|---|---|---|
| `--db <PATH>` | 全 9 個 | `index.db` |
| `--json` | 全 9 個 (§4 で新設) | off |
| `--config <PATH>` | `fulltext` / `search` / `tag` / `tags` (§5 で新設) | `./sagasu.toml`(あれば) |
| `--no-fresh` | `search` / `find` / `tags` / `browse` | off (= 鮮度マージ・プローブ有効) |
| `--delta-limit <N>` | `--no-fresh` と同じ 4 個 | `delta::DEFAULT_DELTA_LIMIT` |
| `-n, --limit <N>` | `search`(10) / `find`(20) / `tags`(20) | 下記 |
| `-n, --files <N>` | `browse`(プレビュー件数) | `browse::DEFAULT_PREVIEW` |

`-n` の既定値がコマンドごとに違うのは意図的で、`search` は 1 画面に収める前提、
`find` / `tags` は列挙用途、という使われ方の差。**意味は 4 つとも「stdout に出す
結果行の上限」で揃っている**(`browse -n` だけフラグ名が `--files` なのは、
browse の行が「結果」ではなく「グループの中身のプレビュー」だから)。

### 2-2. 各サブコマンド固有の引数(現状の記録)

`index`: `--exclude`(repeatable) / `--no-default-excludes` / `--skip-hidden` /
`--use-gitignore` / `--threads`
`hash`: `--max-size`(既定 4 MiB)
`fulltext`: `--index-dir` / `--max-size`(既定 2 MiB) / `--ext`(repeatable) /
`--no-sniff` / `--threads` / `--heap-mb`
`search`: `--index-dir` / `--no-db` / `--snippet-chars` / `--ext`
`tag`: `--no-read-magic` / `--magic-max-size` / `--no-read-embedded` /
`--embedded-max-size`
`tags`: `--file <PATH>`(1 ファイルの説明モード)
`browse`: `--axes` / `--values` / `--label-terms`
`status`: `--check-journal`(既定 off) / `--journal-warn-hours <N>`(既定 24。`--check-journal`
と併用時のみ意味がある)

---

## 3. 出力の 2 系統と共通規約

現状の人間向け出力は「左に固定幅のラベル、右に値」の 1 行 1 事実。これは維持する。

**stdout と stderr の役割は既に分かれており、これも維持する**:

- **stdout** = レポートと結果。`grep` / `awk` に食わせられる形。
- **stderr** = `WARNING:` と `error:`。**索引が古い・0 件だった・除外規則が
  読めない、といった「答えが不完全かもしれない」通知は全部こちら。**
  design.md §5 の設計全体が「stale を黙らせない」ことに乗っているので、
  この分離は装飾ではなく仕様。

`--json` を付けても**この分離は変わらない**。stderr の文言はそのまま人間向けに
残り、加えて同じ内容が JSON 側にも乗る(§4-2)。片方を読めば済むようにするのが
目的で、「stderr をパースさせる」も「警告を JSON に移して stderr を空にする」も
採らない。

---

## 4. `--json` — 機械可読出力

**対象読者はシェルスクリプト・他ツール・エージェント。** M4 の Tauri UI は
対象外で、従来どおり `sagasu-core` に直接リンクする(§8)。

### 4-1. 形: 2 系統

rg の前例に従い、**出力の性質で分ける**。

| 系統 | 形 | 対象 |
|---|---|---|
| 結果ストリーム系 | **JSON Lines**(1 行 1 イベント、`type` フィールド必須) | `search` `find` `tags` `browse` |
| サマリ系 | **単一 JSON オブジェクト**(1 回の実行結果) | `index` `hash` `fulltext` `tag` `status` |

分ける理由は、前者が「N 件の結果 + それに付随するメタ情報」で件数が先験的に
決まらないのに対し、後者は「1 回の実行の総括」で本質的に 1 オブジェクトだから。
ストリーム系を 1 つの巨大オブジェクトにすると、消費側は最後の `}` まで読まないと
1 件目を処理できない。

**イベントの順序は人間向け出力と同じ。** これは規約であってテスト可能な性質でもある
(§10 の Smoke が実際にこれを突く)。

### 4-2. 全系統に共通する規約

1. **人間向け出力に出る数値・文言は、すべて JSON 側にも乗る。** 「`--json` を
   使うと情報が減る」は禁止。件数・比率・タイミング・警告のいずれも欠かさない。
   逆向き(JSON にだけある)は許す — 例えば `mtime_ns` の生値。
2. **警告は二重化する。** stderr には従来どおり `WARNING: ...` を出し、同じ
   メッセージを JSON にも入れる。ストリーム系は `{"type":"warning","message":...}`
   行、サマリ系は `"warnings": ["...", ...]` 配列。
3. **エラーは JSON にしない。** 失敗時は従来どおり stderr に `error: ...` を出して
   終了コード 2 (§6)。JSON ストリームは途中で終わる。理由: 失敗はほぼ全部 `anyhow` の
   文脈付きチェーンで、これを構造化すると「エラー型の設計」という別の仕事になる。
   消費側は**終了コードで判定する**(JSON の有無で判定しない)。
4. **1 行 = 1 JSON 値、UTF-8、ANSI エスケープなし、改行区切り。** 整形はしない
   (`jq` に通す前提)。日本語はエスケープせずそのまま出す(`serde_json` の既定)。
5. **`--json` は他のフラグの意味を変えない。** `-n` の効き方も `--no-fresh` の
   効き方も同じ。終了コードも同じ(§6)。

### 4-3. スキーマの安定性の約束 (v0)

PoC 段階なので**破壊的変更はありうる**。そのうえで最低限を約束する:

- ストリーム系の 1 行目は必ず `{"type":"meta","schema":"v0","command":"<name>", ...}`。
  サマリ系のオブジェクトは必ず `"schema":"v0"` と `"command":"<name>"` を持つ。
- **`schema` の値が変わらない限り、フィールドの削除・改名・意味の変更はしない。**
  追加はいつでもする。消費側は**未知のフィールドと未知の `type` を無視する**こと。
- 削除・改名が必要になったら `schema` を `"v1"` に上げ、PR 本文と CHANGELOG に
  変更点を列挙する。黙って消さない。
- 数値は JSON の number。バイト数・件数は整数、ミリ秒・比率は浮動小数。
  **ファイルサイズや USN のような 64bit 値は number のまま出す**(JS の
  `Number.MAX_SAFE_INTEGER` = 2^53 を超えるのは USN くらいで、そこは文字列にする)。
- `null` は「値がない」を意味し、フィールドごと省略はしない(消費側が
  `has key` で分岐しなくて済む)。

### 4-4. ストリーム系のイベント語彙

型は全コマンドで共有する。同じ `type` は同じ意味。

| `type` | 出るコマンド | 主なフィールド |
|---|---|---|
| `meta` | 全ストリーム系 | `schema` `command` + コマンド固有(`query` `db` `fresh` など) |
| `summary` | `search` `find` `tags` | `hits` `total`(あれば) `live_hits` `index_hits` |
| `delta` | `search` `find` `tags` `browse` | `entries` `source` `cached` `scanned` `excluded` `status` `rescan_reason` `errors` `detects_renames` |
| `timing` | `search` `find` | `setup_ms` `index_ms` `delta_ms` `live_ms` `merge_ms` `overhead_ms` `total_ms` |
| `merge` | `search` `find` | `index_candidates` `dropped_changed` `dropped_deleted` |
| `hit` | `search` `find` | `origin` `file_id` `path` `score` `size` `mtime_ns` `snippet` |
| `file` | `tags` `browse` | `file_id` `path` `exists` |
| `namespace` | `tags` | `namespace` `files` `distinct` |
| `tag_count` | `tags` | `tag` `files` |
| `tag_layer` | `tags` `browse` | `built` `rows` `files` `distinct` `generation` `scan_generation` `behind` `rules` |
| `view` | `browse` | `selected` `matched` `corpus` `share` `label` `label_vocabulary` `universal` `axes_total` `axes_refining` |
| `axis` | `browse` | `namespace` `score` `coverage` `files` `distinct` `tail_assignments` `multi_valued` `values[]` |
| `next` | `browse` | `command` `tag` `bits` `files` / または `reason` |
| `warning` | 全ストリーム系 | `message` |

`axis` の値リストだけは**入れ子の配列**にする(1 行 1 イベントの例外)。
値は軸に属していて、平坦化すると消費側が親子を組み直すことになるため。

### 4-5. 具体例

`sagasu search "needle" --json`:

```json
{"type":"meta","schema":"v0","command":"search","query":"needle","db":"index.db","index_dir":"fulltext-index","fresh":true,"text_policy":"+text obj (sagasu.toml)"}
{"type":"summary","hits":3,"live_hits":1,"index_hits":2,"total_docs":1234}
{"type":"delta","entries":20,"source":"mtime","cached":false,"scanned":63901,"excluded":900,"status":"complete","rescan_reason":null,"errors":0,"detects_renames":true}
{"type":"timing","setup_ms":4.1,"index_ms":2.2,"delta_ms":32.5,"live_ms":0.7,"merge_ms":0.15,"overhead_ms":33.35,"total_ms":35.55}
{"type":"merge","index_candidates":10,"dropped_changed":1,"dropped_deleted":0}
{"type":"hit","origin":"live","file_id":null,"path":"/home/u/notes/新しい原稿.md","score":3.0,"size":812,"mtime_ns":1754500000000000000,"snippet":"…needle…"}
{"type":"hit","origin":"index","file_id":4211,"path":"/home/u/notes/old.md","score":1.234,"size":null,"mtime_ns":null,"snippet":"…needle…"}
{"type":"warning","message":"index is stale: more than 10000 files changed …"}
```

`sagasu status --json`(サマリ系。以下は読みやすさのため整形してあるが、
**実出力は §4-1 の規約どおり 1 行の compact 形式**):

```json
{
  "schema": "v0",
  "command": "status",
  "root_path": "/home/u",
  "schema_version": 0,
  "scan_marker_age_secs": 3612.4,
  "delta_marker": {"kind": "mtime"},
  "exclusion": {"state": "present", "names": 12, "hidden": "…", "gitignore": {"applied": false, "rules": 0, "digest": null}},
  "unreadable": 0,
  "scan_generation": 3,
  "live_files": 63901,
  "tombstones": 12,
  "null_hashes": 0,
  "fulltext": {"built": true, "dir": "fulltext-index", "documents": 55914, "scan_generation": 3, "behind": 0},
  "tags": {"built": true, "rows": 555664, "files": 63901, "distinct": 20412, "scan_generation": 3, "behind": 0, "rules": "sagasu.toml"},
  "journal": {"checked": false, "reason": "not requested (--check-journal)"},
  "warnings": []
}
```

`delta_marker` は USN のとき
`{"kind":"usn","volume":"C:","journal_id":"…","next_usn":"…","maximum_size":33554432}`。
**`journal_id` と `next_usn` は文字列**(§4-3 の 2^53 の理由)。

`--check-journal` を付けたときは `journal` が次の形になる(詳細は §9-1):

```json
"journal": {"checked": true, "next_usn": "1247000012345678", "consumed_bytes": 12897845,
            "rate_bytes_per_sec": 232.7, "elapsed_secs": 55440, "remaining_secs": 88560,
            "expired": false, "live_maximum_size": 33554432,
            "journal_matches": true, "rolled_off": false}
```

`next_usn` は**文字列**(2^53 の理由)。`remaining_secs` はレートが未観測なら `null`。
`elapsed_secs` は人間向けの「over N h」に対応する。`journal_matches` / `rolled_off`
は §4-3 の「追加はいつでも可」に従って追加した**判定フィールド**で、`checked: true` のときだけ現れる。

### 4-6. 実装方針

- `serde_json` を workspace 依存に追加し、CLI 側に `#[derive(Serialize)]` の
  DTO を置く。**コアの型に `Serialize` を生やさない** — コアの型は M4 が直接
  触る内部インターフェースで、JSON の都合(フィールド名・64bit の文字列化)を
  そこに持ち込むと 2 つの契約が 1 つの型に同居する。CLI 側で詰め替える。
- 詰め替えは `crates/sagasu-cli/src/json.rs` に集約する。既に `output.rs` が
  「複数コマンドが共有する印字」の置き場なので、その JSON 版。
- 人間向けの印字関数と JSON の出力関数は**同じ値から分岐する**。
  「人間向けだけ後から数字が増えて JSON が置いていかれる」を構造で防ぐ。

### 4-7. `fulltext` の索引健全性フィールド (issue #52)

Lindera は `\n` `\t` `。` `、` でしか文を切らない。**空白は区切りではない**ので、
この 4 文字が 1 つも無い長文は本文全体が 1 つの Viterbi 格子になり、およそ
13.5 万ノードで経路コストが `i32::MAX` に飽和して**残り全部が 1 個の巨大トークン**
として返る(lindera/lindera#871)。tantivy は `MAX_TOKEN_LEN`(65,530 バイト)超の
トークンを `warn!` だけ出して捨てるので、**文書の尾部がフレーズも単語も丸ごと
索引から消える**。

`sagasu fulltext` は索引投入の直前に、区切りの無い連続を 32 KiB 以下に保つよう
`\n` を入れる。挿入は原則として既存の空白 1 文字の置換なので、長さも語も変わらない。
32 KiB という値は「`MAX_TOKEN_LEN` より十分小さい」と「飽和の規模より十分小さい」の
両方を満たす点で、緩めても得は無い。

この書き換えは**索引されるテキストがファイルの中身と一字一句同じでなくなる唯一の
箇所**なので、黙って行わず必ず報告する。人間向け出力の該当行と JSON のフィールドは
対応する:

| 人間向け | JSON | 意味 |
|---|---|---|
| `long lines : N document(s) split into M segment breaks` | `lattice_split_docs` / `lattice_breaks` | 区切りを挿入した文書数と挿入総数。**skip ではない**(これらの文書は索引されている) |
| その下のパス一覧 | `lattice_split_samples`(`{path, breaks}`) | 先頭 20 件まで |
| `dropped terms: ...` | `dropped_long_tokens` / `longest_token_bytes` | `MAX_TOKEN_LEN` を超えて捨てられたトークン数と、観測した最長トークン長 |

`dropped_long_tokens` は**常に 0 のはず**である。上の 32 KiB 制限がある限り
`MAX_TOKEN_LEN` 超のトークンは構造的に作れない。0 でなければ前提が壊れたという
報告なので、人間向け出力にもその旨を出す。`longest_token_bytes` を併記するのは
「0 件でした」が上限にどれだけ近いところでの 0 件なのか分からないと意味を持たない
ため。

ライブ grep 側(`fresh`)は素の文字列走査なのでこの欠陥の影響を受けない。つまり
緩和前は索引側だけが尾部を失っており、鮮度マージの両側が食い違っていた。緩和は
この乖離を縮める方向にしか働かない。

---

## 5. 設定ファイル: `sagasu.toml` への 1 本化

### 5-1. 決定

旧 `sagasu-tags.toml`(タグルール) と `sagasu-text.toml`(本文抽出の拡張子)を
**単一の `sagasu.toml`** に統合した(実装済み。以下は現行仕様。例は
`docs/examples/sagasu.toml`)。

```toml
# sagasu.toml

[text]
# 拡張子に先頭のドットは付けても付けなくてもよい。大文字小文字は無視。
text_ext   = ["tmpl", "hbs", "j2"]   # 許可リストに足す(拒否リストより強い)
binary_ext = ["dat", "pak"]          # 拒否リストに足す

[[tags.rule]]
name = "顧客案件"
path = "clients/**"
tags = ["project:client-work"]

[[tags.rule]]
file = "*.psd"
tags = ["app:photoshop"]
```

- `[text]` の中身は旧 `sagasu-text.toml` のトップレベルと**同一**
  (`text_ext` / `binary_ext`)。
- `[[tags.rule]]` の中身は旧 `sagasu-tags.toml` の `[[rule]]` と**同一**
  (`name` / `path` / `file` / `ext` / `tags`)。
- **未知のキーはエラー**(`deny_unknown_fields`)。これは旧両ファイルの仕様で、
  統合後も維持している。`text_exts` と打ち間違えた設定が「読めたが何もしない」に
  なると、利用者は sniffer を疑うことになる。
- **セクションはどちらも省略可。** `[text]` だけの `sagasu.toml` も
  `[[tags.rule]]` だけのものも正当。

### 5-2. 探索順

1. `--config <PATH>` が明示されていればそれ。**見つからなければエラー**
   (ユーザーが名指ししたものが無いのは事故)。
2. なければカレントディレクトリの `./sagasu.toml`。**あれば読む、無ければ
   「設定なし」で続行**(設定が無いのは正常な状態であって失敗ではない)。
3. どちらの場合も、**使ったファイル(または使わなかったこと)を 1 行目に出す**。
   これは現行 `sagasu tag` の挙動で、統合後は全 4 コマンドで揃える。

**祖先ディレクトリを遡る探索は採らない。** 理由は 2 つ:

- 「どこから実行したか」で答えが変わる度合いが上がる。design.md §4-2 が
  「自動探索だけに頼っていた版は、別ディレクトリから検索すると空振りして
  組み込みリストに戻り、ライブ grep が索引と違う判定をした」と記録している
  失敗の、範囲を広げた版になる。
- そもそも**判定規則は索引が持っている**(§5-4)。設定ファイルの探索が効くのは
  索引を作る瞬間だけで、遡り探索が救う場面は狭い。

`RIPGREP_CONFIG_PATH` 相当の環境変数も**今は入れない**。必要になったら足す
(足すのは後方互換だが、消すのは破壊的変更)。

### 5-3. 旧 2 ファイルの扱い: 後方互換は持たない、ただし黙らない

**PoC 段階で利用者はいないので `sagasu-tags.toml` / `sagasu-text.toml` は
読まない。** ただし**存在を検出したらエラーで案内する**:

```
error: sagasu-tags.toml is no longer read — the two config files were merged
       into a single sagasu.toml (docs/cli.md §5).
         [[rule]] sections move under [[tags.rule]]
         sagasu-text.toml's text_ext / binary_ext move under [text]
       Remove or rename the old file once you have migrated.
```

「読まない」を黙ってやると、利用者から見えるのは「タグルールが効かなくなった」
だけになる。これは design.md が一貫して禁じている silent omission そのもの。
**`sagasu.toml` が既にある場合でもエラーにする** — 片方だけ移行した中途半端な
状態を「動いているように見える」まま通すほうが危ない。

検出場所は探索場所と同じ(`--config` の隣、またはカレントディレクトリ)。

旧フラグ `--rules` / `--text-config` は `--config` に置き換える。clap の
「unknown argument」ではなく**専用のエラーで `--config` へ誘導する**
(隠しフラグとして受け取り、値の有無にかかわらず案内して終了)。

### 5-4. 索引に永続化済みの規則との関係 — 変えない

design.md §4-2 / §5-1 の「判定規則も索引が持つ」は**そのまま**:

- `sagasu fulltext` は使った `TextPolicy` を `meta.text_policy` に書き、
  `sagasu search` はそこから復元する。
- `sagasu index` は使った `ExcludeSet` を `meta.exclude_policy` に書き、
  検索時の差分照会がそれを再生する。
- `sagasu tag` は使ったルールファイルのパスと digest を記録する。

**設定ファイル → 索引 の向きは現行踏襲。** 変わるのは「どのファイルから
読むか」だけで、読んだ後の流れには一切触らない。`meta` に保存される
`text_policy` の `source=` 行の値が `sagasu-text.toml` から `sagasu.toml` に
変わるだけ(この値は情報提供用で、再読み込みには使われない)。

**索引の再構築は不要**: 旧索引の `text_policy` は依然として復元可能で、
`source=sagasu-text.toml` と記録されているだけ。

---

## 6. 終了コード

rg の 3 値契約を踏襲する (issue #49): **0 = マッチあり、1 = マッチなし、2 = エラー**。
`1` は「コマンドが正しく走り、答えが空だった」に予約する(読みコマンドのみ)。従来
「問題」を意味したものはすべて `2` に移る — `anyhow::Error` も含めて。書き/要約
コマンドはマッチという概念を持たず、0/2 のみ。

| コマンド | 0 | 1 (正しく走り、答えが空) | 2 (エラー / 使えない状態) |
|---|---|---|---|
| `search` | ヒット 1 件以上 | 0 件 (索引は使える) | エラー全般。全文索引がない / 空(`total_docs == 0` — 使えない状態であり、正当な「マッチなし」ではない) |
| `find` | 結果 1 件以上 | 0 件 | エラー全般。メタデータ索引が空(§7 #3 の警告ケース) |
| `tags` | 現在のモード(名前空間一覧 / 値一覧 / ファイル絞り込み)で出力あり | タグ層は構築済みだがクエリが何も返さない(`tags ext:zzz` のような存在しない値、タグの無いファイルへの `tags --file`) | エラー全般。タグ層が未構築 |
| `browse` | ビューができ、選択が 1 ファイル以上にマッチ(無選択のルートビューはコーパス > 0) | 選択にマッチするファイルが 0 | エラー全般。タグ層がない |
| `index` | 索引 1 ファイル以上 | — | エラー全般。索引 0 件(旧 1) |
| `hash` | 成功 | — | エラー全般 |
| `fulltext` | 索引 1 文書以上 | — | エラー全般。0 文書(旧 1) |
| `tag` | 成功かつ live ファイル 1 以上 | — | エラー全般。live 0 件またはタグ付き 0 件(旧 1) |
| `status` | レポート出力 | — | エラー全般 |

- clap の usage エラーは従来どおり 2(clap 既定)。偶然の一致だったのが契約として意味を持つようになった。
- `--json` は終了コードを変えない(§4-2 rule 5)。
- 実装: サブコマンドは `Outcome` を返し、`main.rs` が 0/1/2 へ写像する。散在する `process::exit` はこの混在の原因だったので増やさない。

### 6-1. 旧契約 (issue #49 より前) — スクリプト移行用の記録

| コマンド | 0 以外を返す条件 |
|---|---|
| `index` | 索引 0 件 → 1 |
| `hash` | なし |
| `fulltext` | 索引 0 文書 → 1 |
| `tag` | live ファイル 0 件、またはタグ付き 0 件 → 1 |
| `search` | 全文索引が空(`total_docs == 0`) → 1 |
| `find` | なし |
| `tags` | タグが 1 つも無い(名前空間一覧モード) → 1 |
| `browse` | タグ層が無い → 1 |
| `status` | なし |
| 全部 | 実行エラー(`anyhow::Error`) → 1 |

**移行メモ (スクリプト利用者向け)**: 旧契約の `1` は「エラー」と「使える答えが
ない」の両方を意味していた。新契約では `1` は読みコマンドの「正しく走って答えが
空」に予約され、旧 `1` のほとんど(実行エラー、索引 0 件、タグ層なしなど)は `2`
に移る。`0` 以外を「失敗」とだけ判定していたスクリプトの挙動は変わらないが、
`1` と `2` を区別するスクリプトは意味の移動に注意すること。

### 6-2. 判断: rg の 3 値化 — 実施済み (issue #49)

rg は `0` = マッチあり、`1` = マッチなし、`2` = エラー。sagasu は旧契約で 1 に
「エラー」と「使える答えがない」を同居させていたが、issue #49 で 3 値に揃えた
(本節冒頭の表)。「マッチなし」の定義がコマンドごとに違う(`search` の 0 件は
「索引が空」と「ヒット 0」の 2 つがあり、前者は 2)— その整理が表の行ごとの
条件になっている。

---

## 7. rg 風 UX の棚卸し — 一貫していない点とその処遇

| # | 指摘 | 処遇 |
|---|---|---|
| 1 | `-n` が `search`/`find`/`tags` では `--limit`、`browse` では `--files` | **直さない。** 意味(出力行数の上限)は揃っており、フラグ名の差は「結果」と「プレビュー」の差を反映している。改名は docs/browse.md の到達ログを全部無効にする |
| 2 | `-n` の既定値が 10 / 20 / 20 / 5 とばらばら | **直さない。** §2-1 の通り用途差 |
| 3 | `find` だけ索引が空でも警告も非 0 終了もしない | **修正済み(PR #48)。** `search` と同じ「メタデータ索引が空」警告を追加(終了コードは当時の §6 に従い変えなかった。その後 issue #49 でこの空索引ケースは終了コード 2 に変更 — §6) |
| 4 | `--ext` が `fulltext` と `search` にあり `tag` に無い | **直さない。** `--ext` は本文抽出の判定を触るフラグで、タグ生成は本文を読まない |
| 5 | `--no-fresh` が `status` に無い | **直さない。** `status` は差分プローブをしない(read-only を保つ設計)。USN マーカーの寿命を知りたいときだけ `--check-journal`(§9-1、**実装済み**)で能動的に照会できる(既定 off) |
| 6 | 設定フラグが `--rules` と `--text-config` の 2 つ | **修正済み(PR #48)**(§5-3、`--config` に統合) |
| 7 | rg の `--files` 相当が無い | **入れない。** `sagasu find ""` や `sagasu tags` が同じ用途を埋めており、名前が `browse --files` と衝突する |
| 8 | 色付き出力 / `--color` が無い | **入れない。** 現状 stdout に ANSI は一切出ておらず(依存に termcolor 類なし)、パイプ検出まで含めると独立した仕事。`--json` が先 |
| 9 | `tags` の 3 モード(名前空間一覧 / 値一覧 / ファイル絞り込み)が位置引数の形で暗黙に切り替わる | **直さない。** `sagasu tags` / `sagasu tags ext:` / `sagasu tags ext:png` の階段は rg の `--files`/`-l`/通常と同じ「引数が増えるほど具体的」の形で、`--json` では `type` が違うイベントとして明示的に区別される(§4-4) |
| 10 | `browse` だけ先頭に空行を出す | **修正済み(PR #48)**(整形の一貫性。`--json` では無関係) |

---

## 8. `--json` と M4 Tauri の関係 — 契約は 2 本にならない

PR #41 が `sagasu browse` に `--json` を入れなかった理由は
「M4 はコアに直接リンクするので 2 本目の契約を作らない」だった。
**この判断はそのまま維持する。**

- **M4 Tauri UI は `sagasu-core` に直接リンクする**(design.md §3
  「コアが Rust ならそのまま接続」)。`browse::browse(&Store, &BrowseQuery)
  -> BrowseView` のような**コア API がそのまま M4 の内部インターフェース**で、
  JSON を経由しない。ここは変わらない。
- **`--json` の読者は別**: シェルスクリプト、他ツール、エージェント。
  プロセス境界の向こう側にいて Rust でリンクできない相手のためのもの。
- したがって `--json` は「コア API の JSON 版」ではなく「CLI 出力の機械可読版」。
  **人間向け出力と 1:1**(§4-2 の規約 1)であって、コアの構造体と 1:1 ではない。
  コアの型に `Serialize` を生やさない(§4-6)のはこの線引きの実装上の担保。

一言でいうと: **`--json` が増えても契約は 2 本にならない。CLI 出力という
1 本の契約に、人間向けと機械向けの 2 つの表現があるだけ。**

---

## 9. 設計メモ — 実装済み(§9-1)と持ち越し(§9-2)

### 9-1. `sagasu status --check-journal` — USN マーカー寿命の能動的な警告(実装済み、issue #60)

USN ジャーナルはリングバッファで、実測で数分に約 8 MiB 消費される。マーカーが
ロールオフすると次回検索は `RescanRequired`(安全側)になるが、利用者には予告が
なかった。PR #36 が `delta::estimate_lifetime(marker, next_usn_now, now_ns)
-> Option<MarkerLifetime>` を実装した時点では `sagasu status` からは呼んでおらず、
その理由は **`status` を read-only に保つため** — 現在の NextUsn を得るには
ボリュームハンドルを開いて USN ジャーナルに問い合わせる必要があり、`status` は
「DB を読むだけ」を保っていた。issue #60 でオプトインの照会として実装した。

**現状の挙動**:

- **オプトインのフラグ `--check-journal`。既定は off。**「read-only を保つ」は既定の
  話であって、利用者が明示的に頼んだ照会まで禁じる理由はない。`status` が黙って
  I/O を増やさなければよい。
- **`--journal-warn-hours <N>`**(既定 **24**)。残り時間がこの闇値未満のときの
  警告を制御する。`--check-journal` との併用時のみ意味がある。
- **どちらのフラグも全プラットフォームで受け付ける。** Linux と Windows で
  使い分けるスクリプトが片方だけで clap の usage エラー(終了コード 2)になるのを
  避けるため、フラグ定義を `cfg(windows)` で消さない。非 Windows では
  `--check-journal` は受け付けたうえで「not checked」と理由を報告する。
- off のとき: 従来どおり `delta marker : usn on C:` と保存済みの
  `journal size` / `marker usn` を出す(人間向け出力は従来と同一)。JSON では
  `"journal": {"checked": false, "reason": "not requested (--check-journal)"}`。
- on + 成功: `estimate_lifetime` の結果を出す。

  ```
  delta marker   : usn on C:
    journal size : 32.0 MiB
    marker usn   : 1234567890
    consumed     : 12.3 MiB since the marker (0.8 MiB/h over 15.4h)
    lifetime     : ~24.6h remaining
  ```

  JSON:
  ```json
  "journal": {"checked": true, "next_usn": "1247000012345678", "consumed_bytes": 12897845,
              "rate_bytes_per_sec": 232.7, "elapsed_secs": 55440, "remaining_secs": 88560,
              "expired": false, "live_maximum_size": 33554432,
              "journal_matches": true, "rolled_off": false}
  ```

  `next_usn` は**文字列**(§4-3 の 2^53 の理由)。`remaining_secs` はレートが未観測
  (経過時間ゼロ、またはマーカー以降 0 バイト)なら `null` — 数値を捏造せず、キーを
  省かない。`elapsed_secs` は人間向けの「over N h」に対応する。`journal_matches` /
  `rolled_off` は判定フィールド(§4-5)。
- **警告は 2 条件**、どちらも stderr の `WARNING:` + JSON の `warnings`。1 回の
  実行で出るのは最大 1 つ(死んだマーカーの残り時間警告は rescan 要求に付け足す
  情報がない):
  - マーカーが既にロールオフ(`rolled_off`、またはライブの journal id がマーカーと
    不一致) → 「次回検索は差分を判定できず全再走査を要求する。`sagasu index <root>`
    を実行せよ」
  - `remaining_secs` が `--journal-warn-hours` 未満 → およそ残り何時間かを告げる。

  **`rolled_off` の判定は差分読み取りと同一**、すなわちライブの `FirstUsn` が
  マーカーを追い越したか(＋journal id 不一致)だけで決める。**推定値の `expired` は
  この判定に混ぜない。** `expired` はマーカーに**記録された** `MaximumSize` に対する
  消費量の計算で、NTFS はこの値を上限ではなく目標として扱い、トリムも遅延する
  (issue #37 の実機では 512 KB のジャーナルが約 9 万レコードを一周させずに保持した)。
  混ぜると、**差分読み取りは成功するのに「索引は死んだ、再索引せよ」と言う偽警告**に
  なる。`expired` が立ったが `rolled_off` は立っていない状態は矛盾ではなく実在する
  状態なので、人間向けにはその旨を明示する(「記録容量は超えたが、マーカーのレコードは
  まだジャーナルにある」)。同じ理由で JSON にはライブの容量 `live_maximum_size` も
  出す — 記録値と食い違うとき、`remaining_secs` が弱い数字である理由がそこにある。
- **照会に失敗したら失敗として出す。** 非 Windows / 権限不足(ボリュームが開けない)/
  ジャーナル無効・非 NTFS は `"checked": false, "reason": "<なぜ>"` で、人間向けには
  `journal check : not checked — <なぜ>` の 1 行を出す。**起きていない照会を
  `checked: true` と報告しない。** `status` 自体は失敗しない(レポートの 1 項目が
  取れなかっただけ) — 終了コードは従来どおり 0 または 2(§6)。
- **実装の置き場所**: 判定は `delta::check_journal(marker, now_ns)` /
  `delta::classify_journal(marker, &LiveJournal, now_ns)` が全プラットフォームで
  コンパイルされる形で持ち、CLI はプラットフォーム分岐をしない。実 fetch は
  `usn::query_live_journal(volume)`(Windows のみ、`FSCTL_QUERY_USN_JOURNAL` 1 回)。
  人間向けと JSON は同じ 1 つの `JournalCheck` 値から分岐する(§4-6)。
- **未検証**: 判定は純関数として Linux で単体テスト済み(issue #60)。実際の
  `FSCTL_QUERY_USN_JOURNAL` を含む経路は Windows 実機での確認が残件
  (x86_64-pc-windows-gnu へのコンパイルはゲートとして確認済み)。

### 9-2. 別 issue に切るもの

- ~~終了コードの 3 値化(§6-2)~~ — **issue #49 で実施済み**(§6 が現行契約)
- 色付き出力 / `--color` / TTY 検出(§7 #8)
- `RIPGREP_CONFIG_PATH` 相当の環境変数(§5-2)

---

## 10. 実装の割り当て

**PR #48 で入ったもの**(issue #6、2026-08-07):

1. `--json` を 9 サブコマンドに実装(§4)。`crates/sagasu-cli/src/json.rs` 新設、
   `serde_json` を workspace 依存に追加。
2. 設定ファイルの統合(§5)。`sagasu.toml` の `[text]` / `[[tags.rule]]`、
   `--config` フラグ、旧ファイル・旧フラグの案内エラー。
3. §7 の #3 / #6 / #10。
4. `docs/index_scope.md` §2-2、`docs/tag_rules.md` §2、
   `docs/examples/sagasu-tags.toml` → `docs/examples/sagasu.toml` の追随。

**入らなかったもの**: §9-2 の全部(`--color` / `RIPGREP_CONFIG_PATH` 相当。§9-1 の
`--check-journal` は issue #60 で実装済み)。
§4-7 の索引健全性フィールドはその後 issue #52 / PR #54 で入った。

**受け入れの確認は「件数が一致すること」で行う**: `--json` の出力を `jq` で
数えた結果が、人間向け出力の数字と一致すること。§4-2 の規約 1
(情報が減らない)は、この形でしかテストできない。
