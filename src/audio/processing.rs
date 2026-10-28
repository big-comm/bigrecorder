use std::ffi::OsString;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;

#[derive(Clone, Debug)]
pub struct AudioBuffer {
    sample_rate: u32,
    channels: u16,
    samples: Vec<f32>,
}

impl AudioBuffer {
    fn frame_count(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.samples.len() / usize::from(self.channels)
    }
}

#[derive(Clone, Copy)]
enum AudioFormat {
    Pcm,
    Float,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportFormat {
    Wav,
    Mp3,
    OggOpus,
    Flac,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportQuality {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportChannelMode {
    Original,
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportSampleRate {
    Original,
    Hz(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportOptions {
    pub format: ExportFormat,
    pub quality: ExportQuality,
    pub channel_mode: ExportChannelMode,
    pub sample_rate: ExportSampleRate,
}

pub const EQUALIZER_BANDS: [f32; 10] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceEffectPreset {
    Child,
    Deep,
    Robot,
    Radio,
    Echo,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::OggOpus => "opus",
            Self::Flac => "flac",
        }
    }
}

impl VoiceEffectPreset {
    fn output_suffix(self) -> &'static str {
        match self {
            Self::Child => "Child Voice",
            Self::Deep => "Deep Voice",
            Self::Robot => "Robot Voice",
            Self::Radio => "Radio Voice",
            Self::Echo => "Echo Voice",
        }
    }

    fn filters(self, amount: f32) -> Result<Vec<gst::Element>> {
        let amount = amount.clamp(0.0, 1.0);
        match self {
            Self::Child => Ok(vec![
                pitch_filter(2.0_f32.powf((4.0 + 5.0 * amount) / 12.0))?,
                equalizer_filter([-4.0, -3.0, -2.0, 0.0, 1.0, 2.5, 3.5, 4.0, 2.0, 0.0])?,
            ]),
            Self::Deep => Ok(vec![
                pitch_filter(2.0_f32.powf((-4.0 - 4.0 * amount) / 12.0))?,
                equalizer_filter([4.0, 3.5, 2.0, 0.5, -1.0, -1.5, -2.0, -2.5, -3.0, -3.0])?,
            ]),
            Self::Robot => Ok(vec![
                pitch_filter(0.98)?,
                equalizer_filter([-8.0, -7.0, -5.0, -2.0, 2.0, 4.0, 5.0, 3.0, -2.0, -6.0])?,
                echo_filter(22_000_000, 0.18 + amount * 0.22, 0.25 + amount * 0.35)?,
            ]),
            Self::Radio => Ok(vec![
                chebyshev_filter("high-pass", 260.0 + amount * 120.0)?,
                chebyshev_filter("low-pass", 3_800.0 - amount * 700.0)?,
                equalizer_filter([-12.0, -12.0, -9.0, -4.0, 1.0, 3.0, 3.0, 1.0, -6.0, -12.0])?,
            ]),
            Self::Echo => Ok(vec![echo_filter(
                90_000_000 + (amount * 180_000_000.0) as u64,
                0.20 + amount * 0.35,
                0.35 + amount * 0.35,
            )?]),
        }
    }
}

struct WaveFormat {
    audio_format: AudioFormat,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
}

impl WaveFormat {
    fn bytes_per_sample(&self) -> usize {
        usize::from(self.bits_per_sample / 8)
    }

    fn frame_size(&self) -> usize {
        self.bytes_per_sample() * usize::from(self.channels)
    }
}

struct WavHeader {
    format: WaveFormat,
    /// Byte offset of the `data` chunk payload.
    data_offset: u64,
    /// Payload length, clamped to the bytes actually present in the file.
    data_len: u64,
    /// Length stored in the `data` chunk header, which may be bogus.
    declared_data_len: u32,
    declared_riff_len: u32,
}

impl WavHeader {
    fn frame_count(&self) -> u64 {
        self.data_len / self.format.frame_size() as u64
    }
}

const MAX_CHANNELS: u16 = 64;
const MAX_SAMPLE_RATE: u32 = 1_536_000;
const MAX_FORMAT_CHUNK_SIZE: u64 = 1024;
const SAMPLE_CHUNK_BYTES: usize = 64 * 1024;
const WRITE_CHUNK_SAMPLES: usize = 16 * 1024;
const WAV_HEADER_SIZE: u32 = 36;
const PIPELINE_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads the playback length from the WAV header without decoding samples.
pub fn duration(path: &Path) -> Result<Duration> {
    let file =
        fs::File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let file_len = file.metadata()?.len();
    let header = read_header(&mut BufReader::new(file), file_len)
        .with_context(|| format!("Unsupported WAV file: {}", path.display()))?;

    Ok(Duration::from_secs_f64(
        header.frame_count() as f64 / f64::from(header.format.sample_rate),
    ))
}

/// Rewrites the RIFF and `data` sizes of a WAV file whose writer never got to
/// finalize them (for example a recording interrupted before EOS).
pub fn repair_wav_header(path: &Path) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let file_len = file.metadata()?.len();
    let header = read_header(&mut BufReader::new(&mut file), file_len)
        .with_context(|| format!("Unsupported WAV file: {}", path.display()))?;

    let data_len = u32::try_from(header.data_len).unwrap_or(u32::MAX);
    let riff_len = u32::try_from(file_len.saturating_sub(8)).unwrap_or(u32::MAX);
    if header.declared_data_len != data_len {
        file.seek(SeekFrom::Start(header.data_offset - 4))?;
        file.write_all(&data_len.to_le_bytes())?;
    }
    if header.declared_riff_len != riff_len {
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&riff_len.to_le_bytes())?;
    }

    file.flush()
        .with_context(|| format!("Failed to write {}", path.display()))
}

pub fn trim(path: &Path, start: Duration, end: Duration) -> Result<PathBuf> {
    let mut audio = read_wav(path)?;
    let start_frame = duration_to_frame(start, audio.sample_rate);
    let end_frame = duration_to_frame(end, audio.sample_rate).min(audio.frame_count());
    if start_frame >= end_frame {
        return Err(anyhow!("Trim start must be before trim end"));
    }

    let channels = usize::from(audio.channels);
    audio.samples.truncate(end_frame * channels);
    audio.samples.drain(..start_frame * channels);
    write_processed(path, "Trimmed", &audio)
}

pub fn change_volume(path: &Path, gain: f32) -> Result<PathBuf> {
    let mut audio = read_wav(path)?;
    for sample in &mut audio.samples {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }

    write_processed(path, "Volume", &audio)
}

pub fn change_speed(path: &Path, factor: f32) -> Result<PathBuf> {
    if factor <= 0.0 || !factor.is_finite() {
        return Err(anyhow!("Speed factor must be greater than zero"));
    }

    let mut audio = read_wav(path)?;
    let frames = audio.frame_count();
    let new_frames = ((frames as f64 / f64::from(factor)).round() as usize).max(1);
    audio.samples = resample_frames(&audio.samples, audio.channels, new_frames);
    write_processed(path, "Speed", &audio)
}

pub fn change_pitch(path: &Path, semitones: f32) -> Result<PathBuf> {
    let factor = 2.0_f32.powf(semitones / 12.0);
    filter_processed(path, "Pitch", vec![pitch_filter(factor)?])
}

pub fn equalize(path: &Path, gains_db: [f32; EQUALIZER_BANDS.len()]) -> Result<PathBuf> {
    filter_processed(path, "Equalized", vec![equalizer_filter(gains_db)?])
}

pub fn apply_voice_effect(path: &Path, preset: VoiceEffectPreset, amount: f32) -> Result<PathBuf> {
    let amount = amount.clamp(0.0, 1.0);
    filter_processed(path, preset.output_suffix(), preset.filters(amount)?)
}

pub fn reverse(path: &Path) -> Result<PathBuf> {
    let mut audio = read_wav(path)?;
    reverse_frames(&mut audio.samples, audio.channels);
    write_processed(path, "Reversed", &audio)
}

pub fn merge(first_path: &Path, second_path: &Path) -> Result<PathBuf> {
    let mut first = read_wav(first_path)?;
    let mut second = read_wav(second_path)?;
    second = convert_for_merge(second, first.sample_rate, first.channels);
    first.samples.extend(second.samples);
    write_processed(first_path, "Merged", &first)
}

pub fn export(path: &Path, destination: &Path, options: ExportOptions) -> Result<()> {
    let audio = prepare_for_export(read_wav(path)?, options);
    write_atomically(destination, |partial_path| match options.format {
        ExportFormat::Wav => write_wav(partial_path, &audio),
        ExportFormat::Mp3 | ExportFormat::OggOpus | ExportFormat::Flac => {
            let temp_path = temporary_wav_path()?;
            let result = (|| {
                write_wav(&temp_path, &audio)?;
                transcode_wav(&temp_path, partial_path, options)
            })();
            let _ = fs::remove_file(&temp_path);
            result
        }
    })
    .with_context(|| {
        format!(
            "Failed to export recording from {} to {}",
            path.display(),
            destination.display()
        )
    })
}

fn read_wav(path: &Path) -> Result<AudioBuffer> {
    let file =
        fs::File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let file_len = file.metadata()?.len();
    read_wav_from(BufReader::new(file), file_len)
        .with_context(|| format!("Unsupported WAV file: {}", path.display()))
}

#[cfg(test)]
fn parse_wav(bytes: &[u8]) -> Result<AudioBuffer> {
    read_wav_from(std::io::Cursor::new(bytes), bytes.len() as u64)
}

fn read_wav_from<R: Read + Seek>(mut reader: R, file_len: u64) -> Result<AudioBuffer> {
    let header = read_header(&mut reader, file_len)?;
    reader.seek(SeekFrom::Start(header.data_offset))?;
    let samples = decode_samples(&mut reader, &header)?;

    Ok(AudioBuffer {
        sample_rate: header.format.sample_rate,
        channels: header.format.channels,
        samples,
    })
}

/// Walks the RIFF chunks up to the audio payload. Sizes left unfinalized by an
/// interrupted writer (zero, 0xFFFFFFFF or past the end of the file) are clamped
/// to the bytes actually on disk instead of being rejected.
fn read_header<R: Read + Seek>(reader: &mut R, file_len: u64) -> Result<WavHeader> {
    let mut riff = [0_u8; 12];
    reader.seek(SeekFrom::Start(0))?;
    if file_len < 12
        || reader.read_exact(&mut riff).is_err()
        || &riff[0..4] != b"RIFF"
        || &riff[8..12] != b"WAVE"
    {
        return Err(anyhow!("File is not a RIFF/WAVE file"));
    }
    let declared_riff_len = read_u32(&riff, 4)?;

    let mut format = None;
    let mut data = None;
    let mut offset = 12_u64;

    while offset + 8 <= file_len && (format.is_none() || data.is_none()) {
        let mut chunk_header = [0_u8; 8];
        reader.seek(SeekFrom::Start(offset))?;
        reader.read_exact(&mut chunk_header)?;
        let chunk_size = u64::from(read_u32(&chunk_header, 4)?);
        let chunk_start = offset + 8;
        let available = file_len - chunk_start;

        match &chunk_header[0..4] {
            b"fmt " => {
                if !(16..=MAX_FORMAT_CHUNK_SIZE).contains(&chunk_size) || chunk_size > available {
                    return Err(anyhow!("Invalid WAV format chunk"));
                }
                let mut bytes = vec![0_u8; chunk_size as usize];
                reader.read_exact(&mut bytes)?;
                format = Some(parse_format(&bytes)?);
            }
            b"data" => {
                if chunk_size == 0 || chunk_size > available {
                    // Nothing trustworthy follows an unfinalized payload.
                    data = Some((chunk_start, available, chunk_size as u32));
                    break;
                }
                data = Some((chunk_start, chunk_size, chunk_size as u32));
            }
            _ => {}
        }

        if chunk_size > available {
            break;
        }
        offset = chunk_start + chunk_size + (chunk_size % 2);
    }

    let format = format.ok_or_else(|| anyhow!("Missing WAV format chunk"))?;
    let (data_offset, data_len, declared_data_len) =
        data.ok_or_else(|| anyhow!("Missing WAV data chunk"))?;

    Ok(WavHeader {
        format,
        data_offset,
        data_len,
        declared_data_len,
        declared_riff_len,
    })
}

fn decode_samples<R: Read>(reader: &mut R, header: &WavHeader) -> Result<Vec<f32>> {
    let bytes_per_sample = header.format.bytes_per_sample();
    let frame_size = header.format.frame_size();
    let usable_len = header.data_len - header.data_len % frame_size as u64;
    let sample_count =
        usize::try_from(usable_len / bytes_per_sample as u64).context("Audio file is too large")?;

    let mut samples = Vec::with_capacity(sample_count);
    let mut buffer = vec![0_u8; (SAMPLE_CHUNK_BYTES / frame_size).max(1) * frame_size];
    let mut remaining = usable_len;
    while remaining > 0 {
        let len = remaining.min(buffer.len() as u64) as usize;
        reader
            .read_exact(&mut buffer[..len])
            .context("Unexpected end of WAV file")?;
        for sample_bytes in buffer[..len].chunks_exact(bytes_per_sample) {
            samples.push(decode_sample(sample_bytes, header.format.audio_format)?);
        }
        remaining -= len as u64;
    }

    Ok(samples)
}

fn parse_format(bytes: &[u8]) -> Result<WaveFormat> {
    if bytes.len() < 16 {
        return Err(anyhow!("Invalid WAV format chunk"));
    }

    let raw_format = read_u16(bytes, 0)?;
    let channels = read_u16(bytes, 2)?;
    let sample_rate = read_u32(bytes, 4)?;
    let bits_per_sample = read_u16(bytes, 14)?;
    let audio_format = match raw_format {
        1 => AudioFormat::Pcm,
        3 => AudioFormat::Float,
        0xfffe => extensible_audio_format(bytes)?,
        _ => return Err(anyhow!("Unsupported WAV audio format {raw_format}")),
    };

    if channels == 0 || channels > MAX_CHANNELS {
        return Err(anyhow!("Unsupported WAV channel count {channels}"));
    }
    if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
        return Err(anyhow!("Unsupported WAV sample rate {sample_rate}"));
    }
    match (audio_format, bits_per_sample) {
        (AudioFormat::Pcm, 8 | 16 | 24 | 32) | (AudioFormat::Float, 32 | 64) => {}
        _ => return Err(anyhow!("Unsupported WAV sample size {bits_per_sample}")),
    }

