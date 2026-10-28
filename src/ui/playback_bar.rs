use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use gtk::prelude::*;
use gtk::{gio, glib};

use crate::ui::style::theme_color;

const PLAYBACK_BINS: usize = 52;
const MIN_PROGRESS_DELTA: f64 = 0.00005;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const MAX_CACHED_WAVEFORMS: usize = 512;

type CacheKey = (PathBuf, Option<SystemTime>, u64);

thread_local! {
    /// Peaks of already analysed files, so re-creating rows is cheap. Keyed by
    /// path, modification time and size so edited files are re-analysed.
    static PEAK_CACHE: RefCell<HashMap<CacheKey, Vec<f64>>> = RefCell::new(HashMap::new());
}

#[derive(Clone)]
pub struct PlaybackBar {
    area: gtk::DrawingArea,
    progress: Rc<Cell<f64>>,
}

impl PlaybackBar {
    /// Builds the bar immediately with flat placeholder bars; the real peaks
    /// are computed off the main thread and drawn once available.
    pub fn new(path: &Path) -> Self {
        let progress = Rc::new(Cell::new(0.0));
        let key = cache_key(path);
        let cached = key
            .as_ref()
            .and_then(|key| PEAK_CACHE.with(|cache| cache.borrow().get(key).cloned()));
        let needs_analysis = cached.is_none();
        let segments = Rc::new(RefCell::new(cached.unwrap_or_else(placeholder_segments)));

        let area = gtk::DrawingArea::builder()
            .height_request(30)
            .hexpand(true)
            .build();
        area.add_css_class("playback-progress");

        let draw_progress = Rc::clone(&progress);
        let draw_segments = Rc::clone(&segments);
        area.set_draw_func(move |area, context, width, height| {
            draw_playback_bar(
                area,
                context,
                f64::from(width),
                f64::from(height),
                &draw_segments.borrow(),
                draw_progress.get(),
            );
        });

        if needs_analysis {
            spawn_analysis(path.to_path_buf(), key, &area, segments);
        }

        Self { area, progress }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn set_progress(&self, progress: f64) {
        let progress = progress.clamp(0.0, 1.0);
        if (self.progress.get() - progress).abs() < MIN_PROGRESS_DELTA {
            return;
        }

        self.progress.set(progress);
        self.area.queue_draw();
    }
}

fn cache_key(path: &Path) -> Option<CacheKey> {
    let metadata = fs::metadata(path).ok()?;
    Some((path.to_path_buf(), metadata.modified().ok(), metadata.len()))
}

fn spawn_analysis(
    path: PathBuf,
    key: Option<CacheKey>,
    area: &gtk::DrawingArea,
    segments: Rc<RefCell<Vec<f64>>>,
) {
    let weak_area = area.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let analyzed = gio::spawn_blocking(move || analyze_path(&path))
            .await
            .ok()
            .flatten();

        let peaks = match analyzed {
            Some(peaks) => {
                if let Some(key) = key {
                    PEAK_CACHE.with(|cache| {
                        let mut cache = cache.borrow_mut();
                        if cache.len() >= MAX_CACHED_WAVEFORMS {
                            cache.clear();
                        }
                        cache.insert(key, peaks.clone());
                    });
                }
                peaks
            }
            None => default_segments(),
        };

        // The row may have been destroyed while the file was being analysed.
        let Some(area) = weak_area.upgrade() else {
            return;
        };
        *segments.borrow_mut() = peaks;
        area.queue_draw();
    });
}

fn draw_playback_bar(
    widget: &gtk::DrawingArea,
    context: &gtk::cairo::Context,
    width: f64,
    height: f64,
    segments: &[f64],
    progress: f64,
) {
    if segments.is_empty() || width <= 1.0 || height <= 1.0 {
        return;
    }

    let padding = 1.0;
    let available_width = (width - padding * 2.0).max(1.0);
    let slot = available_width / segments.len() as f64;
    let bar_width = (slot * 0.58).clamp(1.5, 3.0);
    let center_y = height / 2.0;
    let max_height = (height - 4.0).max(2.0);
    let progress_x = padding + available_width * progress.clamp(0.0, 1.0);
    let accent = theme_color(widget, "accent_bg_color", (0.21, 0.52, 0.89));
    let secondary = theme_color(widget, "accent_color", (0.21, 0.52, 0.89));

    context.set_line_cap(gtk::cairo::LineCap::Round);
    context.set_line_width(bar_width);

    for (index, segment) in segments.iter().enumerate() {
        let level = segment.clamp(0.0, 1.0);
        let x = padding + index as f64 * slot + slot / 2.0;
        // A subtle baseline keeps even quiet regions readable as a waveform.
        let bar_height = (max_height * level).max(2.5);
        let y1 = center_y - bar_height / 2.0;
        let y2 = center_y + bar_height / 2.0;

        // Played bars are bright; upcoming bars are dimmed but still visible.
        let (color, alpha) = if x <= progress_x {
            (secondary, 0.95)
        } else {
            (accent, 0.42 + level * 0.18)
        };

        context.set_source_rgba(color.0, color.1, color.2, alpha);
        context.move_to(x, y1);
        context.line_to(x, y2);
        let _ = context.stroke();
    }
}

