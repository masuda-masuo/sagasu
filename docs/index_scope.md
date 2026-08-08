# 索引スコープ — 何を索引し、何を索引しないか (issue #14 / #15)

sagasu が「引けない」と言うとき、原因は3つしかない。

1. そもそもクロールが**ファイルを見ていない**(除外規則)
2. クロールは見たが、**本文を取っていない**(本文抽出の対象判定)
3. 索引には入っているが、クエリが当たらない

この文書は 1 と 2 を扱う。**黙って欠けるのが本プロジェクト最悪の失敗**なので、
どちらも「除外した件数と理由」を必ず出力する。出ていない除外は存在しない。

```
sagasu index <root> --db index.db     # 1. 除外規則が効く段
sagasu fulltext --db index.db         # 2. 本文抽出の対象判定が効く段
sagasu status --db index.db           # 両方の結果と、空振りの警告
```

## 1. 既定の除外規則

### 1-1. gitignore と hidden は継承しない

走査には `ignore` クレート(ripgrep と同じもの)を使うが、**そのクレートの既定の
フィルタはすべて切ってある**。`hidden` / `ignore` / `git_ignore` / `git_global` /
`git_exclude` の5つ全部。

> gitignore の除外規則は「バージョン管理に入れるべきでないもの」の定義であって、
> 「検索できなくてよいもの」の定義ではない。

この2つを同一視した**プロトタイプ**の実測(2026-07-29, Windows 実機)が issue #14:

| 対象 | クレート既定 | 実体 |
|---|---|---|
| 95ファイルのコーパス | 35 | 95(`.opencode/` `.github/` 配下の md 19件が黙って消えた) |
| `C:\Users\` | 494 | 151,674(**0.3%**) |

これは**プロトタイプの**数字であって、現在の sagasu の以前のリリースの数字ではない。
`hidden(false)` と `git_ignore(false)` は M0〜M1 の時点で入っている。
この文書が書いている規則は、その判断を型と設定と出力に落として
**説明可能・再現可能**にしたもの。詳しくは design.md §4-1。

### 1-2. 既定除外セット(独立に定義する)

除外してよいのは**ビルド生成物とキャッシュ**だけ。量が多く、意味が薄く、
索引されている別のもの(ソース)から再生成できる。

```
node_modules  target  __pycache__  .git  .hg  .svn  .venv  venv  .cache  .npm
.cargo/registry   ← .cargo 自体とその他の子は索引する
```

ディレクトリ**名**での判定(basename、大文字小文字を無視)。

- `--exclude <NAME>` で足す
- `--no-default-excludes` でこのリストごと落とす

**ドットディレクトリは除外しない。** `.github/` `.config/` `.vscode/` `.opencode/`
は開発者のマシンで意味のあるテキストが実際に置かれている場所であって、
「隠す」ためのものではない。

### 1-3. hidden 属性(Windows)と先頭ドット(Unix)は別物

`--skip-hidden` は **OS が hidden 属性を立てているエントリ**を除外する。

| プラットフォーム | 判定 |
|---|---|
| Windows | `FILE_ATTRIBUTE_HIDDEN` (0x2) を読む |
| Unix | **常に false**。Unix に hidden 属性は存在しない |

先頭ドットはどちらのプラットフォームでも hidden とは**みなさない**。
`ls` が隠すのは命名規約に対する慣習であって、ファイルの属性ではない。
`.github` は hidden 属性が立っていないので、Windows でも `--skip-hidden` の
対象にならない。

既定は `--skip-hidden` **なし**(hidden も索引する)。

### 1-4. `.gitignore` は opt-in、しかもディレクトリだけ

`--use-gitignore` を付けると、**クロールルート直下の `.gitignore`** を読んで、
その**ディレクトリ規則だけ**を除外に足す。

なぜ絞るか:

- gitignore がディレクトリを外す理由はほぼ一意で「この下は生成物」。これは既定
  除外セットが言っていることと同じで、既定が知りようのないプロジェクト固有の
  名前(`dist/` `out/` `.next/`)を自動で拾える
- gitignore が**ファイル**を外す理由は3つある — 生成物・秘密・ローカル専用 — で、
  後ろ2つ(`.env`, `notes.local.md`, `TODO.md`)は**まさに自分のディスクを
  検索する動機そのもの**

```
$ cat .gitignore
*.log
dist/
.env

