[English](README.md) | **日本語**

# gates

Claude Codeの[completion hook](https://docs.anthropic.com/en/docs/claude-code/hooks)用品質ゲート。knip・tsgo・madgeを並列実行し、失敗時にエージェントの完了をブロックします。

## 特徴

| 機能                 | 説明                                                                |
| -------------------- | ------------------------------------------------------------------- |
| 並列実行             | 有効な全ゲートをOSスレッドで同時実行                                |
| フェイルオープン設計 | タイムアウト・未インストールがエージェントをブロックしない          |
| 自動検出             | プロジェクトに該当するゲートのみ実行（package.json, tsconfig.json） |
| バイナリ解決         | `node_modules/.bin`から`.git`境界まで探索                           |
| 60秒タイムアウト     | プロセスグループ単位でSIGKILL                                       |

## 仕組み

```text
エージェント完了 → completion hook 発火 → gates バイナリ実行
  ├─ .claude/tools.json から有効ゲートを読み込み
  ├─ プロジェクト種別を検出（package.json, tsconfig.json, src/）
  ├─ 該当ゲートを OS スレッドで並列実行
  └─ 最初の失敗を block JSON として stdout に出力
        → エージェントに修正を指示
```

## ゲート

| ゲート | 条件                         | 引数                                  |
| ------ | ---------------------------- | ------------------------------------- |
| knip   | `package.json` あり          | （なし）                              |
| tsgo   | `tsconfig.json` あり         | （なし）                              |
| madge  | `package.json` + `src/` あり | `--circular --extensions ts,tsx src/` |

ゲートのバイナリはまず `node_modules/.bin` から解決し、見つからなければ `$PATH` にフォールバックします。

## 必要なツール

使いたいゲートに対応するツールをインストールしてください。

| ツール                                             | インストール                                |
| -------------------------------------------------- | ------------------------------------------- |
| [knip](https://knip.dev)                           | `npm i -D knip`（プロジェクトローカル推奨） |
| [tsgo](https://github.com/microsoft/typescript-go) | `npm i -g @typescript/native-preview`       |
| [madge](https://github.com/pahen/madge)            | `npm i -g madge`                            |

未インストールのツールは静かにスキップされます。

## インストール

### Claude Code Plugin（推奨）

バイナリのインストールとhookの登録が自動で行われます。

```bash
claude plugins marketplace add github:thkt/gates
claude plugins install gates
```

バイナリが未インストールの場合、同梱のインストーラを実行してください。

```bash
~/.claude/plugins/cache/gates/gates/*/hooks/install.sh
```

### Homebrew

```bash
brew install thkt/tap/gates
```

### リリースバイナリから

[Releases](https://github.com/thkt/gates/releases)から最新バイナリをダウンロードしてください。

```bash
# macOS (Apple Silicon)
curl -L https://github.com/thkt/gates/releases/latest/download/gates-aarch64-apple-darwin.tar.gz | tar xz
mv gates ~/.local/bin/
```

### ソースから

```bash
cd /tmp
git clone https://github.com/thkt/gates.git
cd gates
cargo build --release
cp target/release/gates ~/.local/bin/
cd .. && rm -rf gates
```

## 使い方

### Claude Code Hookとして

`~/.claude/settings.json` に追加してください。

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "gates",
            "timeout": 70000
          }
        ]
      }
    ]
  }
}
```

Stop hookとして登録すると、`gates` はプロジェクトディレクトリで自動的に実行されます。

### 直接実行

```bash
gates              # カレントディレクトリを使用
gates /path/to/project  # ディレクトリを明示指定
```

出力がなければ全ゲート通過。失敗時はblock JSONを出力します。

```json
{ "decision": "block", "reason": "knip failed. Fix the issues:\nUnused export ..." }
```

## 設定

プロジェクトルートの `.claude/tools.json` に `gates` キーを追加します。

設定ファイルがない場合、すべてのゲートが無効（デフォルト）です。有効にしたいゲートを `true` に設定してください。

```json
{
  "gates": {
    "knip": true,
    "tsgo": true,
    "madge": true
  }
}
```

### 設定例

knipのみ有効にする設定です。

```json
{
  "gates": {
    "knip": true
  }
}
```

### 設定ファイルの解決

設定ファイルは引数で渡されたプロジェクトディレクトリの `.claude/tools.json` から読み込まれます。

```text
project-root/
├── .claude/
│   └── tools.json     ← {"gates": {"knip": true, "tsgo": true}}
├── .git/
├── package.json
├── tsconfig.json
└── src/
```

## 関連ツール

| ツール | Hook | タイミング | 役割 |
| --- | --- | --- | --- |
| [guardrails](https://github.com/thkt/guardrails) | PreToolUse | Write/Edit 前 | リント + セキュリティチェック |
| [formatter](https://github.com/thkt/formatter) | PostToolUse | Write/Edit 後 | 自動コード整形 |
| [reviews](https://github.com/thkt/reviews) | PreToolUse | レビュー系 Skill 実行時 | 静的解析コンテキスト提供 |
| **gates** | Stop | エージェント完了時 | 品質ゲート (knip/tsgo/madge) |

## ライセンス

MIT