fn analyze_path(path: &Path) -> Option<Vec<f64>> {
    let file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    analyze_wav(BufReader::with_capacity(READ_BUFFER_BYTES, file), file_len)
}

/// Streams a WAV file and returns normalised per-segment peaks, without
/// loading the audio data into memory. A data chunk whose declared size is
/// zero or past the end of the file (an interrupted recording) is clamped to
/// what is actually on disk.
fn analyze_wav<R: Read + Seek>(mut reader: R, file_len: u64) -> Option<Vec<f64>> {
    let mut header = [0_u8; 12];
    reader.read_exact(&mut header).ok()?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }

    let mut format = None;
    let mut data = None;
    let mut offset = 12_u64;

    while offset + 8 <= file_len {
        let mut chunk_header = [0_u8; 8];
        reader.seek(SeekFrom::Start(offset)).ok()?;
        if reader.read_exact(&mut chunk_header).is_err() {
            break;
        }

        let chunk_size = u64::from(read_u32(&chunk_header, 4)?);
        let chunk_start = offset + 8;
        let available = file_len - chunk_start;

        match &chunk_header[0..4] {
            b"fmt " => {
                // 40 bytes cover WAVE_FORMAT_EXTENSIBLE; anything past is unused.
                let mut fmt = vec![0_u8; chunk_size.min(available).min(64) as usize];
                reader.read_exact(&mut fmt).ok()?;
                format = parse_format(&fmt);
            }
            b"data" => {
                if chunk_size == 0 || chunk_size > available {
                    data = Some((chunk_start, available));
                    // Nothing after an unterminated data chunk can be trusted.
                    break;
                }
                data = Some((chunk_start, chunk_size));
            }
            _ => {}
        }

        if format.is_some() && data.is_some() {
            break;
        }
        offset = chunk_start + chunk_size + (chunk_size % 2);
    }

    let format = format?;
    let (data_start, data_len) = data?;
    let channels = usize::from(format.channels);
    if channels == 0 {
        return None;
    }

    // Use the container size from block_align (e.g. 24-bit samples stored in
    // 32-bit slots), falling back to the nominal bit depth.
    let block_align = usize::from(format.block_align);
    let sample_bytes = if block_align > 0 && block_align % channels == 0 {
        block_align / channels
    } else {
        usize::from(format.bits_per_sample.div_ceil(8))
    };
    if sample_bytes == 0 {
        return None;
    }

    let frame_size = sample_bytes.checked_mul(channels)?;
    let frame_count = data_len / frame_size as u64;
    if frame_count == 0 {
        return Some(vec![0.0; PLAYBACK_BINS]);
    }

    reader.seek(SeekFrom::Start(data_start)).ok()?;
    let frames_per_read = (READ_BUFFER_BYTES / frame_size).max(1);
    let mut buffer = vec![0_u8; frames_per_read * frame_size];
    let mut peaks = vec![0.0_f64; PLAYBACK_BINS];
    let mut frame_index = 0_u64;

    while frame_index < frame_count {
        let frames_left = frame_count - frame_index;
        let wanted =
            frames_per_read.min(usize::try_from(frames_left).unwrap_or(usize::MAX)) * frame_size;
        let read = read_full(&mut reader, &mut buffer[..wanted]).ok()?;

        for frame in buffer[..read].chunks_exact(frame_size) {
            let bin = (frame_index * PLAYBACK_BINS as u64 / frame_count) as usize;
            let frame_peak = frame
                .chunks_exact(sample_bytes)
                .filter_map(|sample| sample_amplitude(sample, format.audio_format))
                .fold(0.0_f64, f64::max);
            peaks[bin] = peaks[bin].max(frame_peak);
            frame_index += 1;
        }

        if read < wanted {
            // The file is shorter than its header claims.
            break;
        }
    }

    Some(normalize_peaks(peaks))
}

/// Fills `buffer` as far as possible, stopping early only at end of file.
fn read_full<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[derive(Clone, Copy)]
struct WaveFormat {
    audio_format: AudioFormat,
    channels: u16,
    block_align: u16,
    bits_per_sample: u16,
}

#[derive(Clone, Copy)]
enum AudioFormat {
    Pcm,
    Float,
}

fn parse_format(bytes: &[u8]) -> Option<WaveFormat> {
    if bytes.len() < 16 {
        return None;
    }

    let raw_format = read_u16(bytes, 0)?;
    let channels = read_u16(bytes, 2)?;
    let block_align = read_u16(bytes, 12)?;
    let bits_per_sample = read_u16(bytes, 14)?;
    let audio_format = match raw_format {
        1 => AudioFormat::Pcm,
        3 => AudioFormat::Float,
        0xfffe => extensible_audio_format(bytes)?,
        _ => return None,
    };

    Some(WaveFormat {
        audio_format,
        channels,
        block_align,
        bits_per_sample,
    })
}

