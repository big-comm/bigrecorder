use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::Context;
use gtk::gio;
use gtk::glib;

use crate::audio::player::{PlaybackStatus, Player};
use crate::audio::processing::{
    self, EQUALIZER_BANDS, ExportChannelMode, ExportFormat, ExportOptions, ExportQuality,
    ExportSampleRate, VoiceEffectPreset,
};
use crate::audio::recorder::{Recorder, default_input_is_muted, input_devices, saved_recordings};
use crate::i18n::{format_message, gettext, ngettext};
use crate::ui::playback_bar::PlaybackBar;
use crate::ui::waveform::Waveform;

#[derive(Clone, Copy, Eq, PartialEq)]
enum RecordingMode {
    Idle,
    Recording,
    Paused,
}

struct Session {
    mode: RecordingMode,
    started_at: Option<Instant>,
    accumulated: Duration,
    current_path: Option<PathBuf>,
}

#[derive(Clone)]
struct RecorderWidgets {
    window: adw::ApplicationWindow,
    page_stack: gtk::Stack,
    start_button: gtk::Button,
    ready_pill: gtk::Widget,
    input_controls: InputControls,
    record_button: gtk::Button,
    record_symbol: gtk::Stack,
    record_label: gtk::Label,
    stop_button: gtk::Button,
    status_label: gtk::Label,
    timer_label: gtk::Label,
    waveform: Waveform,
}

#[derive(Clone)]
struct RecordingRow {
    child: gtk::FlowBoxChild,
    row_widget: gtk::Box,
    path: PathBuf,
    // Shared with the row's callbacks so a rename updates them in place.
    path_cell: Rc<RefCell<PathBuf>>,
    title_text: String,
    title_label: gtk::Label,
    playback_symbol: gtk::Stack,
    playback_bar: PlaybackBar,
    waveform_holder: gtk::Widget,
    subtitle: gtk::Label,
    meta_box: gtk::Widget,
}

#[derive(Clone)]
struct ModeControls {
    page_stack: gtk::Stack,
    start_button: gtk::Button,
    ready_pill: gtk::Widget,
    input_ready: Rc<Cell<bool>>,
    record_button: gtk::Button,
    record_symbol: gtk::Stack,
    record_label: gtk::Label,
    stop_button: gtk::Button,
    status_label: gtk::Label,
}

#[derive(Clone)]
struct InputControls {
    ready_pill: gtk::Button,
    icon_stack: gtk::Stack,
    ready_title: gtk::Label,
    ready_subtitle: gtk::Label,
    start_button: gtk::Button,
    selected_mic: Rc<RefCell<Option<String>>>,
    input_ready: Rc<Cell<bool>>,
    block_reason: Rc<Cell<InputBlockReason>>,
    // Device probing runs off the main thread; only the newest result wins.
    refresh_generation: Rc<Cell<u64>>,
    refresh_in_flight: Rc<Cell<bool>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InputBlockReason {
    None,
    NoDevice,
    Muted,
    SelectedUnavailable,
}

#[derive(Clone, Copy)]
enum ToolAction {
    Trim,
    Volume,
    Speed,
    Pitch,
    VoiceEffects,
    Equalizer,
    Reverse,
    Merge,
    Export,
}

#[derive(Clone, Copy)]
enum ToolIllustration {
    Trim,
    Volume,
    Speed,
    Pitch,
    VoiceEffects,
    Reverse,
    Merge,
    Export,
}

#[derive(Clone)]
struct ToolContext {
    window: adw::ApplicationWindow,
    recordings_flow: gtk::FlowBox,
    empty_recordings_label: gtk::Label,
    count_label: gtk::Label,
    recording_rows: RecordingRows,
    selected_recording: SelectedRecording,
    player: Rc<RefCell<Player>>,
    playback_ticker: Rc<RefCell<Option<glib::SourceId>>>,
    // Set while a processing/export task runs, to refuse duplicate work.
    busy: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct RecordingControls {
    recorder: Rc<RefCell<Recorder>>,
    session: Rc<RefCell<Session>>,
    timer_label: gtk::Label,
    mode_controls: ModeControls,
    tool_context: ToolContext,
}

type RecordingRows = Rc<RefCell<Vec<RecordingRow>>>;
type SelectedRecording = Rc<RefCell<Option<PathBuf>>>;

impl Session {
    fn new() -> Self {
        Self {
            mode: RecordingMode::Idle,
            started_at: None,
            accumulated: Duration::ZERO,
            current_path: None,
        }
    }

    fn elapsed(&self) -> Duration {
        match self.started_at {
            Some(started_at) => self.accumulated + started_at.elapsed(),
            None => self.accumulated,
        }
    }

    fn start(&mut self, path: PathBuf) {
        self.mode = RecordingMode::Recording;
        self.started_at = Some(Instant::now());
        self.accumulated = Duration::ZERO;
        self.current_path = Some(path);
    }

    fn pause(&mut self) {
        self.accumulated = self.elapsed();
        self.started_at = None;
        self.mode = RecordingMode::Paused;
    }

    fn resume(&mut self) {
        self.started_at = Some(Instant::now());
        self.mode = RecordingMode::Recording;
    }

