# mig

git 互換のバージョン管理ツール。中身は普通の `.git` なので、git や GitHub とそのまま併用できる。

- ステージング不要。`mig save` で変更をまとめてコミット
- 履歴の編集が簡単（統合・分割・削除・メッセージ修正・absorb）
- どんな操作も `mig undo` で取り消せる
- API キーや巨大ファイルのうっかりコミット／push を防ぐ
- ネイティブ GUI（エディタ・実行・自動コミット付き）
- AI 連携：コミットメッセージ、説明・レビュー、履歴の整理、AI エージェント作業の巻き戻し、MCP サーバー（Anthropic / OpenAI / OpenRouter / Gemini など各社に対応）

このドキュメントは 2 部構成になっている。

| 読む人 | 場所 | 内容 |
|---|---|---|
| 人間（使う人） | [人間向けガイド](#人間向けガイド) | インストール、使い方、GUI、設定 |
| AI（エージェント・AI ツール） | [AI 向けガイド](#ai-向けガイド) | 出力と終了コードの約束事、安全ルール、推奨手順、MCP、コードの構成 |

---

## 人間向けガイド


### インストール

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

### はじめかた

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

### コマンド

コマンドは用途ごとのグループに分かれている。毎日使うものだけはグループ名なしで打てる。
`mig --help`、`mig <グループ> --help` で一覧と説明が出る。

#### 毎日使う

| コマンド | 内容 |
|---|---|
| `mig st` | 変更されたファイルの一覧（シークレットが含まれていれば警告） |
| `mig save [-m MSG] [PATH…]` | 変更を全部コミット。メッセージを省略すると自動で付く |
| `mig sync` | fetch → 自分のコミットを最新の上に付け直し → push |
| `mig log [-n N]` | 番号付きの履歴（1 が最新） |
| `mig diff [REV]` | 未保存の差分、または指定コミットの差分 |
| `mig undo` / `mig redo` | 直前の操作の取り消し／やり直し |
| `mig sw [-c] NAME` | ブランチの切り替え（`-c` で作成してから切り替え） |

#### 履歴 — `mig history`（短縮 `mig h`）

| コマンド | 内容 |
|---|---|
| `mig h fold 2..5` | 2〜5 番のコミットを 1 つに統合。`fold 3` なら 3 番を親と統合、引数なしなら 1 番と 2 番 |
| `mig h reword REV -m MSG` | コミットメッセージを修正 |
| `mig h drop REV` | コミットを履歴から削除 |
| `mig h split REV PATH…` | 指定したパスの変更を 1 つ目、残りを 2 つ目のコミットに分割（`-i` で hunk ごとに選ぶ） |
| `mig h absorb [-n]` | 未保存の修正を、その行を最後に触った未 push のコミットへ自動で合体（`-n` は確認だけ） |
| `mig h log` / `mig h diff` | 毎日使うものと同じ |

#### ブランチ — `mig branch`（短縮 `mig br`）

| コマンド | 内容 |
|---|---|
| `mig br` / `mig br list` | 一覧 |
| `mig br new NAME [REV]` | 作成 |
| `mig br rm NAME` | 削除 |
| `mig br switch NAME` | 切り替え（`mig sw` と同じ） |

#### リモート — `mig remote`

| コマンド | 内容 |
|---|---|
| `mig remote` / `mig remote list` | 一覧 |
| `mig remote add NAME URL` | 追加 |
| `mig remote rm NAME` | 削除 |
| `mig remote publish [NAME] [--private] [--ssh]` | 今のリポジトリを GitHub に作って origin に登録し、push |

#### 操作ログ — `mig op`

| コマンド | 内容 |
|---|---|
| `mig op undo` / `mig op redo` | `mig undo` / `mig redo` と同じ |
| `mig op log [-n N]` | これまでの操作の一覧（取り消したものには `(undone)`） |

#### リポジトリ — `mig repo`

| コマンド | 内容 |
|---|---|
| `mig repo init [PATH]` | 新しいフォルダ、または既存のフォルダをリポジトリにする（下記） |
| `mig repo scan` | 作業ツリー全体の API キー・秘密鍵スキャン |
| `mig repo gc` | オブジェクトをデルタ圧縮した pack にまとめ、不要なファイルを削除 |
| `mig repo size [-n N]` | リポジトリの容量と、履歴中の大きいファイル |

#### AI — `mig ai`

| コマンド | 内容 |
|---|---|
| `mig ai msg` | 未保存の変更からコミットメッセージを提案（`mig save --ai` ならそのままコミット） |
| `mig ai explain [REV]` | コミット（省略時は未保存の変更）が何をしているか説明 |
| `mig ai review [REV]` | コミット（省略時は未保存の変更）の問題点をレビュー |
| `mig ai tidy [-n N] [-y]` | 最近の未 push コミットを意味のまとまりで fold する案を出し、確認して適用（`mig undo` で戻せる） |
| `mig ai config [--provider P] [--model M] [--base-url URL] [--language L]` | 使う AI の設定（引数なしで現在の設定を表示） |
| `mig ai key PROVIDER` / `--clear` | API キーを標準入力から保存／削除 |
| `mig ai providers` | 対応している提供元とキーの状態 |

#### AI エージェントのセッション — `mig agent`

| コマンド | 内容 |
|---|---|
| `mig agent start [NAME]` | 今の作業ツリーを記録してセッション開始 |
| `mig agent cp [-m MSG]` | チェックポイント（コミットはしない） |
| `mig agent log` | チェックポイント一覧 |
| `mig agent restore N` | 作業ツリーをチェックポイント N（または ID）の状態に戻す |
| `mig agent finish [-m MSG] [--ai]` | 開始以降の作業を 1 つのコミットにまとめて終了 |
| `mig agent abort` | 開始以降の作業（ファイルもエージェントのコミットも）を捨てて終了 |
| `mig agent status` | セッションの状態 |

#### AI ツール向け

| コマンド | 内容 |
|---|---|
| `mig context [--diff] [-f PATH…] [-n N]` | リポジトリの概要を Markdown で出力（AI のチャットに貼る用、シークレットは伏せ字） |
| `mig mcp` | MCP サーバーとして起動（下記） |

#### GUI

| コマンド | 内容 |
|---|---|
| `mig gui` | ようこそ画面で起動 |
| `mig gui here` / `mig gui PATH` | 今のフォルダ（または指定したフォルダ）のリポジトリを直接開く。リポジトリでなければようこそ画面 |

#### 共通

- `REV` には `mig log` の番号、`@`（最新）、コミットハッシュ、ブランチ名が使える
- `-C DIR` を付けると、そのフォルダで実行したのと同じになる
- 以前の書き方（`mig init`、`mig fold`、`mig gc`、`mig oplog`、`mig br NAME`、`mig br -d NAME` など）もそのまま使える

---

### リポジトリの作成

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

### 安全チェック

#### シークレット（API キーなど）

AWS / GitHub / OpenAI / Anthropic / Slack / Google / Stripe のキー、秘密鍵、`api_key = "..."` のような推測しにくい値を検出する。

| タイミング | 動作 |
|---|---|
| `mig save` | 見つかったらコミットを止める（`--force` で続行） |
| `mig sync` | 未 push のコミットに含まれていたら push を止める |
| `mig st`、GUI | 未保存の変更に含まれていれば警告（GUI は OS の通知も出す） |

誤検知は、その行に `mig:allow` と書くか `.mig.toml` で除外する。

#### 巨大ファイルと改行コード

- 上限（初期値 10 MB）を超えるファイルは、シークレットと同じくコミットを止める
- CRLF と LF の混在や、ファイル全体の改行コードの変更は警告だけ出す

---

### GUI

`mig gui` で起動する。リポジトリの外ではようこそ画面になり、リポジトリを開く・既存フォルダをリポジトリにする・新規作成ができる。

| 画面 | できること |
|---|---|
| Editor | ファイルツリー、タブ、シンタックスハイライト、実行（F5／停止 Shift+F5）。Ctrl/Cmd+S で保存 |
| ビュワー | Markdown（プレビュー）、SQLite（テーブル一覧と読み取り専用の SQL）、画像（PNG / JPEG / GIF / WebP / BMP）、その他のバイナリ（16 進ダンプ）。タブの上の **Plain** と専用表示を切り替えられる |
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

ボタンにマウスを乗せると短い説明が出る。

大きなフォルダでも画面が止まらないよう、状態の読み込み・コミット・作成は裏で行い、履歴 → 変更一覧 → 差分の順に表示する。

---

### AI 連携

#### 提供元

| `--provider` | 提供元 | API の形式 | キーの環境変数 |
|---|---|---|---|
| `anthropic` | Anthropic | Messages API | `ANTHROPIC_API_KEY` |
| `openai` | OpenAI | Chat Completions | `OPENAI_API_KEY` |
| `openrouter` | OpenRouter | Chat Completions | `OPENROUTER_API_KEY` |
| `gemini` | Google Gemini（OpenAI 互換エンドポイント） | Chat Completions | `GEMINI_API_KEY` |
| `groq` | Groq | Chat Completions | `GROQ_API_KEY` |
| `mistral` | Mistral | Chat Completions | `MISTRAL_API_KEY` |
| `deepseek` | DeepSeek | Chat Completions | `DEEPSEEK_API_KEY` |
| `xai` | xAI | Chat Completions | `XAI_API_KEY` |
| `ollama` | Ollama（ローカル） | Chat Completions | 不要 |
| `custom` | その他の OpenAI 互換 API（`--base-url` 必須） | Chat Completions | 任意 |

```sh
mig ai config --provider openrouter --model anthropic/claude-sonnet-5-5 --language Japanese
mig ai key openrouter        # キーを貼り付けて Enter
mig ai msg
```

- モデルの初期値は Anthropic のみ（`claude-opus-5-5`）。ほかの提供元はモデル名の指定が必要
- キーは環境変数を優先し、なければ `mig ai key` / GUI で保存したもの（設定フォルダの `credentials.toml`、自分だけが読める権限）を使う。リポジトリには保存しない
- **送信前に、シークレットらしい値はすべて `[REDACTED]` に置き換える**（置き換えた数を表示）。それ以外の差分やファイルの中身は選んだ提供元に送られる
- 長すぎる差分は途中で切り、切ったことを AI にも伝える
- 設定は設定フォルダの `mig/ai.toml`。GUI では Settings → AI（Test ボタンで疎通確認できる）

GUI では、ツールバーの **AI**（コミットメッセージ）、History の **Explain** / **Review** / **AI tidy**、コミットの右クリック、**Agent** メニュー、Repo メニューの **Copy context for AI** から使える。

#### エージェントのセッション

Claude Code などの AI エージェントに作業を任せる前に `mig agent start` しておくと、

- エージェントの作業中も `mig agent cp` で細かく状態を残せる（ブランチやコミット履歴は汚さない）
- 終わったら `mig agent finish` で 1 つのきれいなコミットに（`--ai` でメッセージも AI が書く）
- 気に入らなければ `mig agent abort` で開始前の状態に戻る。捨てた作業も `mig agent restore <ID>` で取り戻せる

状態は `refs/mig/agent/*` と `.git/mig/agent.json` に保存され、push されない。

#### MCP サーバー

`mig mcp` は MCP（Model Context Protocol）サーバーとして標準入出力で動く。AI クライアントの設定に次のように登録する（パスは自分のリポジトリに置き換える）。

```json
{
  "mcpServers": {
    "mig": { "command": "mig", "args": ["-C", "/path/to/repo", "mcp"] }
  }
}
```

使えるツール: `status`、`diff`、`log`、`context`、`save`、`undo`、`scan`、`agent_start`、`checkpoint`、`agent_finish`、`agent_abort`。
`save` はシークレットや巨大ファイルがあると拒否し、`diff` / `context` の出力はシークレットを伏せ字にする。

### 設定

GUI の **Settings**（ようこそ画面では下の **Settings…**）から変更できる。

| タブ | 内容 | 保存先 |
|---|---|---|
| Appearance | テーマ（ダーク／ライト／OS に合わせる、初期値はダーク）、画面の拡大率、コードの文字サイズ | 設定フォルダの `mig/settings.toml` |
| Identity | コミットの作者名とメールアドレス | git のグローバル設定 |
| This repository | 自動コミットの行数、チェック・実行コマンド、ファイルサイズの上限、シークレット検出の除外パスと許可リスト | リポジトリの `.mig.toml` |
| AI | 提供元、モデル、ベース URL、回答の言語、API キー、疎通テスト | 設定フォルダの `mig/ai.toml` と `mig/credentials.toml` |
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

### 仕組み

#### undo

すべての操作で、ブランチ・タグ・HEAD の状態を前後で記録する（`.git/mig/oplog.json`）。
undo / redo はその記録に戻し、作業ツリーも合わせる。未保存の変更があるときは止まる（`--force` で破棄して実行）。
`sync` を undo しても手元が戻るだけで、リモートは戻らない。

#### gc

libgit2 には `git gc` 相当の自動整理がないため、mig が自前で行う。
参照・reflog・操作ログ・インデックスから辿れるオブジェクトを 1 つの pack にまとめ、その中に入ったバラバラのオブジェクトと、中身がすべて新しい pack に含まれる古い pack を削除する。
どこからも辿れないオブジェクトは 14 日経ってから削除する。バラバラのオブジェクトが 1000 個を超えると、save の後に自動で実行される。

---

## AI 向けガイド

mig を**操作する** AI エージェント（Claude Code など）と、mig の**コードを読み書きする** AI のための節。
そのまま `AGENTS.md` / `CLAUDE.md` に貼って使ってもよい。人間向けの説明は上の「人間向けガイド」を見ること。

### 1. 要点

- mig は `.git` をそのまま使う。`git` コマンドと混ぜて使ってよいが、**履歴を書き換える操作（fold / reword / drop / split / absorb / agent finish・abort）は mig で行う**こと。undo の記録は mig の操作だけが残す
- 出力は**人間向けのプレーンテキスト**。`--json` は無い。パースするなら `mig log` の番号付き行や `mig st` の `記号 パス` 行など、下の「出力」の形に頼る。構造化データが要るなら MCP（`mig mcp`）を使う
- **コミットや push の前に必ず `mig st` で状態を確認**する。シークレットや巨大ファイルがあると `mig save` / `mig sync` は止まる
- 失敗しても多くの操作は `mig undo` で戻せる（戻せないものは「5. 戻せない・戻りにくいもの」）

### 2. 推奨ワークフロー（エージェントが作業するとき）

```sh
mig st                          # 0. 状態を確認（未保存の変更があれば先に扱いを決める）
mig agent start "作業の名前"     # 1. 作業前の状態を記録（コミットが 1 つ以上ある、ブランチ上にいること）
# ... ファイルを編集 ...
mig agent cp -m "step 1"        # 2. 区切りごとにチェックポイント（コミットは作られない）
# ... さらに編集 ...
mig repo scan                   # 3. シークレットが混ざっていないか確認
mig agent finish -m "変更の要約" # 4a. 開始以降の作業を 1 つのコミットにして終了
mig agent abort                 # 4b. または、すべて捨てて開始前に戻して終了
```

- セッションは同時に 1 つだけ。`mig agent status` で現在の状態が分かる
- `finish` は、開始以降に自分で作ったコミットと未保存の変更をまとめて**1 つのコミット**にする。push はしない（公開するなら人間の指示を受けてから `mig sync`）
- `abort` は、ファイルの内容とブランチの位置を**開始時点に戻す**。開始時点で既にあった未保存の変更は残る。捨てた作業は `refs/mig/saved` に残り、`mig agent restore <表示された ID>` で取り戻せる
- `cp` は変更が無ければ何も作らず「nothing changed since the last checkpoint」を出す（終了コードは 0）
- `mig agent restore N` は作業ツリーをチェックポイント N（`mig agent log` の番号）の状態に戻す。戻す前の状態も自動で保存される

セッションを使わない小さな作業なら、`mig st` → 編集 → `mig save -m "..."` で十分。

### 3. やってはいけないこと

| してはいけない | 理由と代わりの手段 |
|---|---|
| ユーザーの許可なく `--force` を付ける | `save --force` はシークレットや巨大ファイルのチェックを飛ばす。`sync --force` は秘密情報の入ったコミットを push する。まず `mig repo scan` で内容を確認し、誤検知なら該当行に `mig:allow` を書くか `.mig.toml` で除外する |
| ユーザーの許可なく `mig sync` で push する | push は mig では取り消せない（`mig undo` は手元の状態しか戻さない） |
| push 済みのコミットを `fold` / `drop` / `reword` / `split` する | 公開済みの履歴を書き換えると他の人と食い違う。mig はこの操作を止めないので、自分で気を付ける。`absorb` は未 push のコミットしか対象にしない。迷ったら人間に確認する |
| `mig undo --force` / `mig redo --force` | 未保存の変更を**捨てる**。通常の `undo` は未保存の変更があると止まるので、先に `mig save` するか `mig agent cp` で残す |
| `mig h split -i` | hunk ごとに標準入力で y/n を聞く対話式。エージェントからはパス指定の `mig h split REV PATH…` を使う |
| `.mig.toml` や `.gitignore` を黙って緩める | 検出や除外の設定を変えるときは、変えたことをユーザーに伝える |
| API キーを出力・コミット・メッセージに含める | `mig context` / MCP の出力と AI への送信内容は自動で `[REDACTED]` に置き換わるが、ファイルの中身そのものは自分で扱いに注意する |

### 4. 出力と終了コード

| 項目 | 内容 |
|---|---|
| 終了コード | `0` 成功、`1` 失敗（実行時のエラー、シークレット等で止められた場合を含む）、`2` コマンドの使い方の誤り |
| 通常の結果 | 標準出力 |
| エラー・警告・シークレットの指摘 | 標準エラー出力。エラーは `error: ...` の 1 行、警告は `warning: ...`、補足は `note: ...` |
| 色 | 標準出力が端末のときだけ付く。パイプやリダイレクトでは付かない。`NO_COLOR` を設定すると端末でも付かない |
| 作業フォルダ | `-C DIR` を付けると、そのフォルダで実行したのと同じ。付けなければカレントディレクトリ（親をたどってリポジトリを探す） |
| 質問される操作 | `mig ai tidy`（`-y` が無く標準入力が端末のときだけ。端末でなければ案を表示して「re-run with --yes to apply」と出し、**何も変更せず終了コード 0**）、`mig h split -i`、`mig ai key`（キーを標準入力から読む） |

主な出力の形（行頭の空白は省略）:

```text
mig st      on main                         # 1 行目: ブランチ
            M a.txt                         # 以降: 記号 パス（M 変更 / A 追加 / D 削除 / R 改名 / ? 未追跡 / C 衝突）
            nothing changed                 # 変更が無いとき
mig log     1  3523821  2026-10-15 14:14  first (main, origin/main)   # 番号 短縮ハッシュ 日時 件名 (ブランチ・タグ)
mig save    saved 3523821 first
mig diff    通常の unified diff
```

シークレットの指摘は標準エラーに次の形で出る（`mig save` / `mig sync` / `mig st`）。

```text
⚠ 1 possible secret(s) found:
  s.py:1  AWS access key  AKIAAB… (20 chars)
```

### 5. 戻せない・戻りにくいもの

| 操作 | 戻し方 |
|---|---|
| `save` / `fold` / `reword` / `drop` / `split` / `absorb` / `ai tidy` / `agent finish` / `agent abort` / `sw` / `br` | `mig undo`（`mig op log` で履歴が見える）。未保存の変更があると止まる |
| `mig sync` | 手元の状態は `mig undo` で戻るが、**push 済みのものはリモートで戻らない** |
| `agent abort` で捨てた作業 | `mig agent restore <ID>`（ID は abort 時に表示される `refs/mig/saved` のコミット） |
| `mig undo --force` で捨てた未保存の変更 | 戻らない |
| ファイルの直接編集・削除（mig を通さない変更） | `agent cp` を取っていればそこへ `mig agent restore N`。取っていなければ戻らない |

### 6. コマンドのまとめ（エージェント向け）

| 目的 | コマンド | 備考 |
|---|---|---|
| 状態 | `mig st` | 副作用なし |
| 履歴を見る | `mig log -n N` / `mig diff [REV]` | `REV` は `mig log` の番号、`@`（最新）、ハッシュ、ブランチ名 |
| 全部コミット | `mig save -m "..."` | 変更が無いとエラー（終了コード 1）。`PATH…` で一部だけ |
| 区切りを残す | `mig agent cp -m "..."` | コミットを作らない |
| 作業の開始・終了 | `mig agent start/finish/abort` | 上の「推奨ワークフロー」 |
| シークレット確認 | `mig repo scan` | 見つかると終了コード 1 |
| 取り消し | `mig undo` / `mig redo` | |
| コミットを整える | `mig h fold A..B -m "..."` / `mig h reword REV -m "..."` / `mig h drop REV` / `mig h split REV PATH…` / `mig h absorb` | `absorb -n` は何も変えず、どの変更がどのコミットに入るかだけ表示 |
| ブランチ | `mig br new NAME` / `mig sw NAME` / `mig sw -c NAME` | |
| 公開 | `mig sync` | **ユーザーの許可を得てから** |
| 人間向けの要約を作る | `mig ai msg` / `mig ai explain [REV]` / `mig ai review [REV]` | AI の提供元とキーの設定が必要（無ければ `error: no API key ...`） |
| 状況を AI に渡す | `mig context [--diff] [-f PATH]… [-n N]` | Markdown。シークレットは `[REDACTED]` |

### 7. MCP サーバー

`mig -C /path/to/repo mcp` を起動すると、標準入出力で MCP（JSON-RPC 2.0、1 行 1 メッセージ）を話す。標準出力にはプロトコルのメッセージしか出さない。
登録例は人間向けガイドの「MCP サーバー」を参照。`initialize` の `instructions` には「`agent_start` → `checkpoint` → `agent_finish`（または `agent_abort`）の順で使うこと」と入れてある。

| ツール | 引数（必須は太字） | 返すもの |
|---|---|---|
| `status` | なし | ブランチ、変更ファイル（`記号 パス`）、エージェントセッションの状態 |
| `diff` | `rev`（省略時は未保存の変更） | unified diff（シークレットは `[REDACTED]`、長すぎる場合は途中で切る） |
| `log` | `count`（1〜200、既定 20） | `番号 短縮ハッシュ 件名` の行 |
| `context` | `diff`（真偽値）、`files`（パスの配列） | Markdown の概要（シークレットは `[REDACTED]`） |
| `save` | **`message`** | `saved <ハッシュ>`。シークレットや巨大ファイルがあると、場所つきのエラーで拒否 |
| `undo` | なし | `undid: <操作名>` |
| `scan` | なし | 見つかったシークレットの一覧（値は伏せ字）、または `no secrets found` |
| `agent_start` | `name` | セッションの開始メッセージ |
| `checkpoint` | `message` | `checkpoint <ハッシュ>`、または変更が無い旨 |
| `agent_finish` | **`message`** | 終了メッセージと新しい HEAD |
| `agent_abort` | なし | 終了メッセージと、捨てた作業の保存先 |

ツールの失敗は JSON-RPC のエラーではなく、結果の `isError: true` と本文のエラー文で返る。MCP からは push（`sync`）や `--force` は使えない。

### 8. 状態の保存場所

| 場所 | 内容 |
|---|---|
| `.git/mig/oplog.json` | undo / redo 用の操作ログ（最大 500 件） |
| `.git/mig/agent.json` | 進行中のエージェントセッション |
| `refs/mig/agent/start` / `base` / `cp` | セッション開始時の HEAD、開始時の作業ツリーのスナップショット、最新のチェックポイント。チェックポイントは `cp` から親をたどる |
| `refs/mig/saved` | 直近に捨てた（`abort` / `restore` の前の）作業のスナップショット |
| `.mig.toml`（リポジトリ直下） | `[editor]` `[save]` `[scan]` の設定 |
| 設定フォルダの `mig/ai.toml` / `credentials.toml` / `settings.toml` / `recent.json` | AI の設定、API キー（自分だけが読める権限）、GUI の設定、最近開いたリポジトリ |

`refs/mig/*` は push されない。`git gc` や他のツールが参照されていないオブジェクトを消さないよう、ref として残している。

### 9. AI 連携で外に出る情報

- AI への送信前に、シークレットらしい値（AWS / GitHub / OpenAI / Anthropic / Slack / Google / Stripe のキー、秘密鍵、`api_key = "..."` のような値）を `[REDACTED]` に置き換える。置き換えた数は標準エラーに `note:` で出る
- それ以外の差分やファイルの中身は、選んだ提供元（`mig ai config` で確認）に送られる。長すぎる差分は 12 万バイトで切り、切ったことを AI に伝える
- API キーは環境変数、なければ設定フォルダの `credentials.toml`。リポジトリには書かない
- Anthropic にはサーバー側のフォールバック（`fallbacks: "default"`）を付けて送る。回答が拒否されたときは `error: the model declined this request: ...` で終わる

### 10. コードを読み書きする AI のために

```sh
cargo build                         # GUI 付き（既定）
cargo build --no-default-features   # CLI のみ。警告が出ないこと
cargo test
cargo fmt && cargo clippy
```

- Rust 2024、字下げは**スペース 2 つ**（`rustfmt.toml`）。コメントは必要最小限
- GUI は `gui` フィーチャの後ろ。GUI 専用のモジュールには `#[cfg(feature = "gui")]` を付け、CLI のみのビルドで未使用の警告が出ないようにする
- Git の操作は `git2`（libgit2）。`git` コマンドは呼ばない。git2 0.21 では `Commit::summary()` が `Result<Option<&str>>`、`message()` が `Result<&str>` など戻り値の形が古い資料と違うので、`repo::summary` / `repo::message` を使う
- 履歴を変える操作は必ず `oplog::record(repo, "名前", || ...)` で包む。そうしないと `mig undo` で戻せない
- GUI で時間のかかる処理は別スレッドで行い、スレッド内で `repo::open` し直す（`git2::Repository` はスレッド間で共有できない）

| ファイル | 役割 |
|---|---|
| `main.rs` / `cli.rs` | 入口、コマンド定義（clap）、グループ分けとヘルプ、出力の整形 |
| `repo.rs` / `rev.rs` | リポジトリ操作の補助、`REV`（番号・`@`・ハッシュ）の解決 |
| `ops.rs` / `oplog.rs` | save・fold・reword・drop・branch・sync と、undo / redo の記録 |
| `absorb.rs` / `split.rs` / `patch.rs` | absorb、split、hunk の適用 |
| `scan.rs` / `guard.rs` | シークレットの検出と伏せ字、巨大ファイル・改行コードの検査 |
| `gc.rs` | pack への整理と容量の集計 |
| `setup.rs` / `github.rs` | リポジトリの作成・リモート・GitHub への作成 |
| `config.rs` | `.mig.toml` と、プロジェクトの種類の判定 |
| `ai.rs` / `assist.rs` | 提供元ごとの API 呼び出し、コミットメッセージ・説明・レビュー・tidy・context |
| `agent.rs` / `mcp.rs` | エージェントセッション、MCP サーバー |
| `gui.rs` / `editor.rs` / `viewer.rs` / `welcome.rs` / `settings.rs` / `aiui.rs` | GUI（画面全体、エディタ、ビュワー、ようこそ画面、設定、AI まわり） |

テストの流儀: 実際の外部サービスには繋がず、手元に偽のサーバー（GitHub API、Anthropic / OpenAI 互換 API）を立てて確認している。`MIG_GITHUB_API` で GitHub API の向き先を変えられる。AI の向き先は `mig ai config --base-url` で変える。

### 11. 既知の制限

- `--json` 出力は無い（構造化データが必要なら MCP）
- AI 機能と GitHub への作成は、本物のサービスでの動作確認がまだ。偽のサーバーでの確認のみ
- `mig sync` の undo はリモートに及ばない
- `absorb` は未 push のコミットだけが対象。push 済みの行を触った変更は作業ツリーに残る
- `fold` / `drop` / `split` はマージコミットを扱えない
- Windows と macOS では未確認（Linux で開発・確認している）
