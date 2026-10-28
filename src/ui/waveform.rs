use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::audio::recorder::WAVEFORM_BINS;
use crate::ui::style::theme_color;

const FRESH_BARS: usize = 3;
/// The smoothing factors below were tuned for one step every 33 ms; tick
/// callbacks run at the display refresh rate, so steps are rescaled by time.
const REFERENCE_STEP_US: f64 = 33_000.0;
const SILENCE: f64 = 0.003;

#[derive(Clone)]
pub struct Waveform {
    area: gtk::DrawingArea,
    state: Rc<State>,
}

struct State {
    active: Cell<bool>,
    ticking: Cell<bool>,
    last_frame_time: Cell<Option<i64>>,
    target_level: Cell<f64>,
    current_level: Cell<f64>,
    target_peaks: RefCell<[f64; WAVEFORM_BINS]>,
    current_peaks: RefCell<[f64; WAVEFORM_BINS]>,
}

impl State {
    /// Advances the smoothing animation by `steps` reference steps and reports
    /// whether anything is still visible or moving.
    fn advance(&self, steps: f64) -> bool {
        let active = self.active.get();
        let target = if active { self.target_level.get() } else { 0.0 };
        let current = self.current_level.get();
        let mut level = current + (target - current) * smoothing(0.22, steps);
        if !active && level < SILENCE {
            level = 0.0;
        }
        self.current_level.set(level);

        let target_peaks = self.target_peaks.borrow();
        let mut current_peaks = self.current_peaks.borrow_mut();
        let mut visible = level > 0.0;
        for (current_peak, target_peak) in current_peaks.iter_mut().zip(target_peaks.iter()) {
            let target_peak = if active { *target_peak } else { 0.0 };
            let factor = if target_peak > *current_peak {
                smoothing(0.48, steps)
            } else {
                smoothing(0.18, steps)
            };
            *current_peak += (target_peak - *current_peak) * factor;
            if *current_peak < SILENCE {
                *current_peak = 0.0;
            }
            visible |= *current_peak > 0.0;
        }

        active || visible
    }
}

/// Converts a per-reference-step smoothing factor into one for `steps` steps.
fn smoothing(factor: f64, steps: f64) -> f64 {
    1.0 - (1.0 - factor).powf(steps)
}

