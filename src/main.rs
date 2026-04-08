//! Larris – GRBL laser terminal – Relm4/GTK4 GUI entry point.
//!
//! Architecture:
//!  - Relm4 0.9 Component drives the GTK4 main loop.
//!  - Serial I/O runs on a dedicated OS thread (see `serial.rs`).
//!  - A 50 ms glib timer fires AppMsg::Tick to drain serial events and
//!    forward pending RGBA preview images to GTK textures.
//!  - SVG→GCode conversion and GCode→image rasterisation run synchronously
//!    on the GTK main thread (they are fast enough for typical files).
//!  - File dialogs use gtk::FileChooserNative (compatible with GTK 4.6+).

mod actions;
mod app;
mod converter;
mod grbl;
mod serial;
mod svg2gcode;

use std::time::Duration;

use relm4::gtk::prelude::*;
use relm4::gtk::{self, gdk, glib};
use relm4::prelude::*;

use actions::{
    do_abort_stream, do_connect, do_convert, do_disconnect, do_frame_job, do_home, do_jog,
    do_poll_status, do_refresh_ports, do_render_preview, do_send_gcode, drain_serial_events,
    load_file, save_gcode_to, send_realtime, send_serial, send_serial_raw, tick_status_poll,
};
use app::{ActiveTab, App, AppMode, BaudRate, ConversionStatus, LineKind, MachineSettings};
use grbl::JogDir;
use serial::discover_ports;

// ── Model ─────────────────────────────────────────────────────────────────────

struct AppComponent {
    app: App,
    /// Pending SVG/source preview image waiting to be uploaded to the GTK Picture.
    pending_svg_image: Option<image::RgbaImage>,
    /// Pending GCode toolpath preview image.
    pending_gcode_image: Option<image::RgbaImage>,
    /// How many console lines have already been appended to the TextView buffer.
    console_synced_len: usize,
    /// False when gcode_text has changed; sync_widgets resets it to true.
    gcode_synced: bool,
    /// Cached port list length – used to detect when the list needs rebuilding.
    port_list_synced_len: usize,
    /// Cached layer count.
    layers_synced_len: usize,
    /// Cached grbl_settings count.
    grbl_settings_synced_len: usize,
}

impl AppComponent {
    // ── Private message handler ───────────────────────────────────────────
    // Called from both `update` (trait) and `update_with_view` (override).
    fn handle_message(&mut self, msg: AppMsg) {
        match msg {
            // ── Timer ─────────────────────────────────────────────────────
            AppMsg::Tick => {
                drain_serial_events(&mut self.app);
                tick_status_poll(&mut self.app);
                self.app.tick_status();
                if let Some(img) = self.app.preview_image.take() {
                    self.pending_svg_image = Some(img);
                }
                if let Some(img) = self.app.gcode_preview_image.take() {
                    self.pending_gcode_image = Some(img);
                }
            }

            // ── Navigation ────────────────────────────────────────────────
            AppMsg::TabSwitched(p) => {
                self.app.active_tab = match p {
                    0 => ActiveTab::Connect,
                    1 => ActiveTab::Control,
                    2 => ActiveTab::GCode,
                    3 => ActiveTab::Preview,
                    4 => ActiveTab::Settings,
                    _ => ActiveTab::Connect,
                };
            }

            // ── Connect tab ───────────────────────────────────────────────
            AppMsg::PortSelected(i) => {
                self.app.port_list_selected = Some(i as usize);
            }
            AppMsg::BaudSelected(i) => {
                self.app.selected_baud_idx = i as usize;
            }
            AppMsg::Connect => do_connect(&mut self.app),
            AppMsg::Disconnect => do_disconnect(&mut self.app),
            AppMsg::RefreshPorts => do_refresh_ports(&mut self.app),
            AppMsg::Home => do_home(&mut self.app),
            AppMsg::CommandSubmit => {
                let line = self.app.input_submit();
                if !line.trim().is_empty() {
                    send_serial(&mut self.app, line);
                }
            }

            // ── GCode tab ─────────────────────────────────────────────────
            // OpenFile / SaveGCode are handled in update_with_view (need root).
            AppMsg::OpenFile | AppMsg::SaveGCode => {}
            AppMsg::FileChosen(path) => {
                load_file(&mut self.app, &path);
                self.gcode_synced = false;
                self.layers_synced_len = 0;
            }
            AppMsg::Convert => {
                do_convert(&mut self.app);
                self.gcode_synced = false;
            }
            AppMsg::SendGCode => do_send_gcode(&mut self.app),
            AppMsg::AbortStream => do_abort_stream(&mut self.app),
            AppMsg::FrameJob => do_frame_job(&mut self.app),
            AppMsg::GCodeSavePath(path) => save_gcode_to(&mut self.app, &path),
            AppMsg::ToggleTravelLines(v) => self.app.show_travel_lines = v,
            AppMsg::ToggleInvertImage(v) => self.app.invert_image = v,
            AppMsg::LayerSelected(i) => self.app.layer_selected = i as usize,

            // ── Control tab ───────────────────────────────────────────────
            AppMsg::JogMove(dir) => do_jog(&mut self.app, dir),
            AppMsg::JogStepLarger => self.app.jog_step_larger(),
            AppMsg::JogStepSmaller => self.app.jog_step_smaller(),
            AppMsg::PollStatus => do_poll_status(&mut self.app),
            AppMsg::SendRealtime(b) => send_realtime(&mut self.app, b),
            AppMsg::SendRaw(s) => send_serial_raw(&mut self.app, &s),

            // ── Preview tab ───────────────────────────────────────────────
            AppMsg::RenderPreview => do_render_preview(&mut self.app),

            // ── Settings tab ──────────────────────────────────────────────
            AppMsg::SettingChanged { idx, value } => {
                let _ = self.app.machine_settings.set_field(idx, &value);
            }
            AppMsg::SettingToggleBool(idx) => {
                self.app.settings_selected = idx;
                self.app.settings_toggle_bool();
            }

            // ── Error popup ───────────────────────────────────────────────
            AppMsg::DismissError => self.app.dismiss_conversion_error(),
        }
    }

