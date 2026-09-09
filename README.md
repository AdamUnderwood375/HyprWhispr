# Hyprland VoiceType

Wispr Flow-style voice dictation for Linux, in Rust. Deepgram **streaming** STT,
GTK4 layer-shell pill overlay, clipboard+paste injection. Defaults to local
[faster-whisper](https://github.com/SYSTRAN/faster-whisper) so no API key is
needed.

Why it's fast: audio goes to Deepgram over a websocket *while you speak*, so on
key-release only the tail is outstanding (~300–500 ms measured), instead of
uploading a whole WAV or loading a local Whisper model after the fact. Local
mode streams to a Python worker over a Unix socket with interim transcripts.

## Quick start

### A. Local — no API key (default)

```bash
pip install faster-whisper numpy
# model is downloaded on first run (~40–150 MB depending on local_model);
# or pre-fetch: python -c "from faster_whisper import WhisperModel; WhisperModel('tiny.en')"
cargo build --release
install -m755 target/release/speakspic ~/.local/bin/speakspic
install -m755 local_worker.py ~/.local/bin/local_worker.py
# ensure ~/.local/bin is on PATH:
export PATH="$HOME/.local/bin:$PATH"   # add to ~/.bashrc / ~/.zshrc
speakspic          # daemon — first run writes ~/.config/speakspic/config.toml with provider="local"
```

`provider = "local"` is the default when no `api_key` is set. See **Config** below for `provider`/`local_model`.

### B. Deepgram — cloud streaming (lowest tail latency)

```bash
export DEEPGRAM_API_KEY="your-key-here"   # or set api_key in config.toml
# set provider to deepgram:
# ~/.config/speakspic/config.toml -> provider = "deepgram"
cargo build --release
install -m755 target/release/speakspic ~/.local/bin/speakspic
speakspic
```

On first run with `provider = "deepgram"` you must have `api_key` or
`$DEEPGRAM_API_KEY` set or the daemon will refuse to start.

## Build & install

```bash
cargo build --release
install -m755 target/release/speakspic ~/.local/bin/speakspic
install -m755 local_worker.py ~/.local/bin/local_worker.py
```

### System dependencies

```bash
# Arch
sudo pacman -S gtk4 gtk4-layer-shell wl-clipboard pkg-config
# Debian/Ubuntu
sudo apt install libgtk-4-dev libgtk4-layer-shell-dev wl-clipboard pkg-config
```

Also needs `wl-copy`, and `hyprctl` or `wtype` for paste injection.

> **Note:** building `gtk4`/`gtk4-layer-shell` crates requires `pkg-config` and
> the GTK4 development headers (`libgtk-4-dev` / `gtk4`). If `cargo build`
> fails with `pkg-config not found` or `gtk4 not found`, install the packages
> above.

Needs: GTK4, gtk4-layer-shell, `wl-copy`, and `hyprctl` or `wtype`.

## Config — `~/.config/speakspic/config.toml`

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
| `local_model` | `"tiny.en"` | faster-whisper model for local provider. Larger models are more accurate but slower. |

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
cargo run --example probe   # speak for 4 s — requires DEEPGRAM_API_KEY (Deepgram only)
```

Asserts mic capture is non-empty and prints sample rate, peak level, tail
latency, and the transcript. This example uses the Deepgram backend directly
and requires `DEEPGRAM_API_KEY` to be set (it does not test the local
faster-whisper path).

## License

MIT OR Apache-2.0 — see [LICENSE](LICENSE).
