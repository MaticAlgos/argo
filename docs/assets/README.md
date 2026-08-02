# README assets

The agent marks in [`agents/`](agents/) identify command-line tools that Argo can
orchestrate. They do not imply sponsorship or endorsement. Product names,
logos, and trademarks belong to their respective owners.

| Local asset | Source |
|---|---|
| `agents/antigravity.png` | [Google Antigravity press assets](https://www.antigravity.google/press) |
| `agents/claude.png` | [Claude website favicon](https://claude.com/) |
| `agents/codex-*.svg` | Light/dark OpenAI mark from [Simple Icons](https://simpleicons.org/?q=openai); project reference: [OpenAI Codex](https://github.com/openai/codex) |
| `agents/command-code-*.svg` | [Command Code repository](https://github.com/CommandCodeAI/command-code/tree/main/.github/commandcode/logo) |
| `agents/grok-*.png` | Official light/dark marks referenced by the [Grok Build repository](https://github.com/xai-org/grok-build) |
| `agents/kiro.png` | [Kiro repository](https://github.com/kirodotdev/Kiro/blob/main/assets/kiro-icon.png) |
| `agents/opencode-*.svg` | [OpenCode repository](https://github.com/anomalyco/opencode/tree/dev/packages/console/app/src/asset) |

The source images were resized only where a smaller README payload was useful.
Light and dark variants are selected with HTML `<picture>` elements when the
owner publishes both.

[`screenshots/argo-home.svg`](screenshots/argo-home.svg) is a deterministic
screenshot-style rendering of the current Ratatui launch layout. Keeping it as
SVG makes the text sharp on HiDPI displays and lets contributors update the
version and capability labels without a machine-specific terminal theme.
