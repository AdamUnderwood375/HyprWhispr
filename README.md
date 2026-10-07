# Linux Whisper

Wispr Flow-style voice dictation for Linux, in Rust. GTK4 layer-shell pill
overlay, streaming local [faster-whisper](https://github.com/SYSTRAN/faster-whisper)
by default so no API key is needed, Deepgram optional.

Why it's fast: audio is streamed to the worker over a Unix socket *while you
speak*, so on key-release only the tail is outstanding (~300–500 ms), instead
of uploading a whole WAV or loading a model after the fact. Interim transcripts
land in the pill live.

## RAM: nothing is resident between dictations

Neither the daemon nor the model sits in RAM while you're not dictating.

- **No autostart.** `linux-whisper toggle` starts the daemon on the first
  press (~80 ms) and connects once its socket is bound.
- **No pre-warm.** The worker is spawned by the first dictation press, not at
  daemon startup.
- **A watchdog releases the model** (`SIGTERM` + socket cleanup) after **300 s
  with no dictation sessions** — the same idle-TTL rule as the pi `dictate`
  extension. The daemon also takes its worker down when it is shut down.
- Re-toggles inside that window are instant; after it, the next press pays a
  ~2–3 s model load, which the overlay shows as a **Loading model** pill with a
  spinner rather than pretending it is already listening. The mic is open the
  whole time, so nothing you say in those seconds is lost.

Idle cost is **zero processes**.

Tune it in `src/local.rs`:

```rust
const IDLE_TTL_SECS: u64 = 300;
```

## Quick start

### Local — no API key (default)

```bash
pip install faster-whisper numpy
cargo build --release
install -m755 target/release/linux-whisper ~/.local/bin/linux-whisper
install -m755 local_worker.py ~/.local/bin/local_worker.py
linux-whisper        # daemon — first run writes ~/.config/linux-whisper/config.toml
```

`provider = "local"` is the default when no `api_key` is set.

### Deepgram — cloud streaming (optional)

```bash
export DEEPGRAM_API_KEY="your-key-here"
# and set provider = "deepgram" in ~/.config/linux-whisper/config.toml
```

## System dependencies

```bash
# Arch
sudo pacman -S gtk4 gtk4-layer-shell wl-clipboard pkg-config
```

Needs `wl-copy`, and `hyprctl` or `wtype` for paste injection, plus `pw-record`
for mic capture.

## Run

No autostart needed — the keybind starts the daemon on demand:

```bash
linux-whisper toggle        # daemon spawns itself, overlay appears, starts recording
linux-whisper               # ...or run the daemon in the foreground to watch its log
```

First press records, second stops, transcribes and pastes at the cursor.

Hyprland (`hyprland.lua`) — no autostart line needed:

```lua
hl.bind("CTRL + " .. mainMod .. " + V",
        hl.dsp.exec_cmd("linux-whisper toggle"),
        { locked = true, description = "Dictation (Linux Whisper)" })
```

### Shell-bar tile

The daemon publishes its state to `/tmp/linux-whisper-state` — one of
`idle`, `loading`, `recording`, `transcribing`, `offline` — and removes it on
exit, so a crashed session can never leave a stale file behind. The Quickshell
tile (`settings/QuickTiles.qml`) polls that file and shells out to
`linux-whisper toggle`.

Before the first press the file doesn't exist, so the tile reads `offline`
rather than claiming a daemon is there.

## Config — `~/.config/linux-whisper/config.toml`

```toml
api_key = ""            # or $DEEPGRAM_API_KEY (only for provider="deepgram")
model = "nova-3"        # Deepgram model
language = "en"
endpointing = 300       # ms of silence before Deepgram finalizes a segment
preserve_clipboard = false
provider = "local"      # "local" | "deepgram"
local_model = "tiny.en" # faster-whisper model: "tiny.en", "base.en", "small.en", etc.
```

| Key | Default | Description |
|-----|---------|-------------|
| `api_key` | `""` | Deepgram API key (or `$DEEPGRAM_API_KEY`). Required only when `provider = "deepgram"`. |
| `model` | `"nova-3"` | Deepgram model name. |
| `language` | `"en"` | Language code. |
| `endpointing` | `300` | ms of silence before Deepgram finalizes a segment. |
| `preserve_clipboard` | `false` | Restore clipboard after paste. |
| `provider` | `"local"` | STT backend: `"local"` (faster-whisper, no API key) or `"deepgram"` (cloud streaming). |
| `local_model` | `"tiny.en"` | faster-whisper model for local provider. `tiny.en` is ~240 MB RSS int8; larger models are more accurate and heavier. |

## Self-check

```bash
cargo run --example probe   # requires DEEPGRAM_API_KEY — Deepgram path only
```

## License

MIT OR Apache-2.0 — see [LICENSE](LICENSE).