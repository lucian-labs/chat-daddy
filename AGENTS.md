# AGENTS.md

Instructions for AI coding agents working with this repository.

## What This Is

Chat Daddy is a minimal, keyboard-driven chat transcript viewer for AI coding assistants (Claude Code, Cursor, Codex). Pixel-buffer rendered with minifb, no GPU. Rust only.

## One-Shot Setup Script

Copy-paste this entire block. It detects the platform, downloads the correct binary, handles the font requirement, and launches. **No decisions required.**

```bash
#!/usr/bin/env bash
set -euo pipefail

# ── Asset naming convention ──
# Binary names follow: chat-daddy-{os}-{arch}
# where {os} = lowercase uname -s (darwin, linux)
# and {arch} = uname -m (arm64, x86_64)
# This means agents can construct the name directly — no lookup table needed.
ASSET="chat-daddy-$(uname -s | tr A-Z a-z)-$(uname -m)"

INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"

# ── Download ──
if command -v gh &>/dev/null; then
  gh release download --repo ELI7VH/chat-daddy --pattern "$ASSET" --dir /tmp --clobber
else
  curl -fSL -o "/tmp/$ASSET" "https://github.com/ELI7VH/chat-daddy/releases/latest/download/$ASSET"
fi
chmod +x "/tmp/$ASSET"
mv "/tmp/$ASSET" "$INSTALL_DIR/chat-daddy"

# ── Ensure ~/.local/bin is on PATH ──
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$HOME/.zshrc" 2>/dev/null \
    || echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$HOME/.bashrc" 2>/dev/null
  export PATH="$INSTALL_DIR:$PATH"
fi

# ── Font check ──
FONT_OK=false
if [ "$(uname -s)" = "Darwin" ]; then
  find ~/Library/Fonts /Library/Fonts /System/Library/Fonts -iname "*firacode*" -print -quit 2>/dev/null | grep -q . && FONT_OK=true
elif [ "$(uname -s)" = "Linux" ]; then
  fc-list 2>/dev/null | grep -qi "fira code" && FONT_OK=true
fi
if ! $FONT_OK; then
  echo "Fira Code not found. Install it or edit ~/.chat-daddy/config.json -> \"font\" to any monospace font you have."
fi

# ── Launch ──
chat-daddy &
```

For **Windows**: `chat-daddy-windows-x86_64.exe` — download from the latest release and run.

If no prebuilt binary exists for the platform (the curl/gh step fails), fall back to `cargo build --release`.

## Asset Naming Convention

Binary names follow a **deterministic pattern** that matches raw `uname` output:

```
chat-daddy-{os}-{arch}
```

- `{os}` = `uname -s | tr A-Z a-z` → `darwin`, `linux`
- `{arch}` = `uname -m` → `arm64`, `x86_64`

This means an agent can **construct** the asset name instead of looking it up:

```bash
ASSET="chat-daddy-$(uname -s | tr A-Z a-z)-$(uname -m)"
```

### All Available Assets

| Platform               | Asset name                       | How `uname` maps                |
|------------------------|----------------------------------|---------------------------------|
| macOS ARM64 (M1/M2+)   | `chat-daddy-darwin-arm64`        | `darwin` + `arm64`              |
| macOS x64 (Intel)       | `chat-daddy-darwin-x86_64`       | `darwin` + `x86_64`            |
| Linux x86_64            | `chat-daddy-linux-x86_64`        | `linux` + `x86_64`             |
| Windows x86_64          | `chat-daddy-windows-x86_64.exe`  | N/A — just use this name        |

### Download (manual)

```bash
# Construct asset name automatically
ASSET="chat-daddy-$(uname -s | tr A-Z a-z)-$(uname -m)"

# gh CLI (always fetches latest)
gh release download --repo ELI7VH/chat-daddy --pattern "$ASSET" --dir .

# or curl
curl -fSL -o chat-daddy "https://github.com/ELI7VH/chat-daddy/releases/latest/download/$ASSET"
chmod +x chat-daddy
```

## Build from Source (Fallback)

Only needed if no prebuilt binary exists for the user's OS/arch.

```bash
git clone https://github.com/ELI7VH/chat-daddy.git
cd chat-daddy
cargo build --release
```

Binary output: `target/release/chat-daddy` (Linux/macOS) or `target/release/chat-daddy.exe` (Windows).

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `error: font not found` or blank window | Fira Code not installed | Install Fira Code **or** edit `~/.chat-daddy/config.json` → set `"font"` to any installed monospace font |
| `Permission denied` | Binary not executable | `chmod +x chat-daddy` |
| `gh: command not found` | GitHub CLI not installed | Use the `curl` method instead |
| `cargo: command not found` | Rust not installed | Use the binary download method instead of building |
| Window opens but no chats | No transcript sources found | Check that `~/.claude/`, `~/.cursor/`, or `~/.codex/` exist with chat data |

## Run

```bash
chat-daddy              # if on PATH
./chat-daddy            # if in current directory
cargo run --release     # if building from source
```

No arguments needed — config auto-generates on first run at `~/.chat-daddy/config.json`.

## Architecture

- **Single file app**: `src/main.rs` (~3000 lines) — all logic, rendering, networking
- **Join codes / sync crypto**: `src/join.rs` — code format, key derivation, sealed beacons and TCP channel
- **Font rendering**: `src/font.rs` — TTF rasterization via fontdue with glyph caching
- **Config**: `~/.chat-daddy/config.json` — auto-generated, stores sources, font, theme, LLM endpoint
- **Chat metadata**: `~/.chat-daddy/chats.json` — per-chat names, stars, auto_named flags
- **No async**: Uses `std::thread` + `Arc<Mutex<>>` for background networking (UDP discovery, TCP server)

## Dependencies

All in Cargo.toml — no system libraries needed except a monospace TTF font (defaults to Fira Code, weight 300).

## Key Concepts

- **Sources**: Chat transcript directories (Claude `~/.claude/projects`, Cursor `~/.cursor/agent-transcripts`, Codex `~/.codex/sessions`). Auto-detected on first run.
- **LAN Sync**: off until a join code is set (`J` join, `Shift+J` new code; `"join_code"` in config). Machines sharing the code discover each other via sealed UDP beacons on port 21847 and exchange chats over TCP (random port) with a nonce handshake + ChaCha20-Poly1305 frames — see `src/join.rs`. Peers appear inline with `hostname · platform` labels.
- **LLM Auto-Naming**: Optional local Qwen2.5-0.5B on port 1235 for naming unnamed chats. Not required — app works fine without it.

## Commit Rules

- Co-author line: `Co-Authored-By: Ana Iliovic <ana@thevii.app> [<agent>, <model>, <provider>]`
- Push to `main` directly — no branches, no PRs for this project
