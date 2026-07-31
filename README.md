# Argo

**One conversation. Many coding-agent CLIs.**

Argo is a terminal-first orchestrator for the coding agents you already have
installed. Start a conversation with Claude Code, switch to Codex mid-task, and
Codex picks up where Claude left off—because Argo owns the conversation, not the
CLI.

```text
$ argo
 █████╗ ██████╗  ██████╗  ██████╗
██╔══██╗██╔══██╗██╔════╝ ██╔═══██╗
███████║██████╔╝██║  ███╗██║   ██║
██╔══██║██╔══██╗██║   ██║██║   ██║
██║  ██║██║  ██║╚██████╔╝╚██████╔╝
╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝  ╚═════╝

one conversation · many coding CLIs
```

## Quick install

Argo currently supports macOS and Linux and builds from source with Rust 1.82 or
newer:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh | bash
```

The installer uses no `sudo`, performs a locked release build, and installs to
`~/.local/bin/argo`. To inspect the script first, pin a version, choose another
install directory, update, uninstall, or install from a private repository, see
the **[complete installation guide](docs/installation.md)**.

After installation:

```bash
cd your-project
argo doctor
argo
```

## Why Argo

Every coding CLI keeps its own session store. Switching tools normally means
starting over: re-explaining the task, re-establishing decisions, and losing the
thread. Skills configured for one agent are invisible to another, and an MCP
server added to Claude does not automatically exist for Codex.

Argo makes the conversation authoritative. Sessions, full message history,
project conventions, shared skills, MCP servers, and agent selections live in
Argo and are projected into whichever coding CLI handles the next turn.

## Feature overview

### One durable conversation across agents

- Switch agent, model, or reasoning effort without abandoning the conversation.
- Reuse a CLI's native session only when its agent, model, workspace, and
  conversation cursor still match.
- Reseed safely from canonical history when a saved session is missing, stale,
  unsupported, or behind another agent's completed turn.
- Preserve the complete transcript in SQLite even when the context sent to a
  model must be compacted.
- Preview the exact projected next-turn context and the reason for a fresh or
  resumed session with `/context` or `argo context`.
- Show an explicit transcript alert whenever model/workspace/conversation changes
  force canonical context into a fresh native session.
- Preserve bounded tool results in transferred history, so identifiers returned
  by MCP calls remain available after switching agents.
- Persist readable conversation titles derived from the first meaningful prompt.

### Native TUI designed for agent work

- Stream assistant responses with a visible response rail and agent-specific
  turn headers.
- Render assistant Markdown as terminal-native presentation: headings, bold,
  italics, strikethrough, inline and fenced code, ordered and unordered lists,
  task lists, blockquotes, links, images, tables, rules, and footnotes.
- Keep canonical Markdown unchanged in history; styling is presentation-only.
- Show reasoning emitted by the CLI, tool starts/results, file writes, plans,
  diagnostics, token usage, and child-agent activity as distinct transcript rows.
  Argo never invents hidden chain-of-thought that a CLI did not emit.
- Keep long and wrapped responses aligned with transcript scrollback.
- Detect deliberate numbered choice questions and open a selectable response
  picker; ordinary numbered explanations are left alone.
- Filter large model and conversation pickers while typing.
- Compose multiline prompts and navigate command suggestions and input history.

### Deterministic FIFO message queue

Messages submitted while an agent is running are retained in FIFO order. Argo
uses a two-phase peek/commit protocol: a queued item is removed only after the
daemon confirms that its run started.

- Success starts the next queued message automatically.
- Cancelling/stopping the active turn also continues with the next queued item.
- Failure pauses the queue because a follow-up may depend on work that did not
  happen; press `Enter` with an empty composer to retry.
- Press `Esc` while idle to discard the paused queue.
- Queue depth is visible in activity and status text.

### Shared Agent Skills

Argo discovers Agent Skills from Argo and vendor locations, including:

- `.argo/skills`
- `.claude/skills`
- `.agents/skills`
- `.opencode/skills`
- `.kiro/skills`
- corresponding user-level skill directories

Workspace skills override global skills predictably. Skills are validated,
staged as copies, and exposed across agents without allowing an agent to edit the
original source. Side files are preserved, unchanged skills are not recopied,
and generated staging directories are gitignored.

### MCP once, available everywhere possible

Configure an MCP server once in Argo and it is injected into every adapter with a
verified MCP mechanism. Existing non-Argo server entries in vendor configs are
preserved.

- Add local stdio or remote HTTP MCP servers.
- Import existing servers from supported agent configuration files.
- Use repeatable headers and `{env:VARIABLE}` substitutions.
- Check connectivity and authentication with `argo mcp check`.
- Log in once to OAuth-protected MCP servers with `argo mcp login <name>`.
- OAuth 2.1 support includes protected-resource and authorization-server
  discovery, dynamic client registration, PKCE S256, loopback callbacks, token
  exchange, expiry handling, and refresh tokens.
- Stored bearer tokens are attached only when an explicit user Authorization
  header is absent.
- Token-bearing files are written atomically with owner-only (`0600`)
  permissions; corrupt shared configs are reported and preserved rather than
  overwritten.

MCP delivery by adapter:

| Agent | MCP delivery |
|---|---|
| Claude Code | generated per-turn configuration |
| Codex CLI | generated configuration overrides |
| Kiro CLI | ACP `session/new` descriptors |
| Antigravity | merged Gemini MCP configuration |
| OpenCode | merged OpenCode JSONC configuration |
| Command Code | merged Command Code configuration |
| Grok CLI | no verified MCP mechanism |

### Cross-runtime delegation

Every Argo-managed host turn can delegate exploratory work to any installed target
CLI. Claude, Codex, and Kiro receive native `argo_delegate` MCP tools through safe
per-run configuration. OpenCode, Antigravity, Command Code, Grok, and future
adapters also receive a daemon-backed command fallback:

```text
"$ARGO_BIN" delegate <agent> <self-contained task>
```

The fallback carries parent conversation/run identity in the turn environment, so
it does not rely on unsafe shared global MCP state. It requires the host CLI to
execute its shell tool; plain-output hosts cannot expose that host tool invocation,
but the delegated child's own Argo stream remains visible.

Each delegated task runs in a child conversation with real parent/child run
lineage, bounded nesting depth, its own native session, and the user's MCP servers.
The parent transcript records durable spawn/completion events, while the TUI
subscribes to the child run and shows its emitted reasoning, messages, tools,
files, plans, errors, and completion with child-agent attribution. A child
`RunFinished` is only the child's commit barrier and never finishes the parent or
advances the parent's FIFO queue. `/children` lists spawned conversations and
`/open <id>` opens the complete historical child transcript.

Native CLI subagents are separate from Argo delegation. Claude nested frames are
attributed when the installed build emits them with `--forward-subagent-text`.
Kiro's current vendor extension has no verified child-event schema, and the other
supported streams expose no stable native-child identity, so Argo reports those
limits instead of inventing lifecycle or hidden reasoning.

### Execution modes and process safety

Argo offers `full`, `plan`, `accept-edits`, and `read-only` modes where adapters
can honor them. Restrictions are expressed both through native CLI flags and in
the projected prompt. Unsupported modes are reported honestly rather than
pretended.

Each turn runs as a supervised child process. On Unix, Argo owns the child's
process group so cancellation and timeout handling terminate descendants instead
of leaving compilers, language servers, or shell commands orphaned.

## Supported agents

| Agent | Transport | Native resume | Structured activity | Modes |
|---|---|---:|---:|---|
| Claude Code | `stream-json` | yes | yes | plan, accept-edits |
| Codex CLI | JSONL events | yes | yes | accept-edits, read-only |
| OpenCode | JSONL events | yes | yes | plan |
| Kiro CLI | ACP over stdio | yes | yes | — |
| Command Code | plain text + session sidecar | yes | no | plan, accept-edits |
| Antigravity | stream JSON | yes | yes | plan, accept-edits |
| Grok CLI | plain text | no | no | plan |

Command Code session IDs are discovered from its workspace JSONL sidecar and
resumed with `--resume <id>` for every model. Argo never uses Command Code's
ambiguous global `--continue` behavior. Grok is reseeded from Argo history
because no verified native resume mechanism exists.

Plain-text adapters can provide final prose but cannot expose structured tool
activity through stdout. Argo states this limitation instead of fabricating
activity.

Adding an adapter is a declarative file under
[`crates/argo-runtime/src/defs/`](crates/argo-runtime/src/defs/). A new parser is
needed only when a CLI introduces a wire format Argo does not already support.

## Installation

For prerequisites, inspect-first installation, version pinning, updates,
uninstallation, private-repository access, and troubleshooting, see the
**[complete installation guide](docs/installation.md)**.

Argo requires Rust 1.82 or newer and currently targets Unix-style environments
(macOS and Linux).

One-shot installation from GitHub after the repository is public:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh | bash
```

