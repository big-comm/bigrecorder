use gtk::prelude::*;

const STYLE: &str = r#"
window,
.app-bg {
  background-color: @window_bg_color;
  color: @window_fg_color;
}

.flat-header,
.flat-toolbar,
headerbar,
windowhandle,
.titlebar {
  background-color: @window_bg_color;
  color: @window_fg_color;
  border: 0;
  box-shadow: none;
}

.app-title {
  font-weight: 800;
}

.app-subtitle {
  color: alpha(@window_fg_color, 0.55);
  font-size: 11px;
}

/* ---- Recordings page header ---- */

.page-heading {
  font-size: 19px;
  font-weight: 800;
  color: @window_fg_color;
}

.page-subheading {
  color: alpha(@window_fg_color, 0.55);
  font-size: 12px;
}

.search-field {
  border-radius: 999px;
  background-color: @card_bg_color;
  border: 1px solid alpha(@window_fg_color, 0.10);
  min-height: 34px;
  padding: 0 6px;
  color: @window_fg_color;
}

.search-field image {
  color: alpha(@window_fg_color, 0.55);
}

.view-toggle {
  min-width: 34px;
  min-height: 34px;
  padding: 0;
  border-radius: 9px;
  background-color: @card_bg_color;
  color: alpha(@window_fg_color, 0.70);
  border: 1px solid alpha(@window_fg_color, 0.10);
  -gtk-icon-size: 16px;
}

.view-toggle:hover {
  background-color: alpha(@accent_bg_color, 0.10);
}

.view-toggle:checked {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  border-color: @accent_bg_color;
}

/* ---- Recording surface (recording in progress) ---- */

.recorder-surface {
  background-color: @card_bg_color;
  color: @window_fg_color;
  border: 1px solid alpha(@window_fg_color, 0.08);
  border-radius: 18px;
  padding: 22px;
}

.timer {
  font-size: 52px;
  font-weight: 700;
  font-feature-settings: "tnum";
  color: @window_fg_color;
}

.timer-caption {
  color: alpha(@window_fg_color, 0.55);
  font-size: 13px;
}

.status-pill {
  background-color: alpha(@accent_bg_color, 0.14);
  color: @accent_color;
  border-radius: 999px;
  padding: 5px 16px 5px 12px;
  font-weight: 700;
}

.status-pill.recording {
  background-color: alpha(@accent_bg_color, 0.18);
  color: @accent_color;
}

.status-pill.paused {
  background-color: alpha(@window_fg_color, 0.10);
  color: alpha(@window_fg_color, 0.78);
}

.status-dot {
  min-width: 9px;
  min-height: 9px;
  border-radius: 999px;
  background-color: @accent_bg_color;
}

.status-pill.paused .status-dot {
  background-color: alpha(@window_fg_color, 0.55);
}

/* ---- Record FAB (start) ---- */

.record-fab {
  min-width: 72px;
  min-height: 72px;
  border-radius: 999px;
  background-color: @accent_bg_color;
  color: @accent_fg_color;
  border: 3px solid alpha(@window_bg_color, 0.9);
  box-shadow: 0 4px 22px alpha(@accent_bg_color, 0.55);
  -gtk-icon-size: 28px;
}

.record-fab:hover {
  background-color: shade(@accent_bg_color, 1.08);
}

.record-fab:active {
  background-color: shade(@accent_bg_color, 0.94);
}

.record-fab:disabled {
  background-color: alpha(@window_fg_color, 0.12);
  color: alpha(@window_fg_color, 0.40);
  box-shadow: none;
}

/* ---- Recording controls (pause / stop) ---- */

.control-pill {
  min-width: 132px;
  min-height: 52px;
  border-radius: 999px;
  background-color: alpha(@window_fg_color, 0.08);
  color: @window_fg_color;
  border: 0;
  padding: 0 22px;
  -gtk-icon-size: 18px;
}

.control-pill:hover {
  background-color: alpha(@window_fg_color, 0.14);
}

.control-pill.resume-pill {
  background-color: alpha(@accent_bg_color, 0.16);
  color: @accent_color;
}

.record-dot {
  min-width: 16px;
  min-height: 16px;
  border-radius: 999px;
  background-color: @accent_fg_color;
}

.stop-pill {
  min-width: 132px;
  min-height: 52px;
  border-radius: 999px;
  background-color: @destructive_bg_color;
  color: @destructive_fg_color;
  border: 0;
  padding: 0 22px;
  -gtk-icon-size: 16px;
}

.stop-pill:hover {
  background-color: shade(@destructive_bg_color, 1.08);
}

.control-label {
  font-size: 15px;
  font-weight: 700;
}

.waveform {
  background-color: @view_bg_color;
  border-radius: 14px;
}

/* ---- Folder location row ---- */

