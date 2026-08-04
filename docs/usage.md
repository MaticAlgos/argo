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
| `Option+←` / `Option+→` | Move the caret one word left or right |
| `Cmd+←` / `Cmd+→` | Move the caret to the start or end of the current line |
| `Backspace` / `Delete` | Delete around the cursor |
| `Ctrl+Y` | Restore the most recent composer edit |
| `Ctrl+T` | Show or collapse CLI-emitted thinking and tool/file activity immediately |
| `Ctrl+P` / `Ctrl+N` | Previous/next submitted user prompt |
| `↑` / `↓` | Navigate a visible picker/completion, otherwise composer history |
| `Tab` | Accept the highlighted slash-command completion |

Word movement accepts `Ctrl+←`/`Ctrl+→` as well, which is what most non-macOS
terminals send. Terminals configured to send Option as Meta deliver `Esc b`/`Esc f`
rather than an arrow, so `Alt+B`/`Alt+F` move by word too. A word is
whitespace-delimited — the same span `Option+Backspace` deletes.

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
| `Esc` | Close an overlay; cancel an active turn and continue its queued follow-ups; cancel Telegram linking; or discard a paused queue |
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
| `/backup [id [model]\|none]` | Configure or clear quota-exhaustion failover |
| `/telegram [status|setup|allow|remove]` | Set up, inspect, or remove phone access |
| `/thinking [show\|hide\|toggle]` | Control rendered CLI-emitted thinking and tool activity |
| `/status` | Show selection, context, active run and how long it has run, and queue |
| `/update [check\|install\|force]` | Check for updates, or exit and update directly |
| `/agents` | Browse/switch CLIs and set or clear the startup default |

### Conversations and work

| Command | Action |
|---|---|
| `/new [title]` | Create a conversation |
| `/resume [n\|id]` | List conversations or reopen one; `/open` is an alias |
| `/context` | Preview the next context projection and resume decision |
| `/compact` | Fold history into a summary now, freeing context without deleting the transcript |
| `/children` | Inspect delegated children in a read-only snapshot |
| `/parent` or `/back` | Return from a directly opened child to its parent |
| `/delegate <agent> <task>` | Start a delegated child task |
| `/queue` | Review messages waiting to send; `Del` or `Ctrl+D` drops the highlighted one |
| `/cancel` | Stop the current run; the next queued message starts automatically |
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

## Thinking and tool activity

Argo shows only reasoning text a CLI actually emits. `/thinking hide` (or
`Ctrl+T`) suppresses both those transcript blocks **and** tool/file activity
lines, replacing the run of hidden rows with a single marker. They are hidden and
revealed together because they are the same class of intermediate detail: leaving
a wall of tool calls on screen defeats the purpose of collapsing reasoning.
Nothing is deleted — `/thinking show` renders it all again, and the canonical
rows are untouched throughout. Visibility never changes the model's reasoning
configuration.

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

## How long a turn has been running

While a turn streams, the spinner line carries its elapsed time — `7s`, then
`1:35`, then `2:03:05` — beside the activity label and any queued count. It is
measured from a wall clock rather than the redraw timer, so a stalled turn keeps
counting honestly instead of appearing to freeze. When the turn ends, the status
line reports the total (`done · took 1:12 · 4210 in / 980 out`), including for a
cancelled or failed turn. `/status` shows the same figure for the active run.

## Backup failover

`/backup` opens a guided CLI → model → optional effort selection for a standby.
`/backup <agent>` starts the same model/effort flow, `/backup <agent> <model>`
sets a concrete route, and `/backup none` disables it. The standby must differ
from the active CLI.

Failover is deliberately narrow: it happens only when the active CLI reports
that its plan or quota is exhausted before it emits output or performs side
effects. Argo prints the failover diagnostic in the existing run, changes the
header/status selection to the CLI now answering, and attributes completed token
usage to that CLI. The successful standby becomes the active selection and the
exhausted route moves into the backup slot. Ordinary agent errors do not consume
the backup.

## Telegram phone access

Bare `/telegram` reports a linked bridge and otherwise opens guided setup. The
wizard sends the bot token through a masked field, validates it, and shows the
bot link. Then it opens a 90-second window and authorizes **the sender of the
first private message to arrive in it** — tapping *Start* in Telegram sends
`/start`, so nothing has to be typed. The wait runs in the background, so the
interface stays usable; press `Esc` to cancel it. Linking starts the bridge
immediately—there is no restart step.

