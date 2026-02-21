# OpenAI Realtime Transcription Repro + Fix

## Branch / Commit
- Branch: `voice-realtime-repro`
- Commit: `9649a20`
- Pushed: `origin/voice-realtime-repro`

## Standalone repro binary
- Location: `/tmp/realtime-repro`
- Built with: `tokio-tungstenite` (`native-tls`), `base64`, `serde_json`, `futures-util`

## Exact working protocol

1. Connect URL:
   - `wss://api.openai.com/v1/realtime?intent=transcription`
   - Important: **no `model` query parameter** when `intent=transcription`.

2. Headers:
   - `Authorization: Bearer <OPENAI_API_KEY>`
   - `OpenAI-Beta: realtime=v1`

3. First server event after connect:
   - `transcription_session.created`

4. Send session update event:

```json
{
  "type": "transcription_session.update",
  "session": {
    "input_audio_format": "pcm16",
    "turn_detection": null,
    "input_audio_transcription": {
      "model": "gpt-4o-mini-transcribe"
    }
  }
}
```

5. Send audio and commit:

```json
{
  "type": "input_audio_buffer.append",
  "audio": "<base64 pcm16 mono 24kHz>"
}
```

```json
{
  "type": "input_audio_buffer.commit"
}
```

6. Successful response flow observed:
   - `input_audio_buffer.committed`
   - `conversation.item.created`
   - `conversation.item.input_audio_transcription.completed`

## Key finding
- With default `turn_detection: { type: "server_vad", ... }`, non-speech test audio (sine wave) was dropped and commit failed with:
  - `input_audio_buffer_commit_empty` / `buffer only has 0.00ms`
- Setting `turn_detection: null` made the exact same append+commit flow work reliably end-to-end.

## Full successful session log (live API)

```text
Connecting to: wss://api.openai.com/v1/realtime?intent=transcription
UPDATE_MODE=transcription_session_manual_mini
SEND_RESPONSE_CREATE=false
SEND_BINARY_AUDIO=false
SKIP_APPEND=false
APPEND_MODE=audio
BASE64_MODE=standard
APPEND_COMMIT_DELAY_MS=0
AUDIO_SECONDS=1
Connected. HTTP status: 101 Switching Protocols
\n<-- RECV\n{
  "event_id": "event_DBcMGg3SqZG0ZtWvwZXrt",
  "session": {
    "client_secret": null,
    "expires_at": 1771665028,
    "id": "sess_DBcMGJFSKS3ltwdJPQlz3",
    "include": null,
    "input_audio_format": "pcm16",
    "input_audio_noise_reduction": null,
    "input_audio_transcription": null,
    "object": "realtime.transcription_session",
    "turn_detection": {
      "prefix_padding_ms": 300,
      "silence_duration_ms": 200,
      "threshold": 0.5,
      "type": "server_vad"
    }
  },
  "type": "transcription_session.created"
}
\n--> SEND\n{
  "session": {
    "input_audio_format": "pcm16",
    "input_audio_transcription": {
      "model": "gpt-4o-mini-transcribe"
    },
    "turn_detection": null
  },
  "type": "transcription_session.update"
}
\n<-- RECV\n{
  "event_id": "event_DBcMHgb23N4hIuG4gkyBQ",
  "session": {
    "client_secret": null,
    "expires_at": 1771665028,
    "id": "sess_DBcMGJFSKS3ltwdJPQlz3",
    "include": null,
    "input_audio_format": "pcm16",
    "input_audio_noise_reduction": null,
    "input_audio_transcription": {
      "language": null,
      "model": "gpt-4o-mini-transcribe",
      "prompt": null
    },
    "object": "realtime.transcription_session",
    "turn_detection": null
  },
  "type": "transcription_session.updated"
}
\n--> SEND\n{
  "audio": "<base64:64000 chars>",
  "type": "input_audio_buffer.append"
}
\n--> SEND\n{
  "type": "input_audio_buffer.commit"
}
\n<-- RECV\n{
  "event_id": "event_DBcMH36wsUUUq9vsZQZA1",
  "item_id": "item_DBcMHGioaChTAbTYTV2Sr",
  "previous_item_id": null,
  "type": "input_audio_buffer.committed"
}
\n<-- RECV\n{
  "event_id": "event_DBcMHfEdZC44IsdMd7vGt",
  "item": {
    "content": [
      {
        "transcript": null,
        "type": "input_audio"
      }
    ],
    "id": "item_DBcMHGioaChTAbTYTV2Sr",
    "object": "realtime.item",
    "role": "user",
    "status": "completed",
    "type": "message"
  },
  "previous_item_id": null,
  "type": "conversation.item.created"
}
\n<-- RECV\n{
  "content_index": 0,
  "event_id": "event_DBcMIifMGVIPY0K8ORgoL",
  "item_id": "item_DBcMHGioaChTAbTYTV2Sr",
  "transcript": "",
  "type": "conversation.item.input_audio_transcription.completed",
  "usage": {
    "input_token_details": {
      "audio_tokens": 10,
      "text_tokens": 0
    },
    "input_tokens": 10,
    "output_tokens": 2,
    "total_tokens": 12,
    "type": "tokens"
  }
}
\n===== RESULT =====
Final transcript: ""
\n<-- RECV CLOSE: None
```

## Voice app changes (`src-tauri/src/transcription/realtime.rs`)

1. Endpoint handling:
   - Always enforce `intent=transcription`.
   - Strip `model` query parameter from websocket URL.

2. WebSocket headers:
   - Added `OpenAI-Beta: realtime=v1`.

3. Session update payload:
   - Switched from unsupported `session.update` transcription schema to:
     - `type: "transcription_session.update"`
     - `session.input_audio_format = "pcm16"`
     - `session.turn_detection = null`
     - `session.input_audio_transcription = { model, language?, prompt? }`

4. Transcript parsing:
   - Accepts empty completed transcripts (e.g. `""`) as valid completion events.
   - Prevents false failures where API completed successfully but returned empty text.

5. Final result handling:
   - If `completed` event arrives, returns transcript even when empty.
   - Still errors if no completion/delta transcript is ever received.

6. Tests updated:
   - Session update payload shape tests.
   - Endpoint query behavior tests (`intent` enforced, `model` stripped).
   - Websocket protocol flow test expectations for new payload and URL.

## Validation run

- `cd /Users/sawyerhood/worktrees/voice-realtime-repro/src-tauri && cargo build` ✅
- `cd /Users/sawyerhood/worktrees/voice-realtime-repro/src-tauri && cargo test` ✅
- `cd /Users/sawyerhood/worktrees/voice-realtime-repro && pnpm install` ✅
- `cd /Users/sawyerhood/worktrees/voice-realtime-repro && pnpm build` ✅