    // ── Private widget sync ───────────────────────────────────────────────
    // Needs &mut self so we can take() pending images.
    fn sync_widgets(&mut self, widgets: &mut AppWidgets, sender: &ComponentSender<AppComponent>) {
        let app = &self.app;

        // ── Status bar ────────────────────────────────────────────────────
        widgets
            .status_label
            .set_label(app.status_message.as_deref().unwrap_or("Ready"));

        // ── Connect / Disconnect buttons ──────────────────────────────────
        let connected = app.mode == AppMode::Connected;
        widgets.connect_btn.set_sensitive(!connected);
        widgets.disconnect_btn.set_sensitive(connected);

        // ── Port listbox — rebuild when list changes ──────────────────────
        if app.port_list.len() != self.port_list_synced_len {
            while let Some(child) = widgets.port_listbox.first_child() {
                widgets.port_listbox.remove(&child);
            }
            for port in &app.port_list {
                let lbl = gtk::Label::new(Some(port));
                lbl.set_xalign(0.0);
                lbl.set_margin_all(4);
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&lbl));
                widgets.port_listbox.append(&row);
            }
            self.port_list_synced_len = app.port_list.len();
        }
        if let Some(sel) = app.port_list_selected {
            if let Some(row) = widgets.port_listbox.row_at_index(sel as i32) {
                widgets.port_listbox.select_row(Some(&row));
            }
        }

        // ── Baud dropdown ─────────────────────────────────────────────────
        widgets
            .baud_dropdown
            .set_selected(app.selected_baud_idx as u32);

        // ── Console — append only new lines ──────────────────────────────
        if app.console_lines.len() > self.console_synced_len {
            let buf = &widgets.console_buffer;
            for line in app.console_lines.iter().skip(self.console_synced_len) {
                let mut end = buf.end_iter();
                let prefix = match line.kind {
                    LineKind::Sent => "→ ",
                    LineKind::Received => "← ",
                    LineKind::Info => "  ",
                    LineKind::Error => "! ",
                };
                buf.insert(&mut end, &format!("{}{}\n", prefix, line.text));
            }
            self.console_synced_len = app.console_lines.len();
            if app.console_follow {
                if let Some(adj) = widgets.console_view.vadjustment() {
                    adj.set_value(adj.upper() - adj.page_size());
                }
            }
        }

        // Clear command entry after submit
        if widgets.command_entry.text() == app.input_buffer.as_str() {
            // nothing to do
        } else {
            widgets.command_entry.set_text(&app.input_buffer);
        }

        // ── GCode text ────────────────────────────────────────────────────
        if !self.gcode_synced {
            widgets
                .gcode_buffer
                .set_text(app.gcode_text.as_deref().unwrap_or(""));
            self.gcode_synced = true;
        }

        // ── File label ────────────────────────────────────────────────────
        let file_str = app
            .svg_path
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No file loaded".to_string());
        widgets.file_label.set_label(&file_str);

        // ── Conversion status label ───────────────────────────────────────
        let conv_text = match &app.conversion_status {
            ConversionStatus::Idle => "—".to_string(),
            ConversionStatus::Running => "⏳ Converting…".to_string(),
            ConversionStatus::Ok => {
                let n = app
                    .gcode_text
                    .as_ref()
                    .map(|g| g.lines().count())
                    .unwrap_or(0);
                format!("✓ {n} lines")
            }
            ConversionStatus::Failed(e) => format!("✗ {e}"),
        };
        widgets.conversion_status_label.set_label(&conv_text);

        // ── Stream progress ───────────────────────────────────────────────
        widgets.stream_progress.set_visible(app.is_streaming);
        if app.is_streaming && app.stream_total > 0 {
            widgets
                .stream_progress
                .set_fraction(app.stream_sent as f64 / app.stream_total as f64);
            widgets
                .stream_progress
                .set_text(Some(&format!("{}/{}", app.stream_sent, app.stream_total)));
        }

        // ── GCode action buttons ──────────────────────────────────────────
        let has_gcode = app.gcode_text.is_some();
        let has_file = app.svg_path.is_some();
        widgets.convert_btn.set_sensitive(has_file);
        widgets
            .send_btn
            .set_sensitive(has_gcode && connected && !app.is_streaming);
        widgets.abort_btn.set_sensitive(app.is_streaming);
        widgets.frame_btn.set_sensitive(has_gcode && connected);
        widgets.save_btn.set_sensitive(has_gcode);

        // ── Layer listbox — rebuild when layers change ────────────────────
        if app.layers.len() != self.layers_synced_len {
            while let Some(child) = widgets.layer_listbox.first_child() {
                widgets.layer_listbox.remove(&child);
            }
            for layer in &app.layers {
                let text = format!(
                    "{} — {}",
                    layer.label,
                    layer.summary(
                        app.machine_settings.feedrate,
                        app.machine_settings.laser_power,
                    )
                );
                let lbl = gtk::Label::new(Some(&text));
                lbl.set_xalign(0.0);
                lbl.set_margin_all(4);
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&lbl));
                widgets.layer_listbox.append(&row);
            }
            self.layers_synced_len = app.layers.len();
        }
        if !app.layers.is_empty() {
            if let Some(row) = widgets
                .layer_listbox
                .row_at_index(app.layer_selected as i32)
            {
                widgets.layer_listbox.select_row(Some(&row));
            }
        }

        // ── GRBL status label ─────────────────────────────────────────────
        let status_text = match &app.grbl_status {
            None => "Not connected / no status received".to_string(),
            Some(st) => {
                let pos_str = st
                    .work_pos()
                    .map(|p| format!("X:{:.3}  Y:{:.3}  Z:{:.3}", p.x, p.y, p.z))
                    .unwrap_or_else(|| "position unknown".to_string());
                format!(
                    "State : {}\nPos   : {}\nFeed% : {}   Laser% : {}",
                    st.state.label(),
                    pos_str,
                    app.override_feed,
                    app.override_spindle,
                )
            }
        };
        widgets.grbl_status_label.set_label(&status_text);

        // ── Jog step label ────────────────────────────────────────────────
        widgets
            .jog_step_label
            .set_label(&format!("{} mm", app.jog_step_mm()));

        // ── GRBL settings listbox — rebuild when entries change ───────────
        if app.grbl_settings.len() != self.grbl_settings_synced_len {
            while let Some(child) = widgets.grbl_settings_listbox.first_child() {
                widgets.grbl_settings_listbox.remove(&child);
            }
            for (key, val) in &app.grbl_settings {
                let text = format!("{} = {}", key, val);
                let lbl = gtk::Label::new(Some(&text));
                lbl.set_xalign(0.0);
                lbl.set_margin_all(2);
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&lbl));
                widgets.grbl_settings_listbox.append(&row);
            }
            self.grbl_settings_synced_len = app.grbl_settings.len();
        }

        // ── Machine settings grid (16 fields, cheap to rebuild each tick) ─
        while let Some(child) = widgets.settings_box.first_child() {
            widgets.settings_box.remove(&child);
        }
        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        grid.set_margin_all(8);
        for i in 0..MachineSettings::field_count() {
            let name = MachineSettings::FIELD_NAMES[i];
            let val = app.machine_settings.field_value(i);

            let name_lbl = gtk::Label::new(Some(name));
            name_lbl.set_xalign(1.0);
            grid.attach(&name_lbl, 0, i as i32, 1, 1);

            if i >= 13 {
                // Boolean field — use CheckButton
                let check = gtk::CheckButton::new();
                check.set_active(val == "true");
                let s = sender.clone();
                check.connect_toggled(move |_| {
                    s.input(AppMsg::SettingToggleBool(i));
                });
                grid.attach(&check, 1, i as i32, 1, 1);
            } else {
                let entry = gtk::Entry::new();
                entry.set_text(&val);
                entry.set_hexpand(true);
                let s = sender.clone();
                entry.connect_changed(move |e| {
                    s.input(AppMsg::SettingChanged {
                        idx: i,
                        value: e.text().to_string(),
                    });
                });
                grid.attach(&entry, 1, i as i32, 1, 1);
            }
        }
        widgets.settings_box.append(&grid);

        // ── Preview images ────────────────────────────────────────────────
        if let Some(img) = self.pending_svg_image.take() {
            let texture = rgba_to_gdk_texture(&img);
            widgets.svg_picture.set_paintable(Some(&texture));
        }
        if let Some(img) = self.pending_gcode_image.take() {
            let texture = rgba_to_gdk_texture(&img);
            widgets.gcode_picture.set_paintable(Some(&texture));
        }

        // ── Error popup ───────────────────────────────────────────────────
        if let Some(popup) = app.conversion_error_popup.clone() {
            let dialog = gtk::MessageDialog::builder()
                .transient_for(&widgets.root)
                .modal(true)
                .message_type(gtk::MessageType::Error)
                .buttons(gtk::ButtonsType::Ok)
                .text(popup.title.as_str())
                .secondary_text(popup.body.as_str())
                .build();
            let s = sender.clone();
            dialog.connect_response(move |dlg, _| {
                dlg.close();
                s.input(AppMsg::DismissError);
            });
            dialog.show();
        }
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum AppMsg {
    Tick,
    TabSwitched(u32),
    // Connect tab
    PortSelected(u32),
    BaudSelected(u32),
    Connect,
    Disconnect,
    RefreshPorts,
    Home,
    CommandSubmit,
    // GCode tab
    OpenFile,
    FileChosen(std::path::PathBuf),
    Convert,
    SendGCode,
    AbortStream,
    FrameJob,
    SaveGCode,
    GCodeSavePath(std::path::PathBuf),
    ToggleTravelLines(bool),
    ToggleInvertImage(bool),
    LayerSelected(u32),
    // Control tab
    JogMove(JogDir),
    JogStepLarger,
    JogStepSmaller,
    PollStatus,
    SendRealtime(u8),
    SendRaw(String),
    // Preview tab
    RenderPreview,
    // Settings tab
    SettingChanged { idx: usize, value: String },
    SettingToggleBool(usize),
    // Error popup
    DismissError,
}