$ sagasu index . --db index.db --use-gitignore
gitignore    : 3 rule(s) from the root .gitignore, directories only
indexed      : 5
skipped      : 2
  (gitignore): 2        ← dist/ の2ファイルだけ。app.log と .env は索引される
```

**規則は索引に焼き込まれる。** クロールが読んだ `.gitignore` の行はそのまま索引に
入り、以後どのクエリもファイルを読み直さない。参照にとどめると2通りに壊れる:
索引後にファイルを壊すと全クエリが死に、消すと規則が0件に縮んで、クロールが刈った
`dist/` がライブヒットとして復活する(索引に行が無いファイルが答えに出る)。
どの版の規則で作られた索引かは digest で分かる。

```
$ sagasu index . --db index.db --use-gitignore
gitignore    : 3 rule(s) from the root .gitignore, directories only (digest 44c9df3196dd)

$ sagasu status --db index.db
  gitignore    : 3 rule(s) baked in, directories only (digest 44c9df3196dd)
```

**既知の制限**: 読むのはルートの `.gitignore` 1枚だけ。サブプロジェクトの
ネストした `.gitignore`、`.git/info/exclude`、グローバル gitignore は読まない。
ファイルが無いのはエラーではない(規則0件として扱う)。逆に、**読めるが壊れている
ファイルはエラーで止まる** — 頼んだのに黙って効かないのが一番悪い。
エラーは壊れている規則を名指しする。

### 1-5. 除外セットは1つ。索引と検索で同じものを使う

クロールが使った除外規則は `meta.exclude_policy` に保存され、検索時の差分照会
(design.md §5)がそれを**そのまま復元して**使う。

これは対称に効く。差分側の規則がクロールより緩いと、クロールが索引しなかった
ファイルがライブヒットとして答えに現れる(索引に行が無いのに結果に出る)。
逆に厳しいと、変更が見えなくなる。`sagasu index --exclude` が後続のクエリに
伝わっていなかったのは design.md §5-2 の残論点だったが、この保存で解消した。

規則を保存できない設定は**クロール前に拒否する**。改行を含む除外名
(`--exclude $'foo\nbar'`)は policy の行を壊すので、通すと「クロールは成功と
報告し、以後の全クエリがパースエラーで死ぬ」になる。末尾 `\r` も同じ理由で拒否する
(こちらは静かに別の名前として復元され、クロールと差分が違う集合を除外する)。

読めない policy に出会ったクエリは**落ちずに degrade する**。索引だけで答え、
「除外規則を復元できなかったので索引以降の変更はマージしていない」と警告する。
未知のキーやバージョンを黙って無視しない(近似した除外セットで答える方が悪い)
一方で、新しい版の索引を古いバイナリで開いても検索が死なないのはこのため。

`sagasu status` はその規則を出す:

```
exclusion      : 11 dir name(s)
  hidden       : include
  gitignore    : 3 rule(s) baked in, directories only (digest 44c9df3196dd)
```

規則を持たない索引(古いビルドが作ったもの)には警告が出る。差分側が既定セットで
代用するため、`--exclude` 付きで作られていた場合は答えと食い違う。

### 1-6. 除外は必ず数え上げる

```
scanned      : 14349
indexed      : 75
skipped      : 14274
  target: 14014
  .git: 260
```

規則名の行はディレクトリ名そのもの。opt-in の2つは `(os hidden)` と `(gitignore)`
という括弧付きの行になり(ディレクトリ名と混ざらないように)、**0 件のときは
行ごと出ない**。

**読めなかったものは除外ではない。** 開けなかったディレクトリ(その下は丸ごと
索引に入らない)や stat できなかったファイルは、誰も落とせと言っていないので
別のカウンタに出る:

```
scanned      : 42
indexed      : 39
skipped      : 2
  node_modules: 2