    fn stop(&mut self) {
        self.accumulated = self.elapsed();
        self.started_at = None;
        self.mode = RecordingMode::Idle;
    }
}

pub fn build(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Big Recorder")
        .default_width(680)
        .default_height(720)
        .resizable(false)
        .build();
    window.add_css_class("bigrecorder");

    let recorder = Rc::new(RefCell::new(Recorder::new()));
    let player = Rc::new(RefCell::new(Player::new()));
    let session = Rc::new(RefCell::new(Session::new()));
    let recording_rows: RecordingRows = Rc::new(RefCell::new(Vec::new()));
    let selected_recording: SelectedRecording = Rc::new(RefCell::new(None));
    let search_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    let header = adw::HeaderBar::new();
    header.add_css_class("flat-header");
    let title = adw::WindowTitle::new("Big Recorder", &gettext("Local voice recorder"));
    header.set_title_widget(Some(&title));

    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text(gettext("Menu"))
        .build();
    let menu = gtk::gio::Menu::new();
    menu.append(Some(&gettext("About")), Some("win.about"));
    menu.append(Some(&gettext("Close")), Some("win.close"));
    menu_button.set_menu_model(Some(&menu));
    header.pack_end(&menu_button);
    install_window_actions(&window);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_css_class("flat-toolbar");
    toolbar.add_top_bar(&header);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("app-bg");

    let page_stack = gtk::Stack::new();
    page_stack.set_vexpand(true);
    page_stack.set_transition_type(gtk::StackTransitionType::Crossfade);

    // ---- Recordings page ----
    let recordings_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let recordings_clamp = adw::Clamp::builder()
        .maximum_size(660)
        .tightening_threshold(660)
        .build();
    recordings_clamp.set_margin_top(14);
    recordings_clamp.set_margin_bottom(14);
    recordings_clamp.set_margin_start(16);
    recordings_clamp.set_margin_end(16);

    let recordings_content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    recordings_content.set_hexpand(true);

    // Header row: heading + count on the left, search + view toggles on the right.
    let header_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header_row.set_hexpand(true);

    let heading_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
    heading_box.set_hexpand(true);
    heading_box.set_valign(gtk::Align::Center);

    let heading = gtk::Label::new(Some(&gettext("Recordings")));
    heading.add_css_class("page-heading");
    heading.set_halign(gtk::Align::Start);

    let count_label = gtk::Label::new(None);
    count_label.add_css_class("page-subheading");
    count_label.set_halign(gtk::Align::Start);

    heading_box.append(&heading);
    heading_box.append(&count_label);

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_placeholder_text(Some(&gettext("Search recordings")));
    search_entry.add_css_class("search-field");
    search_entry.set_valign(gtk::Align::Center);
    search_entry.set_width_chars(16);

    let list_toggle = gtk::ToggleButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text(gettext("List view"))
        .valign(gtk::Align::Center)
        .active(true)
        .build();
    list_toggle.add_css_class("view-toggle");

    let grid_toggle = gtk::ToggleButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text(gettext("Grid view"))
        .valign(gtk::Align::Center)
        .group(&list_toggle)
        .build();
    grid_toggle.add_css_class("view-toggle");

    let toggle_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toggle_box.set_valign(gtk::Align::Center);
    toggle_box.append(&list_toggle);
    toggle_box.append(&grid_toggle);

    header_row.append(&heading_box);
    header_row.append(&search_entry);
    header_row.append(&toggle_box);

    let recordings_flow = gtk::FlowBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(1)
        .max_children_per_line(1)
        .row_spacing(10)
        .column_spacing(10)
        .homogeneous(false)
        .hexpand(true)
        // Anchor at the top with natural height: a FlowBox stretches its
        // lines to fill whatever height it is allocated, which made each
        // card grow to split the whole page when only a few recordings
        // existed. Rows must stay compact and stack from the top.
        .valign(gtk::Align::Start)
        .build();
    recordings_flow.add_css_class("recordings-flow");
    recordings_flow.add_css_class("list-view");

    let empty_recordings_label = gtk::Label::new(Some(&gettext("No recordings yet")));
    empty_recordings_label.add_css_class("empty-list");
    empty_recordings_label.set_halign(gtk::Align::Center);
    empty_recordings_label.set_valign(gtk::Align::Center);
    empty_recordings_label.set_vexpand(true);

    recordings_content.append(&header_row);
    recordings_content.append(&recordings_flow);
    recordings_content.append(&empty_recordings_label);
    recordings_clamp.set_child(Some(&recordings_content));
    recordings_scroll.set_child(Some(&recordings_clamp));

    // ---- Recording page ----
    let recording_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    recording_page.set_vexpand(true);

    let recording_clamp = adw::Clamp::builder()
        .maximum_size(660)
        .tightening_threshold(660)
        .build();
    recording_clamp.set_margin_top(16);
    recording_clamp.set_margin_bottom(16);
    recording_clamp.set_margin_start(16);
    recording_clamp.set_margin_end(16);
    recording_clamp.set_valign(gtk::Align::Center);

    let recorder_surface = gtk::Box::new(gtk::Orientation::Vertical, 18);
    recorder_surface.add_css_class("recorder-surface");

    let status_label = gtk::Label::new(Some(&gettext("Ready")));
    let status_row = status_pill(&status_label);

    let waveform = Waveform::new();

    let timer_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    timer_box.set_halign(gtk::Align::Center);

    let timer_label = gtk::Label::new(Some("00:00:00"));
    timer_label.add_css_class("timer");
    timer_label.set_halign(gtk::Align::Center);

    let timer_caption = gtk::Label::new(Some(&gettext("Recording time")));
    timer_caption.add_css_class("timer-caption");
    timer_caption.set_halign(gtk::Align::Center);

    timer_box.append(&timer_label);
    timer_box.append(&timer_caption);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    controls.set_halign(gtk::Align::Center);

    let record_symbol = gtk::Stack::new();
    record_symbol.set_halign(gtk::Align::Center);
    record_symbol.set_valign(gtk::Align::Center);
    record_symbol.set_size_request(20, 20);

    let pause_icon = gtk::Image::from_icon_name("media-playback-pause-symbolic");
    pause_icon.set_pixel_size(18);

    let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
    play_icon.set_pixel_size(18);

    record_symbol.add_named(&pause_icon, Some("pause"));
    record_symbol.add_named(&play_icon, Some("play"));
    record_symbol.set_visible_child_name("pause");

    let record_label = gtk::Label::new(Some(&gettext("Pause")));
    record_label.add_css_class("control-label");

    let record_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    record_content.set_halign(gtk::Align::Center);
    record_content.set_valign(gtk::Align::Center);
    record_content.append(&record_symbol);
    record_content.append(&record_label);

    let record_button = gtk::Button::builder()
        .tooltip_text(gettext("Pause"))
        .build();
    record_button.set_child(Some(&record_content));
    record_button.add_css_class("control-pill");

    let stop_icon = gtk::Image::from_icon_name("media-playback-stop-symbolic");
    stop_icon.set_pixel_size(16);

    let stop_label = gtk::Label::new(Some(&gettext("Stop")));
    stop_label.add_css_class("control-label");

    let stop_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    stop_content.set_halign(gtk::Align::Center);
    stop_content.set_valign(gtk::Align::Center);
    stop_content.append(&stop_icon);
    stop_content.append(&stop_label);

    let stop_button = gtk::Button::builder()
        .tooltip_text(gettext("Stop"))
        .sensitive(false)
        .build();
    stop_button.set_child(Some(&stop_content));
    stop_button.add_css_class("stop-pill");

    controls.append(&record_button);
    controls.append(&stop_button);

    let folder_row = folder_location_row(&window);

    recorder_surface.append(&status_row);
    recorder_surface.append(waveform.widget());
    recorder_surface.append(&timer_box);
    recorder_surface.append(&controls);
    recorder_surface.append(&folder_row);

    recording_clamp.set_child(Some(&recorder_surface));
    recording_page.append(&recording_clamp);

    page_stack.add_named(&recordings_scroll, Some("recordings"));
    page_stack.add_named(&recording_page, Some("recording"));
    page_stack.set_visible_child_name("recordings");

    // ---- Bottom area: tools panel + action bar ----
    let tools_footer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tools_footer.add_css_class("tools-footer");

    let tools_revealer = gtk::Revealer::new();
    tools_revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    tools_revealer.set_transition_duration(180);
    tools_revealer.set_reveal_child(false);
    // The panel floats over the list (it does not push the layout or resize
    // the window): it is added to an overlay, anchored above the action bar.
    tools_revealer.set_valign(gtk::Align::End);
    tools_revealer.set_halign(gtk::Align::Fill);
    tools_revealer.set_margin_start(16);
    tools_revealer.set_margin_end(16);
    tools_revealer.set_margin_bottom(80);

    let tools_panel = gtk::Box::new(gtk::Orientation::Vertical, 12);
    tools_panel.add_css_class("tools-panel");

    let tools_header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let tools_header_text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    tools_header_text.set_hexpand(true);
    tools_header_text.set_valign(gtk::Align::Center);
    let tools_title = gtk::Label::new(Some(&gettext("Tools")));
    tools_title.add_css_class("section-title");
    tools_title.set_halign(gtk::Align::Start);
    let tools_subtitle = gtk::Label::new(Some(&gettext("Apply effects to the selected recording")));
    tools_subtitle.add_css_class("panel-subtitle");
    tools_subtitle.set_halign(gtk::Align::Start);
    tools_header_text.append(&tools_title);
    tools_header_text.append(&tools_subtitle);

    let restore_button = gtk::Button::with_label(&gettext("Restore"));
    restore_button.add_css_class("panel-action");
    restore_button.set_valign(gtk::Align::Center);
    restore_button.set_sensitive(false);
    restore_button.set_tooltip_text(Some(&gettext("Restore the original recording")));
    let undo_button = gtk::Button::with_label(&gettext("Undo"));
    undo_button.add_css_class("panel-action");
    undo_button.set_valign(gtk::Align::Center);
    undo_button.set_sensitive(false);

    tools_header.append(&tools_header_text);
    tools_header.append(&restore_button);
    tools_header.append(&undo_button);

    let tools_grid = gtk::Grid::builder()
        .column_spacing(10)
        .row_spacing(10)
        .hexpand(true)
        .column_homogeneous(true)
        .build();

    let tools = [
        (
            "edit-cut-symbolic",
            gettext("Trim"),
            gettext("Remove part of the audio"),
            ToolAction::Trim,
        ),
        (
            "audio-volume-high-symbolic",
            gettext("Volume"),
            gettext("Adjust the loudness"),
            ToolAction::Volume,
        ),
        (
            "media-seek-forward-symbolic",
            gettext("Speed"),
            gettext("Speed up or slow down"),
            ToolAction::Speed,
        ),
        (
            "audio-x-generic-symbolic",
            gettext("Pitch"),
            gettext("Change the pitch"),
            ToolAction::Pitch,
        ),
        (
            "applications-multimedia-symbolic",
            gettext("Effects"),
            gettext("Apply a voice effect"),
            ToolAction::VoiceEffects,
        ),
        (
            "view-more-symbolic",
            gettext("Equalizer"),
            gettext("Tune frequency bands"),
            ToolAction::Equalizer,
        ),
        (
            "object-flip-horizontal-symbolic",
            gettext("Reverse"),
            gettext("Play the audio backwards"),
            ToolAction::Reverse,
        ),
        (
            "list-add-symbolic",
            gettext("Merge"),
            gettext("Join with another recording"),
            ToolAction::Merge,
        ),
        (
            "document-save-symbolic",
            gettext("Export"),
            gettext("Save in another format"),
            ToolAction::Export,
        ),
    ];

    let tool_context = ToolContext {
        window: window.clone(),
        recordings_flow: recordings_flow.clone(),
        empty_recordings_label: empty_recordings_label.clone(),
        count_label: count_label.clone(),
        recording_rows: Rc::clone(&recording_rows),
        selected_recording: Rc::clone(&selected_recording),
        player: Rc::clone(&player),
        playback_ticker: Rc::new(RefCell::new(None)),
        busy: Rc::new(Cell::new(false)),
    };

    let tools_toggle = gtk::ToggleButton::builder()
        .tooltip_text(gettext("Tools"))
        .build();
    tools_toggle.add_css_class("tools-toggle");
    tools_toggle.set_child(Some(&pill_content(
        "emblem-system-symbolic",
        &gettext("Tools"),
    )));

    let tools_revealer_clone = tools_revealer.clone();
    tools_toggle.connect_toggled(move |button| {
        tools_revealer_clone.set_reveal_child(button.is_active());
    });

    for (index, (icon, label, desc, action)) in tools.iter().enumerate() {
        let button = tool_card(icon, label, desc);
        button.set_tooltip_text(Some(&tool_tooltip(*action)));
        let tool_context = tool_context.clone();
        let tools_toggle_clone = tools_toggle.clone();
        let action = *action;
        button.connect_clicked(move |_| {
            run_tool(action, &tool_context);
            tools_toggle_clone.set_active(false);
        });
        tools_grid.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
    }

    tools_panel.append(&tools_header);
    tools_panel.append(&tools_grid);
    tools_revealer.set_child(Some(&tools_panel));

    // Start (record) FAB — raised above the action bar (added to the overlay).
    let start_icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
    start_icon.set_pixel_size(28);
    let start_button = gtk::Button::builder()
        .tooltip_text(gettext("Record"))
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .margin_bottom(12)
        .build();
    start_button.set_child(Some(&start_icon));
    start_button.add_css_class("record-fab");

    let selected_mic: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let input_ready = Rc::new(Cell::new(false));
    let input_block_reason = Rc::new(Cell::new(InputBlockReason::NoDevice));

    // "Ready to record" device pill — opens a microphone picker popover.
    let ready_pill = gtk::Button::new();
    ready_pill.add_css_class("ready-pill");
    ready_pill.set_valign(gtk::Align::Center);
    ready_pill.set_tooltip_text(Some(&gettext("Choose input device")));
    let ready_content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let ready_icon_stack = gtk::Stack::new();
    ready_icon_stack.set_size_request(20, 20);
    ready_icon_stack.set_valign(gtk::Align::Center);
    let ready_level_icon = level_icon();
    ready_icon_stack.add_named(&ready_level_icon, Some("ready"));
    let warning_icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    warning_icon.set_pixel_size(18);
    warning_icon.add_css_class("input-warning-icon");
    ready_icon_stack.add_named(&warning_icon, Some("unavailable"));
    ready_icon_stack.set_visible_child_name("ready");
    ready_content.append(&ready_icon_stack);
    let ready_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    ready_text.set_valign(gtk::Align::Center);
    let ready_title = gtk::Label::new(Some(&gettext("Ready to record")));
    ready_title.add_css_class("ready-title");
    ready_title.set_xalign(0.0);
    let ready_subtitle = gtk::Label::new(Some(&gettext("Default microphone")));
    ready_subtitle.add_css_class("ready-subtitle");
    ready_subtitle.set_xalign(0.0);
    ready_subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    ready_subtitle.set_max_width_chars(22);
    ready_text.append(&ready_title);
    ready_text.append(&ready_subtitle);
    ready_content.append(&ready_text);
    let ready_chevron = gtk::Image::from_icon_name("go-next-symbolic");
    ready_chevron.set_pixel_size(15);
    ready_chevron.add_css_class("chevron");
    ready_content.append(&ready_chevron);
    ready_pill.set_child(Some(&ready_content));

    let input_controls = InputControls {
        ready_pill: ready_pill.clone(),
        icon_stack: ready_icon_stack.clone(),
        ready_title: ready_title.clone(),
        ready_subtitle: ready_subtitle.clone(),
        start_button: start_button.clone(),
        selected_mic: Rc::clone(&selected_mic),
        input_ready: Rc::clone(&input_ready),
        block_reason: Rc::clone(&input_block_reason),
        refresh_generation: Rc::new(Cell::new(0)),
        refresh_in_flight: Rc::new(Cell::new(false)),
    };

    let mic_popover = gtk::Popover::new();
    mic_popover.add_css_class("mic-popover");
    mic_popover.set_position(gtk::PositionType::Top);
    mic_popover.set_parent(&ready_pill);
    let parented_popover = mic_popover.clone();
    ready_pill.connect_destroy(move |_| parented_popover.unparent());

    // The picker is rebuilt every time it opens so freshly plugged-in
    // microphones show up. The choice applies to the next recording.
    let mic_recorder = Rc::clone(&recorder);
    let picker_controls = input_controls.clone();
    let popup_popover = mic_popover.clone();
    ready_pill.connect_clicked(move |_| {
        // Enumerating devices can stall on a busy sound server, so do it off
        // the main thread and open the picker once the list is known.
        let handle = gio::spawn_blocking(input_devices);
        let mic_recorder = Rc::clone(&mic_recorder);
        let picker_controls = picker_controls.clone();
        let popup_popover = popup_popover.clone();
        glib::MainContext::default().spawn_local(async move {
            let devices = handle.await.unwrap_or_default();
            let list = build_mic_list(&devices, &mic_recorder, &picker_controls, &popup_popover);
            popup_popover.set_child(Some(&list));
            popup_popover.popup();
        });
    });

    let action_bar = gtk::CenterBox::new();
    action_bar.add_css_class("action-bar");
    action_bar.set_start_widget(Some(&tools_toggle));
    action_bar.set_end_widget(Some(&ready_pill));

    tools_footer.append(&action_bar);
    root.append(&page_stack);
    root.append(&tools_footer);

    // The list scroll must not push the window wider (keeps width constant when
    // switching between list and grid views).
    recordings_scroll.set_propagate_natural_width(false);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&root));
    overlay.add_overlay(&tools_revealer);
    overlay.add_overlay(&start_button);
    toolbar.set_content(Some(&overlay));
    window.set_content(Some(&toolbar));

    // Functional search.
    let search_filter_rows = Rc::clone(&recording_rows);
    let search_text_clone = Rc::clone(&search_text);
    let flow_for_filter = recordings_flow.clone();
    flow_for_filter.set_filter_func(move |child| {
        let query = search_text_clone.borrow().to_lowercase();
        if query.is_empty() {
            return true;
        }
        search_filter_rows
            .borrow()
            .iter()
            .find(|row| row.child == *child)
            .map(|row| row.title_text.to_lowercase().contains(&query))
            .unwrap_or(true)
    });

    let flow_for_search = recordings_flow.clone();
    let search_text_clone = Rc::clone(&search_text);
    search_entry.connect_search_changed(move |entry| {
        search_text_clone.replace(entry.text().to_string());
        flow_for_search.invalidate_filter();
    });

    // Functional list / grid view toggle.
    let flow_for_view = recordings_flow.clone();
    let view_rows = Rc::clone(&recording_rows);
    grid_toggle.connect_toggled(move |button| {
        apply_view_mode(&flow_for_view, &view_rows, button.is_active());
    });

    load_saved_recordings(&tool_context);
    refresh_input_controls(&input_controls, true);

    connect_controls(
        RecorderWidgets {
            window: window.clone(),
            page_stack,
            start_button,
            ready_pill: ready_pill.upcast::<gtk::Widget>(),
            input_controls,
            record_button,
            record_symbol,
            record_label,
            stop_button,
            status_label,
            timer_label,
            waveform,
        },
        recorder,
        player,
        session,
        recording_rows,
        tool_context.clone(),
    );

    window.present();
}

fn level_icon() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(18)
        .content_height(18)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(|widget, cr, width, height| {
        let green = css_color(widget, "success_color", (0.18, 0.76, 0.45));
        cr.set_source_rgb(green.0, green.1, green.2);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_width(2.4);
        let w = f64::from(width);
        let h = f64::from(height);
        let center_y = h / 2.0;
        let heights = [0.45, 0.85, 0.6, 1.0, 0.5];
        let count = heights.len();
        let step = w / (count as f64 + 1.0);
        for (index, factor) in heights.iter().enumerate() {
            let x = step * (index as f64 + 1.0);
            let bar = (h - 4.0) * factor;
            cr.move_to(x, center_y - bar / 2.0);
            cr.line_to(x, center_y + bar / 2.0);
        }
        let _ = cr.stroke();
    });
    area
}

fn build_mic_list(
    devices: &[(String, gst::Device)],
    recorder: &Rc<RefCell<Recorder>>,
    controls: &InputControls,
    popover: &gtk::Popover,
) -> gtk::Box {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("mic-list");

    if devices.is_empty() {
        let empty_label = gtk::Label::new(Some(&gettext("No input devices found")));
        empty_label.add_css_class("muted");
        empty_label.set_margin_top(8);
        empty_label.set_margin_bottom(8);
        empty_label.set_margin_start(10);
        empty_label.set_margin_end(10);
        list.append(&empty_label);
        return list;
    }

    let default_item = mic_menu_item(
        &gettext("Default microphone"),
        controls.selected_mic.borrow().is_none(),
    );
    let default_recorder = Rc::clone(recorder);
    let default_controls = controls.clone();
    let default_popover = popover.clone();
    default_item.connect_clicked(move |_| {
        default_controls.selected_mic.replace(None);
        default_recorder.borrow_mut().set_device(None);
        refresh_input_controls(&default_controls, true);
        default_popover.popdown();
    });
    list.append(&default_item);

    for (name, device) in devices {
        let checked = controls.selected_mic.borrow().as_deref() == Some(name.as_str());
        let item = mic_menu_item(name, checked);
        let item_recorder = Rc::clone(recorder);
        let item_controls = controls.clone();
        let item_popover = popover.clone();
        let name = name.clone();
        let device = device.clone();
        item.connect_clicked(move |_| {
            item_controls.selected_mic.replace(Some(name.clone()));
            item_recorder.borrow_mut().set_device(Some(device.clone()));
            refresh_input_controls(&item_controls, true);
            item_popover.popdown();
        });
        list.append(&item);
    }

    list
}