.folder-row {
  background-color: alpha(@window_fg_color, 0.05);
  border: 1px solid alpha(@window_fg_color, 0.08);
  border-radius: 12px;
  padding: 10px 14px;
  color: @window_fg_color;
}

.folder-row:hover {
  background-color: alpha(@window_fg_color, 0.09);
}

.folder-row image {
  color: @accent_color;
}

.folder-row .chevron {
  color: alpha(@window_fg_color, 0.45);
}

.folder-caption {
  color: alpha(@window_fg_color, 0.55);
  font-size: 11px;
}

.folder-path {
  color: @window_fg_color;
  font-size: 13px;
}

.section-title {
  font-weight: 800;
  color: @window_fg_color;
}

/* ---- Bottom action bar ---- */

.tools-footer {
  background-color: @window_bg_color;
  padding: 8px 16px 12px;
}

.action-bar {
  padding: 2px 2px;
}

.tools-panel {
  /* Opaque surface: a translucent card tint composited over the solid window
     background, so the list never shows through the floating panel. */
  background-color: @window_bg_color;
  background-image: linear-gradient(alpha(@window_fg_color, 0.06), alpha(@window_fg_color, 0.06));
  border: 1px solid alpha(@window_fg_color, 0.12);
  border-radius: 16px;
  padding: 14px;
  box-shadow: 0 -6px 24px alpha(black, 0.14);
}

.panel-subtitle {
  color: alpha(@window_fg_color, 0.55);
  font-size: 12px;
}

.tools-toggle {
  min-height: 48px;
  border-radius: 14px;
  padding: 0 18px;
  font-weight: 700;
  background-color: alpha(@window_fg_color, 0.08);
  color: @window_fg_color;
  border: 0;
}

.tools-toggle:hover {
  background-color: alpha(@window_fg_color, 0.14);
}

.tools-toggle:checked {
  background-color: alpha(@accent_bg_color, 0.16);
  color: @accent_color;
}

.ready-pill {
  min-height: 48px;
  border-radius: 14px;
  padding: 4px 14px;
  background-color: alpha(@window_fg_color, 0.06);
  border: 1px solid alpha(@window_fg_color, 0.08);
}

.ready-pill:hover {
  background-color: alpha(@window_fg_color, 0.10);
}

.ready-pill.input-unavailable {
  background-color: alpha(@destructive_color, 0.10);
  border-color: alpha(@destructive_color, 0.24);
}

.ready-pill.input-unavailable:hover {
  background-color: alpha(@destructive_color, 0.15);
}

.ready-title {
  color: @success_color;
  font-weight: 700;
  font-size: 13px;
}

.ready-pill.input-unavailable .ready-title {
  color: @destructive_color;
}

.ready-subtitle {
  color: alpha(@window_fg_color, 0.50);
  font-size: 11px;
}

.ready-pill.input-unavailable .ready-subtitle {
  color: alpha(@destructive_color, 0.82);
}

.ready-pill .chevron {
  color: alpha(@window_fg_color, 0.45);
}

.ready-pill.input-unavailable .chevron,
.input-warning-icon {
  color: @destructive_color;
}

.mic-list {
  min-width: 240px;
}

.mic-item {
  padding: 7px 10px;
  border-radius: 8px;
  font-weight: 600;
}

.mic-item image {
  color: @accent_color;
}

.muted {
  color: alpha(@window_fg_color, 0.55);
  font-size: 12px;
}

.empty-list {
  color: alpha(@window_fg_color, 0.55);
  padding: 36px 18px;
}

/* ---- Recording row card ---- */

.recording-row {
  background-color: @card_bg_color;
  color: @window_fg_color;
  border: 1px solid alpha(@window_fg_color, 0.07);
  border-radius: 14px;
  padding: 12px 14px;
}

.recording-row:hover {
  /* An fg-tinted overlay reads on both white (light) and translucent (dark)
     cards, unlike shade() which is invisible on pure white. */
  background-image: linear-gradient(alpha(@window_fg_color, 0.05), alpha(@window_fg_color, 0.05));
}

.recording-row.selected {
  background-color: alpha(@accent_bg_color, 0.10);
  border-color: alpha(@accent_bg_color, 0.55);
}

.row-icon {
  min-width: 44px;
  min-height: 44px;
  border-radius: 12px;
  background-color: alpha(@window_fg_color, 0.07);
  color: alpha(@window_fg_color, 0.85);
  -gtk-icon-size: 20px;
}

.recording-row.selected .row-icon {
  background-color: alpha(@accent_bg_color, 0.20);
  color: @accent_color;
}

.recording-title {
  color: @window_fg_color;
  font-size: 13px;
  font-weight: 700;
}

.recording-subtitle,
.recording-meta {
  color: alpha(@window_fg_color, 0.55);
  font-size: 11px;
}

