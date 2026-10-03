//! Fallible rodio output for the integrated Spotify player.
//!
//! librespot's rodio backend unwraps device and stream errors, so a missing
//! or busy output device panics on the player thread. This mirrors its
//! device/config selection but returns those failures as `SinkError`s.

use std::{
    thread,
    time::{Duration, Instant},
};

use librespot_playback::{
    audio_backend::{Sink, SinkError, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
    NUM_CHANNELS, SAMPLE_RATE,
};
use rodio::cpal::{
    self,
    traits::{DeviceTrait, HostTrait},
};

/// Packets queued in rodio before `write` waits for playback to drain them;
/// about half a second at librespot's typical packet size.
const MAX_QUEUED_PACKETS: usize = 26;
/// How long the device may go without consuming queued audio before it is
/// treated as stalled. Normal playback drains the whole queue well within it.
const STALL_DEADLINE: Duration = Duration::from_secs(2);
const DRAIN_POLL: Duration = Duration::from_millis(10);

pub(super) struct RodioSink {
    sink: rodio::Sink,
    stall_deadline: Duration,
    _stream: Option<rodio::OutputStream>,
}

/// Open the default output device.
pub(super) fn open() -> SinkResult<Box<dyn Sink>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| SinkError::ConnectionRefused("no default audio output device".into()))?;
    let stream = open_stream(device)?;
    let sink = rodio::Sink::connect_new(stream.mixer());
    Ok(Box::new(RodioSink {
        sink,
        stall_deadline: STALL_DEADLINE,
        _stream: Some(stream),
    }))
}

fn open_stream(device: cpal::Device) -> SinkResult<rodio::OutputStream> {
    fn unavailable<E>(_: E) -> SinkError {
        SinkError::ConnectionRefused("audio output device is unavailable".into())
    }

    // Prefer native stereo at librespot's sample rate, then the device's
    // default rate (rodio resamples), then whatever the device defaults to.
    let default_config = device.default_output_config().map_err(unavailable)?;
    let config = device
        .supported_output_configs()
        .map_err(unavailable)?
        .find(|config| config.channels() == cpal::ChannelCount::from(NUM_CHANNELS))
        .and_then(|config| {
            config
                .try_with_sample_rate(cpal::SampleRate(SAMPLE_RATE))
                .or_else(|| config.try_with_sample_rate(default_config.sample_rate()))
        })
        .unwrap_or(default_config);

    let mut stream = match rodio::OutputStreamBuilder::default()
        .with_device(device.clone())
        .with_config(&config.config())
        .with_sample_format(cpal::SampleFormat::I16)
        .open_stream()
    {
        Ok(stream) => stream,
        Err(_) => rodio::OutputStreamBuilder::from_device(device)
            .and_then(|builder| builder.open_stream_or_fallback())
            .map_err(|_| {
                SinkError::ConnectionRefused("audio output stream could not be opened".into())
            })?,
    };
    stream.log_on_drop(false);
    Ok(stream)
}

impl Sink for RodioSink {
    fn start(&mut self) -> SinkResult<()> {
        self.sink.play();
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        // librespot exits the process when `stop` fails, so a stalled output
        // abandons its queued audio instead of failing or blocking shutdown.
        // `rodio::Sink::clear` waits for the output, so only the stop flag is
        // set; `ReleasingSink` drops this output right after `stop`.
        if !wait_until(self.stall_deadline, || self.sink.empty()) {
            tracing::warn!("Spotify audio output stopped consuming samples; queued audio dropped");
            self.sink.stop();
        }
        self.sink.pause();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|_| SinkError::OnWrite("audio packet has no samples".into()))?;
        let samples: &[f32] = &converter.f64_to_f32(samples);
        self.sink.append(rodio::buffer::SamplesBuffer::new(
            cpal::ChannelCount::from(NUM_CHANNELS),
            SAMPLE_RATE,
            samples,
        ));
        // A write error makes librespot pause, which stops and releases this
        // output through the bounded drain above.
        if wait_until(self.stall_deadline, || {
            self.sink.len() <= MAX_QUEUED_PACKETS
        }) {
            Ok(())
        } else {
            Err(SinkError::OnWrite(
                "audio output stopped consuming samples".into(),
            ))
        }
    }
}

/// Poll `done` until it holds or `deadline` passes; returns whether it held.
fn wait_until(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if done() {
            return true;
        }
        if started.elapsed() >= deadline {
            return false;
        }
        thread::sleep(DRAIN_POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use super::*;

    const DEADLINE: Duration = Duration::from_millis(100);

    /// A sink whose output is never consumed, like a device that stopped
    /// pulling samples. The returned queue output must outlive the test.
    fn stalled_sink() -> (RodioSink, rodio::queue::SourcesQueueOutput) {
        let (sink, output) = rodio::Sink::new();
        let sink = RodioSink {
            sink,
            stall_deadline: DEADLINE,
            _stream: None,
        };
        (sink, output)
    }

    fn packet() -> AudioPacket {
        AudioPacket::Samples(vec![0.0; 2048])
    }

    #[test]
    fn stop_on_a_stalled_output_returns_after_the_deadline() {
        let (mut sink, _output) = stalled_sink();
        sink.sink.append(rodio::source::Zero::new(2, SAMPLE_RATE));

        let started = Instant::now();
        sink.stop().unwrap();

        assert!(started.elapsed() >= DEADLINE);
        assert!(started.elapsed() < DEADLINE * 10);
        assert!(sink.sink.is_paused());
    }

    #[test]
    fn write_reports_a_stalled_output_instead_of_blocking() {
        let (mut sink, _output) = stalled_sink();
        let mut converter = Converter::new(None);
        for _ in 0..MAX_QUEUED_PACKETS {
            sink.write(packet(), &mut converter).unwrap();
        }

        let started = Instant::now();
        let result = sink.write(packet(), &mut converter);

        assert!(matches!(result, Err(SinkError::OnWrite(_))));
        assert!(started.elapsed() < DEADLINE * 10);
        let started = Instant::now();
        sink.stop().unwrap();
        assert!(started.elapsed() < DEADLINE * 10);
    }

    #[test]
    fn a_consuming_output_drains_on_stop_and_resumes() {
        let (mut sink, mut output) = stalled_sink();
        let running = Arc::new(AtomicBool::new(true));
        let consumer = thread::spawn({
            let running = running.clone();
            move || {
                while running.load(Ordering::Relaxed) {
                    let _ = output.next();
                }
            }
        });
        let mut converter = Converter::new(None);

        sink.start().unwrap();
        sink.write(packet(), &mut converter).unwrap();
        sink.stop().unwrap();
        assert!(sink.sink.empty() && sink.sink.is_paused());
        sink.start().unwrap();
        sink.write(packet(), &mut converter).unwrap();
        assert!(!sink.sink.is_paused());

        running.store(false, Ordering::Relaxed);
        consumer.join().unwrap();
    }
}
