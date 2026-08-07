# prototypes

技術リスクの高い要素を個別に検証する使い捨てプロトタイプ群。本体実装とは独立した Cargo ワークスペース。ここでの計測結果は issue #7(ベンチ基盤)の目標値設定に還流する。

Windows 用ビルド済み .exe は GitHub Release(`proto-YYYYMMDD` タグ)に添付してある。ダウンロードすればビルド不要で試せる。
`.github/workflows/prototypes.yml` が Windows ランナー上でネイティブに MSVC ビルドして添付したもので、手元の
mingw クロスビルドを人手でアップロードする経路ではない(詳しくは末尾の「ビルド」参照)。

| プロトタイプ | 検証対象 | 対応 issue |
|---|---|---|
| proto-crawl | `ignore` 並列走査 + BLAKE3 + SQLite 投入の速度 | #1 (M0) |
| proto-fulltext | tantivy + Lindera の索引/検索、**fresh-search**(mtime 差分マージ) | #2 (M1), #3 (M2 フォールバック) |
| proto-ftcompare | tantivy vs SQLite FTS5 の対比計測(同一コーパス・同一本文抽出・同一トークナイザ) | #35 (design.md §11) |
| proto-usn | NTFS USN Journal の差分照会速度(Windows 専用・要管理者権限) | #3 (M2 本命) |

## VM (Linux) での計測結果 — 2026-07-27

生成データ 2000 ファイル / 2.2MiB(小規模なのでスループット参考値):

- proto-crawl: **64,600 files/s**(全件 BLAKE3 込み、0.031s)
- proto-fulltext index: 0.32s、索引サイズ 0.9MiB(元データの ~40%)
- 検索: 日本語「空港」23ms / 英語 "delta merge" 46ms(コールド、プロセス起動込み)
- **fresh-search: 索引を作り直さずに 追加=live反映 / 変更=live置換 / 削除=除外 が全部成立**。差分オーバーヘッドは delta-walk 9.8ms + live-grep 0.3ms

## proto-ftcompare — tantivy vs SQLite FTS5 (issue #35)

`docs/design.md` §11 の open question に数字で答えるための使い捨てプロトタイプ。3エンジン
(`tantivy` / `fts5-lindera` / `fts5-trigram`)が**同じ走査・同じ本文抽出・同じアナライザ**を
共有するので、差はエンジンだけになる。結論と数字は design.md §11 にある。

```bash
cargo build --release --manifest-path prototypes/Cargo.toml -p proto-ftcompare
export PATH="$PWD/prototypes/target/release:$PATH"

bench gen --out /tmp/tree --files 100000 --seed 42

# 索引を作る(構築時間・索引サイズ・本文に対する比を印字)
proto-ftcompare index /tmp/tree --engine tantivy --index-dir /tmp/idx-tantivy

# 検索(--repeat N でプロセス内 warm p50/p95、固定コストを含まない)
proto-ftcompare search 全文検索 --engine tantivy --index-dir /tmp/idx-tantivy --repeat 21

# 検索品質: 同じ抽出結果に対する literal な部分一致(grep 相当)と突き合わせる
proto-ftcompare recall /tmp/tree 全文検索 --engine tantivy --index-dir /tmp/idx-tantivy

# 1文書だけ置き換える差分更新のコスト
proto-ftcompare update --engine tantivy --index-dir /tmp/idx-tantivy --path /tmp/tree/d0000/f000001.txt
```

ベンチ基盤から回す場合は `bench run --config bench/configs/ftcompare-linux.toml`。
**バイナリは PATH に置くこと** — harness は各ターゲットの `{workdir}` を作業ディレクトリにして
子プロセスを起こすので、`./proto-ftcompare` はそのスクラッチディレクトリを見に行って落ちる。

## 自宅 (Windows) での確認手順

### 0. 事前準備 — Defender に隔離された場合の対処(フォールバック)