fn mic_menu_item(label: &str, checked: bool) -> gtk::Button {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);

    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    text.set_ellipsize(gtk::pango::EllipsizeMode::End);
    text.set_max_width_chars(28);

    let check = gtk::Image::from_icon_name("object-select-symbolic");
    check.set_pixel_size(14);
    check.set_visible(checked);

    row.append(&text);
    row.append(&check);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("mic-item");
    button.set_child(Some(&row));
    button
}

enum InputStatus {
    Ready(String),
    NoDevice,
    Muted,
    SelectedUnavailable,
}

/// Probes the input devices (and the default source's mute state) on a
/// worker thread, then updates the pill. Periodic refreshes are skipped while
/// one is still running; `force` refreshes always run and supersede it.
fn refresh_input_controls(controls: &InputControls, force: bool) {
    if !force && controls.refresh_in_flight.get() {
        return;
    }
    let generation = controls.refresh_generation.get().wrapping_add(1);
    controls.refresh_generation.set(generation);
    controls.refresh_in_flight.set(true);

    let handle = gio::spawn_blocking(|| {
        let names: Vec<String> = input_devices().into_iter().map(|(name, _)| name).collect();
        let default_muted = default_input_is_muted().unwrap_or(false);
        (names, default_muted)
    });
    let controls = controls.clone();
    glib::MainContext::default().spawn_local(async move {
        let result = handle.await;
        if controls.refresh_generation.get() != generation {
            return;
        }
        controls.refresh_in_flight.set(false);
        let Ok((devices, default_muted)) = result else {
            return;
        };

        let selected_mic = controls.selected_mic.borrow().clone();
        let status = match selected_mic.as_deref() {
            Some(name) if devices.iter().any(|device_name| device_name == name) => {
                InputStatus::Ready(name.to_string())
            }
            Some(_) => InputStatus::SelectedUnavailable,
            None if devices.is_empty() => InputStatus::NoDevice,
            None if default_muted => InputStatus::Muted,
            None => InputStatus::Ready(gettext("Default microphone")),
        };
        apply_input_status(&controls, status);
    });
}

fn apply_input_status(controls: &InputControls, status: InputStatus) {
    controls.ready_pill.remove_css_class("input-unavailable");
    controls
        .ready_pill
        .set_tooltip_text(Some(&gettext("Choose input device")));

    match status {
        InputStatus::Ready(device_name) => {
            controls.input_ready.set(true);
            controls.block_reason.set(InputBlockReason::None);
            controls.start_button.set_sensitive(true);
            controls.icon_stack.set_visible_child_name("ready");
            controls.ready_title.set_label(&gettext("Ready to record"));
            controls.ready_subtitle.set_label(&device_name);
        }
        InputStatus::NoDevice => {
            controls.input_ready.set(false);
            controls.block_reason.set(InputBlockReason::NoDevice);
            controls.start_button.set_sensitive(false);
            controls.ready_pill.add_css_class("input-unavailable");
            controls
                .ready_pill
                .set_tooltip_text(Some(&gettext("No microphone available")));
            controls.icon_stack.set_visible_child_name("unavailable");
            controls
                .ready_title
                .set_label(&gettext("No microphone available"));
            controls
                .ready_subtitle
                .set_label(&gettext("Connect or enable an input device"));
        }
        InputStatus::Muted => {
            controls.input_ready.set(false);
            controls.block_reason.set(InputBlockReason::Muted);
            controls.start_button.set_sensitive(false);
            controls.ready_pill.add_css_class("input-unavailable");
            controls
                .ready_pill
                .set_tooltip_text(Some(&gettext("Unmute the microphone to record")));
            controls.icon_stack.set_visible_child_name("unavailable");
            controls.ready_title.set_label(&gettext("Microphone muted"));
            controls
                .ready_subtitle
                .set_label(&gettext("Unmute the microphone to record"));
        }
        InputStatus::SelectedUnavailable => {
            controls.input_ready.set(false);
            controls
                .block_reason
                .set(InputBlockReason::SelectedUnavailable);
            controls.start_button.set_sensitive(false);
            controls.ready_pill.add_css_class("input-unavailable");
            controls
                .ready_pill
                .set_tooltip_text(Some(&gettext("Choose another input device")));
            controls.icon_stack.set_visible_child_name("unavailable");
            controls
                .ready_title
                .set_label(&gettext("Microphone unavailable"));
            controls
                .ready_subtitle
                .set_label(&gettext("Choose another input device"));
        }
    }
}

fn status_pill(label: &gtk::Label) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("status-pill");
    row.set_halign(gtk::Align::Center);
    row.set_valign(gtk::Align::Center);

    let dot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dot.add_css_class("status-dot");
    dot.set_valign(gtk::Align::Center);

    row.append(&dot);
    row.append(label);
    row
}

fn folder_location_row(window: &adw::ApplicationWindow) -> gtk::Button {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);

    let icon = gtk::Image::from_icon_name("folder-symbolic");
    icon.set_pixel_size(18);
    icon.set_valign(gtk::Align::Center);

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text_box.set_hexpand(true);

    let caption = gtk::Label::new(Some(&gettext("Recording in")));
    caption.add_css_class("folder-caption");
    caption.set_halign(gtk::Align::Start);

    let directory = saved_recordings_directory_display();
    let path_label = gtk::Label::new(Some(&directory));
    path_label.add_css_class("folder-path");
    path_label.set_halign(gtk::Align::Start);
    path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path_label.set_xalign(0.0);

    text_box.append(&caption);
    text_box.append(&path_label);

    let chevron = gtk::Image::from_icon_name("go-next-symbolic");
    chevron.set_pixel_size(16);
    chevron.add_css_class("chevron");
    chevron.set_valign(gtk::Align::Center);

    row.append(&icon);
    row.append(&text_box);
    row.append(&chevron);

    let button = gtk::Button::builder()
        .tooltip_text(gettext("Open recordings folder"))
        .build();
    button.add_css_class("folder-row");
    button.set_child(Some(&row));

    let window = window.clone();
    button.connect_clicked(move |_| {
        open_recordings_folder(&window);
    });

    button
}

fn saved_recordings_directory_display() -> String {
    let directory = crate::audio::recorder::recordings_directory();
    if let Some(home) = glib::home_dir().to_str().map(str::to_owned)
        && let Some(stripped) = directory.to_string_lossy().strip_prefix(&home)
    {
        return format!("~{stripped}");
    }
    directory.display().to_string()
}

fn open_recordings_folder(window: &adw::ApplicationWindow) {
    let directory = crate::audio::recorder::recordings_directory();
    let _ = std::fs::create_dir_all(&directory);
    let uri = gio::File::for_path(&directory).uri();
    if let Err(err) = gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE) {
        show_message(
            window,
            &gettext("Could not open recordings folder"),
            &err.to_string(),
            MessageKind::Error,
        );
    }
}

fn apply_view_mode(flow: &gtk::FlowBox, rows: &RecordingRows, grid: bool) {
    if grid {
        flow.set_min_children_per_line(2);
        flow.set_max_children_per_line(2);
        flow.set_homogeneous(true);
        flow.remove_css_class("list-view");
        flow.add_css_class("grid-view");
    } else {
        flow.set_min_children_per_line(1);
        flow.set_max_children_per_line(1);
        flow.set_homogeneous(false);
        flow.remove_css_class("grid-view");
        flow.add_css_class("list-view");
    }

    // Grid tiles stay compact (no waveform/meta) so two columns always fit the
    // fixed window width — switching views never resizes the window.
    for row in rows.borrow().iter() {
        row.waveform_holder.set_visible(!grid);
        row.subtitle.set_visible(!grid);
        row.meta_box.set_visible(!grid);
    }
}

fn install_window_actions(window: &adw::ApplicationWindow) {
    // Actions are owned by the window, so they hold it weakly to avoid a cycle.
    let about_action = gtk::gio::SimpleAction::new("about", None);
    let window_weak = window.downgrade();
    about_action.connect_activate(move |_, _| {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let dialog = adw::AboutDialog::builder()
            .application_name("Big Recorder")
            .application_icon(crate::app::APP_ID)
            .developer_name("BigLinux Team")
            .version(crate::app::APP_VERSION)
            .comments(gettext(
                "Local voice recorder built with Rust, GTK4, libadwaita, and GStreamer.",
            ))
            .license_type(gtk::License::MitX11)
            .build();
        dialog.present(Some(&window));
    });
    window.add_action(&about_action);

    let close_action = gtk::gio::SimpleAction::new("close", None);
    let window_weak = window.downgrade();
    close_action.connect_activate(move |_, _| {
        if let Some(window) = window_weak.upgrade() {
            window.close();
        }
    });
    window.add_action(&close_action);
}

fn connect_controls(
    widgets: RecorderWidgets,
    recorder: Rc<RefCell<Recorder>>,
    player: Rc<RefCell<Player>>,
    session: Rc<RefCell<Session>>,
    recording_rows: RecordingRows,
    tool_context: ToolContext,
) {
    let mode_controls = ModeControls {
        page_stack: widgets.page_stack.clone(),
        start_button: widgets.start_button.clone(),
        ready_pill: widgets.ready_pill.clone(),
        input_ready: Rc::clone(&widgets.input_controls.input_ready),
        record_button: widgets.record_button.clone(),
        record_symbol: widgets.record_symbol.clone(),
        record_label: widgets.record_label.clone(),
        stop_button: widgets.stop_button.clone(),
        status_label: widgets.status_label.clone(),
    };
    let recording_controls = RecordingControls {
        recorder: Rc::clone(&recorder),
        session: Rc::clone(&session),
        timer_label: widgets.timer_label.clone(),
        mode_controls: mode_controls.clone(),
        tool_context: tool_context.clone(),
    };

    let mut sources = Vec::new();

    let timer_session = Rc::clone(&session);
    let timer_label_clone = widgets.timer_label.clone();
    sources.push(glib::timeout_add_seconds_local(1, move || {
        if timer_session.borrow().mode != RecordingMode::Idle {
            timer_label_clone.set_label(&format_duration(timer_session.borrow().elapsed()));
        }
        glib::ControlFlow::Continue
    }));

    let monitor_controls = recording_controls.clone();
    let monitor_waveform = widgets.waveform.clone();
    let monitor_window = widgets.window.clone();
    sources.push(glib::timeout_add_local(
        Duration::from_millis(50),
        move || {
            let mode = monitor_controls.session.borrow().mode;
            monitor_waveform.set_active(mode == RecordingMode::Recording);

            if mode == RecordingMode::Recording {
                // Bind the result first so the recorder borrow ends before a
                // failure path needs to borrow it again.
                let frame = monitor_controls.recorder.borrow_mut().poll_frame();
                match frame {
                    Ok(Some(frame)) => {
                        monitor_waveform.set_level(frame.level);
                        monitor_waveform.set_peaks(frame.peaks);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        // The source died (e.g. the microphone was unplugged):
                        // keep what was captured and tell the user.
                        let _ = finish_recording(&monitor_controls, None);
                        show_error(
                            &monitor_window,
                            &gettext("Recording stopped unexpectedly"),
                            &err,
                        );
                    }
                }
            }

            glib::ControlFlow::Continue
        },
    ));

    let input_controls = widgets.input_controls.clone();
    let input_session = Rc::clone(&session);
    sources.push(glib::timeout_add_seconds_local(2, move || {
        if input_session.borrow().mode == RecordingMode::Idle {
            refresh_input_controls(&input_controls, false);
        }

        glib::ControlFlow::Continue
    }));

    let monitor = watch_recordings_directory(&tool_context, Rc::clone(&session));

    let sources = RefCell::new(sources);
    let destroy_ticker = Rc::clone(&tool_context.playback_ticker);
    widgets.window.connect_destroy(move |_| {
        for source in sources.take() {
            source.remove();
        }
        if let Some(source) = destroy_ticker.take() {
            source.remove();
        }
        if let Some(monitor) = &monitor {
            monitor.cancel();
        }
    });

    // Closing mid-recording must still finalize the WAV header, otherwise the
    // file is left unreadable.
    let close_controls = recording_controls.clone();
    let close_player = Rc::clone(&player);
    widgets.window.connect_close_request(move |_| {
        if close_controls.session.borrow().mode != RecordingMode::Idle
            && let Err(err) = finish_recording(&close_controls, None)
        {
            eprintln!("Failed to finish recording on close: {err}");
        }
        let _ = close_player.borrow_mut().stop();
        glib::Propagation::Proceed
    });

    let window_clone = widgets.window.clone();
    let mode_controls_clone = mode_controls.clone();
    let timer_label_clone = widgets.timer_label.clone();
    let recorder_clone = Rc::clone(&recorder);
    let player_clone = Rc::clone(&player);
    let session_clone = Rc::clone(&session);
    let recording_rows_clone = Rc::clone(&recording_rows);
    let input_controls_clone = widgets.input_controls.clone();

    widgets.start_button.connect_clicked(move |_| {
        if session_clone.borrow().mode != RecordingMode::Idle {
            return;
        }

        // The input state is kept current by the background refresh; probing
        // it here again would block the click on the sound server.
        if !input_controls_clone.input_ready.get() {
            let (heading, body) = match input_controls_clone.block_reason.get() {
                InputBlockReason::Muted => (
                    gettext("Microphone muted"),
                    gettext("Unmute the microphone to record"),
                ),
                InputBlockReason::NoDevice => (
                    gettext("No microphone available"),
                    gettext("Connect or enable an input device"),
                ),
                InputBlockReason::SelectedUnavailable => (
                    gettext("Microphone unavailable"),
                    gettext("Choose another input device"),
                ),
                InputBlockReason::None => (
                    gettext("Microphone unavailable"),
                    gettext("Connect or enable an input device"),
                ),
            };
            show_message(&window_clone, &heading, &body, MessageKind::Error);
            refresh_input_controls(&input_controls_clone, true);
            return;
        }

        if let Err(err) = player_clone.borrow_mut().stop() {
            show_error(&window_clone, &gettext("Could not stop playback"), &err);
            return;
        }
        refresh_playback_rows(&recording_rows_clone, None);

        let started = recorder_clone.borrow_mut().start();
        match started {
            Ok(path) => {
                session_clone.borrow_mut().start(path);
                apply_mode(RecordingMode::Recording, &mode_controls_clone);
            }
            Err(err) => show_error(&window_clone, &gettext("Could not start recording"), &err),
        }

        timer_label_clone.set_label(&format_duration(session_clone.borrow().elapsed()));
    });

    let window_clone = widgets.window.clone();
    let mode_controls_clone = mode_controls.clone();
    let timer_label_clone = widgets.timer_label.clone();
    let recorder_clone = Rc::clone(&recorder);
    let session_clone = Rc::clone(&session);

    widgets.record_button.connect_clicked(move |_| {
        let mode = session_clone.borrow().mode;

        match mode {
            RecordingMode::Recording => match recorder_clone.borrow().pause() {
                Ok(()) => {
                    session_clone.borrow_mut().pause();
                    apply_mode(RecordingMode::Paused, &mode_controls_clone);
                }
                Err(err) => show_error(&window_clone, &gettext("Could not pause"), &err),
            },
            RecordingMode::Paused => match recorder_clone.borrow().resume() {
                Ok(()) => {
                    session_clone.borrow_mut().resume();
                    apply_mode(RecordingMode::Recording, &mode_controls_clone);
                }
                Err(err) => show_error(&window_clone, &gettext("Could not resume"), &err),
            },
            RecordingMode::Idle => {}
        }

        timer_label_clone.set_label(&format_duration(session_clone.borrow().elapsed()));
    });

    let window_clone = widgets.window.clone();
    let stop_controls = recording_controls.clone();
    widgets.stop_button.connect_clicked(move |_| {
        let final_duration = stop_controls.session.borrow().elapsed();
        if let Err(err) = finish_recording(&stop_controls, Some(final_duration)) {
            show_error(&window_clone, &gettext("Could not finish recording"), &err);
        }
    });
}

