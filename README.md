# mig

migはgit互換のバージョン管理ツールです。(mig insn't git)

- 履歴の編集が簡単です（統合・分割・削除・メッセージ修正）
- どんな操作も `mig undo` で取り消せます
- API キーなどを自動検知してコミットを停止します。
- ネイティブ GUI を備えています(DataBaseやバイナリも対応)
- AI 連携に対応しています：コミットメッセージ、説明・レビュー、履歴の整理、AI エージェント作業の巻き戻し、MCP サーバー（Anthropic / OpenAI / OpenRouter / Gemini など各社に対応）

このドキュメントは 2 部構成です。

| 読む人 | 場所 | 内容 |
|---|---|---|
| 人間（使う人） | [人間向けガイド](#人間向けガイド) | インストール、使い方、GUI、設定 |
| AI（エージェント・AI ツール） | [AI 向けガイド](#ai-向けガイド) | 出力と終了コードの約束事、安全ルール、推奨手順、MCP、コードの構成 |

---

## 人間向けガイド


### 必要用件


| OS | 事前に必要なもの |
|---|---|
| Windows | Visual Studio Build Tools（C++） |
| macOS | `xcode-select --install`、`brew install openssl@3` |
| Linux | `build-essential pkg-config libssl-dev`、GUI 用に `libxkbcommon-x11-0` と OpenGL ドライバ |


### 使い方

```sh
# 既存のフォルダをリポジトリにし、最初のコミットと GitHub への push まで
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

コマンドは用途ごとのグループに分かれています。毎日使うものだけは、グループ名なしで打てます。
`mig --help` や `mig <グループ> --help` で、一覧と説明が出ます。

#### 毎日使う

| コマンド | 内容 |
|---|---|
| `mig st` | 変更されたファイルの一覧（シークレットが含まれていれば警告） |
| `mig save [-m MSG] [PATH…]` | 変更を全部コミットします。 |
| `mig sync` | fetch → 自分のコミットを最新の上に付け直しをし、pushします。 |
| `mig log [-n N]` | 番号付きの履歴（1 が最新） |
| `mig diff [REV]` | 未保存の差分、または指定コミットの差分 |
| `mig undo` / `mig redo` | 直前の操作の取り消し／やり直し |
| `mig sw [-c] NAME` | ブランチの切り替え（`-c` で作成してから切り替え） |

#### 履歴 — `mig history`（短縮 `mig h`）

| コマンド | 内容 |
|---|---|
| `mig h fold 2..5` | 2〜5 番のコミットを 1 つに統合します。`fold 3` なら 3 番を親と統合、引数なしなら 1 番と 2 番を統合します |
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
| `mig repo size --graph` | サイズの推移を棒グラフで表示（下記） |

サイズの推移は、現在のブランチを最初のコミットから順にたどって、各時点で追跡されているファイルの合計を出します。計算結果は `.git/mig/growth.json` に保存されるので、2回目からは増えたコミット分だけを計算します。履歴を書き換えた（fold など）ときは自動で作り直します。

```
mig repo size --graph                         # コミットごと（既定）
mig repo size --graph --by month              # 月ごと（day / week / month）
mig repo size --graph --metric lines          # 行数（size / files / lines）
mig repo size --graph --last 200              # 新しい方から200コミット（既定 1000）
mig repo size --json --by week                # 数値を JSON で出力
```

ターミナルのグラフは40行までに間引いて表示します。行数はテキストファイルだけを数え、4MB を超えるファイルは数えません。

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
| `mig gui here` / `mig gui PATH` | 今のフォルダ（または指定したフォルダ）のリポジトリを直接開きます。リポジトリでなければ、ようこそ画面を開きます |

#### 共通

- `REV` には、`mig log` の番号、`@`（最新）、コミットハッシュ、ブランチ名が使えます
- `-C DIR` を付けると、そのフォルダで実行したのと同じになります
- 以前の書き方（`mig init`、`mig fold`、`mig gc`、`mig oplog`、`mig br NAME`、`mig br -d NAME` など）も、そのまま使えます

---

### リポジトリの作成

```sh
mig repo init my-app                       # 新しいフォルダを作ってリポジトリに
mig repo init --save                       # 今のフォルダをリポジトリにして最初のコミットまで
mig repo init --save --github --private    # さらに GitHub に作って push
mig repo init --github myorg/tool          # Organization に作る
mig repo init --remote URL                 # origin を登録
```

- `.gitignore` がなければ、プロジェクトの種類（Rust / Node.js / Go / Python）に合わせて作ります。どの種類でも `.env` と OS のゴミファイルは除外します（`--no-ignore` で作らないこともできます）
- `--save` の最初のコミットも、シークレットと巨大ファイルのチェックを通ります。引っかかった場合は、コミットだけが止まります
- 別のリポジトリの中には作りません。すでにリポジトリになっているフォルダでは、`--save` / `--remote` / `--github` だけを適用します

GitHub への作成（`--github`、`mig remote publish`）には、`repo` 権限のあるトークンが必要です。
環境変数 `GITHUB_TOKEN`（または `GH_TOKEN`）か、`gh auth login` 済みの `gh` コマンドから読み取ります。mig 自身はトークンを保存しません。
HTTPS で push するとき、git の認証ヘルパーが使えない場合も、github.com ならこのトークンで push します。

---

### 安全チェック

#### シークレット（API キーなど）

AWS / GitHub / OpenAI / Anthropic / Slack / Google / Stripe のキー、秘密鍵、`api_key = "..."` のような推測しにくい値を検出します。

| タイミング | 動作 |
|---|---|
| `mig save` | 見つかったらコミットを止める（`--force` で続行） |
| `mig sync` | 未 push のコミットに含まれていたら push を止める |
| `mig st`、GUI | 未保存の変更に含まれていれば警告（GUI は OS の通知も出す） |

誤検知は、その行に `mig:allow` と書くか、`.mig.toml` で除外してください。

#### 巨大ファイルと改行コード

- 上限（初期値 10 MB）を超えるファイルは、シークレットと同じようにコミットを止めます
- CRLF と LF の混在や、ファイル全体の改行コードの変更は、警告だけ出します

---

### GUI

`mig gui` で起動します。リポジトリの外ではようこそ画面になり、リポジトリを開く・既存のフォルダをリポジトリにする・新規作成する、のどれかを選べます。

| 画面 | できること |
|---|---|
| Editor | ファイルツリー、タブ、シンタックスハイライト、実行（F5／停止 Shift+F5）。保存は Ctrl/Cmd+S です |
| ビュワー | Markdown（プレビュー）、SQLite（テーブル一覧と読み取り専用の SQL）、画像（PNG / JPEG / GIF / WebP / BMP）、その他のバイナリ（16 進ダンプ）。タブの上の **Plain** と専用表示を切り替えられます |
| History | 履歴と差分を表示します。複数選択して Fold でき、右クリックで Reword / Split / Fold / Drop を選べます |
| Size | リポジトリが大きくなってきた様子をグラフで表示します。横軸は「コミットごと」と「時間（日・週・月）」を切り替えられ、測るものはファイルの合計サイズ・ファイル数・行数から選べます。点にマウスを乗せるとコミットと増減が出て、クリックするとそのコミットを History で開きます |
| ツールバー | Commit、Absorb、Sync、Undo、Redo、Repo メニュー（リポジトリの切り替え、GitHub への公開）、Settings |

**自動コミット**: エディタで保存するたびに、前回のコミットからの変更行数を数えます。上限（初期値 200 行）を超えたらチェックコマンドを実行し、成功すれば `auto: ...` というメッセージで自動コミットします（push はしません）。失敗した場合はコミットせず、出力パネルにエラーを出します。

チェック・実行コマンドは、設定がなければプロジェクトから推測します。

| プロジェクト | チェック | 実行 |
|---|---|---|
| Rust（`Cargo.toml`） | `cargo check` | `cargo run` |
| Node.js（`package.json`） | `npm run build --if-present` | `npm start` |
| Go（`go.mod`） | `go build ./...` | `go run .` |
| Python（`*.py`） | 全 `.py` の構文チェック | `python main.py` |

ボタンにマウスを乗せると、短い説明が出ます。

大きなフォルダでも画面が止まらないよう、状態の読み込み・コミット・作成は裏で行い、履歴 → 変更一覧 → 差分の順に表示します。

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

- モデルの初期値があるのは Anthropic（`claude-opus-5-5`）だけです。ほかの提供元では、モデル名を指定する必要があります
- キーは環境変数を優先し、なければ `mig ai key` や GUI で保存したもの（設定フォルダの `credentials.toml`、自分だけが読める権限）を使います。リポジトリには保存しません
- **送信前に、シークレットらしい値はすべて `[REDACTED]` に置き換えます**（置き換えた数も表示します）。それ以外の差分やファイルの中身は、選んだ提供元に送られます
- 長すぎる差分は途中で切り、切ったことを AI にも伝えます
- 設定は設定フォルダの `mig/ai.toml` に保存されます。GUI では Settings → AI から変更でき、Test ボタンで疎通を確認できます

GUI では、ツールバーの **AI**（コミットメッセージ）、History の **Explain** / **Review** / **AI tidy**、コミットの右クリック、**Agent** メニュー、Repo メニューの **Copy context for AI** から使えます。

#### エージェントのセッション

Claude Code などの AI エージェントに作業を任せる前に `mig agent start` しておくと、次のことができます。

- エージェントの作業中も、`mig agent cp` で細かく状態を残せます（ブランチやコミット履歴は汚れません）
- 終わったら、`mig agent finish` で 1 つのきれいなコミットにまとめられます（`--ai` を付けると、メッセージも AI が書きます）
- 気に入らなければ、`mig agent abort` で開始前の状態に戻せます。捨てた作業も、`mig agent restore <ID>` で取り戻せます

状態は `refs/mig/agent/*` と `.git/mig/agent.json` に保存され、push されません。

#### MCP サーバー

`mig mcp` は、MCP（Model Context Protocol）サーバーとして標準入出力で動きます。AI クライアントの設定に、次のように登録します（パスは自分のリポジトリに置き換えてください）。

```json
{
  "mcpServers": {
    "mig": { "command": "mig", "args": ["-C", "/path/to/repo", "mcp"] }
  }
}
```

Claude Code には、次のコマンドで登録できます。

```sh
claude mcp add mig -- mig mcp                  # 今のプロジェクトだけ
claude mcp add --scope user mig -- mig mcp     # どのプロジェクトでも
```

使えるツールは次のとおりです。

- バージョン管理: `status`、`diff`、`log`、`context`、`save`、`undo`、`scan`
- エージェントセッション: `agent_start`、`checkpoint`、`agent_finish`、`agent_abort`
- ファイル操作: `ls`、`read`、`read_many`、`search`、`edit`、`write`、`move`、`delete`

`save` はシークレットや巨大ファイルがあると拒否し、`diff` と `context` の出力ではシークレットを伏せ字にします。

ファイル操作のツールは、標準の filesystem サーバーより少ないやり取りと文字数で使えるように作ってあります。`.gitignore` に従い、行の範囲を指定して読め、中身を正規表現で検索でき、編集は完全一致の置換（複数まとめて、全部成功か全部失敗）です。常にリポジトリの中だけを対象にし、`.git` には触れません。読むときはシークレットを `[REDACTED]` に伏せ、書き込む内容にシークレットが混ざっていたら断ります。エージェントセッションの途中なら、書き込み・移動・削除のたびに自動でチェックポイントを作るので、`agent_abort` で全部元に戻せます。

### 設定

GUI の **Settings**（ようこそ画面では、下の **Settings…**）から変更できます。

| タブ | 内容 | 保存先 |
|---|---|---|
| Appearance | テーマ（ダーク／ライト／OS に合わせる、初期値はダーク）、画面の拡大率、コードの文字サイズ | 設定フォルダの `mig/settings.toml` |
| Identity | コミットの作者名とメールアドレス | git のグローバル設定 |
| This repository | 自動コミットの行数、チェック・実行コマンド、ファイルサイズの上限、シークレット検出の除外パスと許可リスト | リポジトリの `.mig.toml` |
| AI | 提供元、モデル、ベース URL、回答の言語、API キー、疎通テスト | 設定フォルダの `mig/ai.toml` と `mig/credentials.toml` |
| GitHub | トークンが見つかるかの確認 | — |

`.mig.toml` は手で書いても構いません。GUI から保存したときは、変更した項目だけを書き換え、コメントは残します。

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

設定フォルダは、Linux なら `~/.config/mig/`、macOS なら `~/Library/Application Support/mig/`、Windows なら `%APPDATA%\mig\` です。最近開いたリポジトリの一覧（`recent.json`）もここに入ります。

---

### 仕組み

#### undo

すべての操作で、ブランチ・タグ・HEAD の状態を前後で記録します（`.git/mig/oplog.json`）。
undo / redo は、その記録の状態に戻し、作業ツリーも合わせます。未保存の変更があるときは止まります（`--force` を付けると、破棄して実行します）。
`sync` を undo しても戻るのは手元だけで、リモートは戻りません。

#### gc

libgit2 には `git gc` に相当する自動整理がないため、mig が自前で行います。
参照・reflog・操作ログ・インデックスから辿れるオブジェクトを 1 つの pack にまとめ、その中に入ったバラバラのオブジェクトと、中身がすべて新しい pack に含まれる古い pack を削除します。
どこからも辿れないオブジェクトは、14 日経ってから削除します。バラバラのオブジェクトが 1000 個を超えると、save の後に自動で実行されます。

---

## AI 向けガイド

mig を**操作する** AI エージェント（Claude Code など）と、mig の**コードを読み書きする** AI のための節です。
そのまま `AGENTS.md` や `CLAUDE.md` に貼って使っても構いません。人間向けの説明は、上の「人間向けガイド」を見てください。

### 1. 要点

- mig は `.git` をそのまま使います。`git` コマンドと混ぜて使っても構いませんが、**履歴を書き換える操作（fold / reword / drop / split / absorb / agent finish・abort）は mig で行ってください**。undo の記録を残すのは、mig の操作だけです
- 出力は**人間向けのプレーンテキスト**で、`--json` はありません。パースする場合は、`mig log` の番号付き行や `mig st` の `記号 パス` 行など、下の「出力と終了コード」にある形に頼ってください。構造化データが必要なら MCP（`mig mcp`）を使います
- **コミットや push の前に、必ず `mig st` で状態を確認してください**。シークレットや巨大ファイルがあると、`mig save` と `mig sync` は止まります
- 失敗しても、多くの操作は `mig undo` で戻せます（戻せないものは「5. 戻せない・戻りにくいもの」を参照してください）

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

- セッションは同時に 1 つだけ持てます。`mig agent status` で現在の状態を確認できます
- `finish` は、開始以降に自分で作ったコミットと未保存の変更を、**1 つのコミット**にまとめます。push はしません（公開する場合は、人間の指示を受けてから `mig sync` を実行してください）
- `abort` は、ファイルの内容とブランチの位置を**開始時点に戻します**。開始時点で既にあった未保存の変更は残ります。捨てた作業は `refs/mig/saved` に残り、`mig agent restore <表示された ID>` で取り戻せます
- `cp` は、変更がなければ何も作らず、「nothing changed since the last checkpoint」と表示します（終了コードは 0 です）
- `mig agent restore N` は、作業ツリーをチェックポイント N（`mig agent log` の番号）の状態に戻します。戻す前の状態も自動で保存されます

セッションを使わない小さな作業なら、`mig st` → 編集 → `mig save -m "..."` で十分です。

### 3. やってはいけないこと

| してはいけない | 理由と代わりの手段 |
|---|---|
| ユーザーの許可なく `--force` を付ける | `save --force` はシークレットや巨大ファイルのチェックを飛ばし、`sync --force` は秘密情報の入ったコミットを push してしまいます。まず `mig repo scan` で内容を確認し、誤検知なら該当行に `mig:allow` を書くか、`.mig.toml` で除外してください |
| ユーザーの許可なく `mig sync` で push する | push は mig では取り消せない（`mig undo` は手元の状態しか戻さない） |
| push 済みのコミットを `fold` / `drop` / `reword` / `split` する | 公開済みの履歴を書き換えると、他の人の履歴と食い違います。mig はこの操作を止めないので、自分で気を付けてください。`absorb` は未 push のコミットしか対象にしません。迷ったら人間に確認してください |
| `mig undo --force` / `mig redo --force` | 未保存の変更を**捨ててしまいます**。通常の `undo` は未保存の変更があると止まるので、先に `mig save` するか、`mig agent cp` で残してください |
| `mig h split -i` | hunk ごとに標準入力で y/n を聞く対話式です。エージェントからは、パスを指定する `mig h split REV PATH…` を使ってください |
| `.mig.toml` や `.gitignore` を黙って緩める | 検出や除外の設定を変えるときは、変えたことをユーザーに伝える |
| API キーを出力・コミット・メッセージに含める | `mig context` / MCP の出力と AI への送信内容は自動で `[REDACTED]` に置き換わるが、ファイルの中身そのものは自分で扱いに注意する |

### 4. 出力と終了コード

| 項目 | 内容 |
|---|---|
| 終了コード | `0` 成功、`1` 失敗（実行時のエラー、シークレット等で止められた場合を含む）、`2` コマンドの使い方の誤り |
| 通常の結果 | 標準出力 |
| エラー・警告・シークレットの指摘 | 標準エラー出力です。エラーは `error: ...` の 1 行、警告は `warning: ...`、補足は `note: ...` の形です |
| 色 | 標準出力が端末のときだけ付きます。パイプやリダイレクトでは付きません。`NO_COLOR` を設定すると、端末でも付きません |
| 作業フォルダ | `-C DIR` を付けると、そのフォルダで実行したのと同じです。付けなければカレントディレクトリが対象で、親をたどってリポジトリを探します |
| 質問される操作 | `mig ai tidy`（`-y` がなく標準入力が端末のときだけ。端末でなければ案を表示して「re-run with --yes to apply」と出し、**何も変更せず終了コード 0 で終わります**）、`mig h split -i`、`mig ai key`（キーを標準入力から読みます） |

主な出力の形は次のとおりです（行頭の空白は省略しています）。

```text
mig st      on main                         # 1 行目: ブランチ
            M a.txt                         # 以降: 記号 パス（M 変更 / A 追加 / D 削除 / R 改名 / ? 未追跡 / C 衝突）
            nothing changed                 # 変更が無いとき
mig log     1  3523821  2026-10-15 14:14  first (main, origin/main)   # 番号 短縮ハッシュ 日時 件名 (ブランチ・タグ)
mig save    saved 3523821 first
mig diff    通常の unified diff
```

シークレットの指摘は、標準エラーに次の形で出ます（`mig save` / `mig sync` / `mig st`）。

```text
⚠ 1 possible secret(s) found:
  s.py:1  AWS access key  AKIAAB… (20 chars)
```

### 5. 戻せない・戻りにくいもの

| 操作 | 戻し方 |
|---|---|
| `save` / `fold` / `reword` / `drop` / `split` / `absorb` / `ai tidy` / `agent finish` / `agent abort` / `sw` / `br` | `mig undo`（`mig op log` で履歴を確認できます）。未保存の変更があると止まります |
| `mig sync` | 手元の状態は `mig undo` で戻るが、**push 済みのものはリモートで戻らない** |
| `agent abort` で捨てた作業 | `mig agent restore <ID>`（ID は abort 時に表示される `refs/mig/saved` のコミット） |
| `mig undo --force` で捨てた未保存の変更 | 戻らない |
| ファイルの直接編集・削除（mig を通さない変更） | `agent cp` を取っていれば、`mig agent restore N` で戻せます。取っていなければ戻りません |

### 6. コマンドのまとめ（エージェント向け）

| 目的 | コマンド | 備考 |
|---|---|---|
| 状態 | `mig st` | 副作用なし |
| 履歴を見る | `mig log -n N` / `mig diff [REV]` | `REV` は `mig log` の番号、`@`（最新）、ハッシュ、ブランチ名 |
| 全部コミット | `mig save -m "..."` | 変更がないとエラー（終了コード 1）になります。`PATH…` で一部だけを対象にできます |
| 区切りを残す | `mig agent cp -m "..."` | コミットを作らない |
| 作業の開始・終了 | `mig agent start/finish/abort` | 上の「推奨ワークフロー」 |
| シークレット確認 | `mig repo scan` | 見つかると終了コード 1 |
| サイズの推移 | `mig repo size --json [--by commit\|day\|week\|month]` | 各点の `bytes` / `files` / `lines`（古い順）。`--last N` で新しい方から N コミット |
| 取り消し | `mig undo` / `mig redo` | |
| コミットを整える | `mig h fold A..B -m "..."` / `mig h reword REV -m "..."` / `mig h drop REV` / `mig h split REV PATH…` / `mig h absorb` | `absorb -n` は何も変えず、どの変更がどのコミットに入るかだけ表示 |
| ブランチ | `mig br new NAME` / `mig sw NAME` / `mig sw -c NAME` | |
| 公開 | `mig sync` | **ユーザーの許可を得てから** |
| 人間向けの要約を作る | `mig ai msg` / `mig ai explain [REV]` / `mig ai review [REV]` | AI の提供元とキーの設定が必要（無ければ `error: no API key ...`） |
| 状況を AI に渡す | `mig context [--diff] [-f PATH]… [-n N]` | Markdown です。シークレットは `[REDACTED]` になります |

### 7. MCP サーバー

`mig -C /path/to/repo mcp` を起動すると、標準入出力で MCP（JSON-RPC 2.0、1 行 1 メッセージ）を話します。標準出力には、プロトコルのメッセージしか出しません。
登録例は、人間向けガイドの「MCP サーバー」を参照してください。`initialize` の `instructions` には、「先に `agent_start` を呼ぶと、書き込みのたびにチェックポイントされる」「全文を読むより `search` と範囲指定の `read`、`write` より `edit`」と入れてあります。`write` / `edit` / `move` / `delete` の結果には、セッションが無いときは「元に戻せない」旨、ある時は `checkpoint <ハッシュ>` が付きます。

| ツール | 引数（必須は太字） | 返すもの |
|---|---|---|
| `status` | なし | ブランチ、変更ファイル（`記号 パス`）、エージェントセッションの状態 |
| `diff` | `rev`（省略時は未保存の変更） | unified diff（シークレットは `[REDACTED]`、長すぎる場合は途中で切る） |
| `log` | `count`（1〜200、既定 20） | `番号 短縮ハッシュ 件名` の行 |
| `context` | `diff`（真偽値）、`files`（パスの配列） | Markdown の概要（シークレットは `[REDACTED]`） |
| `save` | **`message`** | `saved <ハッシュ>`。シークレットや巨大ファイルがあると、場所つきのエラーで拒否します |
| `undo` | なし | `undid: <操作名>` |
| `scan` | なし | 見つかったシークレットの一覧（値は伏せ字）、または `no secrets found` |
| `agent_start` | `name` | セッションの開始メッセージ |
| `checkpoint` | `message` | `checkpoint <ハッシュ>`、または変更が無い旨 |
| `agent_finish` | **`message`** | 終了メッセージと新しい HEAD |
| `agent_abort` | なし | 終了メッセージと、捨てた作業の保存先 |
| `ls` | `path`（既定 `.`）、`depth`（1〜6、既定 1） | `.gitignore` を除いた一覧。ファイルはサイズと変更記号（`M` / `A` / `?`）つき |
| `read` | **`path`**、`from`、`to` | 見出し1行（行数・サイズ・範囲）と、行番号つきの本文。既定は 400 行まで（上限 2000）。続きがあれば `from=` の案内が付く。バイナリと 8MB 超は本文なし。シークレットは `[REDACTED]` |
| `read_many` | **`files`**（パス、または `{path, from, to}` の配列） | `read` をまとめた結果。失敗したファイルがあっても他は読む |
| `search` | **`pattern`**（正規表現）、`path`、`glob`、`context`（0〜5）、`max`（既定 50）、`ignore_case` | `パス:行:本文`（前後の行は `パス-行-本文`）。2MB 超とバイナリは対象外 |
| `edit` | **`path`**、**`edits`**（`{old, new, all}` の配列）、`dry_run` | 小さい unified diff。`old` は 1 回だけ一致する必要があり、複数あれば行番号を出して断る。シークレットらしい `new` は拒否 |
| `write` | **`path`**、**`content`** | `created` または `overwrote`（増減行つき）。親フォルダも作る。2MB まで |
| `move` | **`from`**、**`to`** | 移動先が既にあると失敗します |
| `delete` | **`path`**、`recursive` | 空でないフォルダは `recursive` が必要です |

ツールの失敗は、JSON-RPC のエラーではなく、結果の `isError: true` と本文のエラー文で返ります。MCP からは、push（`sync`）も `--force` も使えません。

### 8. 状態の保存場所

| 場所 | 内容 |
|---|---|
| `.git/mig/oplog.json` | undo / redo 用の操作ログ（最大 500 件） |
| `.git/mig/agent.json` | 進行中のエージェントセッション |
| `refs/mig/agent/start` / `base` / `cp` | セッション開始時の HEAD、開始時の作業ツリーのスナップショット、最新のチェックポイントです。チェックポイントは `cp` から親をたどって得られます |
| `refs/mig/saved` | 直近に捨てた（`abort` / `restore` の前の）作業のスナップショット |
| `.mig.toml`（リポジトリ直下） | `[editor]` `[save]` `[scan]` の設定 |
| 設定フォルダの `mig/ai.toml` / `credentials.toml` / `settings.toml` / `recent.json` | AI の設定、API キー（自分だけが読める権限）、GUI の設定、最近開いたリポジトリ |

`refs/mig/*` は push されません。`git gc` や他のツールが、参照されていないオブジェクトを消さないよう、ref として残しています。

### 9. AI 連携で外に出る情報

- AI への送信前に、シークレットらしい値（AWS / GitHub / OpenAI / Anthropic / Slack / Google / Stripe のキー、秘密鍵、`api_key = "..."` のような値）を `[REDACTED]` に置き換えます。置き換えた数は、標準エラーに `note:` として出ます
- それ以外の差分やファイルの中身は、選んだ提供元（`mig ai config` で確認できます）に送られます。長すぎる差分は 12 万バイトで切り、切ったことを AI に伝えます
- API キーは、環境変数、なければ設定フォルダの `credentials.toml` から読みます。リポジトリには書きません
- Anthropic には、サーバー側のフォールバック（`fallbacks: "default"`）を付けて送ります。回答が拒否されたときは、`error: the model declined this request: ...` で終わります

### 10. コードを読み書きする AI のために

```sh
cargo build                         # GUI 付き（既定）
cargo build --no-default-features   # CLI のみ。警告が出ないこと
cargo test
cargo fmt && cargo clippy
```

- Rust 2024 で、字下げは**スペース 2 つ**です（`rustfmt.toml`）。コメントは必要最小限にします
- GUI は `gui` フィーチャの後ろにあります。GUI 専用のモジュールには `#[cfg(feature = "gui")]` を付け、CLI のみのビルドで未使用の警告が出ないようにします
- Git の操作は `git2`（libgit2）で行い、`git` コマンドは呼びません。git2 0.21 では、`Commit::summary()` が `Result<Option<&str>>`、`message()` が `Result<&str>` など、戻り値の形が古い資料と違うので、`repo::summary` と `repo::message` を使います
- 履歴を変える操作は、必ず `oplog::record(repo, "名前", || ...)` で包みます。そうしないと `mig undo` で戻せません
- GUI で時間のかかる処理は別スレッドで行い、スレッド内で `repo::open` し直します（`git2::Repository` はスレッド間で共有できません）

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
| `agent.rs` / `mcp.rs` / `files.rs` | エージェントセッション、MCP サーバー、MCP のファイル操作 |
| `gui.rs` / `editor.rs` / `viewer.rs` / `welcome.rs` / `settings.rs` / `aiui.rs` | GUI（画面全体、エディタ、ビュワー、ようこそ画面、設定、AI まわり） |

テストの進め方: 実際の外部サービスには繋がず、手元に偽のサーバー（GitHub API、Anthropic / OpenAI 互換 API）を立てて確認しています。`MIG_GITHUB_API` で GitHub API の向き先を変えられます。AI の向き先は、`mig ai config --base-url` で変えます。

### 11. 既知の制限

- `--json` 出力はありません（構造化データが必要なら MCP を使ってください）
- AI 機能と GitHub への作成は、本物のサービスでの動作確認がまだです。偽のサーバーでの確認のみです
- `mig sync` の undo は、リモートには及びません
- `absorb` の対象は、未 push のコミットだけです。push 済みの行を触った変更は、作業ツリーに残ります
- `fold` / `drop` / `split` は、マージコミットを扱えません
- Windows と macOS では未確認です（Linux で開発・確認しています）
