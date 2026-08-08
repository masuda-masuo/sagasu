# sagasu

意味からファイルを探す、高速ローカルファイル索引エンジン。

ディレクトリ構造を覚えていなくても、タグ・ファセット・全文検索でファイルに到達できることを目指す。ripgrep 級の速度で動く決定的・オフラインなコアを持ち、LLM や embedding はオプションのレイヤーとして分離する。

## 状態

PoC 段階。M0〜M3 は実装済み(並列クロール + SQLite メタデータ索引 / tantivy + Lindera 全文索引 / 検索時差分マージ / ルールタグ + ファセットドリルダウン)。PDF・Office の本文抽出と埋め込みメタデータタグ、機械可読出力 `--json`、`sagasu.toml` への設定統合も入っている。M4 の Tauri UI は未着手。[docs/design.md](docs/design.md) が設計の正本。

## 使い方

ワークスペースは `crates/sagasu-core`(ロジック)と `crates/sagasu-cli`(バイナリ `sagasu`)の2クレート。

```sh
cargo build --workspace --release
```

サブコマンド:

| コマンド | 内容 |
|---|---|
| `index` | ディレクトリ木を並列クロールしてメタデータ索引(SQLite)を作る/更新する |
| `hash` | まだハッシュのないファイルに BLAKE3 のコンテンツハッシュを埋める |
| `fulltext` | 索引済みファイルから本文を抽出して全文索引を作る(テキスト/コードに加え PDF・docx・xlsx・pptx) |
| `search` | 全文索引をキーワード検索する(スコア順、パス + スニペット) |
| `find` | メタデータ索引をパスの部分一致で引く |
| `tag` | メタデータ索引の上にルールベースの意味タグ層を生成する |
| `tags` | タグを一覧する / タグでファイルを絞る / 1ファイルのタグを説明する |
| `browse` | ファセット階層をドリルダウンする(次に何で絞るか) |
| `status` | 索引の統計を出す |

パイプラインは `index` → (`hash`) → `fulltext` → `search`、タグ層は `index` → `tag` → `tags` / `browse`。`search` と `find` は既定で鮮度マージが効く(`--no-fresh` で無効)。

全 9 サブコマンドが `--json`(機械可読出力)を持つ。設定ファイルは単一の `sagasu.toml`(`[text]` = 本文抽出、`[[tags.rule]]` = タグルール)で、これを読むのは `fulltext` / `search` / `tag` / `tags` の 4 つ(`--config` で明示可)。終了コードは rg 式の 3 値(0 = マッチあり / 1 = マッチなし / 2 = エラー)。サブコマンド体系・引数・出力形式・設定ファイル・終了コードの正本は [docs/cli.md](docs/cli.md)、設定ファイルの例は [docs/examples/sagasu.toml](docs/examples/sagasu.toml)。

## ドキュメント

| 文書 | 内容 |
|---|---|
| [docs/design.md](docs/design.md) | 設計の正本。全体アーキテクチャと、実装で決着した設計判断の記録 |
| [docs/cli.md](docs/cli.md) | CLI インターフェース(サブコマンド・`--json`・設定ファイル・終了コード) |
| [docs/index_scope.md](docs/index_scope.md) | 何を索引し何を除外するか(既定の除外規則、本文抽出の対象判定) |
| [docs/tag_rules.md](docs/tag_rules.md) | タグの名前空間とユーザー定義ルール |
| [docs/browse.md](docs/browse.md) | ファセットドリルダウンの式・実測・到達シナリオ |
| [docs/schema_v0.md](docs/schema_v0.md) | SQLite スキーマ v0 |
| [bench/README.md](bench/README.md) | 計測ハーネス(合成ツリー生成 + 外形計測) |

## prototypes/

技術リスクの高い要素を個別に潰すための使い捨て検証群で、検証は完了済み。正本の実装は `crates/` 側にあり、prototypes/ はルートの workspace から exclude された独立した Cargo ワークスペース。

## コンセプト(要約)

- **CLI ファースト**: まず単体で便利な CLI として成立させる。UI(Tauri 予定)はその上に載せる
- **鮮度の透過化**: インデックスは定期作成でよい。検索時にファイルシステムのジャーナルから差分を取り、変更分だけライブスキャンしてマージすることで、インデックスの古さをユーザーから見えなくする
- **プログラムファースト**: 意味情報(タグ・ファセット階層)はルールベース+統計で機械生成する。LLM / embedding は必須にしない
- **台帳としての将来性**: コンテンツハッシュ・安定ファイルID・アクセス履歴をスキーマ v0 から持ち、将来のストレージティアリングサービスの資産台帳になれる形にしておく