// ── Widgets ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct AppWidgets {
    root: gtk::Window,
    notebook: gtk::Notebook,
    // Connect tab
    port_listbox: gtk::ListBox,
    baud_dropdown: gtk::DropDown,
    connect_btn: gtk::Button,
    disconnect_btn: gtk::Button,
    console_buffer: gtk::TextBuffer,
    console_view: gtk::TextView,
    command_entry: gtk::Entry,
    // GCode tab
    file_label: gtk::Label,
    convert_btn: gtk::Button,
    send_btn: gtk::Button,
    abort_btn: gtk::Button,
    frame_btn: gtk::Button,
    save_btn: gtk::Button,
    travel_check: gtk::CheckButton,
    invert_check: gtk::CheckButton,
    layer_listbox: gtk::ListBox,
    gcode_buffer: gtk::TextBuffer,
    stream_progress: gtk::ProgressBar,
    conversion_status_label: gtk::Label,
    // Preview tab
    svg_picture: gtk::Picture,
    gcode_picture: gtk::Picture,
    // Control tab
    grbl_status_label: gtk::Label,
    jog_step_label: gtk::Label,
    // Settings tab
    settings_box: gtk::Box,
    grbl_settings_listbox: gtk::ListBox,
    // Status bar
    status_label: gtk::Label,
}