For a private checkout, use a fine-grained GitHub token with read-only **Contents**
access (the token is passed to `curl`/the installer and is never stored):

```bash
curl -fsSL \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github.raw+json" \
  "https://api.github.com/repos/MaticAlgos/argo/contents/install.sh?ref=main" \
  | GITHUB_TOKEN="$GITHUB_TOKEN" bash
```

The script downloads a clean source archive, performs a reproducible locked
release build, installs `argo` to `~/.local/bin`, and removes its temporary build
directory. Set `ARGO_INSTALL_DIR` to choose another destination or
`ARGO_INSTALL_REF` to install a specific branch, tag, or commit:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh \
  | ARGO_INSTALL_REF=<tag-or-commit> ARGO_INSTALL_DIR="$HOME/bin" bash
```

If you prefer to inspect remote scripts before execution:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh -o /tmp/argo-install.sh
less /tmp/argo-install.sh
bash /tmp/argo-install.sh
```

Installation from a clone uses the same script without downloading another copy:

```bash
git clone https://github.com/MaticAlgos/argo.git
cd argo
./install.sh
```

`install.sh` builds a release binary and installs it to `~/.local/bin/argo`. To
install manually:

```bash
cargo build --release
mkdir -p ~/.local/bin
install -m 755 target/release/argo ~/.local/bin/argo
```

