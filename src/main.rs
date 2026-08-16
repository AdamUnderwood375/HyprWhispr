mod audio;
mod config;
mod deepgram;
mod inject;
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

    let (ui_tx, ui_rx) = async_channel::unbounded::<State>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        if let Err(e) = rt.block_on(serve(cfg, ui_tx.clone())) {
            eprintln!("fatal: {e:#}");
            let _ = ui_tx.try_send(State::Error);
            std::process::exit(1);
        }
    });

    let app = gtk::Application::builder()
        .application_id("dev.rwhispr")
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let ov = overlay::Overlay::new(app);
        let hold = app.hold();
        let rx = ui_rx.clone();
        gtk::glib::spawn_future_local(async move {
            let _hold = hold;
            while let Ok(state) = rx.recv().await {
                ov.set_state(state);
                if matches!(state, State::Done | State::Error) {
                    let ms = if state == State::Done { 1200 } else { 3000 };
                    gtk::glib::timeout_future(std::time::Duration::from_millis(ms)).await;
                    if ov.state() == state {
                        ov.set_state(State::Hidden);
                    }
                }
            }
        });
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// Client mode: what the compositor keybind runs.
fn toggle() -> Result<()> {
    let path = config::socket_path();
    let mut sock = UnixStream::connect(&path)
        .with_context(|| format!("rwhispr daemon not running ({})", path.display()))?;
    sock.write_all(b"toggle")?;
    sock.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    let _ = sock.read_to_string(&mut reply);
    Ok(())
}

async fn serve(cfg: config::Config, ui: async_channel::Sender<State>) -> Result<()> {
    let path = config::socket_path();
    if path.exists() {
        // Only clear a socket nobody is serving; otherwise a second instance
        // would silently steal the first one's keybind.
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("another rwhispr is already listening on {}", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let listener = UnixListener::bind(&path)?;
    eprintln!("rwhispr ready — listening on {}", path.display());

    let mut active: Option<Recording> = None;
    loop {
        let (mut conn, _) = listener.accept().await?;
        {
            use tokio::io::AsyncWriteExt;
            let _ = conn.write_all(b"ok\n").await;
        }
        match active.take() {
            None => match Recording::start(&cfg).await {
                Ok(r) => {
                    active = Some(r);
                    let _ = ui.send(State::Recording).await;
                }
                Err(e) => {
                    eprintln!("start failed: {e:#}");
                    let _ = ui.send(State::Error).await;
                }
            },
            Some(rec) => {
                let _ = ui.send(State::Processing).await;
                let cfg = cfg.clone();
                let ui = ui.clone();
                tokio::spawn(async move {
                    let state = match rec.finish(&cfg).await {
                        Ok(text) => {
                            println!("{text}");
                            State::Done
                        }
                        Err(e) => {
                            eprintln!("dictation failed: {e:#}");
                            State::Error
                        }
                    };
                    let _ = ui.send(state).await;
                });
            }
        }
    }
}

struct Recording {
    capture: audio::Capture,
    pump: tokio::task::JoinHandle<deepgram::Session>,
}

impl Recording {
    async fn start(cfg: &config::Config) -> Result<Self> {
        let capture = audio::start()?;
        let session = deepgram::Session::connect(cfg, capture.sample_rate).await?;
        let frames = capture.frames.clone();
        // Stream while the user is still speaking; that is where the latency goes.
        let pump = tokio::spawn(async move {
            while let Ok(pcm) = frames.recv().await {
                session.send(pcm);
            }
            session
        });
        Ok(Self { capture, pump })
    }

    async fn finish(self, cfg: &config::Config) -> Result<String> {
        self.capture.stop();
        let session = self.pump.await?;
        let text = session.finish().await?;
        anyhow::ensure!(!text.trim().is_empty(), "empty transcript");
        let preserve = cfg.preserve_clipboard;
        let out = text.clone();
        tokio::task::spawn_blocking(move || inject::inject(&out, preserve)).await??;
        Ok(text)
    }
}
