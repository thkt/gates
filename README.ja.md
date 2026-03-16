[English](README.md) | **日本語**

# gates

Claude Codeの[completion hook](https://docs.anthropic.com/en/docs/claude-code/hooks)用ステートフル品質ゲート。lint・type-check・test・knip・tsgo・madgeを並列実行し、失敗時にエージェントの完了をブロック。全ゲート通過後はレビューフェーズを強制します。

## 特徴

| 機能                 | 説明                                                                |
| -------------------- | ------------------------------------------------------------------- |
| 並列実行             | 有効な全ゲートをOSスレッドで同時実行                                |
| フェイルオープン設計 | タイムアウト・未インストールがエージェントをブロックしない          |
| 自動検出             | プロジェクトに該当するゲートのみ実行（package.json, tsconfig.json） |
| フェーズ判定         | transcriptを読んで fix → review → allow の完了フローを強制          |
| レビューゲート       | 初回の全パス時にレビュー指示でブロック、2回目でcompletion許可       |
| スクリプトゲート     | package.jsonからlint/type-check/testを検出、`nr`経由で実行          |
| バイナリ解決         | `node_modules/.bin`から`.git`境界まで探索                           |
| 60秒タイムアウト     | プロセスグループ単位でSIGKILL                                       |

## 仕組み

```text
エージェント完了 → Stop hook 発火 → stdin JSON を gates バイナリにパイプ
  ├─ .claude/tools.json から有効ゲートを読み込み
  ├─ プロジェクト種別を検出（package.json, tsconfig.json, src/）
  ├─ package.json からスクリプトゲート（lint, type-check, test）を検出
  ├─ 該当ゲートを OS スレッドで並列実行
  ├─ ゲート失敗 → 修正指示でブロック
  └─ 全ゲート通過 →
       ├─ transcript から前回の gates 出力を検索
       ├─ 初回の全パス → レビュー指示でブロック
       └─ 2回目の全パス（レビュー済み）→ completion 許可
```

## ゲート

### 静的ゲート

`node_modules/.bin` から解決し、見つからなければ `$PATH` にフォールバックします。

| ゲート | 条件                         | 引数                                  |
| ------ | ---------------------------- | ------------------------------------- |
| knip   | `package.json` あり          | （なし）                              |
| tsgo   | `tsconfig.json` あり         | （なし）                              |
| madge  | `package.json` + `src/` あり | `--circular --extensions ts,tsx src/` |

### スクリプトゲート

`package.json` のscriptsから検出し、[`nr`](https://github.com/antfu/ni) 経由で実行します。

| ゲート     | スクリプト検出                 | カスケード                  |
| ---------- | ------------------------------ | --------------------------- |
| lint       | `"lint"` スクリプト            | 独立実行                    |
| type-check | `"test:type"` or `"typecheck"` | 独立実行                    |
| test       | `"test:unit"` or `"test"`      | type-check 失敗時はスキップ |

`nr` 未インストール時はスクリプトゲートが静かにスキップされます（フェイルオープン）。環境変数オーバーライド（`$LINT_CMD`, `$TYPE_CMD`, `$UNIT_CMD`）を使うと `nr` をバイパスして直接コマンドを実行できます。

## 必要なツール

使いたいゲートに対応するツールをインストールしてください。

| ツール                                             | インストール                                |
| -------------------------------------------------- | ------------------------------------------- |
| [nr](https://github.com/antfu/ni)                  | `npm i -g @antfu/ni`（スクリプトゲート用）  |
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

Stop hookとして登録すると、`gates` はstdinからhook JSON（transcriptパス、stop_hook_activeフラグ）を読み取り、プロジェクトディレクトリで自動的に実行されます。

### 直接実行

```bash
gates              # カレントディレクトリを使用
gates /path/to/project  # ディレクトリを明示指定
```

出力がなければ全ゲート通過。失敗時はblock JSONを出力します。

```json
{ "decision": "block", "reason": "lint failed. Fix lint errors.\n\nerror output..." }
```

## 設定

プロジェクトルートの `.claude/tools.json` に `gates` キーを追加します。

すべてのゲートはデフォルトで無効です。有効にしたいゲートを `true` に設定してください。

```json
{
  "gates": {
    "knip": true,
    "tsgo": true,
    "madge": true,
    "lint": true,
    "type-check": true,
    "test": true
  }
}
```

### レビューフェーズ

デフォルトでは、全ゲートが初めて通過したときにレビュー指示（コードレビュー、回帰テスト検証、5ステップ検証ゲート）でブロックします。2回目の全パスでcompletion許可されます。

レビューフェーズを無効にするには:

```json
{
  "gates": { "lint": true, "test": true },
  "review": false
}
```

### 環境変数オーバーライド

環境変数でスクリプトゲートのコマンドを上書きできます。

| 変数        | 対象               | 例                        |
| ----------- | ------------------ | ------------------------- |
| `$LINT_CMD` | lint ゲート        | `LINT_CMD="eslint ."`     |
| `$TYPE_CMD` | type-check         | `TYPE_CMD="tsc --noEmit"` |
| `$UNIT_CMD` | test ゲート        | `UNIT_CMD="vitest run"`   |
| `$TEST_CMD` | 全スクリプトゲート | レガシー単一ゲートモード  |

`$TEST_CMD` を設定すると、スクリプトゲートの検出がスキップされ、指定されたコマンドのみ実行されます（completion-gate.shとの後方互換）。

### 設定ファイルの解決

設定ファイルは引数で渡されたプロジェクトディレクトリの `.claude/tools.json` から読み込まれます。

```text
project-root/
├── .claude/
│   └── tools.json     ← {"gates": {"lint": true, "test": true}, "review": true}
├── .git/
├── package.json
├── tsconfig.json
└── src/
```

## 関連ツール

| ツール                                           | Hook        | タイミング              | 役割                          |
| ------------------------------------------------ | ----------- | ----------------------- | ----------------------------- |
| [guardrails](https://github.com/thkt/guardrails) | PreToolUse  | Write/Edit 前           | リント + セキュリティチェック |
| [formatter](https://github.com/thkt/formatter)   | PostToolUse | Write/Edit 後           | 自動コード整形                |
| [reviews](https://github.com/thkt/reviews)       | PreToolUse  | レビュー系 Skill 実行時 | 静的解析コンテキスト提供      |
| **gates**                                        | Stop        | エージェント完了時      | 品質ゲート + レビュー強制     |

## ライセンス

MIT