Ensure `~/.local/bin` is on `PATH`, and install/authenticate whichever coding-agent
CLIs you want Argo to orchestrate. `argo doctor` reports what Argo detects.

## Interactive use

Start Argo from a project workspace:

```bash
cd your-project
argo
# or
argo tui --root /path/to/project
```

### TUI commands

| Command | Purpose |
|---|---|
| `/help` | Show the complete in-app reference |
| `/agent [id]` | Pick or directly select a coding CLI |
| `/model [id]` | Filter/pick or directly select a model |
| `/effort [level]` | Set model-specific reasoning effort |
| `/mode [id]` | Set execution mode; `Shift+Tab` cycles supported modes |
| `/usage` | Show exact token counts reported by the last CLI turn; missing fields stay unavailable |
| `/status` | Show current Argo conversation, selection, context, run, and queue state |
| `/agents` | Show detected CLIs, versions, and limitations |
| `/skills` | List shared skills discovered for this workspace |
| `/mcp` | List configured MCP servers |
| `/context` | Show exactly what the next turn will send |
| `/resume [n\|id]` | List or reopen an earlier conversation |
| `/new [title]` | Start a new conversation |
| `/children` | Show conversations created by delegation |
| `/delegate <agent> <task>` | Delegate work to another CLI |
| `/cancel` | Stop the active turn |
| `/config` | Show settings and state file locations |
| `/doctor` | Run diagnostics |
| `/quit` | Leave the TUI; the daemon keeps running |

### TUI keys

- `Tab`: accept a command completion.
- `↑` / `↓`: navigate suggestions and pickers; with an empty composer, scroll
  through the transcript.
- `Shift+Enter` or `Ctrl+J`: insert a newline.
- `PageUp` / `PageDown` or `Shift+↑` / `Shift+↓`: scroll the transcript by
  rendered rows; `Home` / `End` jump to the beginning/end when the composer is
  empty. Bare `↑` / `↓` also scroll an empty composer. Argo requests terminal
  alternate-scroll mode; on Apple Terminal it additionally enables minimal wheel
  reporting because that profile setting can ignore the request.
- `F2`: toggle between mouse-wheel mode and fully terminal-owned selection. Apple
  Terminal starts in mouse-wheel mode; press `F2` for ordinary drag selection, or
  hold `Fn` while dragging to bypass mouse reporting temporarily.
- `Ctrl+P` / `Ctrl+N`: navigate composer history explicitly.
- Web links: `Cmd+click` uses terminal-native OSC 8 handling when available. In
  Apple Terminal mouse-wheel mode, clicking a rendered `http://` or `https://`
  URL opens that exact validated destination in the default browser because the
  terminal does not report the Command modifier to Argo. Other URL schemes are
  never launched.
- Outside mouse-wheel mode, drag normally to select and copy transcript text.
- `Shift+Tab`: cycle the selected adapter's supported execution modes.
- `Esc`: dismiss an overlay, cancel a running turn, or discard a paused queue
  while idle.
- `Ctrl+C`: quit the TUI.

## Scriptable CLI

Every important operation is available without the TUI:

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
argo mcp add volrix --url https://mcp.volrix.ai/mcp
argo mcp add local-tools -- command --arg
argo mcp login volrix
argo mcp check
argo mcp logout volrix
argo mcp remove volrix