What bounds that claim is the window, not the message. It is opened deliberately
from your machine, it is time-boxed, and Argo advances the bot's update offset
past everything that existed beforehand, so a message already sitting in the
queue cannot claim it. Group chats are ignored entirely: a bot in a group hears
everyone in it. The practical consequence is that **whoever messages the bot
first during those 90 seconds gets access**, so open the window only when you are
ready to message it yourself, and prefer a freshly created bot nobody else knows.
If someone else claims it, run `/telegram remove` and set up again.

TUI actions are:

```text
/telegram status       # bare /telegram is equivalent when already linked
/telegram setup        # connect and link a bot
/telegram allow        # allow the current workspace
/telegram remove       # stop the bridge and delete token/settings
```

For scripts and headless hosts:

```bash
argo telegram setup --root /path/to/project
argo telegram setup --token-file /secure/path/token   # unattended token supply
argo telegram status
argo telegram allow <USER_ID> --root /path/to/project
argo telegram qr
argo telegram start                   # recovery only; linking normally starts it
argo telegram remove
```

**Security warning:** this is remote access to coding agents, not a notification
channel. An authorized Telegram user can select full-access mode and execute
commands or modify files in every allowlisted workspace with your local account's
permissions. Use a private bot, authorize only trusted IDs, minimize allowlisted
workspaces, protect the BotFather token, and run `/telegram remove` immediately if
the bot token or authorized account is compromised.

Telegram linking has its own explicit window: 90 seconds in the TUI, or the
duration `argo telegram setup` announces. Agent turn deadlines are separate and opt-in:
`ARGO_TURN_TIMEOUT_MS=<milliseconds>` limits daemon-owned turns, while unset,
invalid, or `0` means unlimited. Scriptable `argo send`/delegation streaming can
also set `ARGO_STREAM_IDLE_TIMEOUT_MS=<milliseconds>`; unset, invalid, or `0`
waits indefinitely, while a positive value stops the client after that much event
inactivity without cancelling the daemon-owned turn.

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
retained, but is not sent to any agent until re-enabled. Enablement is recorded in
a small HTML comment inside that Markdown file, so no separate project `.argo`
runtime directory is required. A repository containing an unmarked instruction
file remains disabled by default.

### Skill discovery and caching

Argo discovers skills from project roots such as `.claude/skills`,
`.agents/skills`, `.codex/skills`, `.kiro/skills`, and `.argo/skills`, then from
the equivalent global CLI roots and Argo's user-level skills directory. A project
skill with the same name takes precedence over a global one.

The source remains where its CLI or the user installed it. Before a run, Argo
copies the resolved skill into `<Argo data directory>/staging/skills` and gives
the selected CLI that absolute protected path. It compares the complete source
and cached trees on later turns, including scripts, references, assets, added
files, and deleted files. Changed sources are refreshed automatically, and edits
made to a cached copy by an agent are discarded in favor of the source. No skill
cache is written inside the project. Argo v0.1.4 also removes the legacy
`.argo/skills-staged` cache when it encounters one, while preserving user-authored
`.argo/skills` and other custom files.

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

That budget-driven trimming happens automatically on every turn, walking back
from the newest message and folding whatever no longer fits into a mechanical
outline. It is not triggered by a "window almost full" threshold, and it is
recomputed each turn rather than persisted.

`/compact` is the explicit version, for when you want the context reduced now
rather than when the budget forces it. It records a compaction point, summarizes
everything up to it, and drops every stored native session for the conversation
so the next turn actually reseeds from the reduced projection — a vendor CLI still
holding the full history in its own session would otherwise keep answering from
the uncompacted version. Canonical messages are never deleted: the transcript
above stays readable and only what future turns receive changes. The summary
states what was folded away without inventing detail; it is not model-written.
Compacting twice carries the earlier outline forward, and Argo refuses when there
is nothing new to fold or while a turn is still running. Outside the TUI, run
`argo compact <conversation-id>` for the same operation.

Messages typed while a run is active enter a TUI-local in-memory FIFO queue. A
queued item is removed only after the daemon confirms its run started. Success or
cancellation—including `/cancel` or `Esc` during a run—starts the next item;
failure pauses the queue. Press empty `Enter` to retry or `Esc` while idle to
discard the paused items. Leaving the TUI discards items that have not started.

`/queue` opens that queue for review, in send order. `Del` or `Ctrl+D` drops the
highlighted message — two bindings because the key labelled Delete on a Mac
keyboard sends Backspace, which narrows the list instead. `Enter` and `Esc` both
just close it, so review cannot cost you a message by accident. The list tracks
the queue while it is open: a message whose run starts leaves the list, and the
pane closes once nothing is left. Removal is matched by message content, so if
the highlighted item is sent in the moment before the key lands, Argo says it has
already gone rather than dropping whatever moved up into its place.

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
