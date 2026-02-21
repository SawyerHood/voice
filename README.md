# Buzz 🐝

Voice-to-text with a quick buzz.

Buzz is a macOS-native menubar app that turns speech into text wherever your cursor is. Press your hotkey, talk, release, and your transcript is inserted directly into the active app.

## What Buzz Does

- Runs as a lightweight macOS tray/menubar app.
- Listens while you speak after a global hotkey trigger.
- Transcribes your speech with OpenAI.
- Inserts the final text at the current cursor position automatically.

## Key Features

- Global hotkey support with hold-to-talk, toggle, and double-tap modes
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