    Ok(WaveFormat {
        audio_format,
        channels,
        sample_rate,
        bits_per_sample,
    })
}

fn extensible_audio_format(bytes: &[u8]) -> Result<AudioFormat> {
    if bytes.len() < 40 {
        return Err(anyhow!("Invalid WAV extensible format"));
    }

    match &bytes[24..40] {
        [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113] => Ok(AudioFormat::Pcm),
        [3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113] => Ok(AudioFormat::Float),
        _ => Err(anyhow!("Unsupported WAV extensible subtype")),
    }
}

fn decode_sample(bytes: &[u8], format: AudioFormat) -> Result<f32> {
    let sample = match (format, bytes.len()) {
        (AudioFormat::Float, 4) => f32::from_le_bytes(bytes.try_into()?),
        (AudioFormat::Float, 8) => f64::from_le_bytes(bytes.try_into()?) as f32,
        (AudioFormat::Pcm, 1) => (f32::from(bytes[0]) - 128.0) / 128.0,
        (AudioFormat::Pcm, 2) => f32::from(i16::from_le_bytes(bytes.try_into()?)) / 32_768.0,
        (AudioFormat::Pcm, 3) => {
            let value =
                i32::from(bytes[0]) | (i32::from(bytes[1]) << 8) | (i32::from(bytes[2]) << 16);
            ((value << 8) >> 8) as f32 / 8_388_608.0
        }
        (AudioFormat::Pcm, 4) => {
            (f64::from(i32::from_le_bytes(bytes.try_into()?)) / 2_147_483_648.0) as f32
        }
        _ => return Err(anyhow!("Unsupported WAV sample size")),
    };

    Ok(if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    })
}