// ── Component impl ────────────────────────────────────────────────────────────

impl Component for AppComponent {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = ();
    type Root = gtk::Window;
    type Widgets = AppWidgets;

    fn init_root() -> gtk::Window {
        gtk::Window::builder()
            .title("Larris – GRBL Laser Terminal")
            .default_width(1200)
            .default_height(800)
            .build()
    }

    // Note: Relm4 0.9 passes `root` by value (owned).
    fn init(_init: (), root: gtk::Window, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        // ── Initial model ─────────────────────────────────────────────────
        let mut app = App::new();
        app.port_list = discover_ports();
        if app.port_list.is_empty() {
            app.push_info("No serial ports found. Click Refresh to scan.");
        } else {
            app.push_info(format!("Found {} serial port(s).", app.port_list.len()));
            app.port_list_selected = Some(0);
        }
        app.push_info("Welcome to Larris – GRBL Laser Terminal.");

        let model = AppComponent {
            app,
            pending_svg_image: None,
            pending_gcode_image: None,
            console_synced_len: 0,
            gcode_synced: true,
            port_list_synced_len: 0,
            layers_synced_len: 0,
            grbl_settings_synced_len: 0,
        };

        // ── Outer layout ──────────────────────────────────────────────────
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);

        // ── Notebook ─────────────────────────────────────────────────────
        let notebook = gtk::Notebook::new();
        notebook.set_hexpand(true);
        notebook.set_vexpand(true);
        {
            let tx = sender.input_sender().clone();
            notebook.connect_switch_page(move |_, _, page| {
                let _ = tx.send(AppMsg::TabSwitched(page));
            });
        }

        let (connect_page, connect_w) = build_connect_page(&sender);
        notebook.append_page(&connect_page, Some(&gtk::Label::new(Some("Connect"))));

        let (control_page, ctrl_w) = build_control_page(&sender);
        notebook.append_page(&control_page, Some(&gtk::Label::new(Some("Control"))));

        let (gcode_page, gcode_w) = build_gcode_page(&sender);
        notebook.append_page(&gcode_page, Some(&gtk::Label::new(Some("GCode"))));

        let (preview_page, preview_w) = build_preview_page(&sender);
        notebook.append_page(&preview_page, Some(&gtk::Label::new(Some("Preview"))));

        let (settings_page, settings_w) = build_settings_page(&sender, &model.app.machine_settings);
        notebook.append_page(&settings_page, Some(&gtk::Label::new(Some("Settings"))));

        outer.append(&notebook);

        // ── Status bar ────────────────────────────────────────────────────
        let status_label = gtk::Label::new(Some("Ready"));
        status_label.set_xalign(0.0);
        status_label.set_margin_start(8);
        status_label.set_margin_end(8);
        status_label.set_margin_top(3);
        status_label.set_margin_bottom(3);
        outer.append(&status_label);

        root.set_child(Some(&outer));

        // ── 50 ms timer ───────────────────────────────────────────────────
        glib::timeout_add_local(Duration::from_millis(50), {
            let tx = sender.input_sender().clone();
            move || {
                if tx.send(AppMsg::Tick).is_err() {
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }
        });

        let widgets = AppWidgets {
            root: root.clone(),
            notebook,
            port_listbox: connect_w.port_listbox,
            baud_dropdown: connect_w.baud_dropdown,
            connect_btn: connect_w.connect_btn,
            disconnect_btn: connect_w.disconnect_btn,
            console_buffer: connect_w.console_buffer,
            console_view: connect_w.console_view,
            command_entry: connect_w.command_entry,
            file_label: gcode_w.file_label,
            convert_btn: gcode_w.convert_btn,
            send_btn: gcode_w.send_btn,
            abort_btn: gcode_w.abort_btn,
            frame_btn: gcode_w.frame_btn,
            save_btn: gcode_w.save_btn,
            travel_check: gcode_w.travel_check,
            invert_check: gcode_w.invert_check,
            layer_listbox: gcode_w.layer_listbox,
            gcode_buffer: gcode_w.gcode_buffer,
            stream_progress: gcode_w.stream_progress,
            conversion_status_label: gcode_w.conversion_status_label,
            svg_picture: preview_w.svg_picture,
            gcode_picture: preview_w.gcode_picture,
            grbl_status_label: ctrl_w.grbl_status_label,
            jog_step_label: ctrl_w.jog_step_label,
            settings_box: settings_w.settings_box,
            grbl_settings_listbox: ctrl_w.grbl_settings_listbox,
            status_label,
        };

        ComponentParts { model, widgets }
    }

    // Required by trait; all logic is in update_with_view.
    fn update(&mut self, msg: AppMsg, _sender: ComponentSender<Self>, _root: &gtk::Window) {
        self.handle_message(msg);
    }

