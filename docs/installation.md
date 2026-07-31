# Installing Argo

Argo currently supports macOS and Linux. The installer builds from source, so it
works on supported architectures without waiting for a matching prebuilt binary.
It does not use `sudo`.

## Requirements

Before installing, ensure the machine has:

- Bash
- `curl` and `tar` for remote installation
- Rust and Cargo 1.82 or newer
- Network access to GitHub and crates.io

Install Rust with [rustup](https://rustup.rs/) if needed, then confirm the tools:

```bash
rustc --version
cargo --version
curl --version
tar --version
```

Argo orchestrates coding-agent CLIs but does not install them. Install and
authenticate at least one supported CLI separately; `argo doctor` reports what is
available.

## One-shot installation

After the repository is public, install the current `main` branch with:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh | bash
```

The script:

1. Downloads a clean GitHub source archive into a temporary directory.
2. Runs `cargo build --release --locked` using the committed `Cargo.lock`.
3. Stops an existing Argo daemon before replacing the executable.
4. Installs `argo` to `~/.local/bin/argo` with executable permissions.
5. Applies an ad-hoc signature on macOS when `codesign` is available.
6. Deletes the temporary source and build directory.

The installer never stores a GitHub token and does not modify shell profile files.
If `~/.local/bin` is not on `PATH`, it prints the command needed to add it.

## Inspect before running

Piping a remote script directly to a shell is convenient, but reviewing it first
is safer:

```bash
curl -fsSL \
  https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh \
  -o /tmp/argo-install.sh
less /tmp/argo-install.sh
bash /tmp/argo-install.sh
```

For reproducible installation, pin a release tag or full commit SHA instead of
following `main`:

```bash
curl -fsSL \
  https://raw.githubusercontent.com/MaticAlgos/argo/v0.1.0/install.sh \
  | ARGO_INSTALL_REF=v0.1.0 bash
```

The script URL and `ARGO_INSTALL_REF` should identify the same revision.

## Choose an installation directory

The default destination is `~/.local/bin`. Override it with
`ARGO_INSTALL_DIR` (or the equivalent `ARGO_PREFIX`):

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh \
  | ARGO_INSTALL_DIR="$HOME/bin" bash
```

Then ensure the directory is on `PATH`:

```bash
export PATH="$HOME/bin:$PATH"
```

Add that export to the appropriate shell profile only if you want it to persist.

## Install from a clone

The same installer recognizes a local checkout and builds it directly without
performing another download:

```bash
git clone https://github.com/MaticAlgos/argo.git
cd argo
./install.sh
```

To build and install manually:

```bash
cargo build --release --locked
mkdir -p ~/.local/bin
install -m 755 target/release/argo ~/.local/bin/argo
```

On macOS, the installer additionally removes the old executable before copying
and applies an ad-hoc signature. This avoids invalidating an existing Mach-O
signature through in-place replacement.

## Install while the repository is private

Create a fine-grained GitHub token with read-only **Contents** permission for the
repository. Fetch the installer through the GitHub API and pass the token only to
the child installer process:

```bash
curl -fsSL \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "Accept: application/vnd.github.raw+json" \
  "https://api.github.com/repos/MaticAlgos/argo/contents/install.sh?ref=main" \
  | GITHUB_TOKEN="$GITHUB_TOKEN" bash
```

The installer uses `GITHUB_TOKEN` only as an HTTP authorization header while
downloading the private source archive. It does not write the token to disk.
Prefer a short-lived, repository-scoped token and unset it afterward:

```bash
unset GITHUB_TOKEN
```

## Verify the installation

```bash
argo --version
argo doctor
```

`argo doctor` reports the data directory, database health, and detected coding
CLIs. Start Argo from the project you want an agent to work in:

```bash
cd /path/to/project
argo
```

## Update

Run the installer again with the desired revision. It builds the replacement
first, asks the existing daemon to stop, and then replaces the command:

```bash
curl -fsSL https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh | bash
```

For stable environments, update between explicit release tags instead of `main`.
Conversation data is stored outside the executable and is preserved across
updates.

## Uninstall

Stop the daemon and remove the executable:

```bash
argo stop
rm -f ~/.local/bin/argo
```

This intentionally preserves conversations and configuration. Default state
locations are:

- macOS: `~/Library/Application Support/dev.argo.argo`
- Linux: `~/.local/share/argo`

To remove all Argo state as well, inspect the directory first and then delete it.
This permanently removes conversations, sessions, MCP configuration, and OAuth
tokens:

```bash
# macOS
rm -rf "$HOME/Library/Application Support/dev.argo.argo"

# Linux
rm -rf "$HOME/.local/share/argo"
```

If `ARGO_DATA_DIR` was set, remove that custom directory instead.

## Troubleshooting

### `cargo not found` or Rust is too old

Install/update Rust with rustup:

```bash
rustup update stable
rustup default stable
```

### `argo: command not found` after installation

Add the installation directory to the current shell:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

### GitHub download fails

Confirm the repository/ref exists and that the machine can reach
`raw.githubusercontent.com`, `codeload.github.com`, and crates.io. Private
repositories require `GITHUB_TOKEN` as described above.

### Build fails with a dependency resolution error

The installer uses `--locked`; do not delete or regenerate `Cargo.lock` during an
install. Retry with the current installer and a supported Rust toolchain. If the
failure persists, include the selected `ARGO_INSTALL_REF`, operating system,
architecture, `rustc --version`, and complete Cargo error in the issue report.

### Existing daemon is still running

Normally the installer stops it before replacing the executable. Stop it
explicitly and retry:

```bash
argo stop
```

If the old command is no longer available, terminating that user's `argo daemon`
process is safe; the durable SQLite conversation store remains on disk.
