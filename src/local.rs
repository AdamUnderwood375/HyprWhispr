use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Live local transcription session: streams PCM to the Python worker and
/// yields interim transcripts as they arrive. Mirrors deepgram::Session API
/// but adds interim callbacks for the live overlay.
pub struct Session {
    audio_tx: async_channel::Sender<Vec<i16>>,
    done: tokio::task::JoinHandle<Result<String>>,
}

impl Session {
    pub async fn connect(sample_rate: u32, interim_tx: async_channel::Sender<String>) -> Result<Self> {
        ensure_worker().await?;
        let path = crate::config::local_socket_path();
        let mut stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("local whisper worker not running on {}", path.display()))?;

        // handshake: 4-byte LE sample rate
        stream.write_all(&sample_rate.to_le_bytes()).await?;

        let (read_half, mut write_half) = stream.into_split();
        let (audio_tx, audio_rx) = async_channel::unbounded::<Vec<i16>>();

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
        let _ = self.audio_tx.try_send(pcm);
    }

    pub async fn finish(self) -> Result<String> {
        self.audio_tx.close();
        self.done.await?
    }
}

pub async fn ensure_worker() -> Result<()> {
    let path = crate::config::local_socket_path();
    if tokio::net::UnixStream::connect(&path).await.is_ok() {
        return Ok(());
    }
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
        .join("speakspic");
    let _ = std::fs::create_dir_all(&runtime);
    let log_path = runtime.join("worker.log");
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
        .env("SPEAKSPIC_MODEL", &model)
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

    // detach: spawn and forget; worker daemonizes via socket listen loop
    let mut child = cmd.spawn().context("failed to spawn local whisper worker")?;
    // give it a moment to bind (model load can take ~3s on cold start)
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if UnixStream::connect(&path).await.is_ok() {
            // detach: don't wait
            std::mem::forget(child);
            return Ok(());
        }
        // if child exited, bubble error
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!("local worker exited early with {status} (check {})", log_path.display());
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
    let candidates = [
        home_path.join("speakspic/local_worker.py"),
        home_path.join(".local/share/speakspic/local_worker.py"),
        std::path::PathBuf::from("local_worker.py"),
    ];
    for p in candidates {
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!("local_worker.py not found")
}
