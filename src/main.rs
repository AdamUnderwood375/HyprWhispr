mod audio;
mod config;
mod deepgram;
mod inject;
mod local;
mod overlay;

use anyhow::{Context, Result};
use gtk::prelude::*;
use overlay::State;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use tokio::net::UnixListener;

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("toggle") => return toggle(),
        Some(other) => anyhow::bail!("unknown command {other:?} (only `toggle`)"),
        None => {}
    }
    let cfg = config::load()?;
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let (ui_tx, ui_rx) = async_channel::unbounded::<UiMsg>();
    let ui_tx_serve = ui_tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        // bridge State -> UiMsg for the few State sends inside serve
        let (state_tx, state_rx) = async_channel::unbounded::<State>();
        // forward states from serve's internal channel to ui_tx
        let fwd_tx = ui_tx_serve.clone();
        rt.spawn(async move {
            while let Ok(s) = state_rx.recv().await {
                let _ = fwd_tx.send(UiMsg::State(s)).await;
            }
        });
        if let Err(e) = rt.block_on(serve(cfg, state_tx.clone(), ui_tx_serve.clone())) {
            eprintln!("fatal: {e:#}");
            let _ = state_tx.try_send(State::Error);
            std::process::exit(1);
        }
    });

    let app = gtk::Application::builder()
        .application_id("dev.linuxwhisper")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let ov = overlay::Overlay::new(app);
        let hold = app.hold();
        let rx = ui_rx.clone();
        gtk::glib::spawn_future_local(async move {
            let _hold = hold;
            while let Ok(msg) = rx.recv().await {
                match msg {
                    UiMsg::State(s) => {
                        ov.set_state(s);
                        if matches!(s, State::Done | State::Error) {
                            let ms = if s == State::Done { 1200 } else { 3000 };
                            gtk::glib::timeout_future(std::time::Duration::from_millis(ms)).await;
                            if ov.state() == s {
                                ov.set_state(State::Hidden);
                            }
                        }
                    }
                    UiMsg::Transcript(t) => ov.set_transcript(&t),
                }
            }
        });
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// Client mode: what the compositor keybind runs.
///
/// If the daemon isn't up, start it here and wait for its socket. That keeps
/// the overlay + control socket out of RAM between dictations; the cost is one
/// process spawn (~80 ms) on the first press of a session.
fn toggle() -> Result<()> {
    let path = config::socket_path();
    let mut sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) => {
            // Either nothing has run yet, or a stale socket file from a daemon
            // that died without cleaning up (serve() refuses to bind over one).
            let _ = std::fs::remove_file(&path);
            spawn_daemon()?;
            connect_when_ready(&path)?
        }
    };
    sock.write_all(b"toggle")?;
    sock.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    let _ = sock.read_to_string(&mut reply);
    Ok(())
}

/// Relaunch ourselves with no arguments (daemon mode), detached from this
/// short-lived client. Dropping the `Child` does not kill it, so it survives
/// long enough to bind its socket.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the hyprwhispr binary")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/hyprwhispr.log")
        .ok();
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null());
    match log {
        Some(f) => {
            let err = f.try_clone().ok();
            cmd.stdout(std::process::Stdio::from(f));
            cmd.stderr(match err {
                Some(e) => std::process::Stdio::from(e),
                None => std::process::Stdio::null(),
            });
        }
        None => {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
    }
    cmd.spawn().context("failed to start HyprWhispr")?;
    Ok(())
}