fn write_processed(source: &Path, suffix: &str, audio: &AudioBuffer) -> Result<PathBuf> {
    let output_path = next_processed_path(source, suffix)?;
    write_atomically(&output_path, |partial_path| write_wav(partial_path, audio))?;
    Ok(output_path)
}

fn filter_processed(source: &Path, suffix: &str, filters: Vec<gst::Element>) -> Result<PathBuf> {
    let output_path = next_processed_path(source, suffix)?;
    write_atomically(&output_path, |partial_path| {
        process_wav_with_filters(source, partial_path, filters)
    })?;
    Ok(output_path)
}

/// Lets `write` produce a hidden `.<name>.part` sibling and only moves it over
/// `destination` once it is complete, so failures never leave partial files or
/// clobber an existing one.
fn write_atomically(destination: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let partial_path = partial_path(destination)?;
    let result = write(&partial_path).and_then(|()| {
        fs::File::open(&partial_path)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("Failed to write {}", partial_path.display()))?;
        fs::rename(&partial_path, destination)
            .with_context(|| format!("Failed to write {}", destination.display()))
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial_path);
    }
    result
}

fn partial_path(destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| anyhow!("Invalid output file name: {}", destination.display()))?;
    let mut partial_name = OsString::from(".");
    partial_name.push(name);
    partial_name.push(".part");
    Ok(destination.with_file_name(partial_name))
}