    // Override to get &mut self for image taking and to handle file dialogs
    // (which need the root window reference).
    fn update_with_view(
        &mut self,
        widgets: &mut AppWidgets,
        msg: AppMsg,
        sender: ComponentSender<Self>,
        _root: &gtk::Window,
    ) {
        let open_file = matches!(msg, AppMsg::OpenFile);
        let save_gcode_req = matches!(msg, AppMsg::SaveGCode);

        self.handle_message(msg);

        // ── File open dialog ──────────────────────────────────────────────
        if open_file {
            let dialog = gtk::FileChooserNative::new(
                Some("Open SVG or Image File"),
                Some(&widgets.root),
                gtk::FileChooserAction::Open,
                Some("Open"),
                Some("Cancel"),
            );
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("SVG & Image files"));
            for pat in &[
                "*.svg", "*.SVG", "*.png", "*.PNG", "*.jpg", "*.jpeg", "*.bmp",
            ] {
                filter.add_pattern(pat);
            }
            dialog.add_filter(&filter);
            let s = sender.clone();
            dialog.connect_response(glib::clone!(
                #[strong]
                dialog,
                move |_, response| {
                    if response == gtk::ResponseType::Accept {
                        if let Some(file) = dialog.file() {
                            if let Some(path) = file.path() {
                                s.input(AppMsg::FileChosen(path));
                            }
                        }
                    }
                }
            ));
            dialog.show();
        }

        // ── File save dialog ──────────────────────────────────────────────
        if save_gcode_req {
            let default_name = self
                .app
                .svg_path
                .as_deref()
                .and_then(|p| p.file_stem())
                .map(|s| format!("{}.gcode", s.to_string_lossy()))
                .unwrap_or_else(|| "output.gcode".to_string());

            let dialog = gtk::FileChooserNative::new(
                Some("Save GCode"),
                Some(&widgets.root),
                gtk::FileChooserAction::Save,
                Some("Save"),
                Some("Cancel"),
            );
            dialog.set_current_name(&default_name);
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("GCode files"));
            filter.add_pattern("*.gcode");
            filter.add_pattern("*.nc");
            dialog.add_filter(&filter);
            let s = sender.clone();
            dialog.connect_response(glib::clone!(
                #[strong]
                dialog,
                move |_, response| {
                    if response == gtk::ResponseType::Accept {
                        if let Some(file) = dialog.file() {
                            if let Some(path) = file.path() {
                                s.input(AppMsg::GCodeSavePath(path));
                            }
                        }
                    }
                }
            ));
            dialog.show();
        }

        self.sync_widgets(widgets, &sender);
    }
}

// ── Sub-widget builders ───────────────────────────────────────────────────────

struct ConnectWidgets {
    port_listbox: gtk::ListBox,
    baud_dropdown: gtk::DropDown,
    connect_btn: gtk::Button,
    disconnect_btn: gtk::Button,
    console_buffer: gtk::TextBuffer,
    console_view: gtk::TextView,
    command_entry: gtk::Entry,
}

fn build_connect_page(sender: &ComponentSender<AppComponent>) -> (gtk::Paned, ConnectWidgets) {
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_position(220);
    paned.set_shrink_start_child(false);
    paned.set_resize_start_child(false);

    // ── Left: port list + baud + buttons ─────────────────────────────────
    let left = gtk::Box::new(gtk::Orientation::Vertical, 4);
    left.set_margin_all(8);
    left.set_size_request(200, -1);

    let port_frame = gtk::Frame::new(Some("Serial Ports"));
    let port_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();
    let port_listbox = gtk::ListBox::new();
    port_listbox.set_selection_mode(gtk::SelectionMode::Single);
    {
        let s = sender.clone();
        port_listbox.connect_row_selected(move |_, row| {
            if let Some(r) = row {
                s.input(AppMsg::PortSelected(r.index() as u32));
            }
        });
    }
    port_scroll.set_child(Some(&port_listbox));
    port_frame.set_child(Some(&port_scroll));
    left.append(&port_frame);

    // Baud row
    let baud_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    baud_row.append(&gtk::Label::new(Some("Baud:")));
    let baud_strings = gtk::StringList::new(&[BaudRate::B115200.label(), BaudRate::B56000.label()]);
    let baud_dropdown = gtk::DropDown::new(Some(baud_strings), gtk::Expression::NONE);
    baud_dropdown.set_selected(0);
    {
        let s = sender.clone();
        baud_dropdown.connect_selected_notify(move |dd| {
            s.input(AppMsg::BaudSelected(dd.selected()));
        });
    }
    baud_row.append(&baud_dropdown);
    left.append(&baud_row);

    // Action buttons — 2×2 grid so they fit even on narrow screens
    let btn_grid = gtk::Grid::new();
    btn_grid.set_row_spacing(4);
    btn_grid.set_column_spacing(4);
    btn_grid.set_column_homogeneous(true);

    let refresh_btn = gtk::Button::with_label("Refresh");
    refresh_btn.set_hexpand(true);
    {
        let s = sender.clone();
        refresh_btn.connect_clicked(move |_| s.input(AppMsg::RefreshPorts));
    }
    btn_grid.attach(&refresh_btn, 0, 0, 1, 1);

    let connect_btn = gtk::Button::with_label("Connect");
    connect_btn.set_hexpand(true);
    connect_btn.add_css_class("suggested-action");
    {
        let s = sender.clone();
        connect_btn.connect_clicked(move |_| s.input(AppMsg::Connect));
    }
    btn_grid.attach(&connect_btn, 1, 0, 1, 1);

    let disconnect_btn = gtk::Button::with_label("Disconnect");
    disconnect_btn.set_hexpand(true);
    disconnect_btn.set_sensitive(false);
    {
        let s = sender.clone();
        disconnect_btn.connect_clicked(move |_| s.input(AppMsg::Disconnect));
    }
    btn_grid.attach(&disconnect_btn, 0, 1, 1, 1);

    let home_btn = gtk::Button::with_label("Home ($H)");
    home_btn.set_hexpand(true);
    {
        let s = sender.clone();
        home_btn.connect_clicked(move |_| s.input(AppMsg::Home));
    }
    btn_grid.attach(&home_btn, 1, 1, 1, 1);

    left.append(&btn_grid);
    paned.set_start_child(Some(&left));

    // ── Right: console + command input ────────────────────────────────────
    let right = gtk::Box::new(gtk::Orientation::Vertical, 4);
    right.set_margin_all(8);

    let console_frame = gtk::Frame::new(Some("Console"));
    let console_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();
    let console_buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
    let console_view = gtk::TextView::with_buffer(&console_buffer);
    console_view.set_editable(false);
    console_view.set_cursor_visible(false);
    console_view.set_monospace(true);
    console_view.set_wrap_mode(gtk::WrapMode::WordChar);
    console_scroll.set_child(Some(&console_view));
    console_frame.set_child(Some(&console_scroll));
    right.append(&console_frame);

    let command_entry = gtk::Entry::new();
    command_entry.set_placeholder_text(Some("Send GCode command (Enter to send)…"));
    {
        let s = sender.clone();
        command_entry.connect_activate(move |_| s.input(AppMsg::CommandSubmit));
    }
    right.append(&command_entry);
    paned.set_end_child(Some(&right));

    (
        paned,
        ConnectWidgets {
            port_listbox,
            baud_dropdown,
            connect_btn,
            disconnect_btn,
            console_buffer,
            console_view,
            command_entry,
        },
    )
}