impl Waveform {
    pub fn new() -> Self {
        let state = Rc::new(State {
            active: Cell::new(false),
            ticking: Cell::new(false),
            last_frame_time: Cell::new(None),
            target_level: Cell::new(0.0),
            current_level: Cell::new(0.0),
            target_peaks: RefCell::new([0.0_f64; WAVEFORM_BINS]),
            current_peaks: RefCell::new([0.0_f64; WAVEFORM_BINS]),
        });

        let area = gtk::DrawingArea::builder()
            .height_request(108)
            .hexpand(true)
            .build();
        area.add_css_class("waveform");

        let draw_state = Rc::clone(&state);
        area.set_draw_func(move |area, context, width, height| {
            draw_waveform(
                area,
                context,
                f64::from(width),
                f64::from(height),
                draw_state.active.get(),
                draw_state.current_level.get(),
                &draw_state.current_peaks.borrow(),
            );
        });

        Self { area, state }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn set_active(&self, active: bool) {
        self.state.active.set(active);
        if !active {
            self.state.target_level.set(0.0);
            self.state.target_peaks.borrow_mut().fill(0.0);
        }
        // Also when deactivating, so the bars decay smoothly to silence.
        self.ensure_animating();
    }

    pub fn set_level(&self, level: f64) {
        self.state.target_level.set(level.clamp(0.0, 1.0));
        self.ensure_animating();
    }

    pub fn set_peaks(&self, peaks: [f64; WAVEFORM_BINS]) {
        {
            let mut target_peaks = self.state.target_peaks.borrow_mut();
            target_peaks.rotate_left(FRESH_BARS);

            for index in 0..FRESH_BARS {
                let chunk_start = index * peaks.len() / FRESH_BARS;
                let chunk_end = (index + 1) * peaks.len() / FRESH_BARS;
                let peak = peaks[chunk_start..chunk_end]
                    .iter()
                    .copied()
                    .fold(0.0_f64, f64::max);
                target_peaks[WAVEFORM_BINS - FRESH_BARS + index] = peak.clamp(0.0, 1.0);
            }
        }
        self.ensure_animating();
    }

    /// Installs a frame-clock tick callback unless one is already running.
    /// The callback removes itself once the waveform is idle and silent, and
    /// GTK drops it with the widget, so nothing outlives the view.
    fn ensure_animating(&self) {
        if self.state.ticking.replace(true) {
            return;
        }
        self.state.last_frame_time.set(None);

        let state = Rc::clone(&self.state);
        self.area.add_tick_callback(move |area, clock| {
            let now = clock.frame_time();
            let steps = state
                .last_frame_time
                .replace(Some(now))
                .map(|last| ((now - last) as f64 / REFERENCE_STEP_US).clamp(0.0, 10.0))
                .unwrap_or(1.0);

            let keep_going = state.advance(steps);
            area.queue_draw();
            if keep_going {
                glib::ControlFlow::Continue
            } else {
                state.ticking.set(false);
                glib::ControlFlow::Break
            }
        });
    }
}

fn draw_waveform(
    widget: &gtk::DrawingArea,
    context: &gtk::cairo::Context,
    width: f64,
    height: f64,
    active: bool,
    level: f64,
    peaks: &[f64; WAVEFORM_BINS],
) {
    // The rounded `.waveform` background is painted by CSS; only bars here.
    let padding = 14.0;
    let center = height / 2.0;
    let available_width = width - padding * 2.0;
    let slot = available_width / peaks.len() as f64;
    let bar_width = (slot * 0.34).clamp(1.6, 3.0);
    let max_bar = (height - padding * 2.0).max(2.0);
    // Follows the system accent and the light/dark variant.
    let accent_hsv = rgb_to_hsv(theme_color(widget, "accent_color", (0.21, 0.52, 0.89)));

    context.set_line_cap(gtk::cairo::LineCap::Round);

    let last = peaks.len().saturating_sub(1).max(1) as f64;
    for (index, peak) in peaks.iter().enumerate() {
        let bar_height = if active {
            (max_bar * peak).max(3.0)
        } else {
            max_bar * peak
        };
        if bar_height <= 0.5 {
            continue;
        }

        // Smooth gradient across the waveform, centred on the accent colour.
        let (r, g, b) = spectrum_color(accent_hsv, index as f64 / last);
        let x = padding + index as f64 * slot + slot / 2.0;
        let y1 = center - bar_height / 2.0;
        let y2 = center + bar_height / 2.0;

        // Neon halo: a few translucent passes of decreasing width build the glow.
        context.set_line_width(bar_width * 4.0);
        context.set_source_rgba(r, g, b, 0.10 + level * 0.06);
        context.move_to(x, y1);
        context.line_to(x, y2);
        let _ = context.stroke();

        context.set_line_width(bar_width * 2.2);
        context.set_source_rgba(r, g, b, 0.22 + level * 0.08);
        context.move_to(x, y1);
        context.line_to(x, y2);
        let _ = context.stroke();

        // Crisp core bar.
        context.set_line_width(bar_width);
        context.set_source_rgba(r, g, b, 0.95);
        context.move_to(x, y1);
        context.line_to(x, y2);
        let _ = context.stroke();
    }
}

/// Maps a position in `0.0..=1.0` to a hue sweep around the accent colour
/// (the default blue accent gives the original blue -> purple -> pink look).
fn spectrum_color(accent: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    const HUE_START: f64 = -20.0;
    const HUE_END: f64 = 75.0;

    let (hue, saturation, value) = accent;
    let t = t.clamp(0.0, 1.0);
    let shifted = (hue + HUE_START + (HUE_END - HUE_START) * t).rem_euclid(360.0);
    // Greys (e.g. a monochrome accent) stay grey instead of turning a hue.
    hsv_to_rgb((shifted, saturation, value))
}

fn rgb_to_hsv((r, g, b): (f64, f64, f64)) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta <= f64::EPSILON {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max <= f64::EPSILON {
        0.0
    } else {
        delta / max
    };
    (hue, saturation, max)
}

fn hsv_to_rgb((hue, saturation, value): (f64, f64, f64)) -> (f64, f64, f64) {
    let chroma = value * saturation;
    let sector = hue / 60.0;
    let x = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match sector as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    (r + m, g + m, b + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_round_trip() {
        for color in [
            (0.25, 0.49, 1.0),
            (1.0, 0.39, 0.78),
            (0.2, 0.8, 0.3),
            (0.5, 0.5, 0.5),
        ] {
            let (r, g, b) = hsv_to_rgb(rgb_to_hsv(color));
            assert!((r - color.0).abs() < 1e-9);
            assert!((g - color.1).abs() < 1e-9);
            assert!((b - color.2).abs() < 1e-9);
        }
    }

    #[test]
    fn idle_silent_state_stops_animating() {
        let state = State {
            active: Cell::new(false),
            ticking: Cell::new(true),
            last_frame_time: Cell::new(None),
            target_level: Cell::new(0.0),
            current_level: Cell::new(0.5),
            target_peaks: RefCell::new([0.0; WAVEFORM_BINS]),
            current_peaks: RefCell::new([0.8; WAVEFORM_BINS]),
        };
        let mut frames = 0;
        while state.advance(1.0) {
            frames += 1;
            assert!(frames < 200, "decay never settles");
        }
        assert!(state.current_peaks.borrow().iter().all(|peak| *peak == 0.0));
    }
}