fn write_wav(path: &Path, audio: &AudioBuffer) -> Result<()> {
    if audio.channels == 0 || audio.sample_rate == 0 {
        return Err(anyhow!("Cannot write invalid audio format"));
    }

    let too_large = || anyhow!("Audio file is too large");
    let data_size = audio
        .samples
        .len()
        .checked_mul(4)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(too_large)?;
    let riff_size = data_size
        .checked_add(WAV_HEADER_SIZE)
        .ok_or_else(too_large)?;
    let block_align = audio
        .channels
        .checked_mul(4)
        .ok_or_else(|| anyhow!("Cannot write invalid audio format"))?;
    let byte_rate = audio
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| anyhow!("Cannot write invalid audio format"))?;

    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&3_u16.to_le_bytes());
    header.extend_from_slice(&audio.channels.to_le_bytes());
    header.extend_from_slice(&audio.sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&32_u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_size.to_le_bytes());

    let write = || -> std::io::Result<()> {
        let mut file = fs::File::create(path)?;
        file.write_all(&header)?;
        let mut bytes = Vec::with_capacity(WRITE_CHUNK_SAMPLES * 4);
        for chunk in audio.samples.chunks(WRITE_CHUNK_SAMPLES) {
            bytes.clear();
            for sample in chunk {
                bytes.extend_from_slice(&sample.clamp(-1.0, 1.0).to_le_bytes());
            }
            file.write_all(&bytes)?;
        }
        file.flush()
    };
    write().with_context(|| format!("Failed to write {}", path.display()))
}