.recording-meta {
  font-feature-settings: "tnum";
}

.recording-meta image {
  color: alpha(@window_fg_color, 0.45);
  -gtk-icon-size: 13px;
}

.row-play-button,
.row-delete-button,
.row-menu-button {
  min-width: 32px;
  min-height: 32px;
  padding: 0;
  border-radius: 9px;
  border: 0;
  -gtk-icon-size: 15px;
}

.row-play-button {
  background-color: @accent_bg_color;
  color: @accent_fg_color;
}

.row-play-button:hover {
  background-color: shade(@accent_bg_color, 1.08);
}

.row-delete-button {
  background-color: alpha(@destructive_color, 0.12);
  color: @destructive_color;
}

.row-delete-button:hover {
  background-color: alpha(@destructive_color, 0.20);
}

.row-menu-button {
  background-color: transparent;
  color: alpha(@window_fg_color, 0.55);
}

.row-menu-button:hover {
  background-color: alpha(@window_fg_color, 0.10);
}

/* ---- Grid view ---- */

.recordings-flow.grid-view .recording-row {
  padding: 16px;
}

.playback-progress {
  min-height: 14px;
}

/* ---- Tools grid (cards with icon + title + description) ---- */

.tool-card {
  min-height: 50px;
  border-radius: 11px;
  background-color: alpha(@window_fg_color, 0.05);
  color: @window_fg_color;
  border: 1px solid alpha(@window_fg_color, 0.07);
  padding: 7px 9px;
}

.tool-card:hover {
  background-color: alpha(@accent_bg_color, 0.12);
  border-color: alpha(@accent_bg_color, 0.30);
}

.tool-card-icon {
  min-width: 36px;
  min-height: 36px;
  border-radius: 9px;
  background-color: alpha(@accent_bg_color, 0.16);
  color: @accent_color;
  -gtk-icon-size: 16px;
}

.tool-label {
  font-size: 12px;
  font-weight: 700;
}

.tool-desc {
  color: alpha(@window_fg_color, 0.55);
  font-size: 10px;
}

.panel-action {
  min-height: 28px;
  padding: 2px 12px;
  border-radius: 8px;
  background-color: alpha(@window_fg_color, 0.07);
  color: @window_fg_color;
  border: 0;
  font-size: 12px;
  font-weight: 600;
}

.panel-action:hover {
  background-color: alpha(@window_fg_color, 0.12);
}

.panel-action:disabled {
  color: alpha(@window_fg_color, 0.32);
  background-color: alpha(@window_fg_color, 0.04);
}

/* ---- Trim dialog steppers ---- */

.stepper-row {
  background-color: alpha(@window_fg_color, 0.05);
  border: 1px solid alpha(@window_fg_color, 0.08);
  border-radius: 12px;
  padding: 10px 14px;
}

.stepper-title {
  font-size: 14px;
  font-weight: 700;
}

.stepper-desc {
  color: alpha(@window_fg_color, 0.55);
  font-size: 11px;
}

.stepper-icon {
  color: @accent_color;
  -gtk-icon-size: 16px;
}

.selected-duration {
  font-size: 22px;
  font-weight: 800;
  font-feature-settings: "tnum";
  color: @accent_color;
}

.selected-duration-caption {
  color: alpha(@window_fg_color, 0.55);
  font-size: 11px;
}

.range-readout {
  color: @accent_color;
  font-weight: 700;
  font-feature-settings: "tnum";
}

.message-content {
  min-width: 240px;
  margin-top: 4px;
}

.message-icon {
  margin-bottom: 2px;
}

.message-icon.info {
  color: @accent_color;
}

.message-icon.success {
  color: @success_color;
}

.message-icon.error {
  color: @error_color;
}

.message-body {
  color: @window_fg_color;
}

.tool-dialog {
  margin-top: 12px;
  color: @window_fg_color;
}

.tool-dialog spinbutton,
.tool-dialog dropdown,
.tool-dialog combobox {
  min-width: 150px;
}

.tool-illustration {
  min-width: 260px;
  min-height: 96px;
  border: 1px solid @borders;
  border-radius: 8px;
  background-color: @view_bg_color;
}

.horizontal-value-control {
  min-width: 280px;
}

.horizontal-value-label {
  color: @window_fg_color;
  font-weight: 700;
  font-feature-settings: "tnum";
}

.horizontal-tool-scale {
  min-width: 220px;
}

.horizontal-tool-scale trough {
  min-height: 6px;
  border-radius: 999px;
  background-color: alpha(@window_fg_color, 0.18);
}

.horizontal-tool-scale highlight {
  border-radius: 999px;
  background-color: @accent_bg_color;
}

.horizontal-tool-scale slider {
  min-width: 20px;
  min-height: 20px;
  border-radius: 999px;
  background-color: @window_bg_color;
  border: 1px solid alpha(@window_fg_color, 0.32);
}

