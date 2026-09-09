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
        .application_id("dev.speakspic")
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
fn toggle() -> Result<()> {
    let path = config::socket_path();
    let mut sock = UnixStream::connect(&path)
        .with_context(|| format!("speakspic daemon not running ({})", path.display()))?;
    sock.write_all(b"toggle")?;
    sock.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    let _ = sock.read_to_string(&mut reply);
    Ok(())
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
                "another speakspic is already listening on {}",
                path.display()
            );
        }
        std::fs::remove_file(&path)?;
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let listener = UnixListener::bind(&path)?;
    eprintln!("speakspic ready — listening on {}", path.display());

    // Pre-warm local worker so first dictation has 0 cold-start latency
    if cfg.provider == "local" {
        tokio::spawn(async {
            if let Err(e) = local::ensure_worker().await {
                eprintln!("pre-warm local worker failed: {e:#}");
            } else {
                eprintln!("local whisper worker ready");
            }
        });
    }

    // For local provider we forward interim transcripts to the overlay via ui_msg.
    let mut active: Option<Recording> = None;
    loop {
        let (mut conn, _) = listener.accept().await?;
        {
            use tokio::io::AsyncWriteExt;
            let _ = conn.write_all(b"ok\n").await;
        }
        match active.take() {
            None => {
                // live transcript for both providers — WhisperFlow-style interim in the pill
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
                        let _ = ui.send(State::Recording).await;
                    }
                    Err(e) => {
                        eprintln!("start failed: {e:#}");
                        let _ = ui.send(State::Error).await;
                    }
                }
            }
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

enum SessionInner {
    Deepgram(deepgram::Session),
    Local(local::Session),
}

struct Recording {
    capture: audio::Capture,
    pump: tokio::task::JoinHandle<Result<SessionInner>>,
}

impl Recording {
    async fn start(
        cfg: &config::Config,
        interim_tx: Option<async_channel::Sender<String>>,
    ) -> Result<Self> {
        let capture = audio::start()?;
        let sr = capture.sample_rate;
        let provider = cfg.provider.clone();
        let cfg_clone = cfg.clone();
        let frames = capture.frames.clone();
        let pump = tokio::spawn(async move {
            let inner = if provider == "local" {
                let itx = interim_tx.unwrap_or_else(|| async_channel::unbounded::<String>().0);
                let sess = local::Session::connect(sr, itx).await?;
                // Stream while the user is still speaking — that is where
                // the "live" feeling comes from: worker transcribes mid-utterance.
                while let Ok(pcm) = frames.recv().await {
                    sess.send(pcm);
                }
                SessionInner::Local(sess)
            } else {
                let sess = deepgram::Session::connect(&cfg_clone, sr, interim_tx).await?;
                while let Ok(pcm) = frames.recv().await {
                    sess.send(pcm);
                }
                SessionInner::Deepgram(sess)
            };
            Ok::<SessionInner, anyhow::Error>(inner)
        });
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
