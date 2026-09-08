// Self-check: mic produces non-silent audio and Deepgram returns a transcript.
// Run: cargo run --example probe   (speak for ~3 s)
#[path = "../src/audio.rs"] mod audio;
#[path = "../src/config.rs"] mod config;
#[path = "../src/deepgram.rs"] mod deepgram;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider().install_default().ok();
    let cfg = config::load()?;
    let cap = audio::start()?;
    println!("sample_rate={}", cap.sample_rate);
    let session = deepgram::Session::connect(&cfg, cap.sample_rate, None).await?;
    let frames = cap.frames.clone();
    let pump = tokio::spawn(async move {
        let mut peak = 0i16;
        let mut n = 0usize;
        while let Ok(pcm) = frames.recv().await {
            peak = peak.max(pcm.iter().copied().map(i16::saturating_abs).max().unwrap_or(0));
            n += pcm.len();
            session.send(pcm);
        }
        (session, peak, n)
    });
    println!("recording 4 s — speak now");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    cap.stop();
    let (session, peak, n) = pump.await?;
    let t0 = std::time::Instant::now();
    let text = session.finish().await?;
    println!("samples={n} peak={peak} tail_latency={:?}", t0.elapsed());
    assert!(n > 0, "no audio captured");
    println!("transcript: {text:?}");
    Ok(())
}