unreadable   : 1
  /home/u/proj/locked: IO error ... (os error 13)
```

`scanned = indexed + skipped + unreadable` が常に成立する。件数は索引にも残るので
`sagasu status` が後からでも `unreadable : N (as of the last crawl)` と言える。
これが無かった間、読めないディレクトリ配下のファイルはどのカウンタにも現れず、
サマリは健康に見えて終了コードも 0 だった。

`scanned = indexed + skipped` が常に成り立つ。除外ディレクトリの中身も
walk して数えているからで、これは意図的に速度と引き換えにしている
(「何件消えたか言えない除外」を作らないため)。

## 2. 本文抽出の対象判定

### 2-1. 拡張子は入口であって判定ではない

固定の拡張子リストだけで決めると、リストに無いプレーンテキストが**エラーも警告も
なく**索引から消える。実測では `mjs` が無かったために 41 ファイルが消え、残ったのは
`indexed files : 35` という数字だけだった(issue #15)。

判定は3段:

| 段 | 入力 | 結果 |
|---|---|---|
| 1. 許可リスト | 拡張子 | text — 1バイトも読まずに索引 |
| 1.5. 抽出器の照会 | 拡張子 | PDF / docx / xlsx / pptx は専用パーサで本文を取り出す(issue #40) |
| 2. 拒否リスト | 拡張子 | binary — 開かずに落とす(画像/実行形式/古い Office 形式) |
| 3. 内容サンプリング | 先頭512バイト | どちらのリストにも無いものはここで決める |

3段目があるので、`Makefile` `LICENSE` `.tmpl` `.vim` のように**誰も拡張子リストに
足さなかったプレーンテキスト**が索引に入る。判定は同じバイト列に同じ答えを返す
純関数(`text::sniff_is_text`)で、UTF-8 BOM は肯定、NUL バイト・UTF-16 BOM・
制御文字の多さは否定。サンプルは `sagasu hash` が埋めた `files.magic` 列を
再利用するので、多くの場合ファイルを開き直さない。

`--no-sniff` で3段目を切れる。切ると1段目のリストが判定そのものになる。

**既知の制限**: Shift_JIS / EUC-JP / UTF-16 は「デコードできない」ので binary 扱い。
文字コード判定は未実装。**黙って落とすのではなく、`binary or undecodable content`
として数えて出す。**

**PDF / Office は抽出する**(issue #40)。`.pdf` / `.docx` / `.xlsx` / `.pptx` は
1.5 段目の抽出器に回り、本文と埋め込みメタデータ(作成者・タイトル・撮影機種)が取れる。
壊れたファイルは `unsupported format (media/binary/legacy documents)` ではなく
**`document extraction failed`** に別計上し、
先頭数件はパスと理由をそのまま出す(「この形式は読まない」と「読める形式だがこの
ファイルが壊れている」は利用者の取るべき行動が違う)。`.doc` / `.xls` / `.ppt` /
OpenDocument / `.rtf` は拒否リストのまま。抽出 feature を落としてビルドした場合は
抽出器が何も返さず、拒否リストに落ちて `unsupported format` に戻る(#40 以前の挙動)。

### 2-2. 許可リストはユーザーが広げられる

コマンドラインと設定ファイルの2経路。設定ファイルの既定名は
**`sagasu.toml`**(カレントディレクトリ)で、本文抽出の設定は `[text]` セクション。
`--config <PATH>` で明示できる。探索順と旧ファイルの扱いは docs/cli.md §5。

```toml
# sagasu.toml
# 拡張子に先頭のドットは付けても付けなくてもよい。大文字小文字は無視。
[text]
text_ext   = ["tmpl", "hbs", "j2"]   # 許可リストに足す(拒否リストより強い)
binary_ext = ["dat", "pak"]          # 拒否リストに足す
```

**issue #6 で `sagasu-text.toml` と `sagasu-tags.toml` は 1 本に統合された。**
旧ファイルは読まれないが、置かれているのを検出したらエラーで案内する
(黙って無視すると、利用者から見えるのは「設定が効かなくなった」だけになる)。

優先順位(強い順):

1. `text_ext` / `--ext` — ユーザーは実物を見ている、こちらは見ていない
2. `binary_ext`
3. 組み込みの許可リスト
4. 組み込みの拒否リスト
5. どれでもなければ内容サンプリング

TOML なのは `bench/configs/*.toml` がすでに TOML だから(設定言語は 1 つで足りる)。
**未知のキーはエラー** — `text_exts` と打ち間違えた設定ファイルが「読めたが
何もしない」状態になると、利用者は sniffer を疑うことになる。

`--ext` は設定ファイルの**後**に適用される(コマンドラインが勝つ)。

**この規則も索引が持つ。** `sagasu fulltext` は使った規則を索引に書き、
`sagasu search` はそこから復元するので、**別のディレクトリから検索しても
同じ判定が効く**。自動探索だけに頼っていた版は、cwd に設定ファイルが無いと
黙って組み込みリストに戻り、ライブ grep が索引と違う判定をした。その結果は
「編集した瞬間そのファイルが答えから消える」— 索引ヒットは変更として落ち、
ライブヒットは生まれない。

`sagasu search` は使った規則を必ず表示し、明示した規則が索引のものと
食い違えば警告する:

```
$ sagasu search "needle" --db index.db
text    : +text obj (sagasu.toml)          ← 索引から復元したもの

$ sagasu search "needle" --db index.db --ext hbs
WARNING: the full-text index was built with +text obj (…) but this search was
given +text hbs; the live scan used the latter, …
```

### 2-3. 除外の件数と理由、そして拡張子の内訳

```
config       : sagasu.toml (found in the working directory)
  +text      : tmpl
candidates   : 95
indexed      : 84
  by ext     : 48
  by sniff   : 6        ← 大きいとリストに足す価値のある形式がある合図
  by extract : 30       ← PDF / docx / xlsx / pptx(issue #40)
skipped      : 11
  unsupported format (media/binary/legacy documents): 8
  binary or undecodable content: 3
  by extension:
    .png: 8
    (no extension): 3
```

抽出に失敗したファイルがあれば `document extraction failed` の行と、
その下に**パスと理由**が先頭数件だけ並ぶ(`(… and N more)` 付き)。
索引前に格子を分割した文書があれば `long lines :` の行が出る(design.md §4-2-2)。

`by extension` は「11件落ちた」を**次の一手**に変える行。`.mjs: 41` と出れば
足すべき引数がそのまま読める。

### 2-4. sniff の既知の性質

- **ESC (0x1B) は制御文字として数えない。** ANSI 色付きのログや端末キャプチャは
  人が自分のディスクを grep する対象であって、バイナリではない
- 制御文字の比率判定には分母の下限(512 バイト)がある。無いと 40 バイトの
  ファイルが制御文字1個で binary になる
- 許可リストに載っている拡張子は**内容を見ない**。`.csv` を名乗るバイナリは
  そのまま読まれる。判定の非対称は意図的で、「拡張子を偽ったテキスト」を
  取りこぼさない方を優先している

### 2-5. 対象0件は正常終了ではない

- `sagasu index` が 0 件 → stderr に警告、**終了コード 2**(issue #49 で 1→2。正本は docs/cli.md §6)
- `sagasu fulltext` が 0 件 → stderr に警告、**終了コード 2**(同上)
- `sagasu status` は後からでもそれが見える:
  - クロールが無い / live 0件 / 全文索引はあるのに 0 文書 のそれぞれに警告

「索引したが引けない」と「そもそも索引していない」がユーザーから区別できないのが
検索エンジンとして一番たちが悪い。ビルド時の警告は流れてしまうので、
`sagasu status` が後から同じことを言う。
