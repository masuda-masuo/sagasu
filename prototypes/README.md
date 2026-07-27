# prototypes

技術リスクの高い要素を個別に検証する使い捨てプロトタイプ群。本体実装とは独立した Cargo ワークスペース。ここでの計測結果は issue #7(ベンチ基盤)の目標値設定に還流する。

Windows 用ビルド済み .exe は GitHub Release(`proto-YYYYMMDD` タグ)に添付してある。ダウンロードすればビルド不要で試せる。

| プロトタイプ | 検証対象 | 対応 issue |
|---|---|---|
| proto-crawl | `ignore` 並列走査 + BLAKE3 + SQLite 投入の速度 | #1 (M0) |
| proto-fulltext | tantivy + Lindera の索引/検索、**fresh-search**(mtime 差分マージ) | #2 (M1), #3 (M2 フォールバック) |
| proto-usn | NTFS USN Journal の差分照会速度(Windows 専用・要管理者権限) | #3 (M2 本命) |

## VM (Linux) での計測結果 — 2026-07-27

生成データ 2000 ファイル / 2.2MiB(小規模なのでスループット参考値):

- proto-crawl: **64,600 files/s**(全件 BLAKE3 込み、0.031s)
- proto-fulltext index: 0.32s、索引サイズ 0.9MiB(元データの ~40%)
- 検索: 日本語「空港」23ms / 英語 "delta merge" 46ms(コールド、プロセス起動込み)
- **fresh-search: 索引を作り直さずに 追加=live反映 / 変更=live置換 / 削除=除外 が全部成立**。差分オーバーヘッドは delta-walk 9.8ms + live-grep 0.3ms

## 自宅 (Windows) での確認手順

### 0. 事前準備 — Defender 誤検知対策

.exe は `Trojan:Win32/Sabsik.fl.a!ml` として隔離されることがある(無署名 + mingw クロスビルド + 大量ファイル列挙という外形による ML 誤検知。#10 参照)。展開前に作業フォルダを除外しておく:

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
- Windows .exe は gnu ツールチェーンでクロスビルド。Defender 誤検知の一因でもあり、本番は msvc を想定(#10)

## ビルド

```
cd prototypes && cargo build --release
# Windows クロスビルド(要 mingw-w64):
cargo build --release --target x86_64-pc-windows-gnu
```
