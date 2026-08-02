# Using Argo

This guide covers the terminal interface, context transfer, usage reporting,
delegation, and recovery behavior. Run `argo <command> --help` for scriptable CLI
flags and `/help` for the live in-chat reference.

## Launch and selection

Start Argo from the workspace an agent should operate in:

```bash
cd /path/to/project
argo
```

Without a startup default, the welcome screen opens the CLI picker. `Enter`
chooses the highlighted CLI once. `Space` chooses it and, after the model and any
applicable effort choice, saves that exact combination as the default.

Selection is intentionally staged:

1. Choose a CLI.
2. Choose one of its discovered models or verified presets.
3. Choose effort only if that exact model exposes adjustable levels.

Use `/default` to repeat the flow, `/default current` to save the active complete
selection, or `/default clear` to remove it. A valid default is shown under the
Argo logo on later launches; you can type immediately or use `/agent` to change
the current conversation.

`/agents` opens the full supported-CLI inventory. `Enter` switches the current
conversation, `Space` starts the model/effort flow and saves the result as the
default, and `Delete` clears the default. Missing CLIs remain visible but cannot
be selected. If the saved default CLI is no longer detected on a later launch,
Argo removes the stale preference and returns to the startup picker.

## Keyboard and mouse reference

### Writing and editing

| Shortcut | Result |
|---|---|
| `Enter` | Submit the composer or choose a highlighted row |
| `Shift+Enter` | Insert a newline, including in Apple Terminal |
| `Alt+Enter` or `Ctrl+J` | Insert a newline when a terminal cannot encode Shift+Enter |
| `Option+Backspace` or `Ctrl+W` | Delete the previous word |
| `Cmd+Backspace` or `Ctrl+U` | Delete to the start of the current line |
| `Backspace` / `Delete` | Delete around the cursor |
| `Ctrl+Y` | Restore the most recent composer edit |
| `Ctrl+T` | Show or collapse CLI-emitted thinking immediately |
| `Ctrl+P` / `Ctrl+N` | Previous/next submitted user prompt |
| `↑` / `↓` | Navigate a visible picker/completion, otherwise composer history |
| `Tab` | Accept the highlighted slash-command completion |

Pastes arrive through bracketed paste and may contain multiple lines. Use the
terminal's paste shortcut—normally `Cmd+V` on macOS or `Ctrl+Shift+V` on Linux.
Pasting also works in MCP URL, token, and header input screens.

### Reading, selecting, and copying

Argo starts in combined mouse mode:

- the wheel scrolls transcript rows;
- dragging the left button selects visible cells and copies them on release;
- `Cmd+C` (macOS) or `Ctrl+Shift+C` copies the active selection, falling back to
  the latest assistant response; and
- `PageUp` / `PageDown` scroll by keyboard, while `Home` / `End` jump to the top
  or bottom when the composer is empty.

`F2` switches to terminal-native selection for terminal-specific workflows. In
that mode the terminal owns drag selection, and its own modifier (often Shift)
controls whether selection or mouse reporting wins. Press `F2` again to restore
Argo's simultaneous wheel + drag behavior.

HTTP(S) links are emitted as terminal-native hyperlinks. In Apple Terminal's
Argo mouse mode, clicking a displayed URL opens that exact validated URL because
Apple Terminal does not forward the Command modifier to the application.

### Modes, cancellation, and exit

| Shortcut | Result |
|---|---|
| `Shift+Tab` | Cycle supported modes; from `full`, enter `plan` first when available |
| `Esc` | Close an overlay; cancel an active turn; or discard a paused queue |
| `Ctrl+C` twice within 3 seconds | Exit the TUI; the first press warns |
| `Ctrl+D` with empty input | Exit immediately |

Leaving the TUI does not stop the daemon or its managed runs. The farewell line
prints a copyable `argo --resume <conversation-id>` command when applicable.

## Commands

### Selection and display

| Command | Action |
|---|---|
| `/agent [id]` | Pick or set a CLI for the next message |
| `/model [id]` | Pick or set a model |
| `/effort [level]` | Set supported reasoning effort |
| `/default [configure\|current\|clear]` | Manage launch selection |
| `/mode [full\|plan\|accept-edits\|read-only]` | Set a supported mode directly; use `/mode plan` to plan without cycling |
| `/thinking [show\|hide\|toggle]` | Control rendered CLI-emitted thinking |
| `/status` | Show selection, context, active run, and queue |
| `/update [check\|install\|force]` | Check for updates, or exit and update directly |
| `/agents` | Browse/switch CLIs and set or clear the startup default |

### Conversations and work

| Command | Action |
|---|---|
| `/new [title]` | Create a conversation |
| `/resume [n\|id]` | List conversations or reopen one; `/open` is an alias |
| `/context` | Preview the next context projection and resume decision |
| `/children` | Inspect delegated children in a read-only snapshot |
| `/parent` or `/back` | Return from a directly opened child to its parent |
| `/delegate <agent> <task>` | Start a delegated child task |
| `/cancel` | Stop the current run; queued messages remain |
| `/clear-history` | Delete chats for this workspace and create a fresh one |

`/clear-history` is immediate and unavailable while any agent run is active. Use
it only when the stored conversations in the current workspace are no longer
needed.

### Resources and diagnostics

