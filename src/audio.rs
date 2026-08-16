use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::atomic::{AtomicU32, Ordering};

/// Latest mic RMS, 0..1000. Global because only one capture runs at a time and
/// the GTK thread needs to read it without owning the audio thread.
static LEVEL: AtomicU32 = AtomicU32::new(0);

pub fn level() -> f64 {
    LEVEL.load(Ordering::Relaxed) as f64 / 1000.0
}

/// Mic capture running on its own thread (cpal streams are not Send).
pub struct Capture {
    pub frames: async_channel::Receiver<Vec<i16>>,
    pub sample_rate: u32,
    stop: async_channel::Sender<()>,
}

impl Capture {
    /// Stop the stream; the frame channel closes once the thread drops it.
    pub fn stop(&self) {
        let _ = self.stop.try_send(());
    }
}

const PREFERRED_RATE: u32 = 16_000;

pub fn start() -> Result<Capture> {
    let (frames_tx, frames) = async_channel::unbounded::<Vec<i16>>();
    let (stop, stop_rx) = async_channel::bounded::<()>(1);
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<u32>>();

    std::thread::spawn(move || {
        let built = build(frames_tx);
        match built {
            Ok((stream, rate)) => {
                let _ = init_tx.send(Ok(rate));
                let _ = stop_rx.recv_blocking();
                drop(stream);
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
            }
        }
    });

    let sample_rate = init_rx.recv().context("audio thread died")??;
    Ok(Capture {
        frames,
        sample_rate,
        stop,
    })
}

fn build(tx: async_channel::Sender<Vec<i16>>) -> Result<(cpal::Stream, u32)> {
    let device = cpal::default_host()
        .default_input_device()
        .context("no input device")?;
    let default = device.default_input_config()?;
    let channels = default.channels() as usize;

    // Ask the device for 16 kHz directly when it supports it: no resampling,
    // and a third of the bytes on the wire vs 48 kHz.
    let supports_16k = device
        .supported_input_configs()
        .map(|mut it| it.any(|r| r.contains_rate(PREFERRED_RATE)))
        .unwrap_or(false);
    let rate = if supports_16k {
        PREFERRED_RATE
    } else {
        default.sample_rate()
    };
    let mut cfg: cpal::StreamConfig = default.config();
    cfg.sample_rate = rate;
    cfg.buffer_size = cpal::BufferSize::Fixed(rate / 20); // ~50 ms chunks

    let err_fn = |e| eprintln!("audio stream error: {e}");
    let fmt = default.sample_format();
    let stream = match fmt {
        SampleFormat::F32 => device.build_input_stream(
            cfg,
            move |data: &[f32], _: &_| {
                let pcm = downmix(data.iter().map(|s| (s * 32767.0) as i16), channels);
                emit(&tx, pcm);
            },
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            cfg,
            move |data: &[i16], _: &_| {
                let pcm = downmix(data.iter().copied(), channels);
                emit(&tx, pcm);
            },
            err_fn,
            None,
        )?,
        other => anyhow::bail!("unsupported sample format {other}"),
    };
    stream.play()?;
    Ok((stream, rate))
}

fn downmix(samples: impl Iterator<Item = i16>, channels: usize) -> Vec<i16> {
    if channels == 1 {
        return samples.collect();
    }
    let all: Vec<i16> = samples.collect();
    all.chunks(channels)
        .map(|c| (c.iter().map(|&s| s as i32).sum::<i32>() / channels as i32) as i16)
        .collect()
}

fn emit(tx: &async_channel::Sender<Vec<i16>>, pcm: Vec<i16>) {
    if pcm.is_empty() {
        return;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64).powi(2)).sum();
    let rms = (sum / pcm.len() as f64).sqrt() / 32768.0;
    LEVEL.store(((rms * 10.0).min(1.0) * 1000.0) as u32, Ordering::Relaxed);
    let _ = tx.try_send(pcm);
}