CI がネイティブ MSVC ビルドした .exe にはバージョンリソースとマニフェスト(実行レベル・long path 対応)が
入っており、無署名ながら以前の mingw クロスビルドよりは Windows Defender の ML 誤検知(`Trojan:Win32/Sabsik.fl.a!ml`、
#10 参照)が起きにくいはずだが、ゼロにはならない。特に `proto-20260727` より前のタグで配布された資産は mingw
クロスビルドなので、隔離される前提で臨む。隔離された場合は作業フォルダを除外する:

```powershell
# 管理者 PowerShell で
Add-MpPreference -ExclusionPath "C:\path\to\sagasu-proto"

# 除外の確認
Get-MpPreference | Select-Object -ExpandProperty ExclusionPath

# 検証が終わったら除外を戻す
Remove-MpPreference -ExclusionPath "C:\path\to\sagasu-proto"
```

既に隔離された場合は「Windows セキュリティ → ウイルスと脅威の防止 → 保護の履歴」から復元(要除外設定済み、でないと再隔離される)。

### 1. proto-crawl — 実データでの走査速度

```powershell
.\proto-crawl.exe C:\Users\<you>\Documents --hash --db crawl.db
# 見る値: files/s。実ファイル数十万規模でどこまで出るか
# --no-ignore で .gitignore/隠しファイル無視をやめて全量走査も試す
# 既定で Windows / Program Files / Program Files (x86) / $Recycle.Bin /
# System Volume Information / AppData をスキップする(スキップ件数は出力の "skipped dirs" 行)。
# 以前どおりのフルボリューム計測と比較したいときは --full-volume を付ける
```

### 2. proto-fulltext — 日本語検索と鮮度マージ

```powershell
.\proto-fulltext.exe index D:\docs --index-dir ft-index
.\proto-fulltext.exe search "確定申告" --index-dir ft-index
# → その後ファイルを追加・編集・削除してから(再索引せずに):
.\proto-fulltext.exe fresh-search "確定申告" --index-dir ft-index
# 見る値: timing 行(index / delta-walk / live-grep)。[live] 行に変更が反映されるか
```

### 3. proto-usn — USN Journal の差分照会(本命経路)

**管理者権限の PowerShell で:**

```powershell
.\proto-usn.exe C:
# 表示された next usn を控える → ファイルをいくつか作成・変更・削除 →
.\proto-usn.exe C: --since <next_usn>
# 見る値: elapsed(= 検索時差分照会のコスト)。数十ms 級で返るかが M2 成立の鍵
# レコード数が多い時は --close-only で CLOSE のみ表示
```

### 確認したい仮説

1. 実データ数十万ファイルでも proto-crawl が数十秒以内(M0 の速度目標の根拠になる)
2. proto-fulltext の日本語検索が実文書(Office 系以外)で実用精度
3. **USN 差分照会が数十 ms 級** → 「インデックスは古くてよい」設計の成立
4. fresh-search の delta-walk が実データ規模でどこまで伸びるか(mtime フォールバックの限界確認)

## 既知の制約(プロトタイプの割り切り)

- proto-usn はファイル名しか出さない。FRN→フルパス解決(MFT 列挙 or OpenFileById)は次の検証項目
- proto-fulltext の live-grep は素朴な部分一致で、索引側のトークナイズ検索と一致条件が違う
- 結果を `| head` に繋ぐと broken pipe で panic する(SIGPIPE 未処理、実害なし)
- Windows .exe は CI(`.github/workflows/prototypes.yml`)が Windows ランナー上で msvc ネイティブビルドする。
  ローカルの mingw クロスビルドは本番の生成経路ではなく動作確認用の簡便法(詳しくは末尾の「ビルド」)
- proto-usn の実行マニフェストは `requireAdministrator`。USN Journal 読み取りに管理者権限が無条件で
  必要で、asInvoker で起動してから失敗させる意味がないため(詳細は各 build.rs のコメント)

## ビルド

```
cd prototypes && cargo build --release
```

公開している Windows 用 .exe はこのコマンドではなく `.github/workflows/prototypes.yml` が Windows
ランナー上でネイティブに `--target x86_64-pc-windows-msvc` ビルドしたもの(タグ `proto-YYYYMMDD` の push、
または Actions の手動トリガーで実行)。mingw クロスビルドは本番の生成経路ではなく、Windows 実機なしで
手元動作確認したいときの簡便法として残してあるだけ(要 mingw-w64、バージョンリソース/マニフェストは
埋め込まれるが署名は無い):

```
cargo build --release --target x86_64-pc-windows-gnu
```
