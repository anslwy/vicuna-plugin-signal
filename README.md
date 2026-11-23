# Vicuna Signal Bridge Plugin

Talk to any [Vicuna](https://github.com/anslwy/vicuna) Work session from **Signal** using [`signal-cli`](https://github.com/AsamK/signal-cli) in JSON-RPC daemon mode.

## Prerequisites

Install `signal-cli` and make sure it is available on your `PATH` (or set `SIGNAL_CLI_BIN` to its path):

- **macOS (Homebrew):** `brew install signal-cli`
- **Linux / Windows:** download a release from [AsamK/signal-cli](https://github.com/AsamK/signal-cli/releases) (requires Java 21+ or the native GraalVM build).
- **Optional (voice memos):** Signal mobile apps send voice notes as AAC (`.m4a`). If `ffmpeg` is on your `PATH`, the plugin automatically transcodes voice memos to OGG/Opus for Vicuna's built-in Whisper transcription; otherwise voice memos are staged as file attachments.

## Setup

1. Install this plugin in Vicuna (**Settings → Plugins → Install from GitHub**: `anslwy/vicuna-plugin-signal`, or install from a local folder after `cargo build --release`).
2. Open any Work session, click the **Signal** bridge icon, and click **Link Signal device**.
3. On your phone, open **Signal → Settings → Linked Devices → Link New Device** and scan the QR code.
4. Once linked, send `!pair <code>` in **Note to Self** (for a single session) or create a **solo Signal group** (*New Group → leave members empty → Create*) for each Work session you want to pair.

> **Tip (notifications):** Because a linked device shares your own Signal account, Signal won't send push notifications for replies in *Note to Self* or solo groups. If you want push notifications and a separate "Vicuna" contact, register a dedicated number with `signal-cli` and DM it or add it to a 2-person group.

## Chat Commands

- `!pair <code>` — pair this chat/group with a Vicuna Work session
- `!stop` — stop the currently running turn
- `!send` — flush queued messages immediately
- `!status` — show session title, engine, model, and status
- `!model` / `!model <engine> <model>` — show or switch the session's engine/model
- `!help` — list commands

When Vicuna asks a multiple-choice question (`ask`), reply with the option number (`1`, `2`, …) or freeform text.
