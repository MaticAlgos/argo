# Contributing to Argo

Argo is a Rust workspace with adapters for independently evolving third-party
CLIs. Contributions should preserve canonical conversation data, state adapter
capabilities conservatively, and include regression coverage for behavior that
depends on terminal or vendor output.

## Prepare a checkout

Fork the repository when you do not have direct write access, then clone your
fork. Direct contributors can clone the upstream repository.

```bash
git clone https://github.com/YOUR-USER/argo.git
cd argo
git remote add upstream https://github.com/MaticAlgos/argo.git
git fetch upstream
git switch -c fix/short-description upstream/main
```

SSH is equally supported:

```bash
git clone git@github.com:YOUR-USER/argo.git
```

Use a focused branch such as `fix/shift-enter`, `feat/agent-picker`, or
`docs/git-workflow`. Do not develop directly on `main`.

## Make and verify changes

The minimum supported Rust version is 1.82. Before opening a pull request, run:

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release --locked
git diff --check
```

Add tests near the affected crate. Terminal input regressions should cover the
actual `crossterm` event shapes where possible. Adapter changes should include a
trimmed fixture based on verified CLI output and tests for argument construction,
model parsing, resume behavior, or stream normalization as relevant.

Never claim a capability from branding or documentation alone when a local CLI
surface can be checked. Record what version was tested. If the CLI cannot expose
native resume, structured tools, MCP injection, usage, or a mode, keep the UI
honest about that limitation.

Do not commit local state or generated vendor data, including:

- `target/`;
- `.argo/` and SQLite state;
- personal vendor configuration or authentication tokens;
- test transcripts containing prompts, paths, or credentials; and
- editor, operating-system, or temporary capture files.

Review the exact patch before committing:

```bash
git status --short
git diff
git diff --check
```

## Commits

Prefer small commits with an imperative subject that explains the outcome:

```text
fix: preserve Shift+Enter in Apple Terminal
feat: expose model-specific reasoning effort
docs: document the Git update workflow
```

Stage explicit paths so unrelated local work does not enter the commit:

```bash
git add README.md docs/ crates/argo-tui/src/run.rs
git diff --cached
git commit -m "fix: preserve terminal multiline input"
```

Do not rewrite shared history or force-push `main`. If you must update a personal
feature branch after review has begun, use `--force-with-lease`, never `--force`.

## Sync before pushing

Fetch the latest upstream branch and rebase your focused commits:

```bash
git fetch upstream
git rebase upstream/main
```

Resolve each conflict deliberately, stage the resolved files, and continue:

```bash
git add path/to/resolved-file
git rebase --continue
```

Run the checks again, then push the branch:

```bash
git push -u origin HEAD
```

If the upstream repository is your `origin`, replace `upstream/main` with
`origin/main` and push to `origin HEAD`.

## Pull requests

A pull request should include:

- the user-visible problem and resulting behavior;
- CLI names and versions used for adapter verification;
- tests run and any platform-specific checks;
- screenshots for visible TUI changes; and
- known limitations or unverified surfaces.

Keep README command tables, [`docs/usage.md`](docs/usage.md), and adapter
capability labels synchronized with behavior. If README logos change, retain
source attribution in [`docs/assets/README.md`](docs/assets/README.md).

CI checks formatting, Clippy, the workspace tests, and the release build. A green
CI result is required but does not replace manual terminal testing for keyboard,
mouse, clipboard, OAuth browser, or pseudo-terminal behavior.