/// Stops the active recording, resets the recording page and adds the saved
/// file to the list. `known_duration` is used when the session timer is
/// trustworthy; otherwise the duration is read back from the file.
fn finish_recording(
    controls: &RecordingControls,
    known_duration: Option<Duration>,
) -> anyhow::Result<()> {
    let final_duration = controls.session.borrow().elapsed();
    let stopped = controls.recorder.borrow_mut().stop();

    controls.session.borrow_mut().stop();
    controls
        .timer_label
        .set_label(&format_duration(final_duration));
    apply_mode(RecordingMode::Idle, &controls.mode_controls);

    let path = stopped?;
    if let Some(path) = path {
        append_recording_row(&controls.tool_context, path, known_duration, true);
    }
    Ok(())
}

/// Keeps the list in sync with files added, removed or renamed outside the
/// app. The returned monitor must be kept alive for events to arrive.
fn watch_recordings_directory(
    context: &ToolContext,
    session: Rc<RefCell<Session>>,
) -> Option<gio::FileMonitor> {
    let directory = crate::audio::recorder::recordings_directory();
    let _ = std::fs::create_dir_all(&directory);
    let monitor = gio::File::for_path(&directory)
        .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        .map_err(|err| eprintln!("Failed to watch recordings folder: {err}"))
        .ok()?;

    let context = context.clone();
    monitor.connect_changed(move |_, file, other_file, event| {
        let add = |file: &gio::File| {
            let Some(path) = file.path() else {
                return;
            };
            let session = session.borrow();
            let in_progress = session.mode != RecordingMode::Idle
                && session.current_path.as_deref() == Some(path.as_path());
            if !in_progress && is_listed_recording_file(&path) {
                drop(session);
                append_recording_row(&context, path, None, false);
            }
        };
        let remove = |file: &gio::File| {
            if let Some(path) = file.path() {
                remove_recording_row(&context, &path);
            }
        };

        match event {
            gio::FileMonitorEvent::Deleted | gio::FileMonitorEvent::MovedOut => remove(file),
            gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::MovedIn => add(file),
            gio::FileMonitorEvent::Renamed => {
                remove(file);
                if let Some(other_file) = other_file {
                    add(other_file);
                }
            }
            _ => {}
        }
    });

    Some(monitor)
}

/// Visible WAV files only: temporary outputs are written as hidden files.
fn is_listed_recording_file(path: &Path) -> bool {
    let visible = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| !name.starts_with('.'));
    let is_wav = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"));
    visible && is_wav && path.is_file()
}

fn apply_mode(mode: RecordingMode, controls: &ModeControls) {
    let page_stack = &controls.page_stack;
    let start_button = &controls.start_button;
    let ready_pill = &controls.ready_pill;
    let input_ready = &controls.input_ready;
    let record_button = &controls.record_button;
    let record_symbol = &controls.record_symbol;
    let record_label = &controls.record_label;
    let stop_button = &controls.stop_button;
    let status_label = &controls.status_label;

    status_label.remove_css_class("recording");
    status_label.remove_css_class("paused");
    record_button.remove_css_class("resume-pill");

    let recording = mode != RecordingMode::Idle;
    // While recording the controls live on the recording card, so the bottom
    // FAB and the "ready" pill step aside.
    start_button.set_visible(!recording);
    ready_pill.set_visible(!recording);

    match mode {
        RecordingMode::Idle => {
            page_stack.set_visible_child_name("recordings");
            start_button.set_sensitive(input_ready.get());
            status_label.set_label(&gettext("Ready"));
            stop_button.set_sensitive(false);
        }
        RecordingMode::Recording => {
            page_stack.set_visible_child_name("recording");
            status_label.set_label(&gettext("Recording"));
            status_label.add_css_class("recording");
            record_symbol.set_visible_child_name("pause");
            record_label.set_label(&gettext("Pause"));
            record_button.set_tooltip_text(Some(&gettext("Pause")));
            stop_button.set_sensitive(true);
        }
        RecordingMode::Paused => {
            page_stack.set_visible_child_name("recording");
            status_label.set_label(&gettext("Paused"));
            status_label.add_css_class("paused");
            record_symbol.set_visible_child_name("play");
            record_label.set_label(&gettext("Resume"));
            record_button.set_tooltip_text(Some(&gettext("Resume")));
            record_button.add_css_class("resume-pill");
            stop_button.set_sensitive(true);
        }
    }
}

fn load_saved_recordings(context: &ToolContext) {
    match saved_recordings() {
        Ok(paths) => {
            for path in paths.into_iter().rev() {
                append_recording_row(context, path, None, false);
            }
            if let Some(path) = context
                .recording_rows
                .borrow()
                .last()
                .map(|row| row.path.clone())
            {
                select_recording(&context.recording_rows, &context.selected_recording, &path);
            }
        }
        Err(err) => eprintln!("Failed to load saved recordings: {err}"),
    }
    update_count(context);
}

fn update_count(context: &ToolContext) {
    let count = context.recording_rows.borrow().len();
    let has_rows = count > 0;
    context.empty_recordings_label.set_visible(!has_rows);
    context.recordings_flow.set_visible(has_rows);

    let text = format_message(
        &ngettext(
            "{count} recording",
            "{count} recordings",
            u32::try_from(count).unwrap_or(u32::MAX),
        ),
        &[("{count}", &count.to_string())],
    );
    context.count_label.set_label(&text);
}

fn append_recording_row(
    context: &ToolContext,
    path: PathBuf,
    duration: Option<Duration>,
    auto_select: bool,
) {
    // The folder monitor and the app itself can both report the same file.
    let already_listed = context
        .recording_rows
        .borrow()
        .iter()
        .any(|row| row.path == path);
    if already_listed {
        if auto_select {
            select_recording(&context.recording_rows, &context.selected_recording, &path);
        }
        return;
    }

    let title_text = recording_title(&path);
    let duration = duration.or_else(|| processing::duration(&path).ok());
    let path_cell = Rc::new(RefCell::new(path.clone()));
    let grid = context.recordings_flow.has_css_class("grid-view");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("recording-row");
    row.set_hexpand(true);

    // Leading microphone icon.
    let icon_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    icon_holder.add_css_class("row-icon");
    icon_holder.set_valign(gtk::Align::Center);
    let icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
    icon.set_pixel_size(20);
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    icon_holder.append(&icon);

    // Title + subtitle.
    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text_box.set_hexpand(true);
    text_box.set_valign(gtk::Align::Center);

    let title = gtk::Label::new(Some(&title_text));
    title.add_css_class("recording-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let subtitle = gtk::Label::new(Some(&recording_subtitle(duration)));
    subtitle.add_css_class("recording-subtitle");
    subtitle.set_xalign(0.0);
    subtitle.set_visible(!grid);

    text_box.append(&title);
    text_box.append(&subtitle);

    // Inline mini waveform.
    let playback_bar = PlaybackBar::new(&path);
    let waveform_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    waveform_holder.set_size_request(104, -1);
    waveform_holder.set_valign(gtk::Align::Center);
    waveform_holder.set_visible(!grid);
    waveform_holder.append(playback_bar.widget());

    // Metadata: duration + date.
    let meta_box = gtk::Box::new(gtk::Orientation::Vertical, 3);
    meta_box.set_valign(gtk::Align::Center);
    meta_box.set_visible(!grid);
    meta_box.append(&meta_line(
        "document-open-recent-symbolic",
        &duration.map(format_compact_duration).unwrap_or_default(),
    ));
    if let Some(date) = recording_date(&path) {
        meta_box.append(&meta_line("x-office-calendar-symbolic", &date));
    }

    // Action buttons.
    let playback_symbol = gtk::Stack::new();
    playback_symbol.set_size_request(18, 18);
    playback_symbol.set_halign(gtk::Align::Center);
    playback_symbol.set_valign(gtk::Align::Center);
    playback_symbol.add_named(&row_playback_symbol(false), Some("play"));
    playback_symbol.add_named(&row_playback_symbol(true), Some("pause"));
    playback_symbol.set_visible_child_name("play");

    let play_button = gtk::Button::builder()
        .tooltip_text(gettext("Play"))
        .valign(gtk::Align::Center)
        .build();
    play_button.add_css_class("row-play-button");
    play_button.set_child(Some(&playback_symbol));

    let delete_icon = gtk::Image::from_icon_name("user-trash-symbolic");
    delete_icon.set_pixel_size(16);

    let delete_button = gtk::Button::builder()
        .tooltip_text(gettext("Move to Trash"))
        .valign(gtk::Align::Center)
        .build();
    delete_button.add_css_class("row-delete-button");
    delete_button.set_child(Some(&delete_icon));

    let menu_button = build_row_menu(context, &path_cell);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_valign(gtk::Align::Center);
    actions.append(&play_button);
    actions.append(&delete_button);
    actions.append(&menu_button);

    row.append(&icon_holder);
    row.append(&text_box);
    row.append(&waveform_holder);
    row.append(&meta_box);
    row.append(&actions);

    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&row));
    context.recordings_flow.insert(&child, 0);

    context.recording_rows.borrow_mut().push(RecordingRow {
        child: child.clone(),
        row_widget: row.clone(),
        path: path.clone(),
        path_cell: Rc::clone(&path_cell),
        title_text,
        title_label: title.clone(),
        playback_symbol: playback_symbol.clone(),
        playback_bar: playback_bar.clone(),
        waveform_holder: waveform_holder.upcast::<gtk::Widget>(),
        subtitle: subtitle.clone(),
        meta_box: meta_box.upcast::<gtk::Widget>(),
    });
    // The row was inserted before it was registered, so the search filter
    // could not find it; re-run the filter now that it is known.
    context.recordings_flow.invalidate_filter();

    let select_rows = Rc::clone(&context.recording_rows);
    let select_recording_ref = Rc::clone(&context.selected_recording);
    let select_path = Rc::clone(&path_cell);
    let gesture = gtk::GestureClick::new();
    gesture.connect_pressed(move |_, _, _, _| {
        let path = select_path.borrow().clone();
        select_recording(&select_rows, &select_recording_ref, &path);
    });
    row.add_controller(gesture);

    let delete_context = context.clone();
    let delete_path = Rc::clone(&path_cell);
    delete_button.connect_clicked(move |_| {
        let path = delete_path.borrow().clone();
        select_recording(
            &delete_context.recording_rows,
            &delete_context.selected_recording,
            &path,
        );
        confirm_trash_recording(&delete_context, path);
    });

    let play_context = context.clone();
    let play_path = Rc::clone(&path_cell);
    play_button.connect_clicked(move |_| {
        let path = play_path.borrow().clone();
        select_recording(
            &play_context.recording_rows,
            &play_context.selected_recording,
            &path,
        );
        let result = play_context.player.borrow_mut().toggle(&path);

        match result {
            Ok(()) => {
                let status = play_context.player.borrow().status();
                refresh_playback_rows(&play_context.recording_rows, status.as_ref());
                if status.is_some() {
                    ensure_playback_ticker(&play_context);
                }
            }
            Err(err) => show_error(
                &play_context.window,
                &gettext("Could not play recording"),
                &err,
            ),
        }
    });

    if auto_select {
        select_recording(&context.recording_rows, &context.selected_recording, &path);
    }

    update_count(context);
}

