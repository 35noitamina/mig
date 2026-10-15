# mig

git互換のバージョン管理ツール。保存先は普通の `.git` なので、既存のgit/GitHubとそのまま併用できる。

- ステージング不要の `save`
- `fold` / `reword` / `drop` / `split` / `absorb` で履歴を簡単に編集
- 全操作を記録して `undo` / `redo`
- APIキーなどのシークレット検出（save時は警告、sync時はpushをブロック）
- 巨大ファイルのブロックと改行コードの警告
- `gc` でデルタ圧縮したpackファイルに整理（自動実行あり）
- ネイティブGUI（`mig gui`）：エディタ、実行、自動コミット付き

## ビルド

```sh
cargo build --release            # GUI付き
cargo build --release --no-default-features   # CLIのみ
```

Linuxでは GUI 実行時に `libxkbcommon-x11` と OpenGL ドライバが必要。

## コマンド

| コマンド | 動作 |
|---|---|
| `mig init [PATH]` | リポジトリ作成 |
| `mig st` | 変更状況（シークレット警告つき） |
| `mig save [-m MSG] [--force] [PATHS…]` | 変更を全部コミット。メッセージ省略時は自動生成 |
| `mig log [-n N]` | 番号付き履歴（1が最新） |
| `mig diff [REV]` | 未保存の差分 / コミットの差分 |
| `mig fold [A..B \| N] [-m MSG]` | 範囲を1つに統合。`N` だけなら親と統合、省略時は 1 と 2 |
| `mig reword REV -m MSG` | メッセージ修正 |
| `mig drop REV` | コミット削除 |
| `mig absorb [-n]` | 未保存の変更を、その行を最後に触った未pushコミットへ自動で合体。`-n` は確認のみ |
| `mig split REV PATHS… [-m MSG]` | 指定パスの変更を1つ目、残りを2つ目のコミットに分割 |
| `mig split REV -i [-m MSG]` | hunkごとに y/n で選んで分割 |
| `mig undo` / `mig redo` | 操作の取り消し / やり直し |
| `mig oplog` | 操作履歴 |
| `mig br [NAME [AT]] [-d]` | ブランチ一覧 / 作成 / 削除 |
| `mig sw [-c] NAME` | ブランチ切り替え |
| `mig sync [--force]` | fetch → rebase → push |
| `mig scan` | 作業ツリー全体のシークレットスキャン |
| `mig gc` | オブジェクトをpackファイルにまとめて不要ファイルを削除 |
| `mig size [-n N]` | リポジトリ容量と履歴中の大きいファイル |
| `mig gui` | GUI起動 |

REV は `mig log` の番号、`@`（HEAD）、ハッシュやブランチ名が使える。

## シークレット検出

AWS / GitHub / OpenAI / Anthropic / Slack / Google / Stripe のキー、秘密鍵、`api_key = ...` のような高エントロピー値を検出する。
誤検知は行に `mig:allow` を書くか、`.mig.toml` で除外する。

```toml
[scan]
ignore_paths = ["tests/fixtures/**"]
allow = ["AKIAIOSFODNN7EXAMPLE"]
```

## save時のチェック

`max_file_mb`（初期値10MB）を超えるファイルはシークレットと同じく保存を止める（`--force` で続行）。
CRLF/LF の混在や、改行コードの全面的な変更は警告だけ出す。

## GUI エディタ

`Editor` タブでファイルを編集できる。Ctrl/Cmd+S で保存、F5 で実行、Shift+F5 で停止。

保存のたびに前回のコミットからの変更行数（追加＋削除）を数え、`auto_commit_lines` を超えたらチェックコマンドを実行する。
成功すれば `auto: ...` というメッセージで自動コミットする（pushはしない）。失敗したらコミットせず出力パネルにエラーを出す。

チェック・実行コマンドは設定がなければプロジェクトから推測する。

| ファイル | チェック | 実行 |
|---|---|---|
| `Cargo.toml` | `cargo check` | `cargo run` |
| `package.json` | `npm run build --if-present` | `npm start` |
| `go.mod` | `go build ./...` | `go run .` |
| `*.py` | 全 `.py` の構文チェック | `python main.py` |

```toml
[save]
max_file_mb = 10

[editor]
check = "cargo check"
run = "cargo run"
auto_commit_lines = 200   # 0で自動コミットOFF
```

## gc について

libgit2 には `git gc` 相当の自動整理がないので、mig が自前で行う。
参照・reflog・操作ログ・インデックスから辿れるオブジェクトを1つのpackにまとめ、その中に入ったバラバラのオブジェクトと、
内容が全部新しいpackに含まれる古いpackを削除する。どこからも辿れないオブジェクトは14日経ってから削除する。
バラバラのオブジェクトが1000個を超えると save の後に自動で実行される。

## undo について

操作ログは `.git/mig/oplog.json` に保存される。undo/redo はブランチ・タグ・HEAD を記録時点に戻し、作業ツリーもそれに合わせる。
未保存の変更がある場合は止まる（`--force` で破棄して実行）。`sync` を undo してもリモートは戻らない。