argo stop
```

Use `argo <command> --help` for every option. `argo daemon` runs the daemon in the
foreground; `argo mcp-server` is an internal stdio endpoint launched by supported
agents for delegation.

## How switching and context projection work

Argo's SQLite store is authoritative. Each CLI's native session store is treated
as a reusable cache.

A native session is reused only when all of these remain true:

1. The selected agent is unchanged.
2. The selected model is unchanged.
3. The canonical workspace is unchanged.
4. No other completed conversation turn advanced beyond that session's cursor.
5. The adapter supports resume and Argo has a valid session handle.

If any check fails, Argo starts a fresh native session and sends a bounded context
package containing project instructions, workspace facts, available skills, the
recent transcript with per-agent attribution, and a compact summary of older
turns when necessary. Stable instructions are not resent unnecessarily on a
valid native resume.

Nothing is deleted from canonical history to fit a model window. Only the
projection sent for that turn is reduced. Session handles and cursors are saved
only after successful turns, and the daemon emits terminal completion only after
the assistant message, run status, and upstream session state are durable. This
commit barrier makes immediate queued follow-ups safe.

## Project instructions

Argo discovers common project convention files while walking from the workspace
toward the repository root, including cross-vendor and agent-specific instruction
files such as `AGENTS.md` and `CLAUDE.md`. Their source paths and content are
included in fresh context packages so switching agents does not drop project
rules. Missing and empty files are harmless; oversized files are bounded safely.

## Data, configuration, and security

Default state directories:

- macOS: `~/Library/Application Support/dev.argo.argo`
- Linux: `~/.local/share/argo`
- Override: `ARGO_DATA_DIR=/custom/path`

The state directory contains the SQLite database, daemon socket/lock, staged
resources, MCP registry, and OAuth token store. SQLite runs in WAL mode. One
per-user daemon owns database writes and child processes, preventing two clients
from interleaving partial turns in one conversation.

| Environment variable | Purpose |
|---|---|
| `ARGO_DATA_DIR` | Relocate all Argo state |
| `ARGO_TURN_TIMEOUT_MS` | Per-turn ceiling in milliseconds; defaults to 15 minutes, `0` disables it |
| `ARGO_STREAM_IDLE_TIMEOUT_MS` | Client stream inactivity budget; `0` disables it |
| `ARGO_LOG` | Daemon log filter, for example `debug` |

### Authority warning

Argo runs headless coding CLIs with permission prompts bypassed where necessary,
because a child process without a terminal cannot answer an interactive prompt.
In full-access mode, an agent may edit files and run commands without asking. The
TUI keeps this authority visible in its status bar.

`/mode plan` is the intended mode when you want proposals rather than changes. It
withholds bypass flags where supported and states the restriction in the prompt.
That is meaningfully safer, but it is **not a sandbox**: Argo cannot guarantee
that an external CLI will honor every requested boundary.

## Architecture

```text
argo (TUI / scriptable CLI)
      │  newline-delimited JSON over a private Unix socket
      ▼
argo-daemon ──── SQLite (WAL): conversations, messages, runs, events, sessions
      │
      ├── argo-context     transcript flattening, budgets, context packages
      ├── argo-runtime     adapter registry, detection, stream parsers, execution
      └── argo-resources   skills, MCP/OAuth, staging, project instructions
```

| Crate | Responsibility |
|---|---|
| `argo-core` | Domain model, IDs, events, titles, resume policy, mode vocabulary |
| `argo-store` | SQLite schema and repositories |
| `argo-context` | Context assembly, transcript attribution, and compaction |
| `argo-runtime` | Declarative adapters, detection, process supervision, parsers |
| `argo-resources` | Skills, MCP/OAuth, staging, and project instructions |
| `argo-daemon` | Orchestration, IPC, durable commit ordering, delegation MCP server |
| `argo-tui` | Terminal interface, Markdown rendering, queues, pickers, transcript |
| `argo-cli` | The `argo` executable and scriptable client commands |

## Development

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The suite covers domain policy, storage, context transfer, adapter argument
construction, stream normalization, MCP/OAuth handling, daemon orchestration,
queue ordering, session resume, Markdown terminal styling, and TUI viewport
regressions.

When adding or changing an adapter, declare only capabilities verified against
the real CLI. State what was tested and what could not be verified. Do not claim
native resume, structured tools, MCP, or execution modes that the upstream CLI
does not actually expose.

## Prior art

Argo's architecture follows [Open Design](https://github.com/opendesign-ai)'s
approach: a local daemon as the authority, declarative adapters rather than
subclasses, native session handles guarded by identity, and project-local staging
of shared resources. Its context model also draws on OpenCode's separation of
durable history from the projected turn.

## License

Apache-2.0. See [LICENSE](LICENSE).