/// Polls the player while something is playing. The ticker stops itself once
/// playback ends, so nothing runs while the app is idle.
fn ensure_playback_ticker(context: &ToolContext) {
    if context.playback_ticker.borrow().is_some() {
        return;
    }

    let ticker_context = context.clone();
    let source = glib::timeout_add_local(Duration::from_millis(33), move || {
        let polled = ticker_context.player.borrow_mut().poll_finished();
        let status = match polled {
            Ok(true) => None,
            Ok(false) => ticker_context.player.borrow().status(),
            Err(err) => {
                let _ = ticker_context.player.borrow_mut().stop();
                show_error(
                    &ticker_context.window,
                    &gettext("Could not play recording"),
                    &err,
                );
                None
            }
        };
        refresh_playback_rows(&ticker_context.recording_rows, status.as_ref());

        if status.is_some() {
            glib::ControlFlow::Continue
        } else {
            // Returning Break removes the source; just forget its id.
            ticker_context.playback_ticker.replace(None);
            glib::ControlFlow::Break
        }
    });
    context.playback_ticker.replace(Some(source));
}

fn meta_line(icon_name: &str, text: &str) -> gtk::Box {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    line.add_css_class("recording-meta");

    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(13);
    icon.set_valign(gtk::Align::Center);

    let label = gtk::Label::new(Some(text));
    label.add_css_class("recording-meta");
    label.set_xalign(0.0);

    line.append(&icon);
    line.append(&label);
    line
}

fn build_row_menu(context: &ToolContext, path: &Rc<RefCell<PathBuf>>) -> gtk::MenuButton {
    let menu_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text(gettext("More options"))
        .valign(gtk::Align::Center)
        .build();
    menu_button.add_css_class("row-menu-button");

    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);

    let items: [(&str, String, RowMenuAction); 3] = [
        (
            "document-edit-symbolic",
            gettext("Rename"),
            RowMenuAction::Rename,
        ),
        (
            "document-save-symbolic",
            gettext("Export"),
            RowMenuAction::Export,
        ),
        (
            "user-trash-symbolic",
            gettext("Move to Trash"),
            RowMenuAction::Trash,
        ),
    ];

    for (icon, label, action) in items {
        let button = popover_menu_item(icon, &label, matches!(action, RowMenuAction::Trash));
        let item_context = context.clone();
        let item_path = Rc::clone(path);
        let popover_clone = popover.clone();
        button.connect_clicked(move |_| {
            popover_clone.popdown();
            let path = item_path.borrow().clone();
            run_row_menu_action(action, &item_context, &path);
        });
        menu_box.append(&button);
    }

    popover.set_child(Some(&menu_box));
    menu_button.set_popover(Some(&popover));
    menu_button
}

fn popover_menu_item(icon_name: &str, label: &str, destructive: bool) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(15);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    content.append(&icon);
    content.append(&text);

    let button = gtk::Button::new();
    button.set_child(Some(&content));
    button.add_css_class("flat");
    if destructive {
        button.add_css_class("destructive-action-text");
    }
    button
}

#[derive(Clone, Copy)]
enum RowMenuAction {
    Rename,
    Export,
    Trash,
}

fn run_row_menu_action(action: RowMenuAction, context: &ToolContext, path: &Path) {
    select_recording(&context.recording_rows, &context.selected_recording, path);
    match action {
        RowMenuAction::Rename => show_rename_dialog(context, path.to_path_buf()),
        RowMenuAction::Export => export_selected_recording(context),
        RowMenuAction::Trash => confirm_trash_recording(context, path.to_path_buf()),
    }
}

fn show_rename_dialog(context: &ToolContext, path: PathBuf) {
    let dialog = adw::AlertDialog::new(Some(&gettext("Rename recording")), None);

    let entry = gtk::Entry::new();
    entry.set_text(&recording_title(&path));
    entry.set_activates_default(true);
    entry.set_hexpand(true);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.add_css_class("tool-dialog");
    content.append(&entry);
    dialog.set_extra_child(Some(&content));

    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("rename", &gettext("Rename"));
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "rename" {
            return;
        }
        let new_name = entry.text().trim().to_string();
        rename_recording(&action_context, &path, &new_name);
    });
    dialog.present(Some(&context.window));
}

fn rename_recording(context: &ToolContext, path: &Path, new_name: &str) {
    if new_name.is_empty() {
        return;
    }
    if new_name.contains(['/', '\0']) || new_name == "." || new_name == ".." {
        show_notice(
            &context.window,
            &gettext("Could not rename recording"),
            &gettext("The name cannot contain “/”."),
        );
        return;
    }

    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("wav");
    let target = path.with_file_name(format!("{new_name}.{extension}"));

    if target == path {
        return;
    }
    if target.exists() {
        show_notice(
            &context.window,
            &gettext("Could not rename recording"),
            &gettext("A recording with that name already exists."),
        );
        return;
    }

    // Release the file before renaming it if it is the one being played.
    if context
        .player
        .borrow()
        .status()
        .is_some_and(|status| status.path == path)
    {
        let _ = context.player.borrow_mut().stop();
        refresh_playback_rows(&context.recording_rows, None);
    }

    match std::fs::rename(path, &target) {
        Ok(()) => {
            // Update the existing row instead of rebuilding the whole list.
            let title_text = recording_title(&target);
            if let Some(row) = context
                .recording_rows
                .borrow_mut()
                .iter_mut()
                .find(|row| row.path == path)
            {
                row.path = target.clone();
                row.path_cell.replace(target.clone());
                row.title_label.set_label(&title_text);
                row.title_text = title_text;
            }
            context.recordings_flow.invalidate_filter();
            select_recording(
                &context.recording_rows,
                &context.selected_recording,
                &target,
            );
        }
        Err(err) => show_message(
            &context.window,
            &gettext("Could not rename recording"),
            &err.to_string(),
            MessageKind::Error,
        ),
    }
}

fn confirm_trash_recording(context: &ToolContext, path: PathBuf) {
    let dialog = adw::AlertDialog::new(
        Some(&gettext("Move recording to trash?")),
        Some(&format_message(
            &gettext("{recording} will be moved to the system trash."),
            &[("{recording}", &recording_title(&path))],
        )),
    );
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("trash", &gettext("Move to Trash"));
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);

    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "trash" {
            trash_recording(&action_context, &path);
        }
    });
    dialog.present(Some(&context.window));
}

fn trash_recording(context: &ToolContext, path: &Path) {
    if let Err(err) = context.player.borrow_mut().stop() {
        show_error(&context.window, &gettext("Could not stop playback"), &err);
        return;
    }
    refresh_playback_rows(&context.recording_rows, None);

    // Trashing can mean copying across filesystems, so keep it off the
    // main thread.
    let action_context = context.clone();
    let path = path.to_path_buf();
    glib::MainContext::default().spawn_local(async move {
        let result = gio::File::for_path(&path)
            .trash_future(glib::Priority::DEFAULT)
            .await
            .with_context(|| format!("Failed to move recording to trash: {}", path.display()));
        match result {
            Ok(()) => remove_recording_row(&action_context, &path),
            Err(err) => show_error(
                &action_context.window,
                &gettext("Could not move recording to trash"),
                &err,
            ),
        }
    });
}

fn remove_recording_row(context: &ToolContext, path: &Path) {
    let deleted_selected = context
        .selected_recording
        .borrow()
        .as_ref()
        .is_some_and(|selected_path| selected_path == path);

    let Some((child, replacement_path, list_is_empty)) = ({
        let mut rows = context.recording_rows.borrow_mut();
        if let Some(index) = rows.iter().position(|row| row.path == path) {
            let removed_row = rows.remove(index);
            let replacement_path = if deleted_selected {
                rows.get(index)
                    .or_else(|| rows.last())
                    .map(|row| row.path.clone())
            } else {
                None
            };

            Some((removed_row.child, replacement_path, rows.is_empty()))
        } else {
            None
        }
    }) else {
        return;
    };

    context.recordings_flow.remove(&child);

    if list_is_empty {
        context.selected_recording.replace(None);
    } else if let Some(path) = replacement_path {
        select_recording(&context.recording_rows, &context.selected_recording, &path);
    }

    update_count(context);
}

fn select_recording(
    recording_rows: &RecordingRows,
    selected_recording: &SelectedRecording,
    path: &Path,
) {
    selected_recording.replace(Some(path.to_path_buf()));

    for row in recording_rows.borrow().iter() {
        if row.path == path {
            row.row_widget.add_css_class("selected");
        } else {
            row.row_widget.remove_css_class("selected");
        }
    }
}

fn selected_recording_path(context: &ToolContext) -> Option<PathBuf> {
    if let Some(path) = context.selected_recording.borrow().clone()
        && context
            .recording_rows
            .borrow()
            .iter()
            .any(|row| row.path == path)
    {
        return Some(path);
    }

    context
        .recording_rows
        .borrow()
        .last()
        .map(|row| row.path.clone())
}

fn run_tool(action: ToolAction, context: &ToolContext) {
    match action {
        ToolAction::Trim => show_trim_dialog(context),
        ToolAction::Volume => show_volume_dialog(context),
        ToolAction::Speed => show_speed_dialog(context),
        ToolAction::Pitch => show_pitch_dialog(context),
        ToolAction::VoiceEffects => show_voice_effects_dialog(context),
        ToolAction::Equalizer => show_equalizer_dialog(context),
        ToolAction::Reverse => show_reverse_dialog(context),
        ToolAction::Merge => show_merge_dialog(context),
        ToolAction::Export => export_selected_recording(context),
    }
}

fn selected_or_notice(context: &ToolContext) -> Option<PathBuf> {
    let path = selected_recording_path(context);
    if path.is_none() {
        show_notice(
            &context.window,
            &gettext("Select a recording first"),
            &gettext("Click a recording in the list before using a tool."),
        );
    }

    path
}

fn show_trim_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };
    let duration = match processing::duration(&path) {
        Ok(duration) if duration > Duration::ZERO => duration,
        Ok(_) => {
            show_notice(
                &context.window,
                &gettext("Could not trim recording"),
                &gettext("This recording has no audio data."),
            );
            return;
        }
        Err(err) => {
            show_error(&context.window, &gettext("Could not trim recording"), &err);
            return;
        }
    };

    let max_seconds = duration.as_secs_f64();
    let start_spin = number_spin(0.0, max_seconds, 0.1, 2, 0.0);
    let end_spin = number_spin(0.0, max_seconds, 0.1, 2, 0.0);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.add_css_class("tool-dialog");
    content.append(&tool_illustration(ToolIllustration::Trim));

    // Selected-duration readout.
    let duration_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
    duration_box.set_halign(gtk::Align::Center);
    let duration_value = gtk::Label::new(Some(&format_trim_duration(max_seconds)));
    duration_value.add_css_class("selected-duration");
    let duration_caption = gtk::Label::new(Some(&gettext("Selected duration")));
    duration_caption.add_css_class("selected-duration-caption");
    duration_box.append(&duration_value);
    duration_box.append(&duration_caption);
    content.append(&duration_box);

    let fields = gtk::Box::new(gtk::Orientation::Vertical, 10);
    fields.append(&stepper_field(
        &gettext("Initial seconds"),
        &gettext("Remove from the start of the recording"),
        &start_spin,
    ));
    fields.append(&stepper_field(
        &gettext("Final seconds"),
        &gettext("Remove from the end of the recording"),
        &end_spin,
    ));
    content.append(&fields);

    // Keep the readout in sync with both steppers.
    let update_value = {
        let duration_value = duration_value.clone();
        let start_spin = start_spin.clone();
        let end_spin = end_spin.clone();
        move || {
            let selected = (max_seconds - start_spin.value() - end_spin.value()).max(0.0);
            duration_value.set_label(&format_trim_duration(selected));
        }
    };
    let update_clone = update_value.clone();
    start_spin
        .adjustment()
        .connect_value_changed(move |_| update_clone());
    let update_clone = update_value.clone();
    end_spin
        .adjustment()
        .connect_value_changed(move |_| update_clone());

    let dialog = tool_dialog(
        &gettext("Trim recording"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let task_path = path.clone();
        let start = Duration::from_secs_f64(start_spin.value());
        let end = Duration::from_secs_f64((max_seconds - end_spin.value()).max(0.0));
        run_processed_task(
            &action_context,
            gettext("Could not trim recording"),
            move || processing::trim(&task_path, start, end),
        );
    });
    dialog.present(Some(&window));
}

