#!/usr/bin/env python3
"""
Local Whisper worker for speakspic — live streaming dictation.

Protocol (Unix socket, one session per connection):
  Client connects, sends 4-byte LE u32 sample_rate, then streams raw PCM16LE mono
  until it shuts down the write side. Server transcribes incrementally and sends
  back line-delimited JSON:
    {"interim":"partial text"}
    {"final":"complete text"}
    {"error":"..."}
"""
import os, sys, socket, struct, json, time, threading, signal, pathlib

DEFAULT_SOCK = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "speakspic-local.sock")
SOCK_PATH = os.environ.get("SPEAKSPIC_LOCAL_SOCK", DEFAULT_SOCK)
MODEL_NAME = os.environ.get("SPEAKSPIC_MODEL", "tiny.en")
for a in sys.argv[1:]:
    if a.startswith("--sock="):
        SOCK_PATH = a.split("=", 1)[1]
    elif not a.startswith("-"):
        SOCK_PATH = a
    if a.startswith("--model="):
        MODEL_NAME = a.split("=", 1)[1]

print(f"[local_worker] loading faster-whisper model {MODEL_NAME!r} ...", file=sys.stderr, flush=True)
try:
    from faster_whisper import WhisperModel
except ImportError as e:
    print(f"missing faster-whisper: {e}", file=sys.stderr)
    sys.exit(1)

import numpy as np

# tiny.en is fastest; use int8, cpu_threads 4, beam 1 for live
try:
    model = WhisperModel(MODEL_NAME, device="cpu", compute_type="int8", cpu_threads=4)
except Exception as e:
    print(f"failed to load {MODEL_NAME}: {e}, trying tiny.en", file=sys.stderr)
    model = WhisperModel("tiny.en", device="cpu", compute_type="int8", cpu_threads=4)
    MODEL_NAME = "tiny.en"

print(f"[local_worker] model {MODEL_NAME} ready, listening on {SOCK_PATH}", file=sys.stderr, flush=True)

# cleanup stale socket
try:
    if os.path.exists(SOCK_PATH):
        os.unlink(SOCK_PATH)
except: pass
os.makedirs(os.path.dirname(SOCK_PATH) or ".", exist_ok=True)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(SOCK_PATH)
srv.listen(4)
# ensure perms
try: os.chmod(SOCK_PATH, 0o700)
except: pass

model_lock = threading.Lock()

WINDOW_S = 30  # seconds to keep for interim windowed transcribe

