# Hyprland VoiceType

Wispr Flow-style voice dictation for Linux, in Rust. Deepgram **streaming** STT,
GTK4 layer-shell pill overlay, clipboard+paste injection.

Why it's fast: audio goes to Deepgram over a websocket *while you speak*, so on
key-release only the tail is outstanding (~300–500 ms measured), instead of
uploading a whole WAV or loading a local Whisper model after the fact.

## Build & install

```bash
cargo build --release
install -m755 target/release/speakspic ~/.local/bin/speakspic
```

Needs: GTK4, gtk4-layer-shell, `wl-copy`, and `hyprctl` or `wtype`.

## Config — `~/.config/speakspic/config.toml`

```toml
api_key = ""            # or $DEEPGRAM_API_KEY
model = "nova-3"
language = "en"
endpointing = 300       # ms of silence before Deepgram finalizes a segment
preserve_clipboard = false
```

Written with defaults on first run.

## Run

Daemon (holds the overlay + control socket at `$XDG_RUNTIME_DIR/speakspic.sock`):

```bash
speakspic
```

Toggle recording (what the compositor keybind runs):

```bash
speakspic toggle
```

Hyprland:

```
exec-once = ~/.local/bin/speakspic
bindl = SUPER SHIFT, SPACE, exec, ~/.local/bin/speakspic toggle
```

First toggle records, second stops, transcribes and pastes at the cursor.

## Self-check

```bash
cargo run --example probe   # speak for 4 s
```

Asserts mic capture is non-empty and prints sample rate, peak level, tail
latency, and the transcript.