fn stepper_field(title: &str, description: &str, spin: &gtk::SpinButton) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("stepper-row");

    let icon = gtk::Image::from_icon_name("document-open-recent-symbolic");
    icon.set_pixel_size(16);
    icon.add_css_class("stepper-icon");
    icon.set_valign(gtk::Align::Center);

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text_box.set_hexpand(true);
    text_box.set_valign(gtk::Align::Center);

    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("stepper-title");
    title_label.set_xalign(0.0);

    let desc_label = gtk::Label::new(Some(description));
    desc_label.add_css_class("stepper-desc");
    desc_label.set_xalign(0.0);
    desc_label.set_wrap(true);

    text_box.append(&title_label);
    text_box.append(&desc_label);

    spin.set_valign(gtk::Align::Center);

    row.append(&icon);
    row.append(&text_box);
    row.append(spin);
    row
}

fn format_trim_duration(total_seconds: f64) -> String {
    let total = total_seconds.max(0.0);
    let minutes = (total / 60.0).floor() as u64;
    let seconds = total - (minutes as f64) * 60.0;
    let seconds = localize_decimal(&format!("{seconds:05.2}"));
    format!("{minutes:02}:{seconds}")
}

/// Swaps the '.' produced by Rust formatting for the locale's decimal mark.
fn localize_decimal(formatted: &str) -> String {
    formatted.replace('.', decimal_separator())
}

fn decimal_separator() -> &'static str {
    static SEPARATOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SEPARATOR.get_or_init(|| {
        // SAFETY: GTK already called setlocale(); localeconv() returns a
        // pointer to static storage that is copied out immediately.
        unsafe {
            let conventions = libc::localeconv();
            if conventions.is_null() || (*conventions).decimal_point.is_null() {
                return ".".to_string();
            }
            std::ffi::CStr::from_ptr((*conventions).decimal_point)
                .to_str()
                .ok()
                .filter(|separator| !separator.is_empty())
                .unwrap_or(".")
                .to_string()
        }
    })
}

fn show_volume_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };
    let (gain_control, gain_adjustment) = horizontal_value_control(0.0, 300.0, 5.0, 0, 100.0, "%");
    let content = tool_content_with_illustration(
        ToolIllustration::Volume,
        &[(&gettext("Volume percent"), &gain_control)],
    );
    let dialog = tool_dialog(
        &gettext("Change volume"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let task_path = path.clone();
        let gain = (gain_adjustment.value() / 100.0) as f32;
        run_processed_task(
            &action_context,
            gettext("Could not change volume"),
            move || processing::change_volume(&task_path, gain),
        );
    });
    dialog.present(Some(&window));
}

fn show_speed_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };
    let (speed_control, speed_adjustment) = horizontal_value_control(0.25, 4.0, 0.05, 2, 1.25, "x");
    let content = tool_content_with_illustration(
        ToolIllustration::Speed,
        &[(&gettext("Speed factor"), &speed_control)],
    );
    let dialog = tool_dialog(
        &gettext("Change speed"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let task_path = path.clone();
        let speed = speed_adjustment.value() as f32;
        run_processed_task(
            &action_context,
            gettext("Could not change speed"),
            move || processing::change_speed(&task_path, speed),
        );
    });
    dialog.present(Some(&window));
}

fn show_pitch_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };
    let (pitch_control, pitch_adjustment) =
        horizontal_value_control(-12.0, 12.0, 1.0, 0, 2.0, " st");
    let content = tool_content_with_illustration(
        ToolIllustration::Pitch,
        &[(&gettext("Semitones"), &pitch_control)],
    );
    let dialog = tool_dialog(
        &gettext("Change pitch"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let task_path = path.clone();
        let semitones = pitch_adjustment.value() as f32;
        run_processed_task(
            &action_context,
            gettext("Could not change pitch"),
            move || processing::change_pitch(&task_path, semitones),
        );
    });
    dialog.present(Some(&window));
}

fn show_voice_effects_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };

    let effect_combo = choice_dropdown(
        &[
            gettext("Child voice"),
            gettext("Deep voice"),
            gettext("Robot voice"),
            gettext("Radio voice"),
            gettext("Echo voice"),
        ],
        0,
    );

    let (amount_control, amount_adjustment) =
        horizontal_value_control(0.0, 100.0, 5.0, 0, 70.0, "%");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.add_css_class("tool-dialog");
    content.append(&tool_illustration(ToolIllustration::VoiceEffects));

    let fields = gtk::Box::new(gtk::Orientation::Vertical, 10);
    fields.append(&tool_row(&gettext("Effect"), &effect_combo));
    fields.append(&tool_row(&gettext("Intensity"), &amount_control));
    content.append(&fields);

    let dialog = tool_dialog(
        &gettext("Voice effects"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let task_path = path.clone();
        let preset = voice_effect_from_combo(&effect_combo);
        let amount = (amount_adjustment.value() / 100.0) as f32;
        run_processed_task(
            &action_context,
            gettext("Could not apply voice effect"),
            move || processing::apply_voice_effect(&task_path, preset, amount),
        );
    });
    dialog.present(Some(&window));
}

fn show_equalizer_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };

    let adjustments: Vec<gtk::Adjustment> = EQUALIZER_BANDS
        .iter()
        .map(|_| gtk::Adjustment::new(0.0, -12.0, 12.0, 1.0, 3.0, 0.0))
        .collect();
    let content = equalizer_dialog_content(&adjustments);
    let dialog = tool_dialog(
        &gettext("Equalizer"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let task_path = path.clone();
        let gains = std::array::from_fn(|index| adjustments[index].value() as f32);
        run_processed_task(
            &action_context,
            gettext("Could not equalize recording"),
            move || processing::equalize(&task_path, gains),
        );
    });
    dialog.present(Some(&window));
}

fn show_reverse_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };
    let dialog = adw::AlertDialog::new(
        Some(&gettext("Reverse recording")),
        Some(&gettext("Create a new recording with the audio reversed.")),
    );
    let content = tool_illustration_content(ToolIllustration::Reverse);
    dialog.set_extra_child(Some(&content));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("apply", &gettext("Reverse"));
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let path = path.clone();
        run_processed_task(
            &action_context,
            gettext("Could not reverse recording"),
            move || processing::reverse(&path),
        );
    });
    dialog.present(Some(&window));
}

fn show_merge_dialog(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };
    let options: Vec<PathBuf> = context
        .recording_rows
        .borrow()
        .iter()
        .filter(|row| row.path != path)
        .map(|row| row.path.clone())
        .collect();
    if options.is_empty() {
        show_notice(
            &context.window,
            &gettext("Could not merge recordings"),
            &gettext("There is no second recording to merge."),
        );
        return;
    }

    let titles: Vec<String> = options
        .iter()
        .map(|option| recording_title(option))
        .collect();
    let combo = choice_dropdown(&titles, 0);
    let content = tool_content_with_illustration(
        ToolIllustration::Merge,
        &[(&gettext("Append recording"), &combo)],
    );
    let dialog = tool_dialog(
        &gettext("Merge recordings"),
        Some(&recording_title(&path)),
        &content,
    );

    let window = context.window.clone();
    let action_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let index = combo.selected() as usize;
        let path = path.clone();
        let second_path = options.get(index).cloned();
        run_processed_task(
            &action_context,
            gettext("Could not merge recordings"),
            move || {
                let second_path =
                    second_path.ok_or_else(|| anyhow::anyhow!("No recording selected"))?;
                processing::merge(&path, &second_path)
            },
        );
    });
    dialog.present(Some(&window));
}

fn export_selected_recording(context: &ToolContext) {
    let Some(path) = selected_or_notice(context) else {
        return;
    };

    let format_combo = choice_dropdown(
        &[
            gettext("MP3 audio (.mp3)"),
            gettext("WAV audio (.wav)"),
            gettext("Opus audio (.opus)"),
            gettext("FLAC audio (.flac)"),
        ],
        0,
    );

    let quality_combo = choice_dropdown(&[gettext("Low"), gettext("Medium"), gettext("High")], 1);

    let channel_combo = choice_dropdown(
        &[gettext("Original"), gettext("Mono"), gettext("Stereo")],
        1,
    );

    let sample_rate_combo = choice_dropdown(
        &[
            gettext("Original"),
            gettext("48 kHz"),
            gettext("44.1 kHz"),
            gettext("24 kHz"),
            gettext("16 kHz"),
        ],
        0,
    );

    let content = tool_content_with_illustration(
        ToolIllustration::Export,
        &[
            (&gettext("Format"), &format_combo),
            (&gettext("Quality"), &quality_combo),
            (&gettext("Channels"), &channel_combo),
            (&gettext("Sample rate"), &sample_rate_combo),
        ],
    );
    let dialog = adw::AlertDialog::new(
        Some(&gettext("Export recording")),
        Some(&recording_title(&path)),
    );
    dialog.set_extra_child(Some(&content));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("apply", &gettext("Export"));
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);

    let export_context = context.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "apply" {
            return;
        }

        let options = ExportOptions {
            format: export_format_from_combo(&format_combo),
            quality: export_quality_from_combo(&quality_combo),
            channel_mode: export_channel_from_combo(&channel_combo),
            sample_rate: export_sample_rate_from_combo(&sample_rate_combo),
        };
        show_export_file_dialog(&export_context, path.clone(), options);
    });
    dialog.present(Some(&context.window));
}

fn show_export_file_dialog(context: &ToolContext, path: PathBuf, options: ExportOptions) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&export_filter_name(options.format)));
    filter.add_pattern(&format!("*.{}", options.format.extension()));
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    // gtk::FileDialog keeps itself alive for the duration of the async call.
    // The previous FileChooserNative was dropped as soon as this function
    // returned, which cancelled the portal request mid-way.
    let dialog = gtk::FileDialog::builder()
        .title(gettext("Export recording"))
        .accept_label(gettext("Export"))
        .initial_name(default_export_name(&path, options.format))
        .filters(&filters)
        .default_filter(&filter)
        .modal(true)
        .build();

    let export_context = context.clone();
    let parent = context.window.clone();
    dialog.save(
        Some(&context.window),
        gio::Cancellable::NONE,
        move |result| {
            let file = match result {
                Ok(file) => file,
                Err(err)
                    if err.matches(gtk::DialogError::Dismissed)
                        || err.matches(gtk::DialogError::Cancelled) =>
                {
                    return;
                }
                Err(err) => {
                    show_notice(
                        &parent,
                        &gettext("Could not export recording"),
                        err.message(),
                    );
                    return;
                }
            };
            let Some(destination) = file.path() else {
                show_notice(
                    &parent,
                    &gettext("Could not export recording"),
                    &gettext("The selected destination is not a local file."),
                );
                return;
            };
            let destination = destination_with_extension(&destination, options.format);
            run_export_task(&export_context, path, destination, options);
        },
    );
}

fn export_format_from_combo(combo: &gtk::DropDown) -> ExportFormat {
    match combo.selected() {
        1 => ExportFormat::Wav,
        2 => ExportFormat::OggOpus,
        3 => ExportFormat::Flac,
        _ => ExportFormat::Mp3,
    }
}

fn export_quality_from_combo(combo: &gtk::DropDown) -> ExportQuality {
    match combo.selected() {
        0 => ExportQuality::Low,
        2 => ExportQuality::High,
        _ => ExportQuality::Medium,
    }
}

fn export_channel_from_combo(combo: &gtk::DropDown) -> ExportChannelMode {
    match combo.selected() {
        0 => ExportChannelMode::Original,
        2 => ExportChannelMode::Stereo,
        _ => ExportChannelMode::Mono,
    }
}

fn export_sample_rate_from_combo(combo: &gtk::DropDown) -> ExportSampleRate {
    match combo.selected() {
        1 => ExportSampleRate::Hz(48_000),
        2 => ExportSampleRate::Hz(44_100),
        3 => ExportSampleRate::Hz(24_000),
        4 => ExportSampleRate::Hz(16_000),
        _ => ExportSampleRate::Original,
    }
}

fn default_export_name(path: &Path, format: ExportFormat) -> String {
    format!("{}.{}", recording_title(path), format.extension())
}

fn destination_with_extension(destination: &Path, format: ExportFormat) -> PathBuf {
    if destination
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(format.extension()))
    {
        return destination.to_path_buf();
    }

    destination.with_extension(format.extension())
}

fn export_filter_name(format: ExportFormat) -> String {
    match format {
        ExportFormat::Wav => gettext("WAV audio"),
        ExportFormat::Mp3 => gettext("MP3 audio"),
        ExportFormat::OggOpus => gettext("Opus audio"),
        ExportFormat::Flac => gettext("FLAC audio"),
    }
}

fn voice_effect_from_combo(combo: &gtk::DropDown) -> VoiceEffectPreset {
    match combo.selected() {
        1 => VoiceEffectPreset::Deep,
        2 => VoiceEffectPreset::Robot,
        3 => VoiceEffectPreset::Radio,
        4 => VoiceEffectPreset::Echo,
        _ => VoiceEffectPreset::Child,
    }
}