| Command | Action |
|---|---|
| `/usage` | Last-turn token fields plus provider allowance/local history |
| `/skills` | List cross-agent skills |
| `/instructions [enable\|disable\|edit]` | Manage opt-in project instructions |
| `/mcp list` | Show configured MCP servers and state |
| `/mcp add` | Open guided local/remote/import/auth setup |
| `/mcp check [name]` | Check all servers or one server |
| `/mcp reconnect <name>` | Recheck a server; use `argo` for built-in delegation |
| `/mcp login <name>` or `/mcp reauth <name>` | Authenticate again |
| `/mcp logout <name>` | Clear Argo-held authentication |
| `/mcp remove <name>` or `/mcp delete <name>` | Delete a server |
| `/config` | Show preferences and state paths |
| `/doctor` | Run diagnostics |
| `/help` | Show all commands |
| `/quit` | Leave the TUI |

Argo performs a lightweight background version check at startup. It reads only
the published workspace manifest; it never runs downloaded code automatically.
When a newer version exists, the header shows an update badge. `/update install`
restores the normal terminal, exits the running TUI, and then invokes the public
installer. The scriptable equivalents are `argo update --check`, `argo update`,
and `argo update --force`.

## Thinking and model choices

Argo shows only reasoning text a CLI actually emits. `/thinking hide` suppresses
those transcript blocks without changing the model's reasoning configuration;
`/thinking show` renders them again.

Effort is separate from visibility. The effort picker appears only if Argo has a
verified adjustable-effort mapping for the selected model. Choosing another
model clears an incompatible stale effort value. Model IDs that encode their
level—such as fixed `...-high` or `...-medium` variants—do not get a redundant
effort screen. Kiro CLI exposes session-wide `low`, `medium`, `high`, `xhigh`,
and `max` levels; Argo forwards the selected value to its ACP process.

Plan mode belongs to Argo rather than to the selected CLI. `/mode plan` or
Shift+Tab stores the mode on the conversation and prepends a no-write planning
boundary to every turn. If an adapter also advertises a verified native planning
mode, Argo mirrors the selection as a second enforcement layer. Kiro's ACP
`kiro_default`/`kiro_planner` modes are handled this way on fresh and resumed
sessions.

When a response deliberately asks the user to select among numbered alternatives,
Argo opens a simple choice picker. Ordinary numbered prose and procedural lists
remain in the transcript.

## Usage and allowance

Per-turn token counts are populated only from structured fields emitted by the
agent's completed turn. The provider section then uses a local, non-inference
surface:

| CLI | Surface |
|---|---|
| Codex | app-server `account/rateLimits/read` |
| Claude Code | local `/usage` |
| Kiro CLI | non-interactive local `/usage` |
| Command Code | local interactive `/usage` panel captured in a PTY |
| Antigravity | local interactive `/usage` panel captured in a PTY |
| OpenCode | 30-day `opencode stats` history, explicitly not quota |
| Grok CLI | reports the lack of a verified remaining-quota command |

Provider CLIs change. A failed or unavailable command is shown as unavailable;
Argo does not convert context tokens into account allowance.

### Automatic project instructions

`/instructions enable` creates `.argo-instructions.md` in the active workspace
and enables its prompt injection. Argo then appends clearly durable user
directives from later prompts, deduplicating exact repeats. It deliberately does
not save ordinary implementation requests as permanent policy.

`/instructions edit` temporarily restores the terminal and opens the file using
`$VISUAL`, `$EDITOR`, or `vi`. It can be edited while disabled. Run
`/instructions disable` to stop both capture and injection; the Markdown file is
retained, but is not sent to any agent until re-enabled. The enablement marker is
kept under the ignored `.argo/` runtime directory, so the default remains off
even when a repository already contains the Markdown file.

## Context across CLIs

Argo stores the complete canonical transcript in SQLite. It reuses a native CLI
session only if the CLI, model, workspace, and conversation cursor still match.
Otherwise it creates a new native session and sends a bounded projection of the
canonical conversation.

The projection contains current project instructions, workspace facts, enabled
skills, available MCP servers, recent attributed messages/tool results, and a
compact representation of older turns when required. This means switching CLIs
does not transfer an unlimited raw transcript, and it does not discard canonical
history either. `/context` shows the planned package before the next message.

Messages typed while a run is active enter a durable-in-memory FIFO queue. A
queued item is removed only after the daemon confirms its run started. Success or
cancellation advances the queue; failure pauses it. Press empty `Enter` to retry
or `Esc` while idle to discard the paused items.

## Delegated conversations

For ordinary delegation, the active CLI uses its own native subagents and keeps
the work in the current upstream session. Argo's `argo_delegate` tool is an
explicit cross-CLI escape hatch only. It should be called only when the user asks
for Argo-managed or cross-CLI delegation, not automatically for exploration,
parallelism, or a second opinion.

Every Argo delegation creates a linked child conversation and child run. The TUI
subscribes to child events and labels their messages, tools, files, plans, and
completion separately from the parent.

`/children` opens a snapshot, so `Esc` or `Enter` returns immediately while all
agents keep working. If you use `/open <child-id>` to enter the child as the
active conversation, `/parent` returns to its direct parent. Nested children can
be inspected the same way.

## Recovery

- Run `argo doctor` when a CLI, database, or daemon is not responding.
- Run `/mcp reconnect argo` when delegation reports a connection problem. Argo
  verifies the built-in MCP transport and restarts a missing compatible daemon.
- Every agent turn receives a fresh per-turn delegation configuration. Codex uses
  a native inline-table config override rather than modifying
  `~/.codex/config.toml`.
- Run `argo agents --refresh` after installing or updating a vendor CLI.
- Use `/context` if a model switch appears to have lost context.
- Use `argo --resume <conversation-id>` after closing the TUI.
- Use `argo stop` to stop the daemon; the next command starts it again.
- Set `ARGO_LOG=debug` before starting Argo for detailed daemon logs.

See [installation troubleshooting](installation.md#troubleshooting) for PATH,
Rust, GitHub, and locked-build problems.
