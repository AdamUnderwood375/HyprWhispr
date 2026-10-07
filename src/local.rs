use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Release the model this long after the last dictation. Same rule as the pi
/// `dictate` extension (`IDLE_TTL_MS`): spawn on demand, hold for a few minutes
/// so re-toggles are instant, then exit so an idle machine pays zero RAM.
const IDLE_TTL_SECS: u64 = 300;
/// Watchdog poll interval. Keep well under the TTL so the kill lands promptly.
const TTL_TICK_SECS: u64 = 15;

static WORKER_PID: AtomicU32 = AtomicU32::new(0);
static LAST_USE_MS: AtomicU64 = AtomicU64::new(0);
static WATCHDOG_STARTED: AtomicU32 = AtomicU32::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mark the worker as just-used, restarting the idle countdown.
fn touch() {
    LAST_USE_MS.store(now_ms(), Ordering::Relaxed);
}

/// Kill the worker and drop its socket. Idempotent.
pub fn release_worker(why: &str) {
    let pid = WORKER_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        eprintln!("local worker: {why} — releasing whisper model (pid {pid})");
        // SIGTERM via kill(1): local_worker.py installs a SIGTERM handler that
        // exits cleanly. Avoids pulling a libc/nix dependency for one signal.
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = std::fs::remove_file(crate::config::local_socket_path());
}

/// Background loop that enforces IDLE_TTL_SECS. Started once, lives for the
/// daemon's lifetime, costs nothing when no worker is running.
fn spawn_watchdog() {
    if WATCHDOG_STARTED.swap(1, Ordering::SeqCst) == 1 {
        return;
    }
    tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(TTL_TICK_SECS)).await;
            if WORKER_PID.load(Ordering::Relaxed) == 0 {
                continue;
            }
            let last = LAST_USE_MS.load(Ordering::Relaxed);
            if last != 0 && now_ms().saturating_sub(last) > IDLE_TTL_SECS * 1000 {
                release_worker("idle TTL expired");
            }
        }
    });
}

/// Live local transcription session: streams PCM to the Python worker and
/// yields interim transcripts as they arrive. Mirrors deepgram::Session API
/// but adds interim callbacks for the live overlay.
pub struct Session {
    audio_tx: async_channel::Sender<Vec<i16>>,
    done: tokio::task::JoinHandle<Result<String>>,
}

impl Session {
    pub async fn connect(
        sample_rate: u32,
        interim_tx: async_channel::Sender<String>,
    ) -> Result<Self> {
        ensure_worker().await?;
        let path = crate::config::local_socket_path();
        let mut stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("local whisper worker not running on {}", path.display()))?;

        // handshake: 4-byte LE sample rate
        stream.write_all(&sample_rate.to_le_bytes()).await?;

        let (read_half, mut write_half) = stream.into_split();
        // bounded(64) to cap memory if worker stalls; drop-oldest via try_send fallback
        let (audio_tx, audio_rx) = async_channel::bounded::<Vec<i16>>(64);

        // pump PCM -> socket
        tokio::spawn(async move {
            while let Ok(pcm) = audio_rx.recv().await {
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                if write_half.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            let _ = write_half.shutdown().await;
        });

        let done = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(read_half);
            let mut line = String::new();
            let mut final_text = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    break;
                }
                let v: serde_json::Value =
                    serde_json::from_str(line.trim()).unwrap_or(serde_json::Value::Null);
                if let Some(s) = v.get("interim").and_then(|x| x.as_str()) {
                    let _ = interim_tx.try_send(s.to_string());
                } else if let Some(s) = v.get("final").and_then(|x| x.as_str()) {
                    final_text = s.to_string();
                    let _ = interim_tx.try_send(s.to_string());
                    break;
                } else if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
                    anyhow::bail!("local whisper error: {e}");
                }
            }
            Ok(final_text)
        });

        Ok(Self { audio_tx, done })
    }

    pub fn send(&self, pcm: Vec<i16>) {
        // bounded(64) + try_send fallback: cap memory; drop newest if full
        let _ = self.audio_tx.try_send(pcm);
    }

    pub async fn finish(self) -> Result<String> {
        touch();
        self.audio_tx.close();
        self.done.await?
    }
}