fn equalizer_dialog_content(adjustments: &[gtk::Adjustment]) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.add_css_class("tool-dialog");

    let graph = equalizer_curve(adjustments);
    content.append(&graph);

    let preset_combo = choice_dropdown(
        &[
            gettext("Flat"),
            gettext("Voice clarity"),
            gettext("Warm"),
            gettext("Bright"),
            gettext("Bass cut"),
        ],
        0,
    );
    content.append(&tool_row(&gettext("Preset"), &preset_combo));

    let band_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    band_box.set_halign(gtk::Align::Center);
    band_box.add_css_class("equalizer-band-box");

    for (index, adjustment) in adjustments.iter().enumerate() {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
        column.set_halign(gtk::Align::Center);

        let value_label = gtk::Label::new(Some("0"));
        value_label.add_css_class("equalizer-value-label");

        let scale = gtk::Scale::new(gtk::Orientation::Vertical, Some(adjustment));
        scale.set_draw_value(false);
        scale.set_inverted(false);
        scale.set_size_request(24, 118);
        scale.add_css_class("equalizer-band-scale");

        let band_label = gtk::Label::new(Some(&equalizer_band_label(EQUALIZER_BANDS[index])));
        band_label.add_css_class("equalizer-band-label");

        let value_label_clone = value_label.clone();
        let graph_clone = graph.clone();
        adjustment.connect_value_changed(move |adjustment| {
            value_label_clone.set_label(&format!("{:+.0}", adjustment.value()));
            graph_clone.queue_draw();
        });

        column.append(&value_label);
        column.append(&scale);
        column.append(&band_label);
        band_box.append(&column);
    }

    let preset_adjustments = adjustments.to_vec();
    let graph_clone = graph.clone();
    preset_combo.connect_selected_notify(move |combo| {
        let gains = equalizer_preset_gains(combo.selected());
        for (adjustment, gain) in preset_adjustments.iter().zip(gains) {
            adjustment.set_value(f64::from(gain));
        }
        graph_clone.queue_draw();
    });

    content.append(&band_box);
    content
}

fn equalizer_curve(adjustments: &[gtk::Adjustment]) -> gtk::DrawingArea {
    let values = adjustments.to_vec();
    let area = gtk::DrawingArea::builder()
        .content_width(360)
        .content_height(96)
        .halign(gtk::Align::Center)
        .build();
    area.add_css_class("equalizer-curve");
    area.set_draw_func(move |widget, cr, width, height| {
        draw_equalizer_curve(widget, cr, width, height, &values);
    });
    area
}

fn draw_equalizer_curve(
    widget: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    adjustments: &[gtk::Adjustment],
) {
    let w = f64::from(width).max(1.0);
    let h = f64::from(height).max(1.0);
    let padding = 12.0;
    let plot_w = (w - padding * 2.0).max(1.0);
    let plot_h = (h - padding * 2.0).max(1.0);
    let fg = css_color(widget, "window_fg_color", (0.85, 0.88, 0.96));
    let accent = css_color(widget, "accent_bg_color", (0.45, 0.33, 1.0));

    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_width(1.0);
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.18);
    for line in 0..=4 {
        let y = padding + plot_h * f64::from(line) / 4.0;
        cr.move_to(padding, y);
        cr.line_to(w - padding, y);
    }
    let _ = cr.stroke();

    cr.set_line_width(2.4);
    cr.set_source_rgba(accent.0, accent.1, accent.2, 0.92);
    for (index, adjustment) in adjustments.iter().enumerate() {
        let x =
            padding + plot_w * index as f64 / (adjustments.len().saturating_sub(1).max(1)) as f64;
        let normalized = ((adjustment.value() + 12.0) / 24.0).clamp(0.0, 1.0);
        let y = padding + plot_h * (1.0 - normalized);
        if index == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    let _ = cr.stroke();
}

fn equalizer_preset_gains(index: u32) -> [f32; EQUALIZER_BANDS.len()] {
    match index {
        1 => [-8.0, -7.0, -5.0, -2.0, 0.0, 2.0, 4.0, 4.0, 2.0, 0.0],
        2 => [3.0, 3.0, 2.0, 1.0, 0.0, -1.0, -1.0, -2.0, -3.0, -3.0],
        3 => [-4.0, -3.0, -2.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 3.0],
        4 => [-12.0, -10.0, -8.0, -4.0, -2.0, 0.0, 1.0, 1.0, 0.0, 0.0],
        _ => [0.0; EQUALIZER_BANDS.len()],
    }
}

fn equalizer_band_label(frequency: f32) -> String {
    if frequency >= 1_000.0 {
        format!("{:.0}k", frequency / 1_000.0)
    } else {
        format!("{frequency:.0}")
    }
}

fn finish_processed_recording(
    context: &ToolContext,
    result: anyhow::Result<PathBuf>,
    error_heading: &str,
) {
    match result {
        Ok(path) => {
            let duration = processing::duration(&path).ok();
            append_recording_row(context, path, duration, true);
        }
        Err(err) => show_error(&context.window, error_heading, &err),
    }
}

/// Marks the context busy, or tells the user another task is still running.
fn try_begin_task(context: &ToolContext) -> bool {
    if context.busy.replace(true) {
        show_notice(
            &context.window,
            &gettext("Please wait"),
            &gettext("Another operation is still running."),
        );
        return false;
    }
    true
}

fn run_processed_task<F>(context: &ToolContext, error_heading: String, task: F)
where
    F: FnOnce() -> anyhow::Result<PathBuf> + Send + 'static,
{
    if !try_begin_task(context) {
        return;
    }
    if let Err(err) = context.player.borrow_mut().stop() {
        context.busy.set(false);
        show_error(&context.window, &gettext("Could not stop playback"), &err);
        return;
    }
    refresh_playback_rows(&context.recording_rows, None);

    let action_context = context.clone();
    let handle = gtk::gio::spawn_blocking(task);
    glib::MainContext::default().spawn_local(async move {
        let result = handle.await;
        action_context.busy.set(false);
        match result {
            Ok(result) => finish_processed_recording(&action_context, result, &error_heading),
            Err(_) => show_notice(
                &action_context.window,
                &error_heading,
                &gettext("The audio task stopped unexpectedly."),
            ),
        }
    });
}

fn run_export_task(
    context: &ToolContext,
    path: PathBuf,
    destination: PathBuf,
    options: ExportOptions,
) {
    if !try_begin_task(context) {
        return;
    }
    let busy = Rc::clone(&context.busy);
    let window = context.window.clone();
    let handle = gtk::gio::spawn_blocking(move || processing::export(&path, &destination, options));
    glib::MainContext::default().spawn_local(async move {
        let result = handle.await;
        busy.set(false);
        match result {
            Ok(Ok(())) => show_success(
                &window,
                &gettext("Recording exported"),
                &gettext("The file was exported successfully."),
            ),
            Ok(Err(err)) => show_error(&window, &gettext("Could not export recording"), &err),
            Err(_) => show_notice(
                &window,
                &gettext("Could not export recording"),
                &gettext("The export task stopped unexpectedly."),
            ),
        }
    });
}

fn tool_dialog(heading: &str, body: Option<&str>, content: &gtk::Box) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), body);
    dialog.set_extra_child(Some(content));
    dialog.add_response("cancel", &gettext("Cancel"));
    dialog.add_response("apply", &gettext("Apply"));
    dialog.set_default_response(Some("apply"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
    dialog
}

fn tool_content_with_illustration(
    illustration: ToolIllustration,
    rows: &[(&str, &impl IsA<gtk::Widget>)],
) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.add_css_class("tool-dialog");
    content.append(&tool_illustration(illustration));

    let fields = gtk::Box::new(gtk::Orientation::Vertical, 10);
    fields.set_hexpand(true);
    for (label, widget) in rows {
        fields.append(&tool_row(label, *widget));
    }

    content.append(&fields);
    content
}

fn tool_illustration_content(illustration: ToolIllustration) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("tool-dialog");
    content.append(&tool_illustration(illustration));
    content
}

fn tool_row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_hexpand(true);

    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);

    row.append(&text);
    row.append(widget);
    row
}

fn horizontal_value_control(
    min: f64,
    max: f64,
    step: f64,
    digits: u32,
    value: f64,
    suffix: &'static str,
) -> (gtk::Box, gtk::Adjustment) {
    let adjustment = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
    let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adjustment));
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.set_size_request(220, -1);
    scale.add_css_class("horizontal-tool-scale");

    let value_label = gtk::Label::new(Some(&slider_value_text(value, digits, suffix)));
    value_label.add_css_class("horizontal-value-label");
    value_label.set_width_chars(7);
    value_label.set_xalign(1.0);

    let value_label_clone = value_label.clone();
    adjustment.connect_value_changed(move |adjustment| {
        value_label_clone.set_label(&slider_value_text(adjustment.value(), digits, suffix));
    });

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_hexpand(true);
    content.set_valign(gtk::Align::Center);
    content.add_css_class("horizontal-value-control");
    content.append(&scale);
    content.append(&value_label);

    (content, adjustment)
}

fn slider_value_text(value: f64, digits: u32, suffix: &str) -> String {
    if digits == 0 {
        format!("{value:.0}{suffix}")
    } else {
        let value = localize_decimal(&format!("{value:.precision$}", precision = digits as usize));
        format!("{value}{suffix}")
    }
}

fn tool_illustration(kind: ToolIllustration) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(260)
        .content_height(96)
        .halign(gtk::Align::Center)
        .build();
    area.add_css_class("tool-illustration");
    area.set_draw_func(move |widget, cr, width, height| {
        let palette = IllustrationPalette::for_widget(widget);
        draw_tool_illustration(cr, width, height, kind, &palette);
    });
    area
}

fn draw_tool_illustration(
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    kind: ToolIllustration,
    palette: &IllustrationPalette,
) {
    let _ = cr.save();
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    cr.translate(0.5, 0.5);

    let w = f64::from(width).max(1.0);
    let h = f64::from(height).max(1.0);
    draw_panel_grid(cr, (w, h), palette);

    let drawing_width = 124.0;
    let _ = cr.save();
    cr.translate(((w - drawing_width) / 2.0).max(0.0), 0.0);
    match kind {
        ToolIllustration::Trim => draw_trim_illustration(cr, (drawing_width, h), palette),
        ToolIllustration::Volume => draw_volume_illustration(cr, (drawing_width, h), palette),
        ToolIllustration::Speed => draw_speed_illustration(cr, (drawing_width, h), palette),
        ToolIllustration::Pitch => draw_pitch_illustration(cr, (drawing_width, h), palette),
        ToolIllustration::VoiceEffects => {
            draw_voice_effects_illustration(cr, (drawing_width, h), palette)
        }
        ToolIllustration::Reverse => draw_reverse_illustration(cr, (drawing_width, h), palette),
        ToolIllustration::Merge => draw_merge_illustration(cr, (drawing_width, h), palette),
        ToolIllustration::Export => draw_export_illustration(cr, (drawing_width, h), palette),
    }
    let _ = cr.restore();

    let _ = cr.restore();
}

/// Illustration colors resolved from the current theme, so the drawings follow
/// light/dark mode and the user's accent color.
struct IllustrationPalette {
    source: (f64, f64, f64, f64),
    result: (f64, f64, f64, f64),
    arrow: (f64, f64, f64, f64),
    cut: (f64, f64, f64, f64),
    grid: (f64, f64, f64, f64),
}

impl IllustrationPalette {
    fn for_widget(widget: &impl IsA<gtk::Widget>) -> Self {
        let fg = css_color(widget, "window_fg_color", (0.85, 0.88, 0.96));
        let accent = css_color(widget, "accent_color", (0.21, 0.52, 0.89));
        let destructive = css_color(widget, "destructive_color", (0.88, 0.11, 0.14));
        Self {
            source: (fg.0, fg.1, fg.2, 0.55),
            result: (accent.0, accent.1, accent.2, 0.95),
            arrow: (fg.0, fg.1, fg.2, 0.8),
            cut: (destructive.0, destructive.1, destructive.2, 0.95),
            grid: (fg.0, fg.1, fg.2, 0.12),
        }
    }
}

fn draw_panel_grid(cr: &gtk::cairo::Context, size: (f64, f64), palette: &IllustrationPalette) {
    let (w, h) = size;
    set_source(cr, palette.grid);
    cr.set_line_width(1.0);
    for line in 1..4 {
        let y = h * f64::from(line) / 4.0;
        cr.move_to(8.0, y);
        cr.line_to(w - 8.0, y);
    }
    let _ = cr.stroke();
}

fn draw_trim_illustration(
    cr: &gtk::cairo::Context,
    size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_wave(
        cr,
        (13.0, 48.0),
        (98.0, 40.0),
        &[0.25, 0.6, 0.35, 0.85, 0.5, 0.75, 0.3],
        false,
        palette,
    );
    draw_cut_line(cr, 40.0, size.1, palette);
    draw_cut_line(cr, 82.0, size.1, palette);
    draw_arrow(cr, (48.0, 68.0), (74.0, 68.0), palette.arrow);
}

fn draw_volume_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_bars(cr, 16.0, 64.0, &[0.18, 0.3, 0.22, 0.28], palette.source);
    draw_arrow(cr, (52.0, 52.0), (72.0, 52.0), palette.arrow);
    draw_bars(cr, 82.0, 64.0, &[0.35, 0.7, 0.45, 0.85], palette.result);
}

fn draw_speed_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_ticks(cr, 14.0, 48.0, 9.0, 6, palette.source);
    draw_arrow(cr, (50.0, 48.0), (70.0, 48.0), palette.arrow);
    draw_ticks(cr, 82.0, 48.0, 4.5, 7, palette.result);
}

