# Vicuna Signal Bridge Plugin

Talk to any [Vicuna](https://github.com/anslwy/vicuna) Work session from **Signal** using [`signal-cli`](https://github.com/AsamK/signal-cli) in JSON-RPC daemon mode.

## Prerequisites

Install `signal-cli` and make sure it is available on your `PATH` (or set `SIGNAL_CLI_BIN` to its path):

- **macOS (Homebrew):** `brew install signal-cli`
- **Linux / Windows:** download a release from [AsamK/signal-cli](https://github.com/AsamK/signal-cli/releases) (requires Java 21+ or the native GraalVM build).
- **Optional (voice memos):** Signal mobile apps send voice notes as AAC (`.m4a`). If `ffmpeg` is on your `PATH`, the plugin automatically transcodes voice memos to OGG/Opus for Vicuna's built-in Whisper transcription; otherwise voice memos are staged as file attachments.

## Setup

Requires Vicuna v0.0.69 or newer. Install this plugin in Vicuna (**Settings → Plugins → Install from GitHub**: `anslwy/vicuna-plugin-signal`), open a Work session and click the **Signal** bridge icon. Then pick one of two setups:

### A. Separate Vicuna number (recommended)

Vicuna shows up as its own contact, and you get push notifications for its replies and questions. You need a second number that can receive one SMS (a dual-SIM or eSIM phone is fine; VoIP numbers are often rejected).

1. Solve the CAPTCHA at <https://signalcaptchas.org/registration/generate.html> and copy the `signalcaptcha://…` link (it expires within minutes).
2. In the dialog, enter `+<number> signalcaptcha://…` and press **Continue**.
3. Enter the 6-digit SMS code and press **Verify** (add the registration-lock PIN after the code if the number has one, or type `voice` to get a call instead). The profile name is set to **Vicuna**.
4. Send `!pair <code>` to that number from your own Signal account, either directly or in a group with just you and Vicuna (one group per Work session).

The keys live only in `~/vicuna/config/plugins/signal-cli-data`. Keep the plugin running now and then: Signal deactivates accounts that stay offline for months.

### B. Link your own account

1. In the dialog, type `link` and press **Continue**.
2. On your phone, open **Signal → Settings → Linked Devices → Link New Device** and scan the QR code.
3. Send `!pair <code>` in **Note to Self** or in a **solo Signal group** (*New Group → leave members empty → Create*) per Work session.

Replies then come from your own account, so Signal sends no push notifications for them.

## Chat Commands

- `!pair <code>` — pair this chat/group with a Vicuna Work session
- `!stop` — stop the currently running turn
- `!send` — flush queued messages immediately
- `!status` — show session title, engine, model, and status
- `!model` / `!model <engine> <model>` — show or switch the session's engine/model
- `!help` — list commands

When Vicuna asks a multiple-choice question (`ask`), reply with the option number (`1`, `2`, …) or freeform text.