pub async fn ensure_worker() -> Result<()> {
    spawn_watchdog();
    touch();
    let path = crate::config::local_socket_path();
    if tokio::net::UnixStream::connect(&path).await.is_ok() {
        return Ok(());
    }
    // The watchdog may have just killed the worker without unlinking the socket;
    // do it here so the liveness probe above can't see a dead socket.
    WORKER_PID.store(0, Ordering::Relaxed);
    let _ = std::fs::remove_file(&path);
    // spawn detached worker; it will listen on the same socket
    let worker_py = find_worker_py()?;
    let cfg = crate::config::load_for_worker().unwrap_or_default();
    let model = cfg.local_model.clone();

    let runtime = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let h = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(h).join(".local/share")
        })
        .join("linux-whisper");
    let _ = std::fs::create_dir_all(&runtime);
    let log_path = runtime.join("worker.log");
    // log rotation: cap worker.log to 5 MiB (note: truncate on startup if oversize; for production use rotating file appender)
    const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(&log_path)
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = std::fs::write(&log_path, b"");
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let err_file = log_file.as_ref().and_then(|f| f.try_clone().ok());

    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(&worker_py)
        .arg(format!("--model={}", model))
        .arg(format!("--sock={}", path.display()))
        .env("LINUX_WHISPER_MODEL", &model)
        .stdin(std::process::Stdio::null());

    if let Some(f) = log_file {
        cmd.stdout(f);
    } else {
        cmd.stdout(std::process::Stdio::null());
    }
    if let Some(f) = err_file {
        cmd.stderr(f);
    } else {
        cmd.stderr(std::process::Stdio::null());
    }
    cmd.kill_on_drop(false);

    // keep Child handle and wait in background instead of std::mem::forget
    let path_keep = path.clone();
    let mut child = cmd
        .spawn()
        .context("failed to spawn local whisper worker")?;
    if let Some(pid) = child.id() {
        WORKER_PID.store(pid, Ordering::SeqCst);
    }
    // give it a moment to bind (model load can take ~3s on cold start)
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if UnixStream::connect(&path).await.is_ok() {
            let log_display = log_path.display().to_string();
            tokio::spawn(async move {
                match child.wait().await {
                    Ok(s) => {
                        WORKER_PID.store(0, Ordering::SeqCst);
                        let _ = std::fs::remove_file(&path_keep);
                        eprintln!("local worker exited with {s} (log {log_display})")
                    }
                    Err(e) => eprintln!("local worker wait error: {e}"),
                }
            });
            return Ok(());
        }
        // if child exited, bubble error
        if let Ok(Some(status)) = child.try_wait() {
            WORKER_PID.store(0, Ordering::Relaxed);
            anyhow::bail!(
                "local worker exited early with {status} (check {})",
                log_path.display()
            );
        }
    }
    anyhow::bail!(
        "local worker failed to appear on {} after 10s (tried {})",
        path.display(),
        worker_py.display()
    )
}

fn find_worker_py() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = std::path::Path::new(&home);
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // exe-dir sibling — supports `install -m755 target/release/linux-whisper ~/.local/bin/linux-whisper`
    // with `install -m755 local_worker.py ~/.local/bin/local_worker.py`
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("local_worker.py"));
    }
    // ~/.local/bin sibling — covers the common install layout even when exe path is different
    candidates.push(home_path.join(".local/bin/local_worker.py"));
    candidates.extend([
        home_path.join("linux-whisper/local_worker.py"),
        home_path.join(".local/share/linux-whisper/local_worker.py"),
        std::path::PathBuf::from("local_worker.py"),
    ]);
    for p in &candidates {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    anyhow::bail!(
        "local_worker.py not found (searched: {})",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
