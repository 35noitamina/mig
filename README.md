# mig

git互換のバージョン管理ツール。保存先は普通の `.git` なので、既存のgit/GitHubとそのまま併用できる。

- ステージング不要の `save`
- `fold` / `reword` / `drop` で履歴を簡単に編集
- 全操作を記録して `undo` / `redo`
- APIキーなどのシークレット検出（save時は警告、sync時はpushをブロック）
- ネイティブGUI（`mig gui`）

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
| `mig undo` / `mig redo` | 操作の取り消し / やり直し |
| `mig oplog` | 操作履歴 |
| `mig br [NAME [AT]] [-d]` | ブランチ一覧 / 作成 / 削除 |
| `mig sw [-c] NAME` | ブランチ切り替え |
| `mig sync [--force]` | fetch → rebase → push |
| `mig scan` | 作業ツリー全体のシークレットスキャン |
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

## undo について

操作ログは `.git/mig/oplog.json` に保存される。undo/redo はブランチ・タグ・HEAD を記録時点に戻し、作業ツリーもそれに合わせる。
未保存の変更がある場合は止まる（`--force` で破棄して実行）。`sync` を undo してもリモートは戻らない。
