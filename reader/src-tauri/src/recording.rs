use base64::{engine::general_purpose::STANDARD, Engine as _};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::State;

use crate::commands::AssetData;
use crate::AppState;

pub struct RecordingSession {
    stop_tx: mpsc::Sender<()>,
    result_rx: mpsc::Receiver<Result<(Vec<i16>, u32), String>>,
}

fn push_mono(buffer: &Arc<Mutex<Vec<i16>>>, samples: impl Iterator<Item = i16>) {
    if let Ok(mut buf) = buffer.lock() {
        buf.extend(samples);
    }
}

// cpal's Stream is deliberately !Send (its internal platform handles can't
// safely cross threads), so it can never live in Tauri's shared AppState
// directly. Instead a dedicated thread owns the whole cpal lifecycle — opens
// the device, plays the stream, blocks until told to stop, then drops the
// stream and hands back the captured samples — and only plain-Send channels
// cross into AppState.
fn record_on_own_thread(
    ready_tx: mpsc::Sender<Result<(), String>>,
    stop_rx: mpsc::Receiver<()>,
    result_tx: mpsc::Sender<Result<(Vec<i16>, u32), String>>,
) {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            let _ = ready_tx.send(Err("No microphone found".to_string()));
            return;
        }
    };
    let supported = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };
    let sample_format = supported.sample_format();
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.into();

    let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
    let err_fn = |err| eprintln!("weland: microphone stream error: {err}");

    let stream_result = match sample_format {
        cpal::SampleFormat::F32 => {
            let samples = samples.clone();
            device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    push_mono(
                        &samples,
                        data.chunks(channels).map(|frame| {
                            let avg = frame.iter().sum::<f32>() / channels as f32;
                            (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                        }),
                    );
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let samples = samples.clone();
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    push_mono(
                        &samples,
                        data.chunks(channels)
                            .map(|frame| (frame.iter().map(|&s| s as i32).sum::<i32>() / channels as i32) as i16),
                    );
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let samples = samples.clone();
            device.build_input_stream(
                &config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    push_mono(
                        &samples,
                        data.chunks(channels)
                            .map(|frame| (frame.iter().map(|&s| i32::from(s) - 32768).sum::<i32>() / channels as i32) as i16),
                    );
                },
                err_fn,
                None,
            )
        }
        other => {
            let _ = ready_tx.send(Err(format!("Unsupported microphone sample format: {other:?}")));
            return;
        }
    };

    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("Failed to open microphone stream: {e}")));
            return;
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("Failed to start microphone stream: {e}")));
        return;
    }

    if ready_tx.send(Ok(())).is_err() {
        // The caller already gave up waiting; nothing left to report to.
        return;
    }

    // Block until stop_voice_recording signals us, then stop capture before
    // reading out whatever was collected.
    let _ = stop_rx.recv();
    drop(stream);

    let collected = samples.lock().map(|s| s.clone()).unwrap_or_default();
    let _ = result_tx.send(Ok((collected, sample_rate)));
}

#[tauri::command]
pub fn start_voice_recording(state: State<AppState>) -> Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();

    thread::spawn(move || record_on_own_thread(ready_tx, stop_rx, result_tx));

    let ready = ready_rx.recv().map_err(|_| "Microphone thread failed to start".to_string())?;
    ready?;

    let mut guard = state.recording.lock().map_err(|e| e.to_string())?;
    *guard = Some(RecordingSession { stop_tx, result_rx });
    Ok(())
}

#[tauri::command]
pub fn stop_voice_recording(state: State<AppState>) -> Result<AssetData, String> {
    let session = state.recording.lock().map_err(|e| e.to_string())?.take().ok_or("Not currently recording")?;

    let _ = session.stop_tx.send(());
    let outcome = session.result_rx.recv().map_err(|_| "Microphone thread ended unexpectedly".to_string())?;
    let (samples, sample_rate) = outcome?;

    if samples.is_empty() {
        return Err("The recording came out empty — nothing was captured.".to_string());
    }

    // Opus only encodes at 8/12/16/24/48 kHz; resample whatever rate the mic
    // captured at up to 48 kHz (the highest-quality option, and a no-op for
    // the very common case where the device is already 48 kHz native).
    let resampled = resample_linear(&samples, sample_rate, 48000);
    let ogg = ogg_opus::encode::<48000, 1>(&resampled).map_err(|e| format!("Failed to encode voice note: {e}"))?;
    Ok(AssetData { mime_type: "audio/ogg; codecs=opus".to_string(), data_base64: STANDARD.encode(ogg) })
}

