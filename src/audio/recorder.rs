use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;

use super::processing;

pub const WAVEFORM_BINS: usize = 64;

const RECORDINGS_DIRECTORY_NAME: &str = "BigRecorder";
const STOP_EOS_TIMEOUT: Duration = Duration::from_secs(5);
const DROP_EOS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub struct AudioFrame {
    pub level: f64,
    pub peaks: [f64; WAVEFORM_BINS],
}

#[derive(Default)]
pub struct Recorder {
    pipeline: Option<gst::Pipeline>,
    record_valve: Option<gst::Element>,
    sample_sink: Option<gst_app::AppSink>,
    output_path: Option<PathBuf>,
    device: Option<gst::Device>,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the input device used for the next recording. `None` falls back
    /// to the system default microphone.
    pub fn set_device(&mut self, device: Option<gst::Device>) {
        self.device = device;
    }

    pub fn start(&mut self) -> Result<PathBuf> {
        if self.pipeline.is_some() {
            return Err(anyhow!("A recording is already active"));
        }

        let output_path = next_recording_path()?;
        let recording_pipeline = build_recording_pipeline(&output_path, self.device.as_ref())?;

        if let Err(err) = recording_pipeline.pipeline.set_state(gst::State::Playing) {
            // The filesink may already have created the file; don't leave it behind.
            let _ = recording_pipeline.pipeline.set_state(gst::State::Null);
            let _ = fs::remove_file(&output_path);
            return Err(anyhow!("Failed to start recording pipeline: {err:?}"));
        }

        self.record_valve = Some(recording_pipeline.record_valve);
        self.sample_sink = Some(recording_pipeline.sample_sink);
        self.pipeline = Some(recording_pipeline.pipeline);
        self.output_path = Some(output_path.clone());

        Ok(output_path)
    }

    pub fn pause(&self) -> Result<()> {
        let valve = self
            .record_valve
            .as_ref()
            .ok_or_else(|| anyhow!("No active recording to pause"))?;

        valve.set_property("drop", true);

        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        let valve = self
            .record_valve
            .as_ref()
            .ok_or_else(|| anyhow!("No paused recording to resume"))?;

        valve.set_property("drop", false);

        Ok(())
    }

    pub fn stop(&mut self) -> Result<Option<PathBuf>> {
        self.finish(STOP_EOS_TIMEOUT)
    }

    /// Drains the pipeline with EOS so wavenc can finalize the header. When
    /// that fails, the audio already on disk is kept and its header repaired.
    fn finish(&mut self, eos_timeout: Duration) -> Result<Option<PathBuf>> {
        let Some(pipeline) = self.pipeline.take() else {
            return Ok(None);
        };

        if let Some(valve) = self.record_valve.take() {
            valve.set_property("drop", false);
        }
        self.sample_sink.take();
        let output_path = self.output_path.take();

        let _ = pipeline.send_event(gst::event::Eos::new());

        let eos_result = wait_for_eos(&pipeline, eos_timeout);

        let stop_result = pipeline
            .set_state(gst::State::Null)
            .map_err(|err| anyhow!("Failed to stop recording pipeline: {err:?}"));

        if let Err(err) = eos_result {
            let Some(path) = output_path
                .filter(|path| fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0))
            else {
                return Err(err);
            };

            eprintln!(
                "Recording was not finalized cleanly, keeping {}: {err}",
                path.display()
            );
            if let Err(err) = processing::repair_wav_header(&path) {
                eprintln!("Failed to repair recording header: {err}");
            }
            return Ok(Some(path));
        }
        stop_result?;

        Ok(output_path)
    }

    pub fn poll_frame(&mut self) -> Result<Option<AudioFrame>> {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return Ok(None);
        };

        if let Some(bus) = pipeline.bus() {
            while let Some(message) =
                bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
            {
                if let gst::MessageView::Error(error) = message.view() {
                    return Err(anyhow!(
                        "Recording pipeline error: {} ({})",
                        error.error(),
                        error
                            .debug()
                            .map(|debug| debug.to_string())
                            .unwrap_or_else(|| "no debug details".to_string())
                    ));
                }
            }
        }

        let Some(sample_sink) = self.sample_sink.as_ref() else {
            return Ok(None);
        };

        let mut latest_frame = None;
        while let Some(sample) = sample_sink.try_pull_sample(gst::ClockTime::ZERO) {
            latest_frame = Some(audio_frame_from_sample(&sample)?);
        }

        Ok(latest_frame)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(err) = self.finish(DROP_EOS_TIMEOUT) {
            eprintln!("Failed to finalize recording: {err}");
        }
    }
}

struct RecordingPipeline {
    pipeline: gst::Pipeline,
    record_valve: gst::Element,
    sample_sink: gst_app::AppSink,
}

