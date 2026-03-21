**English** | [日本語](README.ja.md)

# gates

Stateful quality gates for Claude Code [completion hooks](https://docs.anthropic.com/en/docs/claude-code/hooks). Runs lint, type-check, test, knip, tsgo, litmus, and circular dependency detection in parallel, blocking agent completion on failure and enforcing a review phase before allowing completion.

## Features

| Feature         | Description                                                         |
| --------------- | ------------------------------------------------------------------- |
| Parallel        | All enabled gates run concurrently on OS threads                    |
| Fail-open       | Timeouts and missing binaries never block the agent                 |
| Auto-detect     | Only runs gates relevant to the project (package.json, tsconfig)    |
| Phase detection | Reads transcript to enforce fix → review → allow completion flow    |
| Review gate     | Blocks with review instructions on first all-pass, allows on second |
| Script gates    | Detects lint/type-check/test from package.json, auto-detects pm     |
| Binary resolve  | Walks `node_modules/.bin` up to `.git` boundary                     |
| 60s timeout     | SIGKILL to entire process group                                     |

## How It Works

```text
Agent stops → Stop hook fires → stdin JSON piped to gates binary
  ├─ Reads enabled gates from .claude/tools.json
  ├─ Detects project type (package.json, tsconfig.json, src/)
  ├─ Detects script gates (lint, type-check, test) from package.json
  ├─ Runs all matching gates in parallel on OS threads
  ├─ Gate failure → blocks with fix instructions
  └─ All gates pass →
       ├─ Reads transcript for previous gates output
       ├─ First all-pass → blocks with review instructions
       └─ Second all-pass (after review) → allows completion
```

## Gates

### Static Gates

Resolved from `node_modules/.bin`, falling back to `$PATH`.

| Gate | Condition              | Args   |
| ---- | ---------------------- | ------ |
| knip | `package.json` exists  | (none) |
| tsgo | `tsconfig.json` exists | (none) |

### Embedded Gates

Built into the `gates` binary. No separate installation required.

| Gate     | Condition                               | Detects                                           |
| -------- | --------------------------------------- | ------------------------------------------------- |
| litmus   | `package.json` + `*.test.ts/tsx` exists | Weak assertions, mock overuse, tautological tests |
| circular | `package.json` + `src/` exists          | Circular import dependencies (oxc-based AST)      |

### Script Gates

Detected from `package.json` scripts. The package manager is auto-detected from lock files (`pnpm-lock.yaml` → pnpm, `bun.lock` → bun, `yarn.lock` → yarn, `package-lock.json` → npm).

| Gate       | Script Detection               | Cascade                     |
| ---------- | ------------------------------ | --------------------------- |
| lint       | `"lint"` script                | Independent                 |
| type-check | `"test:type"` or `"typecheck"` | Independent                 |
| test       | `"test:unit"` or `"test"`      | Skipped if type-check fails |

When no lock file is found, script gates are silently skipped (fail-open). Environment variable overrides (`$LINT_CMD`, `$TYPE_CMD`, `$UNIT_CMD`) bypass auto-detection and run the specified command directly.

## Required Tools

Install the tools for the gates you want to use.

| Tool                                               | Install                               |
| -------------------------------------------------- | ------------------------------------- |
| [knip](https://knip.dev)                           | `npm i -D knip` (project-local)       |
| [tsgo](https://github.com/microsoft/typescript-go) | `npm i -g @typescript/native-preview` |

[litmus](https://github.com/thkt/litmus) and circular dependency detection are embedded in the `gates` binary — no separate installation needed.

Missing tools are skipped (fail-open). A warning is printed to stderr if an enabled gate's binary is not found.

## Installation

### Claude Code Plugin (recommended)

Installs the binary and registers the Stop hook automatically.

```bash
claude plugins marketplace add thkt/sentinels
claude plugins install gates
```

If the binary is not installed, run the bundled installer:

```bash
~/.claude/plugins/cache/gates/gates/*/hooks/install.sh
```

### Homebrew

```bash
brew install thkt/tap/gates
```

### From Release Binary

Download the latest binary from [Releases](https://github.com/thkt/gates/releases).

```bash
# macOS (Apple Silicon)
curl -L https://github.com/thkt/gates/releases/latest/download/gates-aarch64-apple-darwin.tar.gz | tar xz
mv gates ~/.local/bin/
```

### From Source

```bash
cd /tmp
git clone https://github.com/thkt/gates.git
cd gates
cargo build --release
cp target/release/gates ~/.local/bin/
cd .. && rm -rf gates
```

## Usage

### As a Claude Code Hook

Add to `~/.claude/settings.json`:

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

When registered as a Stop hook, `gates` reads hook JSON from stdin (transcript path, stop_hook_active flag) and runs in the project directory automatically.

### Direct Execution

```bash
gates              # uses current directory
gates /path/to/project  # explicit directory
gates --setup          # show install commands for missing tools
```

No output means all gates passed. On failure, block JSON is printed to stdout:

```json
{ "decision": "block", "reason": "lint failed. Fix lint errors.\n\nerror output..." }
```

## Configuration

Add a `gates` key to `.claude/tools.json` in your project root.

When no config file exists, all gates run by default. Once you create `.claude/tools.json` with a `gates` key, only the gates set to `true` are enabled.

```json
{
  "gates": {
    "knip": true,
    "tsgo": true,
    "circular": true,
    "litmus": true,
    "lint": true,
    "type-check": true,
    "test": true
  }
}
```

### Review Phase

By default, when all gates pass for the first time, `gates` blocks with review instructions (code review, regression test verification, 5-step verification gate). On the second all-pass, completion is allowed.

To disable the review phase:

```json
{
  "gates": { "lint": true, "test": true },
  "review": false
}
```

### Environment Variable Overrides

Override script gate commands with environment variables:

| Variable    | Overrides        | Example                   |
| ----------- | ---------------- | ------------------------- |
| `$LINT_CMD` | lint gate        | `LINT_CMD="eslint ."`     |
| `$TYPE_CMD` | type-check       | `TYPE_CMD="tsc --noEmit"` |
| `$UNIT_CMD` | test gate        | `UNIT_CMD="vitest run"`   |
| `$TEST_CMD` | all script gates | Legacy single-gate mode   |

When `$TEST_CMD` is set, script gate detection is skipped and only the specified command runs (backwards compatibility with completion-gate.sh).

### Config Resolution

Config is read from `.claude/tools.json` in the project directory passed as argument.

```text
project-root/
├── .claude/
│   └── tools.json     ← {"gates": {"lint": true, "test": true}, "review": true}
├── .git/
├── package.json
├── tsconfig.json
└── src/
```

## Companion Tools

This tool is part of a 4-tool quality pipeline for Claude Code. Each covers a
different phase — install the full suite for comprehensive coverage:

```bash
brew install thkt/tap/guardrails thkt/tap/formatter thkt/tap/reviews thkt/tap/gates
```

| Tool                                             | Hook        | Timing            | Role                               |
| ------------------------------------------------ | ----------- | ----------------- | ---------------------------------- |
| [guardrails](https://github.com/thkt/guardrails) | PreToolUse  | Before Write/Edit | Lint + security checks             |
| [formatter](https://github.com/thkt/formatter)   | PostToolUse | After Write/Edit  | Auto code formatting               |
| [reviews](https://github.com/thkt/reviews)       | PreToolUse  | Before Skill      | Static analysis context injection  |
| **gates**                                        | Stop        | Agent completion  | Quality gates + review enforcement |

See [thkt/tap](https://github.com/thkt/homebrew-tap) for setup details.

## License

MIT