// Simple linear-interpolation resampler — voice notes don't need a
// high-order windowed-sinc resampler, and this keeps the dependency list
// small. Most mics already report 44.1kHz or 48kHz natively, so this is
// frequently a no-op or a mild upsample.
fn resample_linear(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = f64::from(to_rate) / f64::from(from_rate);
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let last = *input.last().unwrap();
    (0..out_len)
        .map(|i| {
            let src_pos = i as f64 / ratio;
            let idx = src_pos.floor() as usize;
            let frac = src_pos - idx as f64;
            let s0 = f64::from(*input.get(idx).unwrap_or(&last));
            let s1 = f64::from(*input.get(idx + 1).unwrap_or(&last));
            (s0 + (s1 - s0) * frac) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn resample_linear_is_noop_at_same_rate() {
        let input = vec![1, -2, 3, -4, 5];
        assert_eq!(resample_linear(&input, 48000, 48000), input);
    }

    #[test]
    fn resample_linear_handles_empty_input() {
        assert_eq!(resample_linear(&[], 44100, 48000), Vec::<i16>::new());
    }

    #[test]
    fn resample_linear_upsamples_to_expected_length() {
        let input: Vec<i16> = (0..1000).map(|i| (i % 100) as i16).collect();
        let out = resample_linear(&input, 24000, 48000);
        // Exactly doubling the rate should exactly double the sample count.
        assert_eq!(out.len(), input.len() * 2);
    }

    #[test]
    fn resample_linear_downsamples_to_expected_length() {
        let input: Vec<i16> = (0..1000).map(|i| (i % 100) as i16).collect();
        let out = resample_linear(&input, 48000, 24000);
        assert_eq!(out.len(), input.len() / 2);
    }

    #[test]
    fn resample_linear_interpolates_smoothly() {
        // A straight ramp resampled should stay smooth — no interpolated
        // sample should jump by more than one step between neighbors.
        let input: Vec<i16> = (0..100).collect();
        let out = resample_linear(&input, 48000, 96000);
        assert_eq!(out.len(), 200);
        for w in out.windows(2) {
            assert!((w[1] - w[0]).abs() <= 1, "unexpected jump between adjacent resampled values: {w:?}");
        }
    }

    #[test]
    fn encode_pipeline_produces_valid_decodable_ogg_opus() {
        // Synthesize ~0.5s of a 440Hz tone at 44100Hz mono — a realistic
        // stand-in for a real mic capture at a common native rate that
        // isn't already 48kHz, so this also exercises resampling.
        let sample_rate = 44100u32;
        let n = (f64::from(sample_rate) * 0.5) as usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(sample_rate);
                ((t * 440.0 * std::f64::consts::TAU).sin() * f64::from(i16::MAX) * 0.5) as i16
            })
            .collect();

        let resampled = resample_linear(&samples, sample_rate, 48000);
        let ogg = ogg_opus::encode::<48000, 1>(&resampled).expect("encode should succeed");

        assert!(ogg.starts_with(b"OggS"), "output should start with the Ogg capture pattern");
        assert!(
            ogg.len() < samples.len() * 2,
            "opus output ({} bytes) should be much smaller than raw 16-bit PCM ({} bytes)",
            ogg.len(),
            samples.len() * 2
        );

        let (decoded, _) = ogg_opus::decode::<_, 48000>(Cursor::new(ogg)).expect("round-trip decode should succeed");
        assert!(!decoded.is_empty());
        // Roughly the same duration back out, allowing for Opus's pre-skip
        // priming/trailing padding rather than an exact sample count match.
        let expected_len = resampled.len();
        assert!(
            decoded.len() > expected_len / 2 && decoded.len() < expected_len * 2,
            "decoded length {} should be roughly comparable to encoded input length {expected_len}",
            decoded.len(),
        );
    }
}