/// Connect once the daemon has bound its socket. IMPORTANT: exactly one
/// connection — every accepted connection is read as a "toggle" command, so a
/// throwaway liveness probe would flip the daemon into recording.
fn connect_when_ready(path: &std::path::Path) -> Result<UnixStream> {
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(path) {
            return Ok(s);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("HyprWhispr did not come up ({})", path.display())
}

async fn serve(
    cfg: config::Config,
    ui: async_channel::Sender<State>,
    ui_msg: async_channel::Sender<UiMsg>,
) -> Result<()> {
    let path = config::socket_path();
    if path.exists() {
        // Only clear a socket nobody is serving; otherwise a second instance
        // would silently steal the first one's keybind.
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!(
                "another HyprWhispr is already listening on {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)?;
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let listener = UnixListener::bind(&path)?;
    eprintln!("HyprWhispr ready — listening on {}", path.display());
    config::write_state("idle");

    // SIGTERM/SIGINT handler: unlink both daemon sockets on exit
    {
        let sock = path.clone();
        let local_sock = crate::config::local_socket_path();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                let mut int =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
                tokio::select! {
                    _ = async {
                        if let Some(s) = term.as_mut() { s.recv().await; }
                        else { std::future::pending::<()>().await }
                    } => {},
                    _ = async {
                        if let Some(s) = int.as_mut() { s.recv().await; }
                        else { std::future::pending::<()>().await }
                    } => {},
                }
                let _ = std::fs::remove_file(&sock);
                let _ = std::fs::remove_file(&local_sock);
                // Take the worker with us: it is spawned with kill_on_drop(false)
                // so it would otherwise outlive the daemon, still holding the model.
                local::release_worker("daemon shutting down");
                let _ = std::fs::remove_file(config::state_path());
                std::process::exit(0);
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                let _ = std::fs::remove_file(&sock);
                let _ = std::fs::remove_file(&local_sock);
                local::release_worker("daemon shutting down");
                let _ = std::fs::remove_file(config::state_path());
                std::process::exit(0);
            }
        });
    }

    // NOTE: no worker pre-warm here on purpose. Loading the model at login
    // kept whisper resident 24/7. local::ensure_worker() now spawns it on the
    // first dictation and a watchdog releases it after IDLE_TTL_SECS idle.
    // Cost: ~2-3s cold start on the first dictation after a quiet spell.

    // For local provider we forward interim transcripts to the overlay via ui_msg.
    let mut active: Option<Recording> = None;
    loop {
        let (mut conn, _) = listener.accept().await?;
        {
            use tokio::io::AsyncWriteExt;
            let _ = conn.write_all(b"ok\n").await;
            // Ack and hang up: the client waits for EOF, so it must not be held
            // open across a ~3s cold model load.
            drop(conn);
        }
        match active.take() {
            None => {
                // The STT backend may need to spawn the whisper worker and load
                // the model (~3 s cold). Show that honestly instead of claiming
                // to be listening before it can hear anything.
                config::write_state("loading");
                let _ = ui.send(State::Loading).await;
                // live transcript for both providers — Whispr Flow-style interim in the pill
                let (itx, irx) = async_channel::unbounded::<String>();
                let ui_msg2 = ui_msg.clone();
                tokio::spawn(async move {
                    while let Ok(t) = irx.recv().await {
                        let _ = ui_msg2.send(UiMsg::Transcript(t)).await;
                    }
                });
                let interim_tx = Some(itx);
                match Recording::start(&cfg, interim_tx).await {
                    Ok(r) => {
                        active = Some(r);
                        config::write_state("recording");
                        let _ = ui.send(State::Recording).await;
                    }
                    Err(e) => {
                        eprintln!("start failed: {e:#}");
                        config::write_state("offline");
                        let _ = ui.send(State::Error).await;
                    }
                }
            }
            Some(rec) => {
                config::write_state("transcribing");
                let _ = ui.send(State::Processing).await;
                let cfg = cfg.clone();
                let ui = ui.clone();
                tokio::spawn(async move {
                    let state = match rec.finish(&cfg).await {
                        Ok(text) => {
                            println!("{text}");
                            config::write_state("idle");
                            State::Done
                        }
                        Err(e) => {
                            eprintln!("dictation failed: {e:#}");
                            config::write_state("idle");
                            State::Error
                        }
                    };
                    let _ = ui.send(state).await;
                });
            }
        }
    }
}

enum SessionInner {
    Deepgram(deepgram::Session),
    Local(local::Session),
}

struct Recording {
    capture: audio::Capture,
    pump: tokio::task::JoinHandle<Result<SessionInner>>,
}

impl Recording {
    /// Upper bound on how long the overlay sits in `Loading` before giving up.
    /// `local::ensure_worker` already waits 10 s for the worker socket, so this
    /// only trips on a genuinely wedged worker.
    const STT_READY_TIMEOUT_SECS: u64 = 15;

    async fn start(
        cfg: &config::Config,
        interim_tx: Option<async_channel::Sender<String>>,
    ) -> Result<Self> {
        let capture = audio::start()?;
        let sr = capture.sample_rate;
        let provider = cfg.provider.clone();
        let cfg_clone = cfg.clone();
        let frames = capture.frames.clone();
        // Signalled by the pump once the backend can actually accept audio:
        // for the local provider that means the worker socket is bound and the
        // whisper model is loaded. The mic is already open by then, buffering
        // into audio.rs's 640-chunk channel until the pump starts draining.
        let (ready_tx, ready_rx) = async_channel::bounded::<Result<(), String>>(1);
        let pump = tokio::spawn(async move {
            let inner = if provider == "local" {
                let itx = interim_tx.unwrap_or_else(|| async_channel::unbounded::<String>().0);
                let sess = match local::Session::connect(sr, itx).await {
                    Ok(s) => {
                        let _ = ready_tx.try_send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let _ = ready_tx.try_send(Err(msg.clone()));
                        return Err(anyhow::anyhow!("{msg}"));
                    }
                };
                // Stream while the user is still speaking — that is where
                // the "live" feeling comes from: worker transcribes mid-utterance.
                while let Ok(pcm) = frames.recv().await {
                    sess.send(pcm);
                }
                SessionInner::Local(sess)
            } else {
                let sess = deepgram::Session::connect(&cfg_clone, sr, interim_tx).await?;
                let _ = ready_tx.try_send(Ok(()));
                while let Ok(pcm) = frames.recv().await {
                    sess.send(pcm);
                }
                SessionInner::Deepgram(sess)
            };
            Ok::<SessionInner, anyhow::Error>(inner)
        });
        match tokio::time::timeout(
            std::time::Duration::from_secs(Self::STT_READY_TIMEOUT_SECS),
            ready_rx.recv(),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            // Backend failed: report it here rather than showing a dead pill.
            Ok(Ok(Err(msg))) => anyhow::bail!("speech recognition backend not ready: {msg}"),
            // Pump died or timed out without reporting; finish() surfaces it.
            Ok(Err(_)) | Err(_) => {}
        }
        Ok(Self { capture, pump })
    }

    async fn finish(self, cfg: &config::Config) -> Result<String> {
        self.capture.stop();
        let inner = self.pump.await??;
        let text = match inner {
            SessionInner::Deepgram(s) => s.finish().await?,
            SessionInner::Local(s) => s.finish().await?,
        };
        anyhow::ensure!(!text.trim().is_empty(), "empty transcript");
        let preserve = cfg.preserve_clipboard;
        let out = text.clone();
        tokio::task::spawn_blocking(move || inject::inject(&out, preserve)).await??;
        Ok(text)
    }
}

#[derive(Debug, Clone)]
enum UiMsg {
    State(State),
    Transcript(String),
}