fn draw_pitch_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_sine(cr, (14.0, 58.0), (38.0, 18.0), 1.2, palette.source);
    draw_arrow(cr, (54.0, 48.0), (72.0, 48.0), palette.arrow);
    draw_sine(cr, (82.0, 45.0), (34.0, 28.0), 2.3, palette.result);
}

fn draw_voice_effects_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_sine(cr, (12.0, 58.0), (34.0, 18.0), 1.4, palette.source);
    draw_arrow(cr, (47.0, 48.0), (64.0, 48.0), palette.arrow);
    draw_sine(cr, (72.0, 45.0), (22.0, 28.0), 2.8, palette.result);
    set_source(cr, palette.source);
    cr.set_line_width(2.0);
    cr.arc(104.0, 38.0, 7.0, 0.0, std::f64::consts::TAU);
    cr.arc(104.0, 61.0, 7.0, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
}

fn draw_reverse_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_wave(
        cr,
        (14.0, 48.0),
        (38.0, 36.0),
        &[0.25, 0.45, 0.8, 0.35],
        false,
        palette,
    );
    draw_arrow(cr, (72.0, 48.0), (52.0, 48.0), palette.arrow);
    draw_wave(
        cr,
        (78.0, 48.0),
        (36.0, 36.0),
        &[0.35, 0.8, 0.45, 0.25],
        true,
        palette,
    );
}

fn draw_merge_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_wave(
        cr,
        (12.0, 34.0),
        (36.0, 24.0),
        &[0.25, 0.55, 0.35, 0.65],
        false,
        palette,
    );
    draw_wave(
        cr,
        (12.0, 64.0),
        (36.0, 24.0),
        &[0.65, 0.35, 0.55, 0.25],
        false,
        palette,
    );
    draw_arrow(cr, (52.0, 48.0), (72.0, 48.0), palette.arrow);
    draw_wave(
        cr,
        (78.0, 48.0),
        (36.0, 38.0),
        &[0.25, 0.55, 0.35, 0.65, 0.65, 0.35],
        false,
        palette,
    );
}

fn draw_export_illustration(
    cr: &gtk::cairo::Context,
    _size: (f64, f64),
    palette: &IllustrationPalette,
) {
    draw_wave(
        cr,
        (12.0, 52.0),
        (42.0, 36.0),
        &[0.25, 0.65, 0.35, 0.85, 0.45],
        false,
        palette,
    );
    draw_arrow(cr, (56.0, 52.0), (76.0, 52.0), palette.arrow);
    set_source(cr, palette.result);
    cr.set_line_width(2.0);
    cr.rectangle(84.0, 25.0, 25.0, 42.0);
    cr.move_to(99.0, 25.0);
    cr.line_to(109.0, 35.0);
    let _ = cr.stroke();
    draw_wave(
        cr,
        (88.0, 51.0),
        (17.0, 16.0),
        &[0.2, 0.55, 0.3, 0.45],
        false,
        palette,
    );
}

fn draw_wave(
    cr: &gtk::cairo::Context,
    origin: (f64, f64),
    size: (f64, f64),
    amplitudes: &[f64],
    reversed: bool,
    palette: &IllustrationPalette,
) {
    let color = if reversed {
        palette.result
    } else {
        palette.source
    };
    draw_bars(cr, origin.0, origin.1 + size.1 / 2.0, amplitudes, color);
}

fn draw_bars(
    cr: &gtk::cairo::Context,
    x: f64,
    center_y: f64,
    amplitudes: &[f64],
    color: (f64, f64, f64, f64),
) {
    set_source(cr, color);
    cr.set_line_width(3.0);
    for (index, amplitude) in amplitudes.iter().enumerate() {
        let bar_x = x + index as f64 * 8.0;
        let height = 34.0 * amplitude;
        cr.move_to(bar_x, center_y - height / 2.0);
        cr.line_to(bar_x, center_y + height / 2.0);
    }
    let _ = cr.stroke();
}

fn draw_ticks(
    cr: &gtk::cairo::Context,
    x: f64,
    center_y: f64,
    spacing: f64,
    count: usize,
    color: (f64, f64, f64, f64),
) {
    set_source(cr, color);
    cr.set_line_width(2.5);
    for index in 0..count {
        let tick_x = x + index as f64 * spacing;
        cr.move_to(tick_x, center_y - 15.0);
        cr.line_to(tick_x, center_y + 15.0);
    }
    let _ = cr.stroke();
}

fn draw_sine(
    cr: &gtk::cairo::Context,
    origin: (f64, f64),
    size: (f64, f64),
    cycles: f64,
    color: (f64, f64, f64, f64),
) {
    set_source(cr, color);
    cr.set_line_width(2.0);
    let steps = 28;
    for step in 0..=steps {
        let t = f64::from(step) / f64::from(steps);
        let x = origin.0 + t * size.0;
        let y = origin.1 + (t * cycles * std::f64::consts::TAU).sin() * size.1 / 2.0;
        if step == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    let _ = cr.stroke();
}

fn draw_arrow(
    cr: &gtk::cairo::Context,
    start: (f64, f64),
    end: (f64, f64),
    color: (f64, f64, f64, f64),
) {
    set_source(cr, color);
    cr.set_line_width(2.0);
    cr.move_to(start.0, start.1);
    cr.line_to(end.0, end.1);
    let _ = cr.stroke();

    let direction = if end.0 >= start.0 { 1.0 } else { -1.0 };
    cr.move_to(end.0, end.1);
    cr.line_to(end.0 - direction * 7.0, end.1 - 5.0);
    cr.line_to(end.0 - direction * 7.0, end.1 + 5.0);
    cr.close_path();
    let _ = cr.fill();
}

fn draw_cut_line(cr: &gtk::cairo::Context, x: f64, height: f64, palette: &IllustrationPalette) {
    set_source(cr, palette.cut);
    cr.set_line_width(2.0);
    cr.move_to(x, 18.0);
    cr.line_to(x, height - 18.0);
    let _ = cr.stroke();
}

fn set_source(cr: &gtk::cairo::Context, color: (f64, f64, f64, f64)) {
    cr.set_source_rgba(color.0, color.1, color.2, color.3);
}

fn css_color(
    widget: &impl IsA<gtk::Widget>,
    name: &str,
    fallback: (f64, f64, f64),
) -> (f64, f64, f64) {
    crate::ui::style::theme_color(widget, name, fallback)
}

fn choice_dropdown<S: AsRef<str>>(items: &[S], selected: u32) -> gtk::DropDown {
    let labels: Vec<&str> = items.iter().map(AsRef::as_ref).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    dropdown.set_selected(selected);
    dropdown
}

fn number_spin(min: f64, max: f64, step: f64, digits: u32, value: f64) -> gtk::SpinButton {
    let adjustment = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
    gtk::SpinButton::new(Some(&adjustment), step, digits)
}

fn tool_tooltip(action: ToolAction) -> String {
    match action {
        ToolAction::Trim => gettext("Trim the selected recording"),
        ToolAction::Volume => gettext("Change the selected recording volume"),
        ToolAction::Speed => gettext("Change the selected recording speed"),
        ToolAction::Pitch => gettext("Change the selected recording pitch"),
        ToolAction::VoiceEffects => gettext("Apply a voice effect to the selected recording"),
        ToolAction::Equalizer => gettext("Adjust the selected recording frequency bands"),
        ToolAction::Reverse => gettext("Reverse the selected recording"),
        ToolAction::Merge => gettext("Merge the selected recording with another recording"),
        ToolAction::Export => gettext("Export the selected recording"),
    }
}

fn refresh_playback_rows(recording_rows: &RecordingRows, status: Option<&PlaybackStatus>) {
    let playing_path = status.map(|status| status.path.as_path());
    let progress = status.map(playback_progress).unwrap_or(0.0);

    for row in recording_rows.borrow().iter() {
        let is_playing = playing_path.is_some_and(|path| path == row.path.as_path());
        row.playback_symbol
            .set_visible_child_name(if is_playing { "pause" } else { "play" });
        row.playback_bar
            .set_progress(if is_playing { progress } else { 0.0 });
    }
}

fn playback_progress(status: &PlaybackStatus) -> f64 {
    let Some(duration) = status.duration else {
        return 0.0;
    };
    if duration <= Duration::ZERO {
        return 0.0;
    }

    (status.position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
}

fn recording_title(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| gettext("Recording"))
}

fn recording_subtitle(_duration: Option<Duration>) -> String {
    gettext("Saved recording")
}

/// Formats a recording's modification time as a localized date and time.
fn recording_date(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let seconds = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let datetime = glib::DateTime::from_unix_local(seconds).ok()?;
    datetime
        .format("%x %R")
        .ok()
        .map(|formatted| formatted.to_string())
}

fn row_playback_symbol(paused: bool) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(24)
        .content_height(24)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    area.set_draw_func(move |widget, cr, width, height| {
        let fg = css_color(widget, "accent_fg_color", (1.0, 1.0, 1.0));
        cr.set_source_rgb(fg.0, fg.1, fg.2);
        let w = f64::from(width);
        let h = f64::from(height);
        let cx = w / 2.0;
        let cy = h / 2.0;

        if paused {
            let bar_width = 4.0;
            let bar_height = 13.0;
            let gap = 4.0;
            cr.rectangle(
                cx - gap / 2.0 - bar_width,
                cy - bar_height / 2.0,
                bar_width,
                bar_height,
            );
            cr.rectangle(cx + gap / 2.0, cy - bar_height / 2.0, bar_width, bar_height);
        } else {
            cr.move_to(cx - 4.0, cy - 7.0);
            cr.line_to(cx - 4.0, cy + 7.0);
            cr.line_to(cx + 8.0, cy);
            cr.close_path();
        }
        let _ = cr.fill();
    });
    area
}

fn pill_content(icon: &str, label: &str) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.set_halign(gtk::Align::Center);
    content.set_valign(gtk::Align::Center);

    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);

    let text = gtk::Label::new(Some(label));
    text.add_css_class("tool-label");

    content.append(&image);
    content.append(&text);
    content
}

fn tool_card(icon: &str, label: &str, description: &str) -> gtk::Button {
    // Fill the whole button width so the icon is always pinned to the left
    // edge (otherwise the button centres the icon+text group, which shifts the
    // icon depending on each label's length).
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.set_valign(gtk::Align::Center);
    content.set_hexpand(true);

    // Fixed-size icon tile so every tool button shares the exact same badge.
    let icon_holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    icon_holder.add_css_class("tool-card-icon");
    icon_holder.set_size_request(36, 36);
    icon_holder.set_halign(gtk::Align::Start);
    icon_holder.set_valign(gtk::Align::Center);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    image.set_halign(gtk::Align::Center);
    image.set_valign(gtk::Align::Center);
    image.set_hexpand(true);
    image.set_vexpand(true);
    icon_holder.append(&image);

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text_box.set_hexpand(true);
    text_box.set_valign(gtk::Align::Center);

    let title = gtk::Label::new(Some(label));
    title.add_css_class("tool-label");
    title.set_xalign(0.0);

    let desc = gtk::Label::new(Some(description));
    desc.add_css_class("tool-desc");
    desc.set_xalign(0.0);
    desc.set_ellipsize(gtk::pango::EllipsizeMode::End);

    text_box.append(&title);
    text_box.append(&desc);

    content.append(&icon_holder);
    content.append(&text_box);

    let button = gtk::Button::new();
    button.add_css_class("tool-card");
    button.set_hexpand(true);
    button.set_child(Some(&content));
    button
}

#[derive(Clone, Copy)]
enum MessageKind {
    Info,
    Success,
    Error,
}

impl MessageKind {
    fn icon_name(self) -> &'static str {
        match self {
            Self::Info => "dialog-information-symbolic",
            Self::Success => "emblem-ok-symbolic",
            Self::Error => "dialog-error-symbolic",
        }
    }

    fn css_class(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Error => "error",
        }
    }
}

fn show_error(window: &adw::ApplicationWindow, heading: &str, err: &anyhow::Error) {
    show_message(window, heading, &err.to_string(), MessageKind::Error);
}

fn show_notice(window: &adw::ApplicationWindow, heading: &str, body: &str) {
    show_message(window, heading, body, MessageKind::Info);
}

fn show_success(window: &adw::ApplicationWindow, heading: &str, body: &str) {
    show_message(window, heading, body, MessageKind::Success);
}

fn show_message(window: &adw::ApplicationWindow, heading: &str, body: &str, kind: MessageKind) {
    let dialog = adw::AlertDialog::new(Some(heading), None);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.add_css_class("message-content");
    content.set_halign(gtk::Align::Center);

    let icon = gtk::Image::from_icon_name(kind.icon_name());
    icon.set_pixel_size(46);
    icon.add_css_class("message-icon");
    icon.add_css_class(kind.css_class());

    let body_label = gtk::Label::new(Some(body));
    body_label.add_css_class("message-body");
    body_label.set_wrap(true);
    body_label.set_justify(gtk::Justification::Center);
    body_label.set_xalign(0.5);

    content.append(&icon);
    content.append(&body_label);
    dialog.set_extra_child(Some(&content));
    dialog.add_response("ok", &gettext("OK"));
    dialog.set_default_response(Some("ok"));
    dialog.present(Some(window));
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn format_compact_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;

    format!("{minutes:02}:{seconds:02}")
}
