# mig

git 互換のバージョン管理ツール。中身は普通の `.git` なので、git や GitHub とそのまま併用できる。

- ステージング不要。`mig save` で変更をまとめてコミット
- 履歴の編集が簡単（統合・分割・削除・メッセージ修正・absorb）
- どんな操作も `mig undo` で取り消せる
- API キーや巨大ファイルのうっかりコミット／push を防ぐ
- ネイティブ GUI（エディタ・実行・自動コミット付き）

---

## インストール

[Rust](https://rustup.rs) を入れてから:

```sh
git clone https://github.com/35noitamina/mig
cd mig
cargo install --path .                        # GUI 付き
cargo install --path . --no-default-features  # CLI のみ（軽い）
```

| OS | 事前に必要なもの |
|---|---|
| Windows | Visual Studio Build Tools（C++） |
| macOS | `xcode-select --install`、`brew install openssl@3` |
| Linux | `build-essential pkg-config libssl-dev`、GUI 用に `libxkbcommon-x11-0` と OpenGL ドライバ |

コミットの作者情報を一度だけ設定しておく（GUI の Settings → Identity からでも可）:

```sh
git config --global user.name "名前"
git config --global user.email "you@example.com"
```

---

## はじめかた

```sh
# 既存のフォルダをリポジトリにして、最初のコミットと GitHub への push まで
cd my-project
mig repo init --save --github --private

# 普段の作業
mig st                  # 何が変わったか
mig save -m "作業内容"   # 全部まとめてコミット
mig sync                # pull（rebase）して push
mig undo                # 直前の操作を取り消す

# GUI
mig gui here            # 今のフォルダのリポジトリを GUI で開く
```

---

## コマンド

コマンドは用途ごとのグループに分かれている。毎日使うものだけはグループ名なしで打てる。
`mig --help`、`mig <グループ> --help` で一覧と説明が出る。

### 毎日使う

| コマンド | 内容 |
|---|---|
| `mig st` | 変更されたファイルの一覧（シークレットが含まれていれば警告） |
| `mig save [-m MSG] [PATH…]` | 変更を全部コミット。メッセージを省略すると自動で付く |
| `mig sync` | fetch → 自分のコミットを最新の上に付け直し → push |
| `mig log [-n N]` | 番号付きの履歴（1 が最新） |
| `mig diff [REV]` | 未保存の差分、または指定コミットの差分 |
| `mig undo` / `mig redo` | 直前の操作の取り消し／やり直し |
| `mig sw [-c] NAME` | ブランチの切り替え（`-c` で作成してから切り替え） |

### 履歴 — `mig history`（短縮 `mig h`）

| コマンド | 内容 |
|---|---|
| `mig h fold 2..5` | 2〜5 番のコミットを 1 つに統合。`fold 3` なら 3 番を親と統合、引数なしなら 1 番と 2 番 |
| `mig h reword REV -m MSG` | コミットメッセージを修正 |
| `mig h drop REV` | コミットを履歴から削除 |
| `mig h split REV PATH…` | 指定したパスの変更を 1 つ目、残りを 2 つ目のコミットに分割（`-i` で hunk ごとに選ぶ） |
| `mig h absorb [-n]` | 未保存の修正を、その行を最後に触った未 push のコミットへ自動で合体（`-n` は確認だけ） |
| `mig h log` / `mig h diff` | 毎日使うものと同じ |

### ブランチ — `mig branch`（短縮 `mig br`）

| コマンド | 内容 |
|---|---|
| `mig br` / `mig br list` | 一覧 |
| `mig br new NAME [REV]` | 作成 |
| `mig br rm NAME` | 削除 |
| `mig br switch NAME` | 切り替え（`mig sw` と同じ） |

### リモート — `mig remote`

| コマンド | 内容 |
|---|---|
| `mig remote` / `mig remote list` | 一覧 |
| `mig remote add NAME URL` | 追加 |
| `mig remote rm NAME` | 削除 |
| `mig remote publish [NAME] [--private] [--ssh]` | 今のリポジトリを GitHub に作って origin に登録し、push |

### 操作ログ — `mig op`

| コマンド | 内容 |
|---|---|
| `mig op undo` / `mig op redo` | `mig undo` / `mig redo` と同じ |
| `mig op log [-n N]` | これまでの操作の一覧（取り消したものには `(undone)`） |

### リポジトリ — `mig repo`

| コマンド | 内容 |
|---|---|
| `mig repo init [PATH]` | 新しいフォルダ、または既存のフォルダをリポジトリにする（下記） |
| `mig repo scan` | 作業ツリー全体の API キー・秘密鍵スキャン |
| `mig repo gc` | オブジェクトをデルタ圧縮した pack にまとめ、不要なファイルを削除 |
| `mig repo size [-n N]` | リポジトリの容量と、履歴中の大きいファイル |

### GUI

| コマンド | 内容 |
|---|---|
| `mig gui` | ようこそ画面で起動 |
| `mig gui here` / `mig gui PATH` | 今のフォルダ（または指定したフォルダ）のリポジトリを直接開く。リポジトリでなければようこそ画面 |

### 共通

- `REV` には `mig log` の番号、`@`（最新）、コミットハッシュ、ブランチ名が使える
- `-C DIR` を付けると、そのフォルダで実行したのと同じになる
- 以前の書き方（`mig init`、`mig fold`、`mig gc`、`mig oplog`、`mig br NAME`、`mig br -d NAME` など）もそのまま使える

---

## リポジトリの作成

```sh
mig repo init my-app                       # 新しいフォルダを作ってリポジトリに
mig repo init --save                       # 今のフォルダをリポジトリにして最初のコミットまで
mig repo init --save --github --private    # さらに GitHub に作って push
mig repo init --github myorg/tool          # Organization に作る
mig repo init --remote URL                 # origin を登録
```

- `.gitignore` がなければプロジェクトの種類（Rust / Node.js / Go / Python）に合わせて作る。どの種類でも `.env` と OS のゴミファイルは除外する（`--no-ignore` で作らない）
- `--save` の最初のコミットもシークレットと巨大ファイルのチェックを通る。引っかかったらコミットだけ止まる
- 別のリポジトリの中には作らない。すでにリポジトリのフォルダでは `--save` / `--remote` / `--github` だけ適用する

GitHub への作成（`--github`、`mig remote publish`）には `repo` 権限のあるトークンが必要。
環境変数 `GITHUB_TOKEN`（または `GH_TOKEN`）か、`gh auth login` 済みの `gh` コマンドから読み取る。mig 自身はトークンを保存しない。
HTTPS の push で git の認証ヘルパーが使えないときも、github.com ならこのトークンで push する。

---

## 安全チェック

### シークレット（API キーなど）

AWS / GitHub / OpenAI / Anthropic / Slack / Google / Stripe のキー、秘密鍵、`api_key = "..."` のような推測しにくい値を検出する。

| タイミング | 動作 |
|---|---|
| `mig save` | 見つかったらコミットを止める（`--force` で続行） |
| `mig sync` | 未 push のコミットに含まれていたら push を止める |
| `mig st`、GUI | 未保存の変更に含まれていれば警告（GUI は OS の通知も出す） |

誤検知は、その行に `mig:allow` と書くか `.mig.toml` で除外する。

### 巨大ファイルと改行コード

- 上限（初期値 10 MB）を超えるファイルは、シークレットと同じくコミットを止める
- CRLF と LF の混在や、ファイル全体の改行コードの変更は警告だけ出す

---

## GUI

`mig gui` で起動する。リポジトリの外ではようこそ画面になり、リポジトリを開く・既存フォルダをリポジトリにする・新規作成ができる。

| 画面 | できること |
|---|---|
| Editor | ファイルツリー、タブ、シンタックスハイライト、実行（F5／停止 Shift+F5）。Ctrl/Cmd+S で保存 |
| History | 履歴と差分。複数選択して Fold、右クリックで Reword / Split / Fold / Drop |
| ツールバー | Commit、Absorb、Sync、Undo、Redo、Repo メニュー（リポジトリの切り替え、GitHub への公開）、Settings |

**自動コミット**: エディタで保存するたびに前回のコミットからの変更行数を数え、上限（初期値 200 行）を超えたらチェックコマンドを実行する。成功すれば `auto: ...` というメッセージで自動コミットする（push はしない）。失敗したらコミットせず、出力パネルにエラーを出す。

チェック・実行コマンドは設定がなければプロジェクトから推測する。

| プロジェクト | チェック | 実行 |
|---|---|---|
| Rust（`Cargo.toml`） | `cargo check` | `cargo run` |
| Node.js（`package.json`） | `npm run build --if-present` | `npm start` |
| Go（`go.mod`） | `go build ./...` | `go run .` |
| Python（`*.py`） | 全 `.py` の構文チェック | `python main.py` |

大きなフォルダでも画面が止まらないよう、状態の読み込み・コミット・作成は裏で行い、履歴 → 変更一覧 → 差分の順に表示する。

---

## 設定

GUI の **Settings**（ようこそ画面では下の **Settings…**）から変更できる。

| タブ | 内容 | 保存先 |
|---|---|---|
| Appearance | テーマ（ダーク／ライト／OS に合わせる、初期値はダーク）、画面の拡大率、コードの文字サイズ | 設定フォルダの `mig/settings.toml` |
| Identity | コミットの作者名とメールアドレス | git のグローバル設定 |
| This repository | 自動コミットの行数、チェック・実行コマンド、ファイルサイズの上限、シークレット検出の除外パスと許可リスト | リポジトリの `.mig.toml` |
| GitHub | トークンが見つかるかの確認 | — |

`.mig.toml` は手で書いてもよい。GUI から保存したときは、変更した項目だけを書き換えてコメントは残す。

```toml
[editor]
check = "cargo check"
run = "cargo run"
auto_commit_lines = 200   # 0 で自動コミットしない

[save]
max_file_mb = 10

[scan]
ignore_paths = ["tests/fixtures/**"]
allow = ["AKIAIOSFODNN7EXAMPLE"]
```

設定フォルダは Linux なら `~/.config/mig/`、macOS なら `~/Library/Application Support/mig/`、Windows なら `%APPDATA%\mig\`。最近開いたリポジトリの一覧（`recent.json`）もここに入る。

---

## 仕組み

### undo

すべての操作で、ブランチ・タグ・HEAD の状態を前後で記録する（`.git/mig/oplog.json`）。
undo / redo はその記録に戻し、作業ツリーも合わせる。未保存の変更があるときは止まる（`--force` で破棄して実行）。
`sync` を undo しても手元が戻るだけで、リモートは戻らない。

### gc

libgit2 には `git gc` 相当の自動整理がないため、mig が自前で行う。
参照・reflog・操作ログ・インデックスから辿れるオブジェクトを 1 つの pack にまとめ、その中に入ったバラバラのオブジェクトと、中身がすべて新しい pack に含まれる古い pack を削除する。
どこからも辿れないオブジェクトは 14 日経ってから削除する。バラバラのオブジェクトが 1000 個を超えると、save の後に自動で実行される。
