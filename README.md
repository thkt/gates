**English** | [日本語](README.ja.md)

# gates

Quality gates for Claude Code [completion hooks](https://docs.anthropic.com/en/docs/claude-code/hooks). Runs knip, tsgo, and madge in parallel, blocking agent completion on failure.

## Features

| Feature        | Description                                                      |
| -------------- | ---------------------------------------------------------------- |
| Parallel       | All enabled gates run concurrently on OS threads                 |
| Fail-open      | Timeouts and missing binaries never block the agent              |
| Auto-detect    | Only runs gates relevant to the project (package.json, tsconfig) |
| Binary resolve | Walks `node_modules/.bin` up to `.git` boundary                  |
| 60s timeout    | SIGKILL to entire process group                                  |

## How It Works

```text
Agent stops → completion hook fires → gates binary runs
  ├─ Reads enabled gates from .claude/tools.json
  ├─ Detects project type (package.json, tsconfig.json, src/)
  ├─ Runs matching gates in parallel on OS threads
  └─ Outputs first failure as block JSON to stdout
        → Agent is instructed to fix the issues
```

## Gates

| Gate  | Condition                      | Args                                  |
| ----- | ------------------------------ | ------------------------------------- |
| knip  | `package.json` exists          | (none)                                |
| tsgo  | `tsconfig.json` exists         | (none)                                |
| madge | `package.json` + `src/` exists | `--circular --extensions ts,tsx src/` |

Gate binaries are resolved from `node_modules/.bin` first, falling back to `$PATH`.

## Required Tools

Install the tools for the gates you want to use.

| Tool                                               | Install                               |
| -------------------------------------------------- | ------------------------------------- |
| [knip](https://knip.dev)                           | `npm i -D knip` (project-local)       |
| [tsgo](https://github.com/microsoft/typescript-go) | `npm i -g @typescript/native-preview` |
| [madge](https://github.com/pahen/madge)            | `npm i -g madge`                      |

Missing tools are silently skipped.

## Installation

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

When registered as a Stop hook, `gates` runs in the project directory automatically.

### Direct Execution

```bash
gates              # uses current directory
gates /path/to/project  # explicit directory
```

No output means all gates passed. On failure, block JSON is printed to stdout:

```json
{ "decision": "block", "reason": "knip failed. Fix the issues:\nUnused export ..." }
```

## Configuration

Add a `gates` key to `.claude/tools.json` in your project root.

All gates are disabled by default. Set the gates you want to enable to `true`.

```json
{
  "gates": {
    "knip": true,
    "tsgo": true,
    "madge": true
  }
}
```

### Example

Enable only knip:

```json
{
  "gates": {
    "knip": true
  }
}
```

### Config Resolution

Config is read from `.claude/tools.json` in the project directory passed as argument.

```text
project-root/
├── .claude/
│   └── tools.json     ← {"gates": {"knip": true, "tsgo": true}}
├── .git/
├── package.json
├── tsconfig.json
└── src/
```

## License

MIT