// ── Control page ─────────────────────────────────────────────────────────────

struct ControlWidgets {
    grbl_status_label: gtk::Label,
    jog_step_label: gtk::Label,
    grbl_settings_listbox: gtk::ListBox,
}

fn build_control_page(
    sender: &ComponentSender<AppComponent>,
) -> (gtk::ScrolledWindow, ControlWidgets) {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.set_margin_all(8);

    // ── Machine status ────────────────────────────────────────────────────
    let status_frame = gtk::Frame::new(Some("Machine Status"));
    let grbl_status_label = gtk::Label::new(Some("Not connected"));
    grbl_status_label.set_xalign(0.0);

    grbl_status_label.set_selectable(true);
    grbl_status_label.set_margin_all(6);
    status_frame.set_child(Some(&grbl_status_label));
    vbox.append(&status_frame);

    // ── Jog controls ──────────────────────────────────────────────────────
    let jog_frame = gtk::Frame::new(Some("Jog Controls"));
    let jog_vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    jog_vbox.set_margin_all(8);

    // Step size row
    let step_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    step_row.set_halign(gtk::Align::Center);
    let step_minus = gtk::Button::with_label("Step −");
    {
        let s = sender.clone();
        step_minus.connect_clicked(move |_| s.input(AppMsg::JogStepSmaller));
    }
    let jog_step_label = gtk::Label::new(Some("1.0 mm"));
    jog_step_label.set_width_chars(10);
    jog_step_label.set_xalign(0.5);
    let step_plus = gtk::Button::with_label("Step +");
    {
        let s = sender.clone();
        step_plus.connect_clicked(move |_| s.input(AppMsg::JogStepLarger));
    }
    step_row.append(&step_minus);
    step_row.append(&jog_step_label);
    step_row.append(&step_plus);
    jog_vbox.append(&step_row);

    // Direction pad
    let dir_grid = gtk::Grid::new();
    dir_grid.set_row_spacing(4);
    dir_grid.set_column_spacing(4);
    dir_grid.set_halign(gtk::Align::Center);

    macro_rules! jog_btn {
        ($label:expr, $msg:expr, $col:expr, $row:expr) => {{
            let btn = gtk::Button::with_label($label);
            btn.set_size_request(72, 44);
            {
                let s = sender.clone();
                let m = $msg;
                btn.connect_clicked(move |_| s.input(m.clone()));
            }
            dir_grid.attach(&btn, $col, $row, 1, 1);
        }};
    }
    jog_btn!("Y+", AppMsg::JogMove(JogDir::YPlus), 1, 0);
    jog_btn!("X−", AppMsg::JogMove(JogDir::XMinus), 0, 1);
    jog_btn!("X+", AppMsg::JogMove(JogDir::XPlus), 2, 1);
    jog_btn!("Y−", AppMsg::JogMove(JogDir::YMinus), 1, 2);
    jog_btn!("Z+", AppMsg::JogMove(JogDir::ZPlus), 3, 0);
    jog_btn!("Z−", AppMsg::JogMove(JogDir::ZMinus), 3, 2);

    let cancel_btn = gtk::Button::with_label("⊗");
    cancel_btn.set_size_request(72, 44);
    cancel_btn.set_tooltip_text(Some("Cancel jog (0x85)"));
    {
        let s = sender.clone();
        cancel_btn.connect_clicked(move |_| s.input(AppMsg::SendRealtime(0x85)));
    }
    dir_grid.attach(&cancel_btn, 1, 1, 1, 1);
    jog_vbox.append(&dir_grid);

    // Machine control buttons
    let ctrl_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    ctrl_row.set_halign(gtk::Align::Center);

    macro_rules! ctrl_btn {
        ($label:expr, $msg:expr, $tip:expr) => {{
            let btn = gtk::Button::with_label($label);
            btn.set_tooltip_text(Some($tip));
            {
                let s = sender.clone();
                let m = $msg;
                btn.connect_clicked(move |_| s.input(m.clone()));
            }
            ctrl_row.append(&btn);
        }};
    }
    ctrl_btn!("Home", AppMsg::Home, "Run homing cycle ($H)");
    ctrl_btn!("Hold !", AppMsg::SendRaw("!".to_string()), "Feed hold");
    ctrl_btn!(
        "Start ~",
        AppMsg::SendRaw("~".to_string()),
        "Cycle start / resume"
    );
    ctrl_btn!("Reset", AppMsg::SendRealtime(0x18), "Soft reset (Ctrl-X)");
    ctrl_btn!("Poll ?", AppMsg::PollStatus, "Manual status poll");
    ctrl_btn!("$X", AppMsg::SendRaw("$X".to_string()), "Unlock alarm");
    ctrl_btn!(
        "$$",
        AppMsg::SendRaw("$$".to_string()),
        "Dump GRBL settings"
    );
    jog_vbox.append(&ctrl_row);

    // Override buttons
    let ov_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    ov_row.set_halign(gtk::Align::Center);
    ov_row.append(&gtk::Label::new(Some("Overrides:")));

    macro_rules! ov_btn {
        ($label:expr, $byte:expr, $tip:expr) => {{
            let btn = gtk::Button::with_label($label);
            btn.set_tooltip_text(Some($tip));
            {
                let s = sender.clone();
                btn.connect_clicked(move |_| s.input(AppMsg::SendRealtime($byte)));
            }
            ov_row.append(&btn);
        }};
    }
    ov_btn!("Feed 100%", 0x90, "Reset feed to 100%");
    ov_btn!("Feed+10%", 0x91, "Feed override +10%");
    ov_btn!("Feed−10%", 0x92, "Feed override −10%");
    ov_btn!("Laser 100%", 0x99, "Reset laser to 100%");
    ov_btn!("Laser+10%", 0x9A, "Laser override +10%");
    ov_btn!("Laser−10%", 0x9B, "Laser override −10%");
    jog_vbox.append(&ov_row);

    jog_frame.set_child(Some(&jog_vbox));
    vbox.append(&jog_frame);

    // ── GRBL settings list ────────────────────────────────────────────────
    let gs_frame = gtk::Frame::new(Some("GRBL Settings (send $$ to populate)"));
    let gs_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(150)
        .build();
    let grbl_settings_listbox = gtk::ListBox::new();
    grbl_settings_listbox.set_selection_mode(gtk::SelectionMode::None);
    gs_scroll.set_child(Some(&grbl_settings_listbox));
    gs_frame.set_child(Some(&gs_scroll));
    vbox.append(&gs_frame);

    scroll.set_child(Some(&vbox));

    (
        scroll,
        ControlWidgets {
            grbl_status_label,
            jog_step_label,
            grbl_settings_listbox,
        },
    )
}