fn prepare_for_export(mut audio: AudioBuffer, options: ExportOptions) -> AudioBuffer {
    let target_channels = match options.channel_mode {
        ExportChannelMode::Original => audio.channels,
        ExportChannelMode::Mono => 1,
        ExportChannelMode::Stereo => 2,
    };
    if target_channels != audio.channels {
        audio.samples = convert_channels(&audio.samples, audio.channels, target_channels);
        audio.channels = target_channels;
    }

    let target_rate = match options.sample_rate {
        ExportSampleRate::Original => audio.sample_rate,
        ExportSampleRate::Hz(sample_rate) => sample_rate,
    };
    let target_rate = supported_export_sample_rate(options.format, target_rate);
    if target_rate != audio.sample_rate {
        let target_frames = (audio.frame_count() as f64 * f64::from(target_rate)
            / f64::from(audio.sample_rate))
        .round()
        .max(1.0) as usize;
        audio.samples = resample_frames(&audio.samples, audio.channels, target_frames);
        audio.sample_rate = target_rate;
    }

    audio
}

fn supported_export_sample_rate(format: ExportFormat, sample_rate: u32) -> u32 {
    if format != ExportFormat::OggOpus {
        return sample_rate;
    }

    match sample_rate {
        8_000 | 12_000 | 16_000 | 24_000 | 48_000 => sample_rate,
        _ => 48_000,
    }
}

fn process_wav_with_filters(
    source: &Path,
    destination: &Path,
    filters: Vec<gst::Element>,
) -> Result<()> {
    let pipeline = gst::Pipeline::new();
    let source_element = make_gst_element("filesrc")?;
    let wav_parser = make_gst_element("wavparse")?;
    let convert = make_gst_element("audioconvert")?;
    let resample = make_gst_element("audioresample")?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .build(),
        )
        .build()
        .context("Missing GStreamer element: capsfilter")?;
    let output_convert = make_gst_element("audioconvert")?;
    let encoder = make_gst_element("wavenc")?;
    let sink = make_gst_element("filesink")?;

    source_element.set_property("location", source.to_string_lossy().as_ref());
    sink.set_property("location", destination.to_string_lossy().as_ref());

    let mut elements = vec![source_element, wav_parser, convert, resample, caps_filter];
    elements.extend(filters);
    elements.extend([output_convert, encoder, sink]);

    for element in &elements {
        pipeline
            .add(element)
            .context("Failed to add processing element to pipeline")?;
    }

    for pair in elements.windows(2) {
        pair[0]
            .link(&pair[1])
            .context("Failed to link processing pipeline")?;
    }

    run_pipeline(&pipeline, "processing")
}

fn pitch_filter(factor: f32) -> Result<gst::Element> {
    let pitch = make_gst_element("pitch")
        .context("Install the GStreamer SoundTouch plugin to change pitch")?;
    pitch.set_property("pitch", factor.clamp(0.1, 10.0));
    pitch.set_property("tempo", 1.0_f32);
    pitch.set_property("rate", 1.0_f32);
    Ok(pitch)
}

fn equalizer_filter(gains_db: [f32; EQUALIZER_BANDS.len()]) -> Result<gst::Element> {
    let equalizer = make_gst_element("equalizer-10bands")
        .context("Install the GStreamer equalizer plugin to use the equalizer")?;
    for (index, gain) in gains_db.iter().enumerate() {
        equalizer.set_property(&format!("band{index}"), f64::from(gain.clamp(-24.0, 12.0)));
    }
    Ok(equalizer)
}

fn chebyshev_filter(mode: &str, cutoff: f32) -> Result<gst::Element> {
    let filter = make_gst_element("audiocheblimit")
        .context("Install the GStreamer audio effects plugin to use voice effects")?;
    filter.set_property_from_str("mode", mode);
    filter.set_property("cutoff", cutoff);
    filter.set_property("poles", 4_i32);
    filter.set_property("ripple", 0.25_f32);
    Ok(filter)
}

fn echo_filter(delay: u64, feedback: f32, intensity: f32) -> Result<gst::Element> {
    let echo = make_gst_element("audioecho")
        .context("Install the GStreamer audio effects plugin to use echo effects")?;
    echo.set_property("max-delay", delay.max(1));
    echo.set_property("delay", delay.max(1));
    echo.set_property("feedback", feedback.clamp(0.0, 0.95));
    echo.set_property("intensity", intensity.clamp(0.0, 1.0));
    Ok(echo)
}