fn build_recording_pipeline(
    output_path: &Path,
    device: Option<&gst::Device>,
) -> Result<RecordingPipeline> {
    let pipeline = gst::Pipeline::new();
    let source = match device {
        Some(device) => source_from_device(device)?,
        None => make_audio_source()?,
    };
    let convert = make_element("audioconvert")?;
    let resample = make_element("audioresample")?;
    // 16-bit PCM is plenty for voice and halves the size of the recording.
    let record_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("layout", "interleaved")
        .field("channels", 1_i32)
        .field("rate", 48_000_i32)
        .build();
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property("caps", &record_caps)
        .build()
        .context("Missing GStreamer element: capsfilter")?;
    let tee = make_element("tee")?;
    // The level meter only wants the freshest audio, so this branch may drop.
    let analysis_queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 4_u32)
        .property("max-size-bytes", 0_u32)
        .property("max-size-time", 0_u64)
        .build()
        .context("Missing GStreamer element: queue")?;
    let analysis_convert = make_element("audioconvert")?;
    let analysis_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("channels", 1_i32)
        .field("rate", 48_000_i32)
        .build();
    let sample_sink = gst_app::AppSink::builder()
        .caps(&analysis_caps)
        .sync(false)
        .drop(true)
        .max_buffers(2)
        .wait_on_eos(false)
        .build();
    // Never leaky: every buffer here ends up in the file.
    let record_queue = make_element("queue")?;
    let record_valve = gst::ElementFactory::make("valve")
        .property("drop", false)
        .build()
        .context("Missing GStreamer element: valve")?;
    let encoder = make_element("wavenc")?;
    let output_text = output_path.to_string_lossy();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", output_text.as_ref())
        .build()
        .context("Missing GStreamer element: filesink")?;

    pipeline
        .add_many([
            &source,
            &convert,
            &resample,
            &caps_filter,
            &tee,
            &analysis_queue,
            &analysis_convert,
            &record_queue,
            &record_valve,
            &encoder,
            &sink,
        ])
        .context("Failed to add recording elements to pipeline")?;
    pipeline
        .add(sample_sink.upcast_ref::<gst::Element>())
        .context("Failed to add audio monitor to pipeline")?;

    gst::Element::link_many([&source, &convert, &resample, &caps_filter, &tee])
        .context("Failed to link audio input pipeline")?;
    gst::Element::link_many([
        &tee,
        &analysis_queue,
        &analysis_convert,
        sample_sink.upcast_ref::<gst::Element>(),
    ])
    .context("Failed to link audio monitor pipeline")?;
    gst::Element::link_many([&tee, &record_queue, &record_valve, &encoder, &sink])
        .context("Failed to link recording output pipeline")?;

    Ok(RecordingPipeline {
        pipeline,
        record_valve,
        sample_sink,
    })
}

/// Lists the available audio input devices as `(display name, device)` pairs.
pub fn input_devices() -> Vec<(String, gst::Device)> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    if monitor.start().is_err() {
        return Vec::new();
    }
    let devices = monitor.devices();
    monitor.stop();

    devices
        .into_iter()
        .map(|device| (device.display_name().to_string(), device))
        .collect()
}

pub fn default_input_is_muted() -> Option<bool> {
    let output = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SOURCE@"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    default_input_mute_from_wpctl(&String::from_utf8_lossy(&output.stdout))
}

fn default_input_mute_from_wpctl(output: &str) -> Option<bool> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("Volume:"))?;
    let muted = line.to_ascii_uppercase().contains("MUTED");
    let zero_volume = line
        .split_whitespace()
        .nth(1)
        .and_then(|volume| volume.parse::<f64>().ok())
        .is_some_and(|volume| volume <= 0.001);

    Some(muted || zero_volume)
}

fn source_from_device(device: &gst::Device) -> Result<gst::Element> {
    let source = device
        .create_element(None)
        .context("Failed to create a source from the selected microphone")?;
    if source.find_property("do-timestamp").is_some() {
        source.set_property("do-timestamp", true);
    }
    Ok(source)
}

fn make_audio_source() -> Result<gst::Element> {
    if let Ok(source) = gst::ElementFactory::make("pulsesrc")
        .property("client-name", "Big Recorder")
        .property("do-timestamp", true)
        .build()
    {
        return Ok(source);
    }

    if let Ok(source) = gst::ElementFactory::make("pipewiresrc")
        .property("client-name", "Big Recorder")
        .property("do-timestamp", true)
        .build()
    {
        return Ok(source);
    }

    make_element("autoaudiosrc")
}

fn make_element(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .with_context(|| format!("Missing GStreamer element: {factory}"))
}

fn wait_for_eos(pipeline: &gst::Pipeline, timeout: Duration) -> Result<()> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("Recording pipeline has no message bus"))?;
    let deadline = Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("Timed out while finalizing recording"));
        }

        let timeout = remaining.min(Duration::from_millis(100));
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(timeout.as_nanos() as u64),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            match message.view() {
                gst::MessageView::Eos(_) => return Ok(()),
                gst::MessageView::Error(error) => {
                    return Err(anyhow!(
                        "Recording pipeline error: {} ({})",
                        error.error(),
                        error
                            .debug()
                            .map(|debug| debug.to_string())
                            .unwrap_or_else(|| "no debug details".to_string())
                    ));
                }
                _ => {}
            }
        }
    }
}