def transcribe_pcm(pcm_i16: np.ndarray, sr: int, is_interim: bool = False) -> str:
    if pcm_i16.size == 0:
        return ""
    audio = pcm_i16.astype(np.float32) / 32768.0
    if sr != 16000:
        # Proper handling: prefer native 16 kHz capture (see src/audio.rs:26-56 —
        # device is asked for PREFERRED_RATE=16k when supported, so this branch
        # is fallback-only for devices lacking 16k). Linear np.interp is kept as
        # lightweight fallback; for better quality use polyphase (scipy.signal
        # resample_poly or libsamplerate) if available. Note: faster-whisper can
        # ingest any sr but we resample once to match model expectation.
        try:
            from scipy.signal import resample_poly  # type: ignore
            import math as _math
            g = _math.gcd(int(sr), 16000)
            audio = resample_poly(audio, 16000 // g, int(sr) // g).astype(np.float32)
        except ImportError:
            duration = len(audio) / sr
            target_len = int(duration * 16000)
            if target_len > 0:
                x_old = np.linspace(0, 1, len(audio))
                x_new = np.linspace(0, 1, target_len)
                # fallback linear — fast but introduces mild aliasing; rare path
                audio = np.interp(x_new, x_old, audio).astype(np.float32)
    # skip very short
    if len(audio) < 1600:  # <0.1s
        return ""
    with model_lock:
        try:
            # For windowed interim (is_interim=True) we enable
            # condition_on_previous_text (faster-whisper supports it) so the
            # 30s slices maintain continuity; alternatively incremental VAD
            # segments could be emitted but window+condition is the minimal
            # O(W) fix.
            segments, info = model.transcribe(
                audio,
                language="en",
                beam_size=1,
                best_of=1,
                temperature=0.0,
                vad_filter=True,
                vad_parameters=dict(min_silence_duration_ms=300),
                condition_on_previous_text=is_interim,
            )
            text = " ".join(s.text.strip() for s in segments).strip()
            if not text:
                segments, info = model.transcribe(
                    audio,
                    language="en",
                    beam_size=1,
                    best_of=1,
                    temperature=0.0,
                    vad_filter=False,
                    condition_on_previous_text=is_interim,
                )
                text = " ".join(s.text.strip() for s in segments).strip()
            return text
        except Exception as e:
            print(f"transcribe error: {e}", file=sys.stderr)
            return ""

def handle(conn):
    try:
        # first 4 bytes = sample_rate LE
        hdr = b""
        while len(hdr) < 4:
            chunk = conn.recv(4 - len(hdr))
            if not chunk:
                return
            hdr += chunk
        sr = struct.unpack("<I", hdr)[0]
        if sr < 8000 or sr > 192000:
            sr = 16000
        # print(f"session sr={sr}", file=sys.stderr)

        pcm_buf = bytearray()
        lock = threading.Lock()
        stop_flag = threading.Event()
        last_text = {"v": ""}

        def send(obj):
            try:
                conn.sendall((json.dumps(obj) + "\n").encode())
            except: pass

        def worker_loop():
            # CPU win: previously O(n^2) — full buffer re-transcribed every 400 ms
            # so total work for N seconds ≈ sum_{t=400ms}^{N} O(t) = O(N^2); at 60 s
            # that's ~150 transcribes of ~30 s avg => ~4500 s of audio processed.
            # Now slice to last WINDOW_S (30 s) so each tick is O(W) bounded
            # (~480k samples @16k, ~2.9 MB), constant CPU & memory regardless of
            # recording length. condition_on_previous_text=True preserves context.
            while not stop_flag.wait(0.40):
                with lock:
                    cur = bytes(pcm_buf)
                if len(cur) < 3200:  # <0.1s
                    continue
                max_bytes = WINDOW_S * sr * 2  # 2 bytes per i16
                if len(cur) > max_bytes:
                    cur = cur[-max_bytes:]
                arr = np.frombuffer(cur, dtype=np.int16)
                txt = transcribe_pcm(arr, sr, is_interim=True)
                if txt and txt != last_text["v"]:
                    last_text["v"] = txt
                    send({"interim": txt})
            # one final after stop (handled outside)

        t = threading.Thread(target=worker_loop, daemon=True)
        t.start()

        # read PCM stream until EOF
        while True:
            data = conn.recv(8192)
            if not data:
                break
            with lock:
                pcm_buf.extend(data)

        stop_flag.set()
        t.join(timeout=1.0)

        # final transcription
        with lock:
            final_bytes = bytes(pcm_buf)
        if len(final_bytes) < 640:  # <20ms => empty
            send({"final": ""})
        else:
            arr = np.frombuffer(final_bytes, dtype=np.int16)
            txt = transcribe_pcm(arr, sr)
            send({"final": txt})
    except Exception as e:
        try: conn.sendall((json.dumps({"error": str(e)})+"\n").encode())
        except: pass
        print(f"handle error: {e}", file=sys.stderr)
    finally:
        try: conn.shutdown(socket.SHUT_RDWR)
        except: pass
        try: conn.close()
        except: pass

def serve():
    while True:
        try:
            conn, _ = srv.accept()
            handle(conn)
        except KeyboardInterrupt:
            break
        except Exception as e:
            print(f"accept error: {e}", file=sys.stderr)
            time.sleep(0.2)

if __name__ == "__main__":
    # handle SIGTERM
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    serve()
