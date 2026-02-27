<p align="center">
  <img src="./docs/header.png" alt="Buzz - Voice to text with a quick buzz" width="100%" />
</p>

# Buzz 🐝

[![CI](https://github.com/SawyerHood/buzz/actions/workflows/ci.yml/badge.svg)](https://github.com/SawyerHood/buzz/actions/workflows/ci.yml)

Voice-to-text with a quick buzz.

Buzz is a macOS-native menubar app that turns speech into text wherever your cursor is. Press your hotkey, talk, release, and your transcript is inserted directly into the active app.

## What Buzz Does

- Runs as a lightweight macOS tray/menubar app.
- Listens while you speak after a global hotkey trigger.
- Transcribes your speech with OpenAI.
- Inserts the final text at the current cursor position automatically.

## Key Features

- Global hotkey support with hold-to-talk and toggle modes
- OpenAI transcription with `gpt-4o-mini-transcribe`
- Realtime streaming audio input via WebSocket for low latency
- ChatGPT OAuth login (use your ChatGPT subscription, no API key required)
- Manual API key auth option (for direct OpenAI key usage)
- Auto-insert transcribed text at cursor position
- Floating recording overlay while capture is active
- Customizable keyboard shortcuts
- Transcript history view
- Settings UI with a two-column layout
- Launch at login
- Local transcript history storage on device
- macOS-native architecture using Tauri v2 (lightweight compared to Electron apps)

## Tech Stack

- Tauri v2
- React
- TypeScript
- Rust
- shadcn/ui

## Getting Started

### Prerequisites

- Node.js (LTS recommended)
- `pnpm`
- Rust toolchain (`rustup`, `cargo`)
- macOS (this app is macOS-only)

### Clone and Install

```bash
git clone <your-repo-url>
cd voice
pnpm install
```

### Run in Development

```bash
pnpm tauri dev
```

### Build the macOS App

```bash
pnpm tauri build
```

Build output:

- `.app` bundle: `src-tauri/target/release/bundle/macos/`

## Auto Updates (GitHub Releases)

Buzz uses Tauri's updater plugin and checks:

- `https://github.com/SawyerHood/buzz/releases/latest/download/latest.json`

### Signing prerequisites

Update artifacts must be signed at build time.

```bash
export TAURI_SIGNING_PRIVATE_KEY_PATH="$HOME/.tauri/buzz.key"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="<your-key-password>"
pnpm build:release
```

`pnpm build:release` runs `tauri build` with signing env vars so updater signatures are generated.

### Release artifact format

When signing is configured, Tauri generates updater signatures (`.sig`) and update metadata consumed by the updater endpoint. The release must publish `latest.json` in this shape:

```json
{
  "version": "0.1.0",
  "platforms": {
    "darwin-aarch64": {
      "signature": "...",
      "url": "https://github.com/SawyerHood/buzz/releases/download/v0.1.0/Buzz.app.tar.gz.sig"
    }
  }
}
```

## Permissions Needed (macOS)

Buzz requires:

- Microphone access (to capture speech)
- Accessibility access (to insert transcribed text at the cursor in other apps)

## Authentication Options

### 1. ChatGPT OAuth

- Sign in with your ChatGPT account inside Buzz
- Uses your ChatGPT subscription flow
- No manual API key required

### 2. Manual API Key

- Paste your OpenAI API key in settings
- Useful if you prefer direct API-billed usage

## Screenshots

Add screenshots here as they become available:

- Menubar/tray state
- Recording overlay
- Settings (two-column layout)
- Transcript history

```md
![Buzz menubar screenshot](./docs/screenshots/menubar.png)
![Buzz recording overlay](./docs/screenshots/overlay.png)
![Buzz settings](./docs/screenshots/settings.png)
![Buzz transcript history](./docs/screenshots/history.png)
```

## License / Credits

Personal project by Sawyer Hood.

No formal license file yet.