// ── GCode page ────────────────────────────────────────────────────────────────

struct GCodeWidgets {
    file_label: gtk::Label,
    convert_btn: gtk::Button,
    send_btn: gtk::Button,
    abort_btn: gtk::Button,
    frame_btn: gtk::Button,
    save_btn: gtk::Button,
    travel_check: gtk::CheckButton,
    invert_check: gtk::CheckButton,
    layer_listbox: gtk::ListBox,
    gcode_buffer: gtk::TextBuffer,
    stream_progress: gtk::ProgressBar,
    conversion_status_label: gtk::Label,
}

fn build_gcode_page(sender: &ComponentSender<AppComponent>) -> (gtk::Paned, GCodeWidgets) {
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_position(750);

    // ── Left: toolbar + gcode view ────────────────────────────────────────
    let left = gtk::Box::new(gtk::Orientation::Vertical, 4);
    left.set_margin_all(8);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);

    let open_btn = gtk::Button::with_label("Open File…");
    {
        let s = sender.clone();
        open_btn.connect_clicked(move |_| s.input(AppMsg::OpenFile));
    }
    btn_row.append(&open_btn);

    let convert_btn = gtk::Button::with_label("Convert");
    convert_btn.set_sensitive(false);
    convert_btn.add_css_class("suggested-action");
    {
        let s = sender.clone();
        convert_btn.connect_clicked(move |_| s.input(AppMsg::Convert));
    }
    btn_row.append(&convert_btn);

    let send_btn = gtk::Button::with_label("Send GCode");
    send_btn.set_sensitive(false);
    {
        let s = sender.clone();
        send_btn.connect_clicked(move |_| s.input(AppMsg::SendGCode));
    }
    btn_row.append(&send_btn);

    let abort_btn = gtk::Button::with_label("Abort");
    abort_btn.set_sensitive(false);
    abort_btn.add_css_class("destructive-action");
    {
        let s = sender.clone();
        abort_btn.connect_clicked(move |_| s.input(AppMsg::AbortStream));
    }
    btn_row.append(&abort_btn);

    let frame_btn = gtk::Button::with_label("Frame Job");
    frame_btn.set_sensitive(false);
    frame_btn.set_tooltip_text(Some("Trace bounding box with laser off"));
    {
        let s = sender.clone();
        frame_btn.connect_clicked(move |_| s.input(AppMsg::FrameJob));
    }
    btn_row.append(&frame_btn);

    let save_btn = gtk::Button::with_label("Save…");
    save_btn.set_sensitive(false);
    {
        let s = sender.clone();
        save_btn.connect_clicked(move |_| s.input(AppMsg::SaveGCode));
    }
    btn_row.append(&save_btn);

    left.append(&btn_row);

    // Info row
    let info_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let file_label = gtk::Label::new(Some("No file loaded"));
    file_label.set_xalign(0.0);
    file_label.set_hexpand(true);
    file_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    info_row.append(&file_label);
    let conversion_status_label = gtk::Label::new(Some("—"));
    conversion_status_label.set_xalign(1.0);
    info_row.append(&conversion_status_label);
    left.append(&info_row);

    let stream_progress = gtk::ProgressBar::new();
    stream_progress.set_show_text(true);
    stream_progress.set_visible(false);
    left.append(&stream_progress);

    let opts_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let travel_check = gtk::CheckButton::with_label("Show travel moves in preview");
    {
        let s = sender.clone();
        travel_check.connect_toggled(move |cb| s.input(AppMsg::ToggleTravelLines(cb.is_active())));
    }
    opts_row.append(&travel_check);
    let invert_check = gtk::CheckButton::with_label("Invert image (raster)");
    {
        let s = sender.clone();
        invert_check.connect_toggled(move |cb| s.input(AppMsg::ToggleInvertImage(cb.is_active())));
    }
    opts_row.append(&invert_check);
    left.append(&opts_row);

    let gcode_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();
    let gcode_buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
    let gcode_view = gtk::TextView::with_buffer(&gcode_buffer);
    gcode_view.set_editable(false);
    gcode_view.set_cursor_visible(false);
    gcode_view.set_monospace(true);
    gcode_scroll.set_child(Some(&gcode_view));
    left.append(&gcode_scroll);

    paned.set_start_child(Some(&left));

    // ── Right: layer panel ────────────────────────────────────────────────
    let right = gtk::Box::new(gtk::Orientation::Vertical, 4);
    right.set_margin_all(8);
    let layer_title = gtk::Label::new(Some("SVG Layers"));
    layer_title.add_css_class("heading");
    right.append(&layer_title);

    let layer_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .min_content_width(220)
        .build();
    let layer_listbox = gtk::ListBox::new();
    layer_listbox.set_selection_mode(gtk::SelectionMode::Single);
    {
        let s = sender.clone();
        layer_listbox.connect_row_selected(move |_, row| {
            if let Some(r) = row {
                s.input(AppMsg::LayerSelected(r.index() as u32));
            }
        });
    }
    layer_scroll.set_child(Some(&layer_listbox));
    right.append(&layer_scroll);
    paned.set_end_child(Some(&right));

    (
        paned,
        GCodeWidgets {
            file_label,
            convert_btn,
            send_btn,
            abort_btn,
            frame_btn,
            save_btn,
            travel_check,
            invert_check,
            layer_listbox,
            gcode_buffer,
            stream_progress,
            conversion_status_label,
        },
    )
}