fn extensible_audio_format(bytes: &[u8]) -> Option<AudioFormat> {
    if bytes.len() < 40 {
        return None;
    }

    match &bytes[24..40] {
        [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113] => Some(AudioFormat::Pcm),
        [3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113] => Some(AudioFormat::Float),
        _ => None,
    }
}

fn sample_amplitude(bytes: &[u8], format: AudioFormat) -> Option<f64> {
    match (format, bytes.len()) {
        (AudioFormat::Float, 4) => {
            let value = f32::from_le_bytes(bytes.try_into().ok()?) as f64;
            value.is_finite().then_some(value.abs().clamp(0.0, 1.0))
        }
        (AudioFormat::Float, 8) => {
            let value = f64::from_le_bytes(bytes.try_into().ok()?);
            value.is_finite().then_some(value.abs().clamp(0.0, 1.0))
        }
        (AudioFormat::Pcm, 1) => Some(((f64::from(bytes[0]) - 128.0) / 128.0).abs()),
        (AudioFormat::Pcm, 2) => Some(
            (f64::from(i16::from_le_bytes(bytes.try_into().ok()?)) / f64::from(i16::MAX)).abs(),
        ),
        (AudioFormat::Pcm, 3) => {
            let value =
                i32::from(bytes[0]) | (i32::from(bytes[1]) << 8) | (i32::from(bytes[2]) << 16);
            let signed = (value << 8) >> 8;
            Some((f64::from(signed) / 8_388_608.0).abs())
        }
        (AudioFormat::Pcm, 4) => Some(
            (f64::from(i32::from_le_bytes(bytes.try_into().ok()?)) / f64::from(i32::MAX)).abs(),
        ),
        _ => None,
    }
}

fn normalize_peaks(mut peaks: Vec<f64>) -> Vec<f64> {
    let max_peak = peaks.iter().copied().fold(0.0_f64, f64::max);
    if max_peak <= 0.0001 {
        return peaks;
    }

    let silence_threshold = (max_peak * 0.12).clamp(0.0008, 0.035);
    let usable_range = (max_peak - silence_threshold).max(0.0001);
    for peak in &mut peaks {
        if *peak <= silence_threshold {
            *peak = 0.0;
        } else {
            let normalized = ((*peak - silence_threshold) / usable_range).clamp(0.0, 1.0);
            *peak = normalized.powf(0.45).max(0.18);
        }
    }

    peaks
}

/// Flat, low bars shown while the real peaks are being computed.
fn placeholder_segments() -> Vec<f64> {
    vec![0.0; PLAYBACK_BINS]
}

fn default_segments() -> Vec<f64> {
    vec![1.0; PLAYBACK_BINS]
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn analyze(bytes: &[u8]) -> Option<Vec<f64>> {
        analyze_wav(Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn detects_active_and_silent_pcm_regions() {
        let segments = analyze(&wav(1, 16, &pcm16(&bursty(10_000.0)), None)).expect("valid wav");

        assert_eq!(segments.len(), PLAYBACK_BINS);
        assert!(segments.contains(&0.0));
        assert!(segments.iter().any(|segment| *segment > 0.75));
    }

    #[test]
    fn reads_24_bit_pcm() {
        let data: Vec<u8> = bursty(3_000_000.0)
            .iter()
            .flat_map(|sample| (*sample as i32).to_le_bytes()[..3].to_vec())
            .collect();
        let segments = analyze(&wav(1, 24, &data, None)).expect("valid wav");

        assert!(segments.contains(&0.0));
        assert!(segments.iter().any(|segment| *segment > 0.75));
    }

    #[test]
    fn reads_32_bit_float() {
        let data: Vec<u8> = bursty(0.4)
            .iter()
            .flat_map(|sample| (*sample as f32).to_le_bytes())
            .collect();
        let segments = analyze(&wav(3, 32, &data, None)).expect("valid wav");

        assert!(segments.contains(&0.0));
        assert!(segments.iter().any(|segment| *segment > 0.75));
    }

    #[test]
    fn tolerates_unfinished_data_chunk() {
        let samples = pcm16(&bursty(10_000.0));
        for declared in [0, u32::MAX, samples.len() as u32 * 4] {
            let segments = analyze(&wav(1, 16, &samples, Some(declared))).expect("valid wav");
            assert!(segments.iter().any(|segment| *segment > 0.75));
        }
    }

    #[test]
    fn rejects_non_wav() {
        assert!(analyze(b"not a wav file at all").is_none());
    }

    fn bursty(amplitude: f64) -> Vec<f64> {
        let mut samples = vec![0.0; 960];
        for sample in &mut samples[180..360] {
            *sample = amplitude;
        }
        for sample in &mut samples[620..760] {
            *sample = -amplitude * 1.4;
        }
        samples
    }

    fn pcm16(samples: &[f64]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| (*sample as i16).to_le_bytes())
            .collect()
    }

    fn wav(audio_format: u16, bits: u16, data: &[u8], declared_size: Option<u32>) -> Vec<u8> {
        let block_align = bits / 8;
        let data_size = declared_size.unwrap_or(data.len() as u32);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&36_u32.wrapping_add(data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&audio_format.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&(48_000 * u32::from(block_align)).to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }
}