fn temporary_wav_path() -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for index in 0..1000 {
        let candidate = env::temp_dir().join(format!(
            "bigrecorder-export-{}-{nanos}-{index}.wav",
            process::id()
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!("Could not create a temporary export file"))
}

fn transcode_wav(source: &Path, destination: &Path, options: ExportOptions) -> Result<()> {
    let pipeline = gst::Pipeline::new();
    let source_element = make_gst_element("filesrc")?;
    let wav_parser = make_gst_element("wavparse")?;
    let convert = make_gst_element("audioconvert")?;
    let resample = make_gst_element("audioresample")?;
    let encoder = export_encoder(options.format, options.quality)?;
    let sink = make_gst_element("filesink")?;

    source_element.set_property("location", source.to_string_lossy().as_ref());
    sink.set_property("location", destination.to_string_lossy().as_ref());

    match options.format {
        ExportFormat::Mp3 | ExportFormat::Flac => {
            pipeline
                .add_many([
                    &source_element,
                    &wav_parser,
                    &convert,
                    &resample,
                    &encoder,
                    &sink,
                ])
                .context("Failed to add export elements to pipeline")?;
            gst::Element::link_many([
                &source_element,
                &wav_parser,
                &convert,
                &resample,
                &encoder,
                &sink,
            ])
            .context("Failed to link export pipeline")?;
        }
        ExportFormat::OggOpus => {
            let muxer = make_gst_element("oggmux")?;
            pipeline
                .add_many([
                    &source_element,
                    &wav_parser,
                    &convert,
                    &resample,
                    &encoder,
                    &muxer,
                    &sink,
                ])
                .context("Failed to add export elements to pipeline")?;
            gst::Element::link_many([
                &source_element,
                &wav_parser,
                &convert,
                &resample,
                &encoder,
                &muxer,
                &sink,
            ])
            .context("Failed to link export pipeline")?;
        }
        ExportFormat::Wav => return Err(anyhow!("WAV export does not use the transcoder")),
    }

    run_pipeline(&pipeline, "export")
}

/// Plays `pipeline` to completion and always leaves it in the Null state.
fn run_pipeline(pipeline: &gst::Pipeline, label: &str) -> Result<()> {
    let result = pipeline
        .set_state(gst::State::Playing)
        .map_err(|err| anyhow!("Failed to start {label} pipeline: {err:?}"))
        .and_then(|_| wait_for_pipeline_end(pipeline, label));
    let stop_result = pipeline
        .set_state(gst::State::Null)
        .map_err(|err| anyhow!("Failed to stop {label} pipeline: {err:?}"));

    result?;
    stop_result?;
    Ok(())
}

/// Waits for EOS, giving up when neither the bytes written nor the stream
/// position advance for `PIPELINE_STALL_TIMEOUT`.
fn wait_for_pipeline_end(pipeline: &gst::Pipeline, label: &str) -> Result<()> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("The {label} pipeline has no message bus"))?;
    let mut last_progress = None;
    let mut last_progress_at = Instant::now();

    loop {
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(200),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            match message.view() {
                gst::MessageView::Eos(_) => return Ok(()),
                gst::MessageView::Error(error) => {
                    return Err(anyhow!(
                        "The {label} pipeline failed: {} ({})",
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

        let progress = (
            pipeline.query_position::<gst::format::Bytes>(),
            pipeline.query_position::<gst::ClockTime>(),
        );
        if progress != (None, None) && Some(progress) != last_progress {
            last_progress = Some(progress);
            last_progress_at = Instant::now();
        } else if last_progress_at.elapsed() >= PIPELINE_STALL_TIMEOUT {
            return Err(anyhow!("The {label} pipeline stopped making progress"));
        }
    }
}

fn export_encoder(format: ExportFormat, quality: ExportQuality) -> Result<gst::Element> {
    match format {
        ExportFormat::Mp3 => {
            let encoder = make_gst_element("lamemp3enc")
                .context("Install the GStreamer LAME MP3 plugin to export MP3 files")?;
            encoder.set_property_from_str("target", "bitrate");
            encoder.set_property("cbr", true);
            encoder.set_property("bitrate", mp3_bitrate(quality));
            Ok(encoder)
        }
        ExportFormat::OggOpus => {
            let encoder = make_gst_element("opusenc")
                .context("Install the GStreamer Opus plugin to export Opus files")?;
            encoder.set_property_from_str("audio-type", "voice");
            encoder.set_property_from_str("bitrate-type", "constrained-vbr");
            encoder.set_property("bitrate", opus_bitrate(quality));
            encoder.set_property("dtx", true);
            Ok(encoder)
        }
        ExportFormat::Flac => {
            let encoder = make_gst_element("flacenc")
                .context("Install the GStreamer FLAC plugin to export FLAC files")?;
            encoder.set_property_from_str("quality", flac_quality(quality));
            Ok(encoder)
        }
        ExportFormat::Wav => Err(anyhow!("WAV export does not need an encoder")),
    }
}

fn make_gst_element(name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(name)
        .build()
        .with_context(|| format!("Missing GStreamer element: {name}"))
}

fn mp3_bitrate(quality: ExportQuality) -> i32 {
    match quality {
        ExportQuality::Low => 96,
        ExportQuality::Medium => 160,
        ExportQuality::High => 256,
    }
}

fn opus_bitrate(quality: ExportQuality) -> i32 {
    match quality {
        ExportQuality::Low => 32_000,
        ExportQuality::Medium => 64_000,
        ExportQuality::High => 128_000,
    }
}

fn flac_quality(quality: ExportQuality) -> &'static str {
    match quality {
        ExportQuality::Low => "3",
        ExportQuality::Medium => "5",
        ExportQuality::High => "8",
    }
}

fn next_processed_path(source: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = source
        .parent()
        .ok_or_else(|| anyhow!("Recording does not have a parent directory"))?;
    let stem = source
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Recording");

    for index in 1..1000 {
        let candidate = if index == 1 {
            parent.join(format!("{stem} - {suffix}.wav"))
        } else {
            parent.join(format!("{stem} - {suffix} {index}.wav"))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!("Could not create a unique output file"))
}

fn duration_to_frame(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
}

fn resample_frames(samples: &[f32], channels: u16, target_frames: usize) -> Vec<f32> {
    let channels = usize::from(channels);
    if channels == 0 {
        return Vec::new();
    }
    let source_frames = samples.len() / channels;
    if source_frames == 0 || target_frames == 0 {
        return Vec::new();
    }
    if source_frames == target_frames {
        return samples.to_vec();
    }

    let mut output = vec![0.0_f32; target_frames * channels];
    let scale = if target_frames == 1 {
        0.0
    } else {
        (source_frames - 1) as f64 / (target_frames - 1) as f64
    };

    for target_frame in 0..target_frames {
        let source_position = target_frame as f64 * scale;
        let left_frame = (source_position.floor() as usize).min(source_frames - 1);
        let right_frame = (left_frame + 1).min(source_frames - 1);
        let blend = (source_position - left_frame as f64) as f32;

        for channel in 0..channels {
            let left = samples[left_frame * channels + channel];
            let right = samples[right_frame * channels + channel];
            output[target_frame * channels + channel] = left + (right - left) * blend;
        }
    }

    output
}

fn reverse_frames(samples: &mut [f32], channels: u16) {
    let channels = usize::from(channels);
    if channels == 0 {
        return;
    }
    let frames = samples.len() / channels;
    for frame in 0..frames / 2 {
        let opposite = frames - frame - 1;
        for channel in 0..channels {
            samples.swap(frame * channels + channel, opposite * channels + channel);
        }
    }
}

fn convert_for_merge(mut audio: AudioBuffer, sample_rate: u32, channels: u16) -> AudioBuffer {
    if audio.sample_rate != sample_rate {
        let target_frames = (audio.frame_count() as f64 * f64::from(sample_rate)
            / f64::from(audio.sample_rate))
        .round()
        .max(1.0) as usize;
        audio.samples = resample_frames(&audio.samples, audio.channels, target_frames);
        audio.sample_rate = sample_rate;
    }

    if audio.channels != channels {
        audio.samples = convert_channels(&audio.samples, audio.channels, channels);
        audio.channels = channels;
    }

    audio
}

fn convert_channels(samples: &[f32], source_channels: u16, target_channels: u16) -> Vec<f32> {
    let source_channels = usize::from(source_channels);
    let target_channels = usize::from(target_channels);
    if source_channels == 0 {
        return Vec::new();
    }
    let frames = samples.len() / source_channels;
    let mut output = vec![0.0_f32; frames * target_channels];

    for frame in 0..frames {
        for channel in 0..target_channels {
            output[frame * target_channels + channel] = if source_channels == 1 {
                samples[frame * source_channels]
            } else if target_channels == 1 {
                let start = frame * source_channels;
                samples[start..start + source_channels].iter().sum::<f32>() / source_channels as f32
            } else {
                samples[frame * source_channels + channel.min(source_channels - 1)]
            };
        }
    }

    output
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(range(offset, 2)?)
            .ok_or_else(|| anyhow!("Unexpected end of WAV file"))?
            .try_into()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(range(offset, 4)?)
            .ok_or_else(|| anyhow!("Unexpected end of WAV file"))?
            .try_into()?,
    ))
}

fn range(offset: usize, len: usize) -> Result<Range<usize>> {
    Ok(offset
        ..offset
            .checked_add(len)
            .ok_or_else(|| anyhow!("Invalid WAV offset"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_bytes(riff_size: u32, data_size: u32, samples: &[i16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn test_dir(name: &str) -> PathBuf {
        let directory = env::temp_dir().join(format!("bigrecorder-{name}-{}", process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create test dir");
        directory
    }

    #[test]
    fn parse_tolerates_unfinalized_data_sizes() {
        let samples = [i16::MIN, 16_384, 0, i16::MAX];
        for (riff_size, data_size) in [(44, 8), (0, 0), (u32::MAX, u32::MAX), (36, 4_000)] {
            let audio =
                parse_wav(&wav_bytes(riff_size, data_size, &samples)).expect("tolerant parse");
            assert_eq!(audio.sample_rate, 8_000);
            assert_eq!(audio.channels, 1);
            assert_eq!(audio.samples.len(), 4);
            assert_eq!(audio.samples[0], -1.0);
            assert_eq!(audio.samples[1], 0.5);
        }
    }

    #[test]
    fn parse_ignores_trailing_partial_frame() {
        let mut bytes = wav_bytes(0, 0, &[1_000, 2_000]);
        bytes.push(7);
        assert_eq!(parse_wav(&bytes).expect("parse").samples.len(), 2);
    }

    #[test]
    fn parse_rejects_absurd_formats_without_panicking() {
        let mut zero_channels = wav_bytes(44, 8, &[0; 4]);
        zero_channels[22..24].copy_from_slice(&0_u16.to_le_bytes());
        assert!(parse_wav(&zero_channels).is_err());

        let mut zero_rate = wav_bytes(44, 8, &[0; 4]);
        zero_rate[24..28].copy_from_slice(&0_u32.to_le_bytes());
        assert!(parse_wav(&zero_rate).is_err());

        let mut odd_bits = wav_bytes(44, 8, &[0; 4]);
        odd_bits[34..36].copy_from_slice(&0_u16.to_le_bytes());
        assert!(parse_wav(&odd_bits).is_err());

        assert!(parse_wav(b"RIFF\0\0\0\0WAVE").is_err());
        assert!(parse_wav(b"").is_err());
    }

    #[test]
    fn duration_and_repair_use_header_only() {
        let directory = test_dir("repair");
        let path = directory.join("unfinalized.wav");
        fs::write(&path, wav_bytes(u32::MAX, 0, &[0; 8_000])).expect("write wav");

        assert_eq!(duration(&path).expect("duration"), Duration::from_secs(1));
        repair_wav_header(&path).expect("repair");
        let bytes = fs::read(&path).expect("read wav");
        assert_eq!(read_u32(&bytes, 4).expect("riff"), bytes.len() as u32 - 8);
        assert_eq!(read_u32(&bytes, 40).expect("data"), 16_000);

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn atomic_writes_replace_only_on_success() {
        let directory = test_dir("atomic");
        let destination = directory.join("export.wav");
        fs::write(&destination, b"keep me").expect("seed destination");

        let failed = write_atomically(&destination, |partial| {
            fs::write(partial, b"partial")?;
            Err(anyhow!("boom"))
        });
        assert!(failed.is_err());
        assert_eq!(fs::read(&destination).expect("read"), b"keep me");
        assert!(!directory.join(".export.wav.part").exists());

        let audio = AudioBuffer {
            sample_rate: 8_000,
            channels: 2,
            samples: vec![0.25; 64_000],
        };
        write_atomically(&destination, |partial| write_wav(partial, &audio)).expect("write");
        assert!(!directory.join(".export.wav.part").exists());
        let written = read_wav(&destination).expect("read back");
        assert_eq!(written.channels, 2);
        assert_eq!(written.samples, audio.samples);

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reverse_flips_frames_without_swapping_channels() {
        let mut samples = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        reverse_frames(&mut samples, 2);
        assert_eq!(samples, vec![5.0, 6.0, 3.0, 4.0, 1.0, 2.0]);
    }

    #[test]
    fn speed_resampling_changes_frame_count() {
        let samples = vec![0.0, 0.5, 1.0, 0.5];
        let output = resample_frames(&samples, 1, 2);
        assert_eq!(output.len(), 2);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.5);
    }

    #[test]
    fn channel_conversion_mixes_to_mono() {
        let output = convert_channels(&[0.2, 0.6, 0.4, 0.8], 2, 1);
        assert_eq!(output, vec![0.4, 0.6]);
    }

    #[test]
    fn export_format_extensions_are_stable() {
        assert_eq!(ExportFormat::Wav.extension(), "wav");
        assert_eq!(ExportFormat::Mp3.extension(), "mp3");
        assert_eq!(ExportFormat::OggOpus.extension(), "opus");
        assert_eq!(ExportFormat::Flac.extension(), "flac");
    }

    #[test]
    fn export_preparation_changes_channels_and_sample_rate() {
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            samples: vec![0.0, 0.25, 0.5, 0.75],
        };
        let output = prepare_for_export(
            audio,
            ExportOptions {
                format: ExportFormat::Wav,
                quality: ExportQuality::Medium,
                channel_mode: ExportChannelMode::Stereo,
                sample_rate: ExportSampleRate::Hz(24_000),
            },
        );

        assert_eq!(output.sample_rate, 24_000);
        assert_eq!(output.channels, 2);
        assert_eq!(output.frame_count(), 2);
    }

    #[test]
    fn opus_export_uses_supported_sample_rate() {
        assert_eq!(
            supported_export_sample_rate(ExportFormat::OggOpus, 44_100),
            48_000
        );
        assert_eq!(
            supported_export_sample_rate(ExportFormat::OggOpus, 24_000),
            24_000
        );
        assert_eq!(
            supported_export_sample_rate(ExportFormat::Mp3, 44_100),
            44_100
        );
    }
}
