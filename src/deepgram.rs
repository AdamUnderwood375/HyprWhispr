use crate::config::Config;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Message, http::HeaderValue};

/// Live transcription session: audio in, finalized transcript out.
///
/// The socket opens the moment recording starts and audio streams while the
/// user is still talking, so by key-release Deepgram usually only owes us the
/// tail. That is the whole speed win over posting a WAV after the fact.
pub struct Session {
    audio_tx: async_channel::Sender<Vec<i16>>,
    done: tokio::task::JoinHandle<Result<String>>,
}

impl Session {
    pub async fn connect(cfg: &Config, sample_rate: u32) -> Result<Self> {
        let url = format!(
            "wss://api.deepgram.com/v1/listen?model={model}&language={lang}\
             &encoding=linear16&sample_rate={rate}&channels=1\
             &smart_format=true&punctuate=true&interim_results=false&endpointing={ep}",
            model = cfg.model,
            lang = cfg.language,
            rate = sample_rate,
            ep = cfg.endpointing,
        );
        let mut req = url.into_client_request()?;
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Token {}", cfg.api_key))?,
        );

        let (ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .context("Deepgram websocket connect failed")?;
        let (mut sink, mut stream) = ws.split();

        let (audio_tx, audio_rx) = async_channel::unbounded::<Vec<i16>>();
        tokio::spawn(async move {
            while let Ok(pcm) = audio_rx.recv().await {
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                if sink.send(Message::Binary(bytes.into())).await.is_err() {
                    return;
                }
            }
            // Channel closed = recording stopped: flush and ask for the tail.
            let _ = sink.send(Message::text(r#"{"type":"CloseStream"}"#)).await;
        });

        let done = tokio::spawn(async move {
            let mut text = String::new();
            while let Some(msg) = stream.next().await {
                let msg = msg?;
                let Message::Text(t) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&t)?;
                match v["type"].as_str() {
                    Some("Results") => {
                        let alt = &v["channel"]["alternatives"][0]["transcript"];
                        if let Some(s) = alt.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                            if !text.is_empty() {
                                text.push(' ');
                            }
                            text.push_str(s);
                        }
                    }
                    Some("Error") => anyhow::bail!("Deepgram error: {t}"),
                    _ => {}
                }
            }
            Ok(text)
        });

        Ok(Self { audio_tx, done })
    }

    pub fn send(&self, pcm: Vec<i16>) {
        let _ = self.audio_tx.try_send(pcm);
    }

    /// Close the audio stream and await the final transcript.
    pub async fn finish(self) -> Result<String> {
        self.audio_tx.close();
        self.done.await?
    }
}
