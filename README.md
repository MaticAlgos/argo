<div align="center">

# Argo

### One conversation. Many coding CLIs.

Switch coding agents without losing the thread. Argo keeps the conversation,
workspace context, tools, and child-agent lineage while each CLI does what it
does best.

[![CI](https://github.com/MaticAlgos/argo/actions/workflows/ci.yml/badge.svg)](https://github.com/MaticAlgos/argo/actions/workflows/ci.yml)
[![Rust 1.82+](https://img.shields.io/badge/Rust-1.82%2B-f74c00?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![macOS + Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-59636e)](#installation)
[![Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-2ea44f)](LICENSE)

[Install](#installation) · [How it works](#how-context-is-managed) ·
[Shortcuts](#keyboard-and-mouse) · [Commands](#in-chat-commands) ·
[Usage guide](docs/usage.md) · [Contributing](CONTRIBUTING.md)

</div>

![Argo home screen](docs/assets/screenshots/argo-home.svg)

<table>
  <tr align="center">
    <td width="14%"><img src="docs/assets/agents/claude.png" alt="Claude" height="44"></td>
    <td width="14%"><img src="docs/assets/agents/codex.svg" alt="Codex" height="44"></td>
    <td width="14%"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/agents/opencode-dark.svg"><img src="docs/assets/agents/opencode-light.svg" alt="OpenCode" height="44"></picture></td>
    <td width="14%"><img src="docs/assets/agents/kiro.png" alt="Kiro" height="44"></td>
    <td width="14%"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/agents/command-code-dark.svg"><img src="docs/assets/agents/command-code-light.svg" alt="Command Code" height="44"></picture></td>
    <td width="14%"><img src="docs/assets/agents/antigravity.png" alt="Antigravity" height="44"></td>
    <td width="14%"><img src="docs/assets/agents/grok-light.png" alt="Grok" height="44"></td>
  </tr>
  <tr align="center">
    <td>Claude Code</td><td>Codex CLI</td><td>OpenCode</td><td>Kiro CLI</td>
    <td>Command Code</td><td>Antigravity</td><td>Grok CLI</td>
  </tr>
</table>

<sub>Agent marks identify compatible products; their owners retain all rights.
Sources and attribution are in <a href="docs/assets/README.md">docs/assets</a>.</sub>

## What Argo does

- Keeps one durable SQLite conversation across all supported coding CLIs.
- Shows the exact active CLI, model, effort, and execution mode in the TUI.
- Discovers live models where a CLI exposes them and uses verified presets where
  it does not.
- Offers reasoning effort only for the selected models that support it. For
  example, Antigravity exposes effort for adjustable Claude models, not for
  Gemini/GPT model IDs whose level is already part of the model choice.
- Streams responses, CLI-emitted thinking, tools, file changes, plans, usage,
  and delegated-agent activity without inventing hidden reasoning.
- Lets you show or hide thinking with `/thinking`.
- Recognizes deliberate numbered choices and presents a keyboard picker.
- Keeps mouse-wheel scrolling and drag-to-select/copy active at the same time.
- Shares skills and MCP servers across compatible agents.
- Delegates work to another CLI while preserving parent/child lineage.
- Updates the conversation title to the current request and keeps a short
  description of the conversation's starting point and current focus.
- Queues messages submitted during a run and starts them in FIFO order.

## Installation

Argo supports macOS and Linux and builds with Rust 1.82 or newer. The installer
uses a locked release build, needs no `sudo`, and installs to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh | bash
```

Inspect the script first, pin a tag or commit, install from a Git clone, update,
or uninstall using the [complete installation guide](docs/installation.md).
Argo does not install or authenticate vendor CLIs for you.

```bash
cd /path/to/project
argo doctor
argo
```

## Start screen and defaults

With no saved default, Argo opens its CLI picker over the welcome screen. Pick a
CLI, then a model, then an effort level only when that exact model supports one.

- `Enter` chooses the highlighted CLI for this conversation.
- `Space` chooses it and saves the completed CLI + model + optional effort as the
  startup default.
- Typing filters any picker; `↑` and `↓` move the selection.
- A saved default appears directly under the Argo logo. Start typing immediately,
  use `/agent` to change it for the current chat, or `/default` to reconfigure it.
- `/default current` saves the current exact selection; `/default clear` restores
  the startup picker.

Argo always displays the effective CLI and model. It never silently substitutes
a different agent or applies an effort value to an incompatible model.

## Keyboard and mouse

| Shortcut | Action |
|---|---|
| `Enter` | Send a message or choose the highlighted item |
| `Shift+Enter` | Insert a new line; includes an Apple Terminal modifier fallback |
| `Alt+Enter` / `Ctrl+J` | Portable alternate newline shortcuts |
| `Shift+Tab` | Cycle modes supported by the selected CLI |
| `Tab` | Accept the highlighted slash-command completion |
| `↑` / `↓` | Move in a picker/completion list, or navigate composer history |
| `Ctrl+P` / `Ctrl+N` | Navigate composer history explicitly |
| `Option+Backspace` / `Ctrl+W` | Delete the previous word |
| `Cmd+Backspace` / `Ctrl+U` | Delete to the start of the line |
| `Ctrl+Y` | Restore the last composer edit |
| Mouse wheel / `PageUp` / `PageDown` | Scroll rendered transcript rows |
| Drag with the left mouse button | Select visible text and copy it on release |
| `Cmd+C` / `Ctrl+Shift+C` | Copy the selection, or the latest response if none is selected |
| `F2` | Toggle Argo wheel + drag mode and terminal-native selection mode |
| `Home` / `End` | Move within input; with empty input, jump through the transcript |
| `Esc` | Close an overlay, cancel a turn, or discard a paused queue |
| `Ctrl+C`, twice within 3 seconds | Exit Argo; the first press only warns |
| `Ctrl+D` with an empty composer | Exit immediately |

Paste uses the terminal's normal shortcut (`Cmd+V` on macOS or usually
`Ctrl+Shift+V` on Linux). Bracketed paste preserves multiline text and works in
MCP token/header input. Safe rendered HTTP(S) links use terminal-native links;
Apple Terminal also supports clicking the exact displayed URL in Argo mouse mode.

While an agent is active, the status area rotates useful shortcut tips. See the
[full interaction guide](docs/usage.md#keyboard-and-mouse-reference) for terminal
details and queue behavior.

## In-chat commands

| Command | Purpose |
|---|---|
| `/agent [id]` | Choose or switch CLI for the next turn |
| `/model [id]` | Choose a model from the selected CLI's inventory |
| `/effort [level]` | Set effort only when the current model supports it |
| `/default [configure\|current\|clear]` | Manage the startup CLI/model/effort |
| `/mode [id]` | Choose an execution mode; `Shift+Tab` cycles it |
| `/thinking [show\|hide\|toggle]` | Control CLI-emitted thinking visibility |
| `/usage` | Show last-turn tokens and the selected provider's local allowance surface |
| `/status` | Show conversation, selection, context, run, and queue state |
| `/agents` | Show detected CLIs, versions, and verified limitations |
| `/skills` | Show skills available to every agent |
| `/context` | Preview exactly what the next CLI receives |
| `/resume [n\|id]` | List or reopen conversations (`/open` is an alias) |
| `/new [title]` | Start a new conversation |
| `/clear-history` | Delete stored chats for this workspace and start fresh |
| `/children` | Inspect delegated conversations in a snapshot overlay |
| `/parent` or `/back` | Return from an opened child conversation |
| `/delegate <agent> <task>` | Run a self-contained task in a child conversation |
| `/mcp ...` | Add, list, check, auth, reconnect, or delete MCP servers |
| `/cancel` | Stop the active turn without dropping queued follow-ups |
| `/config` | Show preferences and state paths |
| `/doctor` | Run environment and adapter diagnostics |
| `/help` | Open the complete in-app command reference |
| `/quit` | Leave the TUI while the daemon and active agents continue |

Opening a result from `/children` is non-blocking: `Esc` or `Enter` closes the
snapshot and returns to the parent while agents continue working. Opening a real
child chat with `/open <id>` lets you use `/parent` to navigate back.

## Usage reporting

`/usage` separates two different measurements:

1. Exact per-turn token fields emitted by the most recently completed CLI turn.
2. Provider allowance or local history obtained from a non-inference CLI surface.

| CLI | Provider/local surface used by Argo |
|---|---|
| Codex | local app-server rate-limit endpoint |
| Claude Code | local `/usage` command |
| Kiro CLI | `kiro-cli chat --no-interactive /usage` |
| Command Code | interactive local `/usage` panel |
| Antigravity | interactive local `/usage` panel |
| OpenCode | `opencode stats` local history; not presented as remaining quota |
| Grok CLI | local historical totals only; no verified remaining-quota command |

Unavailable values remain unavailable; Argo does not estimate billing balance or
invent a quota from context-window usage.

## MCP management

Use `/mcp add` for guided setup from the TUI. It covers local stdio servers,
remote HTTP servers, imports, OAuth, bearer tokens, and custom headers. Tokens
can be pasted. During OAuth, Argo opens the browser when possible and always
shows the authorization URL so it can be copied manually.

```text
/mcp list
/mcp check [name]
/mcp add
/mcp reconnect <name>
/mcp login <name>        # /mcp reauth is an alias
/mcp logout <name>
/mcp remove <name>       # /mcp delete is an alias
```

Existing non-Argo vendor configuration is preserved. Argo can project MCP
servers through generated Claude/Codex configuration, Kiro ACP descriptors, and
the supported shared configuration formats for OpenCode, Command Code, and
Antigravity. Grok currently has no verified MCP injection mechanism.

## Delegation and child agents

Claude, Codex, and Kiro can receive Argo's native delegation tools through MCP.
Other compatible hosts can use the daemon-backed command supplied in their turn
environment. Each delegated task gets its own conversation, run, session, and
events, linked to its parent.

The parent and children keep running when you inspect another conversation or
leave the TUI. Child completion is never mistaken for parent completion and does
not incorrectly advance the parent's message queue. Argo reports only child
identity exposed by a verified stream; it does not invent subagent frames.

## How context is managed

Argo's SQLite history is authoritative. A vendor CLI's native session is a cache,
reused only while the agent, model, workspace, and canonical conversation cursor
still match. If you switch CLI or model—or another agent advances the chat—Argo
starts a fresh native session and projects a bounded context package containing:

- project instructions and workspace facts;
- active skills and MCP availability;
- recent user/assistant/tool history with agent attribution; and
- a compact summary of older turns when the target context window requires it.

So the entire unbounded transcript is **not** blindly copied to every CLI. The
canonical history remains complete in Argo, while each turn receives the largest
useful bounded projection. `/context` previews that projection and explains
whether the next turn will resume a native session or receive a fresh transfer.

## Supported agents

Capabilities below describe Argo's adapter, not every feature of the vendor UI.
Model inventories may be discovered live and therefore change by installed CLI
version.

| Agent | Output transport | Native resume | Structured activity | Argo delegation host | Modes |
|---|---|:---:|:---:|---|---|
| Claude Code | stream JSON | yes | yes | MCP | plan, accept-edits |
| Codex CLI | JSONL | yes | yes | MCP | accept-edits, read-only |
| OpenCode | JSONL | yes | yes | command | plan |
| Kiro CLI | ACP | yes | yes | MCP | — |
| Command Code | plain text | yes | no | command | plan, accept-edits |
| Antigravity | stream JSON | yes | yes | command | plan, accept-edits |
| Grok CLI | plain text | context replay | no | command target only | — |

Plain-text adapters can return final prose but cannot expose structured tool
events. `argo agents --refresh` reports the exact detected version and current
limitations instead of launching every CLI at startup.

## Scriptable CLI

The same operations are available outside the TUI:

```bash
argo doctor
argo agents --refresh
argo chats --root /path/to/project
argo new --root /path/to/project --title "Investigate latency"
argo show <conversation-id>

argo send "explain this repository"
argo send --conversation-id <id> --agent codex --model <model> "optimize it"
argo select <id> --agent claude --model <model> --reasoning high
argo mode <id> plan
argo context <id> "the next question"
argo delegate codex "inspect this failure and report likely causes"

argo skills --root /path/to/project
argo mcp list
argo mcp import --yes
argo mcp add local-tools -- command --arg
argo mcp add remote --url https://example.test/mcp
argo mcp login remote
argo mcp check
argo mcp logout remote
argo mcp remove remote

argo clear-history --root /path/to/project
argo stop
```

Use `argo <command> --help` for all options.

## Safety and storage

Argo runs headless CLIs, so full-access mode may let the selected agent edit files
and execute commands without an interactive vendor prompt. `/mode plan` reduces
authority where supported, but it is not a sandbox.

Default state locations:

- macOS: `~/Library/Application Support/dev.argo.argo`
- Linux: `~/.local/share/argo`
- override: `ARGO_DATA_DIR=/custom/path`

SQLite uses WAL mode and a per-user daemon serializes writes. MCP secrets are
written atomically with owner-only permissions. See [docs/usage.md](docs/usage.md)
for context, queue, and recovery details.

## Architecture

```text
argo (TUI / scriptable CLI)
      │  newline-delimited JSON over a private Unix socket
      ▼
argo-daemon ─── SQLite: conversations, messages, runs, events, sessions
      │
      ├── argo-context     context projection and compaction
      ├── argo-runtime     CLI discovery, adapters, parsers, execution
      └── argo-resources   skills, MCP/OAuth, project instructions
```

Adding an adapter is primarily declarative under
[`crates/argo-runtime/src/defs`](crates/argo-runtime/src/defs). A new parser is
needed only for a wire format Argo does not already support.

## Development

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release --locked
```

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the Git branch, commit, sync, and pull
request workflow. Installation and update procedures are in
[docs/installation.md](docs/installation.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