// ── Preview page ──────────────────────────────────────────────────────────────

struct PreviewWidgets {
    svg_picture: gtk::Picture,
    gcode_picture: gtk::Picture,
}

fn build_preview_page(sender: &ComponentSender<AppComponent>) -> (gtk::Paned, PreviewWidgets) {
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);

    let left_frame = gtk::Frame::new(Some("SVG / Source Preview"));
    let left_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();
    let svg_picture = gtk::Picture::new();
    svg_picture.set_hexpand(true);
    svg_picture.set_vexpand(true);
    left_scroll.set_child(Some(&svg_picture));
    left_frame.set_child(Some(&left_scroll));
    paned.set_start_child(Some(&left_frame));

    let right_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    right_box.set_margin_all(4);

    let render_btn = gtk::Button::with_label("Render GCode Preview");
    {
        let s = sender.clone();
        render_btn.connect_clicked(move |_| s.input(AppMsg::RenderPreview));
    }
    right_box.append(&render_btn);

    let gcode_frame = gtk::Frame::new(Some("GCode Toolpath"));
    let gcode_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();
    let gcode_picture = gtk::Picture::new();
    gcode_picture.set_hexpand(true);
    gcode_picture.set_vexpand(true);
    gcode_scroll.set_child(Some(&gcode_picture));
    gcode_frame.set_child(Some(&gcode_scroll));
    right_box.append(&gcode_frame);

    paned.set_end_child(Some(&right_box));

    (
        paned,
        PreviewWidgets {
            svg_picture,
            gcode_picture,
        },
    )
}

// ── Settings page ─────────────────────────────────────────────────────────────

struct SettingsWidgets {
    settings_box: gtk::Box,
}

fn build_settings_page(
    _sender: &ComponentSender<AppComponent>,
    _settings: &MachineSettings,
) -> (gtk::ScrolledWindow, SettingsWidgets) {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let frame = gtk::Frame::new(Some("Machine & Conversion Settings"));
    let settings_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_child(Some(&settings_box));
    outer.append(&frame);
    scroll.set_child(Some(&outer));
    (scroll, SettingsWidgets { settings_box })
}

// ── Helper: RGBA → GDK texture ────────────────────────────────────────────────

fn rgba_to_gdk_texture(img: &image::RgbaImage) -> gdk::MemoryTexture {
    let (w, h) = img.dimensions();
    let bytes = glib::Bytes::from(img.as_raw().as_slice());
    gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        (w * 4) as usize,
    )
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    init_logging();
    let app = RelmApp::new("app.larris.grbl");
    app.run::<AppComponent>(());
}

fn init_logging() {
    use std::fs::OpenOptions;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("larris.log")
        .ok();
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"));
    if let Some(file) = log_file {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    builder.init();
}