.equalizer-curve {
  min-width: 360px;
  min-height: 96px;
  border: 1px solid @borders;
  border-radius: 8px;
  background-color: @view_bg_color;
}

.equalizer-band-box {
  padding-top: 2px;
}

.equalizer-band-scale trough {
  min-width: 6px;
  border-radius: 999px;
  background-color: alpha(@window_fg_color, 0.18);
}

.equalizer-band-scale highlight {
  border-radius: 999px;
  background-color: @accent_bg_color;
}

.equalizer-band-scale slider {
  min-width: 18px;
  min-height: 18px;
  border-radius: 999px;
  background-color: @window_bg_color;
  border: 1px solid alpha(@window_fg_color, 0.32);
}

.equalizer-value-label,
.equalizer-band-label {
  color: alpha(@window_fg_color, 0.72);
  font-size: 10px;
  font-feature-settings: "tnum";
}
"#;

/// libadwaita named colors that [`theme_color`] can resolve for custom drawing.
const THEME_COLORS: &[&str] = &[
    "accent_color",
    "accent_bg_color",
    "accent_fg_color",
    "destructive_color",
    "destructive_bg_color",
    "destructive_fg_color",
    "success_color",
    "success_bg_color",
    "success_fg_color",
    "warning_color",
    "warning_bg_color",
    "warning_fg_color",
    "error_color",
    "error_bg_color",
    "error_fg_color",
    "window_bg_color",
    "window_fg_color",
    "view_bg_color",
    "view_fg_color",
    "card_bg_color",
    "card_fg_color",
    "card_shade_color",
    "headerbar_bg_color",
    "headerbar_fg_color",
    "popover_bg_color",
    "popover_fg_color",
    "dialog_bg_color",
    "dialog_fg_color",
    "shade_color",
    "borders",
];

const PROBE_CLASS_PREFIX: &str = "theme-probe-";

thread_local! {
    /// Unparented widget used to resolve named colors through the CSS
    /// cascade, replacing the deprecated `StyleContext::lookup_color`.
    static THEME_PROBE: gtk::Box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
}

/// Resolves a libadwaita named color (e.g. `"accent_color"`) for the current
/// light/dark variant and accent. Translucent colors (such as `card_bg_color`
/// in dark mode) are composited over `window_bg_color`, so the result is the
/// colour actually seen on screen. Unknown names return `fallback`.
pub fn theme_color(
    widget: &impl IsA<gtk::Widget>,
    name: &str,
    fallback: (f64, f64, f64),
) -> (f64, f64, f64) {
    let Some(color) = theme_rgba(widget, name) else {
        return fallback;
    };
    let rgb = rgba_tuple(&color);
    let alpha = f64::from(color.alpha());
    if alpha >= 0.999 || name == "window_bg_color" {
        return rgb;
    }

    let base = theme_rgba(widget, "window_bg_color")
        .map(|base| rgba_tuple(&base))
        .unwrap_or(fallback);
    (
        rgb.0 * alpha + base.0 * (1.0 - alpha),
        rgb.1 * alpha + base.1 * (1.0 - alpha),
        rgb.2 * alpha + base.2 * (1.0 - alpha),
    )
}

/// Like [`theme_color`], but returns the raw color including its alpha.
pub fn theme_rgba(widget: &impl IsA<gtk::Widget>, name: &str) -> Option<gtk::gdk::RGBA> {
    if !THEME_COLORS.contains(&name) {
        return None;
    }

    let class = format!("{PROBE_CLASS_PREFIX}{name}");
    THEME_PROBE.with(|probe| {
        if probe.display() != widget.as_ref().display() {
            return None;
        }
        // Toggling the class invalidates the probe's style, so `color()`
        // recomputes it from the current stylesheet (dark/light, accent).
        probe.add_css_class(&class);
        let color = probe.color();
        probe.remove_css_class(&class);
        Some(color)
    })
}

fn rgba_tuple(color: &gtk::gdk::RGBA) -> (f64, f64, f64) {
    (
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
    )
}

fn probe_css() -> String {
    THEME_COLORS
        .iter()
        .map(|name| format!(".{PROBE_CLASS_PREFIX}{name} {{ color: @{name}; }}\n"))
        .collect()
}

pub fn load() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&format!("{STYLE}\n{}", probe_css()));

    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(local_icon_dir());
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn local_icon_dir() -> std::path::PathBuf {
    let local_dir = std::path::PathBuf::from("usr/share/icons");
    if local_dir.exists() {
        return local_dir;
    }

    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        for directory in parent.ancestors() {
            let candidate = directory.join("usr/share/icons");
            if candidate.exists() {
                return candidate;
            }
        }
    }

    std::path::PathBuf::from("/usr/share/icons")
}