fn next_recording_path() -> Result<PathBuf> {
    let directory = recordings_directory();
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "Failed to create recording directory: {}",
            directory.display()
        )
    })?;

    let timestamp = glib::DateTime::now_local()
        .context("Failed to read local time")?
        .format("%Y-%m-%d %H-%M-%S")
        .context("Failed to format local time")?;

    unique_recording_path(&directory, &format!("Recording {timestamp}"))
}

/// Returns `<stem>.wav`, or `<stem> (2).wav`, `<stem> (3).wav`... when a
/// recording started within the same second already took the name.
fn unique_recording_path(directory: &Path, stem: &str) -> Result<PathBuf> {
    for index in 1..1000 {
        let candidate = if index == 1 {
            directory.join(format!("{stem}.wav"))
        } else {
            directory.join(format!("{stem} ({index}).wav"))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!("Could not create a unique recording file"))
}

pub fn recordings_directory() -> PathBuf {
    let mut directory =
        glib::user_special_dir(glib::UserDirectory::Music).unwrap_or_else(|| PathBuf::from("."));
    directory.push(RECORDINGS_DIRECTORY_NAME);
    directory
}

pub fn saved_recordings() -> Result<Vec<PathBuf>> {
    let directory = recordings_directory();
    if !directory.exists() {
        return Ok(Vec::new());
    }

    let mut recordings = Vec::new();
    for entry in fs::read_dir(&directory).with_context(|| {
        format!(
            "Failed to read recording directory: {}",
            directory.display()
        )
    })? {
        let path = entry?.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        {
            recordings.push(path);
        }
    }

    recordings.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    Ok(recordings)
}

fn audio_frame_from_sample(sample: &gst::Sample) -> Result<AudioFrame> {
    let buffer = sample
        .buffer()
        .context("Audio monitor sample did not contain a buffer")?;
    let map = buffer
        .map_readable()
        .context("Failed to read audio monitor sample")?;

    Ok(audio_frame_from_f32le(map.as_slice()))
}

fn audio_frame_from_f32le(bytes: &[u8]) -> AudioFrame {
    let sample_count = bytes.len() / 4;
    if sample_count == 0 {
        return AudioFrame {
            level: 0.0,
            peaks: [0.0; WAVEFORM_BINS],
        };
    }

    let mut peaks = [0.0_f64; WAVEFORM_BINS];
    let mut sum_squares = 0.0;
    let mut valid_samples = 0_usize;

    for (sample_index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let sample = f32::from_le_bytes(*chunk) as f64;
        if !sample.is_finite() {
            continue;
        }

        let amplitude = sample.abs().clamp(0.0, 1.0);
        let bin = sample_index * WAVEFORM_BINS / sample_count;
        peaks[bin] = peaks[bin].max(display_amplitude(amplitude));
        sum_squares += amplitude * amplitude;
        valid_samples += 1;
    }

    let level = if valid_samples == 0 {
        0.0
    } else {
        display_amplitude((sum_squares / valid_samples as f64).sqrt())
    };

    AudioFrame { level, peaks }
}

fn display_amplitude(amplitude: f64) -> f64 {
    const NOISE_FLOOR: f64 = 0.0015;
    const DISPLAY_GAIN: f64 = 18.0;

    ((amplitude - NOISE_FLOOR).max(0.0) * DISPLAY_GAIN)
        .clamp(0.0, 1.0)
        .powf(0.72)
}

#[cfg(test)]
mod tests {
    use super::{default_input_mute_from_wpctl, unique_recording_path};
    use std::fs;

    #[test]
    fn recording_names_never_collide() {
        let directory =
            std::env::temp_dir().join(format!("bigrecorder-names-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create test dir");

        let first = unique_recording_path(&directory, "Recording X").expect("first name");
        assert_eq!(first, directory.join("Recording X.wav"));
        fs::write(&first, b"").expect("create first");
        let second = unique_recording_path(&directory, "Recording X").expect("second name");
        assert_eq!(second, directory.join("Recording X (2).wav"));
        fs::write(&second, b"").expect("create second");
        let third = unique_recording_path(&directory, "Recording X").expect("third name");
        assert_eq!(third, directory.join("Recording X (3).wav"));

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn parses_wpctl_default_input_mute_state() {
        assert_eq!(default_input_mute_from_wpctl("Volume: 0.60\n"), Some(false));
        assert_eq!(default_input_mute_from_wpctl("Volume: 0.00\n"), Some(true));
        assert_eq!(
            default_input_mute_from_wpctl("Volume: 0.60 [MUTED]\n"),
            Some(true)
        );
        assert_eq!(default_input_mute_from_wpctl("unrelated\n"), None);
    }
}
