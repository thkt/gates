**English** | [日本語](README.ja.md)

# gates

Quality gates for Claude Code [PostToolUse hooks](https://docs.anthropic.com/en/docs/claude-code/hooks). Runs lint, type-check, test, knip, tsgo, dependency-cruiser, litmus, circular dependency detection, coupling metrics, and structural clone detection in parallel after each Write/Edit/MultiEdit, providing failure feedback to guide the agent.

## Features

| Feature        | Description                                                      |
| -------------- | ---------------------------------------------------------------- |
| Parallel       | All enabled gates run concurrently on OS threads                 |
| Fail-open      | Timeouts and missing binaries never block the agent              |
| Auto-detect    | Only runs gates relevant to the project (package.json, tsconfig) |
| Script gates   | Detects lint/type-check/test from package.json, auto-detects pm  |
| Binary resolve | Walks `node_modules/.bin` up to `.git` boundary                  |
| 60s timeout    | SIGKILL to entire process group                                  |

## How It Works

```text
Agent calls Write/Edit/MultiEdit → PostToolUse hook fires → gates binary runs
  ├─ Reads enabled gates from .claude/tools.json
  ├─ Detects project type (package.json, tsconfig.json, src/)
  ├─ Detects script gates (lint, type-check, test) from package.json
  ├─ Runs all matching gates in parallel on OS threads
  ├─ Gate failure → returns feedback with fix instructions
  └─ All gates pass → no output (silent success)
```

## Gates

### Static Gates

Resolved from `node_modules/.bin`, falling back to `$PATH`.

| Gate      | Condition                                      | Args   |
| --------- | ---------------------------------------------- | ------ |
| knip      | `package.json` exists                          | (none) |
| tsgo      | `tsconfig.json` exists                         | (none) |
| depcruise | `.dependency-cruiser.{js,cjs,mjs,json}` exists | `src/` |

### Embedded Gates

Built into the `gates` binary. No separate installation required.

| Gate     | Condition                                            | Detects                                                            |
| -------- | ---------------------------------------------------- | ------------------------------------------------------------------ |
| litmus   | `package.json` + `*.test.ts/tsx` exists              | Weak assertions, mock overuse, tautological tests                  |
| circular | `package.json` + `src/` exists                       | Circular import dependencies (oxc-based AST)                       |
| coupling | `package.json` + `src/` + `coupling.caThreshold` set | God modules (Ca > threshold) via Ca/Ce/instability (oxc-based AST) |
| clone    | `package.json` + `src/` exists                       | Type 1/2 structural code clones via oxc AST hashing                |

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

| Tool                                                                 | Install                                       |
| -------------------------------------------------------------------- | --------------------------------------------- |
| [knip](https://knip.dev)                                             | `npm i -D knip` (project-local)               |
| [tsgo](https://github.com/microsoft/typescript-go)                   | `npm i -g @typescript/native-preview`         |
| [dependency-cruiser](https://github.com/sverweij/dependency-cruiser) | `npm i -D dependency-cruiser` (project-local) |

[litmus](https://github.com/thkt/litmus) and circular dependency detection are embedded in the `gates` binary — no separate installation needed.

Missing tools are skipped (fail-open). A warning is printed to stderr if an enabled gate's binary is not found.

## Installation

### Claude Code Plugin (recommended)

Installs the binary and registers the PostToolUse hook automatically.

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
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit",
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

When registered as a PostToolUse hook, `gates` runs after each file write/edit and provides failure feedback to the agent.

### Direct Execution

```bash
gates              # uses current directory
gates /path/to/project  # explicit directory
```

No output means all gates passed. On failure, block JSON is printed to stdout:

```json
{ "decision": "block", "reason": "lint failed. Fix lint errors.\n\nerror output..." }
```

### Audit Log

Every run appends its pass/fail decision to `$XDG_DATA_HOME/gates/audit.jsonl` (default `~/.local/share/gates/audit.jsonl`) as one JSON object per line. The write is fail-open, so a logging failure never blocks the agent.

```jsonl
{"ts":"2026-04-11T11:00:00Z","project":"/Users/me/foo","decision":"fail","failed":["lint","test"]}
{"ts":"2026-04-11T11:05:30Z","project":"/Users/me/foo","decision":"pass","failed":[]}
```

| Field      | Type    | Description                         |
| ---------- | ------- | ----------------------------------- |
| `ts`       | RFC3339 | Event time in UTC                   |
| `project`  | string  | Absolute project directory          |
| `decision` | string  | `pass` or `fail`                    |
| `failed`   | array   | Failed gate names (empty on a pass) |

Review history with the `show` subcommand:

```bash
gates show                      # last 20 entries
gates show --last 50            # last 50 entries
gates show --decision fail      # only failures, then last 20 of those
```

It prints an aligned table of `TIMESTAMP  DECISION  PROJECT  FAILED_GATES`. With no log file yet, it prints nothing.

## Configuration

Add a `gates` key to `.claude/tools.json` in your project root.

When no config file exists, all gates run by default. Once you create `.claude/tools.json` with a `gates` key, only the gates set to `true` are enabled.

```json
{
  "gates": {
    "knip": true,
    "tsgo": true,
    "depcruise": true,
    "circular": true,
    "coupling": true,
    "litmus": true,
    "lint": true,
    "type-check": true,
    "test": true
  }
}
```

### Coupling Threshold

The coupling gate flags God modules whose afferent coupling (Ca, the number of intra-project files importing it) exceeds `coupling.caThreshold`. It lives outside the `gates` key because that key only accepts booleans. There is no universal default, so the gate reports nothing until a threshold is set. Derive one from a high percentile (for example P90-P95) of the repository's Ca distribution.

```json
{
  "gates": { "coupling": true },
  "coupling": { "caThreshold": 20 }
}
```

### Clone Thresholds

The clone gate hashes the oxc AST of every `src/` file to find Type 1 (whitespace-normalized identical) and Type 2 (structurally identical, identifiers and literals differ) duplicate subtrees, then blocks once the number of clone groups reaches `clone.blockThreshold`. All three keys are optional and fall back to the defaults below.

| Key                    | Default | Meaning                                                     |
| ---------------------- | ------- | ----------------------------------------------------------- |
| `clone.minNodes`       | 20      | Minimum AST node count for a subtree to count as a clone    |
| `clone.minLines`       | 5       | Minimum line span of the largest copy for a group to report |
| `clone.blockThreshold` | 3       | Number of clone groups that triggers a block                |

```json
{
  "gates": { "clone": true },
  "clone": { "minNodes": 20, "minLines": 5, "blockThreshold": 3 }
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

| Tool                                             | Hook        | Timing            | Role                              |
| ------------------------------------------------ | ----------- | ----------------- | --------------------------------- |
| [guardrails](https://github.com/thkt/guardrails) | PreToolUse  | Before Write/Edit | Lint + security checks            |
| [formatter](https://github.com/thkt/formatter)   | PostToolUse | After Write/Edit  | Auto code formatting              |
| [reviews](https://github.com/thkt/reviews)       | PreToolUse  | Before Skill      | Static analysis context injection |
| **gates**                                        | PostToolUse | After Write/Edit  | Quality gates                     |

See [thkt/tap](https://github.com/thkt/homebrew-tap) for setup details.

## License

MIT
