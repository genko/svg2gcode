//! Terminal UI rendering.
//!
//! Layout overview:
//!
//!  ┌─────────────────────────────────────────────────────┐
//!  │ [Connect] [GCode] [Preview]   tab bar               │
//!  ├─────────────────────────────────────────────────────┤
//!  │                                                     │
//!  │  (tab-specific body)                                │
//!  │                                                     │
//!  ├─────────────────────────────────────────────────────┤
//!  │ status bar                                          │
//!  └─────────────────────────────────────────────────────┘
//!
//! Connect tab:
//!  ┌──────────────────┬──────────────────────────────────┐
//!  │ Serial Ports     │ Console                          │
//!  │ (list)           │ (scrollable log)                 │
//!  │                  │                                  │
//!  ├──────────────────┤──────────────────────────────────┤
//!  │ Baud / Connect   │ Command input                    │
//!  │ [Home] button    │                                  │
//!  └──────────────────┴──────────────────────────────────┘
//!
//! GCode tab:
//!  ┌─────────────────────────────────────────────────────┐
//!  │ SVG path + conversion controls                      │
//!  ├─────────────────────────────────────────────────────┤
//!  │ GCode text (scrollable)                             │
//!  └─────────────────────────────────────────────────────┘
//!
//! Preview tab:
//!  ┌───────────────────────┬─────────────────────────────┐
//!  │ Source SVG            │ GCode toolpath              │
//!  └───────────────────────┴─────────────────────────────┘

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
    },
};

use crate::app::{
    ActiveTab, App, AppMode, ConversionStatus, FocusedPane, LineKind, MachineSettings,
};

// ── Palette ───────────────────────────────────────────────────────────────────

const C_BG: Color = Color::Reset;
const C_BORDER_ACT: Color = Color::Cyan;
const C_BORDER_INACT: Color = Color::DarkGray;
const C_TITLE: Color = Color::White;
const C_HL: Color = Color::Cyan;
const C_CONN: Color = Color::Green;
const C_DISC: Color = Color::Red;
const C_INFO: Color = Color::DarkGray;
const C_SENT: Color = Color::Yellow;
const C_RECV: Color = Color::Green;
const C_ERR: Color = Color::Red;
const C_STATUS_BG: Color = Color::DarkGray;
const C_TAB_SEL: Color = Color::Black;
const C_TAB_SEL_BG: Color = Color::Cyan;
const C_TAB_UNSEL: Color = Color::White;
const C_TAB_UNSEL_BG: Color = Color::DarkGray;
const C_WARN: Color = Color::Yellow;
const C_OK: Color = Color::Green;

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();

    // Outer split: tab bar (1 line) + body + status bar (1 line)
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Min(0),    // body
            Constraint::Length(1), // status bar
        ])
        .split(area);

    render_tab_bar(app, frame, outer[0]);

    match app.active_tab {
        ActiveTab::Connect => render_connect_tab(app, frame, outer[1]),
        ActiveTab::Control => render_control_tab(app, frame, outer[1]),
        ActiveTab::GCode => render_gcode_tab(app, frame, outer[1]),
        ActiveTab::Preview => render_preview_tab(app, frame, outer[1]),
        ActiveTab::Settings => render_settings_tab(app, frame, outer[1]),
    }

    render_status_bar(app, frame, outer[2]);

    // Overlays (drawn on top of everything)
    if app.baud_dropdown_open {
        render_baud_dropdown(app, frame, area);
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn render_tab_bar(app: &App, frame: &mut Frame, area: Rect) {
    const TITLE: &str = " Larris ";
    let title_w = TITLE.len() as u16;

    // Split: tabs on the left, program name on the right.
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(title_w)])
        .split(area);

    let tab_titles: Vec<Line> = ActiveTab::ALL
        .iter()
        .map(|t| {
            let is_sel = *t == app.active_tab;
            Line::from(Span::styled(
                format!(" {} ", t.label()),
                if is_sel {
                    Style::default()
                        .fg(C_TAB_SEL)
                        .bg(C_TAB_SEL_BG)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(C_TAB_UNSEL).bg(C_TAB_UNSEL_BG)
                },
            ))
        })
        .collect();

    let selected = ActiveTab::ALL
        .iter()
        .position(|t| *t == app.active_tab)
        .unwrap_or(0);

    let tabs = Tabs::new(tab_titles)
        .select(selected)
        .style(Style::default().fg(C_TAB_UNSEL).bg(C_TAB_UNSEL_BG))
        .highlight_style(
            Style::default()
                .fg(C_TAB_SEL)
                .bg(C_TAB_SEL_BG)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(tabs, chunks[0]);

    frame.render_widget(
        Paragraph::new(Span::styled(
            TITLE,
            Style::default()
                .fg(C_HL)
                .bg(C_TAB_UNSEL_BG)
                .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        ))
        .alignment(Alignment::Right),
        chunks[1],
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Connect tab
// ═══════════════════════════════════════════════════════════════════════════════

fn render_connect_tab(app: &mut App, frame: &mut Frame, area: Rect) {
    // Horizontal: left panel (30 cols) | right panel (rest)
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(0)])
        .split(area);

    // Left: port list (fill) + control block (9 rows)
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(9)])
        .split(h[0]);

    // Right: console (fill) + input (3 rows)
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(h[1]);

    render_port_list(app, frame, left[0]);
    render_control_block(app, frame, left[1]);
    render_console(app, frame, right[0]);
    render_input(app, frame, right[1]);
}

// ── Port list ─────────────────────────────────────────────────────────────────

fn render_port_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let focused = app.focused == FocusedPane::SerialList && app.active_tab == ActiveTab::Connect;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    let block = Block::default()
        .title(Span::styled(
            " Serial Ports ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc))
        .style(Style::default().bg(C_BG));

    if app.port_list.is_empty() {
        let msg = Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No serial ports found",
                Style::default().fg(C_INFO),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Press 'r' to refresh",
                Style::default().fg(C_INFO),
            )),
        ]))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app
        .port_list
        .iter()
        .enumerate()
        .map(|(i, port)| {
            let is_connected = app.connected_port.as_deref() == Some(port.as_str());
            let is_selected = app.port_list_state.selected() == Some(i);
            let prefix_colour = if is_connected { C_CONN } else { C_BORDER_INACT };
            let prefix = if is_connected { "● " } else { "  " };
            let text_colour = if is_selected { C_HL } else { C_TITLE };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(prefix_colour)),
                Span::styled(port.as_str(), Style::default().fg(text_colour)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.port_list_state);
}

// ── Control block (baud + buttons) ────────────────────────────────────────────

fn render_control_block(app: &App, frame: &mut Frame, area: Rect) {
    let focused = app.focused == FocusedPane::SerialList && app.active_tab == ActiveTab::Connect;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    let block = Block::default()
        .title(Span::styled(
            " Controls ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc));

    let baud = app.selected_baud();
    let open_indicator = if app.baud_dropdown_open { "▲" } else { "▼" };

    let conn_span = match app.mode {
        AppMode::Connected => Span::styled(
            "● CONNECTED",
            Style::default().fg(C_CONN).add_modifier(Modifier::BOLD),
        ),
        AppMode::Disconnected => Span::styled("○ DISCONNECTED", Style::default().fg(C_DISC)),
    };

    let content = vec![
        Line::from(conn_span),
        Line::from(vec![
            Span::styled("  Baud: ", Style::default().fg(C_INFO)),
            Span::styled(
                format!("{} {}", baud.label(), open_indicator),
                Style::default().fg(C_HL).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Enter:connect  b:baud  r:refresh",
            Style::default().fg(C_INFO),
        )),
        Line::from(Span::styled(
            " d:disconnect   $H:home (Control tab)",
            Style::default().fg(C_INFO),
        )),
    ];

    let para = Paragraph::new(content).block(block);
    frame.render_widget(para, area);
}

// ── Console ───────────────────────────────────────────────────────────────────

fn render_console(app: &App, frame: &mut Frame, area: Rect) {
    let focused = app.focused == FocusedPane::Console;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    let follow_span = if app.console_follow {
        Span::styled(" ↓FOLLOW ", Style::default().fg(C_CONN))
    } else {
        Span::styled(" SCROLL ", Style::default().fg(C_WARN))
    };

    let block = Block::default()
        .title(vec![
            Span::styled(
                " Console ",
                Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
            ),
            follow_span,
        ])
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc));

    let inner_height = area.height.saturating_sub(2) as usize;

    let all_lines: Vec<Line> = app
        .console_lines
        .iter()
        .map(|cl| console_line_to_ratatui(cl.kind, &cl.text))
        .collect();

    let total = all_lines.len();
    let scroll_offset = app.console_scroll as usize;
    let end = total.saturating_sub(scroll_offset);
    let start = end.saturating_sub(inner_height);
    let visible: Vec<Line> = all_lines[start..end].to_vec();

    let para = Paragraph::new(visible)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(para, area);

    if total > inner_height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let max_offset = total.saturating_sub(inner_height);
        let position = max_offset.saturating_sub(scroll_offset);
        let mut state = ScrollbarState::new(max_offset + 1).position(position);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn console_line_to_ratatui(kind: LineKind, text: &str) -> Line<'static> {
    match kind {
        LineKind::Received => Line::from(vec![
            Span::styled("◀ ", Style::default().fg(C_RECV)),
            Span::styled(text.to_owned(), Style::default().fg(C_RECV)),
        ]),
        LineKind::Sent => Line::from(vec![
            Span::styled("▶ ", Style::default().fg(C_SENT)),
            Span::styled(text.to_owned(), Style::default().fg(C_SENT)),
        ]),
        LineKind::Info => Line::from(vec![
            Span::styled("ℹ ", Style::default().fg(C_INFO)),
            Span::styled(text.to_owned(), Style::default().fg(C_INFO)),
        ]),
        LineKind::Error => Line::from(vec![
            Span::styled("✗ ", Style::default().fg(C_ERR)),
            Span::styled(
                text.to_owned(),
                Style::default().fg(C_ERR).add_modifier(Modifier::BOLD),
            ),
        ]),
    }
}

// ── Command input ─────────────────────────────────────────────────────────────

fn render_input(app: &App, frame: &mut Frame, area: Rect) {
    let focused = app.focused == FocusedPane::CommandInput;
    let connected = app.mode == AppMode::Connected;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    let title = if connected {
        " Command (Enter to send) "
    } else {
        " Command (not connected) "
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(if connected { C_TITLE } else { C_INFO })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc));

    let buf = &app.input_buffer;
    let cp = app.input_cursor;

    let before = &buf[..cp];
    let cur_char = buf[cp..]
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| " ".to_string());
    let after = if cp < buf.len() {
        let clen = buf[cp..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
        &buf[cp + clen..]
    } else {
        ""
    };

    let fg = if connected { Color::White } else { C_INFO };
    let cursor_bg = if focused && connected { C_HL } else { C_INFO };

    let line = Line::from(vec![
        Span::styled(before.to_owned(), Style::default().fg(fg)),
        Span::styled(cur_char, Style::default().fg(Color::Black).bg(cursor_bg)),
        Span::styled(after.to_owned(), Style::default().fg(fg)),
    ]);

    frame.render_widget(Paragraph::new(line).block(block), area);
}

// ── Baud dropdown overlay ─────────────────────────────────────────────────────

fn render_baud_dropdown(app: &App, frame: &mut Frame, screen: Rect) {
    let item_count = app.baud_rates.len() as u16;
    let height = item_count + 2;
    let width = 20u16;

    // Anchor at bottom-left of the left panel (x=0)
    let x = 2u16;
    // Place above the control block; roughly at row (screen.height - 1 - 9 - height)
    let control_top = screen.height.saturating_sub(1 + 9); // 1 status + 9 control block
    let y = control_top.saturating_sub(height);

    let popup = Rect {
        x,
        y,
        width: width.min(screen.width.saturating_sub(x + 2)),
        height: height.min(screen.height.saturating_sub(y)),
    };

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = app
        .baud_rates
        .iter()
        .enumerate()
        .map(|(i, baud)| {
            let sel = i == app.selected_baud_idx;
            ListItem::new(Line::from(vec![
                Span::raw(if sel { "▶ " } else { "  " }),
                Span::styled(
                    baud.label(),
                    Style::default()
                        .fg(if sel { C_HL } else { C_TITLE })
                        .add_modifier(if sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(" Baud Rate ", Style::default().fg(C_TITLE)))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_HL)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    frame.render_widget(list, popup);
}

// ═══════════════════════════════════════════════════════════════════════════════
// GCode tab
// ═══════════════════════════════════════════════════════════════════════════════

fn render_gcode_tab(app: &App, frame: &mut Frame, area: Rect) {
    // Top control bar (4 lines) + gcode text body
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    render_gcode_controls(app, frame, v[0]);
    render_gcode_text(app, frame, v[1]);
}

fn render_gcode_controls(app: &App, frame: &mut Frame, area: Rect) {
    let svg_label = app
        .svg_path
        .as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<no file loaded>".to_string());

    let status_span = match &app.conversion_status {
        ConversionStatus::Idle => Span::styled("idle", Style::default().fg(C_INFO)),
        ConversionStatus::Running => Span::styled(
            "converting…",
            Style::default()
                .fg(C_WARN)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
        ConversionStatus::Ok => Span::styled(
            "OK ✓",
            Style::default().fg(C_OK).add_modifier(Modifier::BOLD),
        ),
        ConversionStatus::Failed(msg) => {
            Span::styled(format!("FAILED: {}", msg), Style::default().fg(C_ERR))
        }
    };

    let gcode_lines = app
        .gcode_text
        .as_deref()
        .map(|t| t.lines().count())
        .unwrap_or(0);

    let content = vec![
        Line::from(vec![
            Span::styled("  SVG:    ", Style::default().fg(C_INFO)),
            Span::styled(
                svg_label,
                Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Status: ", Style::default().fg(C_INFO)),
            status_span,
            Span::styled(
                format!("  ({} lines)", gcode_lines),
                Style::default().fg(C_INFO),
            ),
        ]),
        Line::from(vec![Span::styled(
            format!(
                "  Feed:{:.0}mm/m  Tol:{:.3}mm  DPI:{:.0}  Power:{:.0}",
                app.machine_settings.feedrate,
                app.machine_settings.tolerance,
                app.machine_settings.dpi,
                app.machine_settings.laser_power,
            ),
            Style::default().fg(C_INFO),
        )]),
        Line::from(vec![Span::styled(
            "  o:open SVG   c:convert   s:save GCode   ↑↓/PgUp/Dn:scroll",
            Style::default().fg(C_INFO),
        )]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_INACT))
        .title(Span::styled(
            " SVG → GCode ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

fn render_gcode_text(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_INACT))
        .title(Span::styled(
            " GCode ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ));

    let inner_h = area.height.saturating_sub(2) as usize;

    match &app.gcode_text {
        None => {
            let msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No GCode generated yet.",
                    Style::default().fg(C_INFO),
                )),
                Line::from(Span::styled(
                    "  Press 'o' to load an SVG, then 'c' to convert.",
                    Style::default().fg(C_INFO),
                )),
            ])
            .block(block);
            frame.render_widget(msg, area);
        }
        Some(gcode) => {
            let all_lines: Vec<Line> = gcode
                .lines()
                .enumerate()
                .map(|(i, l)| {
                    let num = Span::styled(
                        format!("{:5} ", i + 1),
                        Style::default().fg(Color::DarkGray),
                    );
                    let code_colour = gcode_line_colour(l);
                    let code = Span::styled(l.to_owned(), Style::default().fg(code_colour));
                    Line::from(vec![num, code])
                })
                .collect();

            let total = all_lines.len();
            let start = app.gcode_scroll.min(total.saturating_sub(1));
            let end = (start + inner_h).min(total);
            let visible: Vec<Line> = all_lines[start..end].to_vec();

            let para = Paragraph::new(visible).block(block);
            frame.render_widget(para, area);

            // Scrollbar
            if total > inner_h {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓"));
                let max_pos = total.saturating_sub(inner_h);
                let mut state =
                    ScrollbarState::new(max_pos + 1).position(app.gcode_scroll.min(max_pos));
                frame.render_stateful_widget(
                    scrollbar,
                    area.inner(Margin {
                        vertical: 1,
                        horizontal: 0,
                    }),
                    &mut state,
                );
            }
        }
    }
}

/// Syntax-colour a single GCode line.
fn gcode_line_colour(line: &str) -> Color {
    let trimmed = line.trim_start();
    if trimmed.starts_with(';') {
        return Color::DarkGray;
    }
    let upper = trimmed.to_uppercase();
    if upper.starts_with("G0 ") || upper == "G0" {
        Color::DarkGray // rapid – de-emphasised
    } else if upper.starts_with("G1 ") || upper == "G1" {
        C_RECV // cut move – green
    } else if upper.starts_with("G2") || upper.starts_with("G3") {
        C_HL // arc – cyan
    } else if upper.starts_with('M') {
        C_WARN // machine commands – yellow
    } else {
        C_TITLE
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Preview tab
// ═══════════════════════════════════════════════════════════════════════════════

fn render_preview_tab(app: &mut App, frame: &mut Frame, area: Rect) {
    // Split horizontally: left = source SVG, right = GCode toolpath
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_svg_preview_panel(app, frame, h[0]);
    render_gcode_preview_panel(app, frame, h[1]);
}

fn render_svg_preview_panel(app: &mut App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Source SVG ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_INACT));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Render the image if we have a protocol state
    if let Some(proto) = app.preview_protocol.as_mut() {
        let image_widget = ratatui_image::StatefulImage::default();
        frame.render_stateful_widget(image_widget, inner, proto);
    } else {
        let hint = if app.svg_path.is_some() {
            "  Press 'p' to render preview"
        } else {
            "  Load an SVG first (GCode tab → 'o')"
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(hint, Style::default().fg(C_INFO))),
            ]),
            inner,
        );
    }
}

fn render_gcode_preview_panel(app: &mut App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " GCode Toolpath ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_INACT));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.gcode_text.is_some() && app.preview_dirty {
        // Signal to the caller that we need a new render; the event loop handles it.
        // While rendering is in progress show a placeholder.
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Rendering toolpath…",
                    Style::default()
                        .fg(C_WARN)
                        .add_modifier(Modifier::SLOW_BLINK),
                )),
            ]),
            inner,
        );
        return;
    }

    if app.gcode_text.is_none() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No GCode yet. Convert an SVG first.",
                    Style::default().fg(C_INFO),
                )),
            ]),
            inner,
        );
        return;
    }

    // If the preview image has been rasterised into preview_image and encoded
    // into preview_protocol, render it. Otherwise show a hint.
    if let Some(proto) = app.preview_protocol.as_mut() {
        // We share the same protocol slot with the SVG panel; for the toolpath
        // we render a second copy. Because ratatui-image's StatefulProtocol is
        // stateful (tracks position/dirty), we keep two separate slots in a
        // future iteration. For now, if a protocol exists the SVG preview owns
        // it and the toolpath shows the raw stats.
        let stats = app
            .gcode_text
            .as_deref()
            .map(|g| {
                let moves = g
                    .lines()
                    .filter(|l| {
                        let u = l.trim_start().to_uppercase();
                        u.starts_with("G0")
                            || u.starts_with("G1")
                            || u.starts_with("G2")
                            || u.starts_with("G3")
                    })
                    .count();
                format!("  {} move commands", moves)
            })
            .unwrap_or_default();

        let _ = proto; // suppress unused warning when sharing slot

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Toolpath ready.",
                    Style::default().fg(C_OK).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(stats, Style::default().fg(C_INFO))),
                Line::from(""),
                Line::from(Span::styled(
                    "  Press 'p' to regenerate preview.",
                    Style::default().fg(C_INFO),
                )),
            ]),
            inner,
        );
    } else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Press 'p' to render toolpath preview.",
                    Style::default().fg(C_INFO),
                )),
            ]),
            inner,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Status bar
// ═══════════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════════
// Control tab
// ═══════════════════════════════════════════════════════════════════════════════

fn render_control_tab(app: &mut App, frame: &mut Frame, area: Rect) {
    // ── Outer layout ──────────────────────────────────────────────────────
    // Top row:  [Status (40)] | [Position (fill)]
    // Mid row:  [Jog pad (40)] | [Overrides (fill)]
    // Bot row:  [Settings list (fill)]
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),  // status + position
            Constraint::Length(14), // jog pad + overrides
            Constraint::Min(4),     // settings / gc-state
        ])
        .split(area);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(40), Constraint::Min(0)])
        .split(rows[0]);

    let mid_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(40), Constraint::Min(0)])
        .split(rows[1]);

    render_ctrl_status(app, frame, top_cols[0]);
    render_ctrl_position(app, frame, top_cols[1]);
    render_ctrl_jog(app, frame, mid_cols[0]);
    render_ctrl_overrides(app, frame, mid_cols[1]);
    render_ctrl_settings(app, frame, rows[2]);
}

// ── Machine status panel ──────────────────────────────────────────────────────

fn render_ctrl_status(app: &App, frame: &mut Frame, area: Rect) {
    use crate::grbl::MachineState;

    let (state_label, state_colour) = match app.grbl_status.as_ref().map(|s| &s.state) {
        Some(MachineState::Idle) => ("Idle", C_OK),
        Some(MachineState::Run) => ("Run", C_RECV),
        Some(MachineState::Jog) => ("Jog", C_HL),
        Some(MachineState::Hold(_)) => ("Hold", C_WARN),
        Some(MachineState::Alarm) => ("ALARM", C_ERR),
        Some(MachineState::Home) => ("Home", C_WARN),
        Some(MachineState::Door(_)) => ("Door", C_WARN),
        Some(MachineState::Check) => ("Check", C_INFO),
        Some(MachineState::Sleep) => ("Sleep", C_INFO),
        Some(MachineState::Unknown(s)) => (s.as_str(), C_INFO),
        None => ("--", C_INFO),
    };

    let feed_str = app
        .grbl_status
        .as_ref()
        .and_then(|s| s.feed)
        .map(|f| format!("{:.0}", f))
        .unwrap_or_else(|| "--".into());

    let speed_str = app
        .grbl_status
        .as_ref()
        .and_then(|s| s.speed)
        .map(|s| format!("{:.0}", s))
        .unwrap_or_else(|| "--".into());

    let buf_str = app
        .grbl_status
        .as_ref()
        .and_then(|s| s.buffer)
        .map(|b| format!("{}/{}", b.available_blocks, b.available_bytes))
        .unwrap_or_else(|| "--".into());

    let ln_str = app
        .grbl_status
        .as_ref()
        .and_then(|s| s.line_number)
        .map(|n| format!("{}", n))
        .unwrap_or_else(|| "--".into());

    // Pin indicators
    let pins = app.grbl_status.as_ref().and_then(|s| s.pins);
    let pin_x = if pins.map(|p| p.x_limit).unwrap_or(false) {
        Span::styled("X", Style::default().fg(C_ERR).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("X", Style::default().fg(C_INFO))
    };
    let pin_y = if pins.map(|p| p.y_limit).unwrap_or(false) {
        Span::styled("Y", Style::default().fg(C_ERR).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("Y", Style::default().fg(C_INFO))
    };
    let pin_z = if pins.map(|p| p.z_limit).unwrap_or(false) {
        Span::styled("Z", Style::default().fg(C_ERR).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("Z", Style::default().fg(C_INFO))
    };
    let pin_p = if pins.map(|p| p.probe).unwrap_or(false) {
        Span::styled(
            "P",
            Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("P", Style::default().fg(C_INFO))
    };

    let connected = app.mode == crate::app::AppMode::Connected;
    let poll_span = if connected {
        Span::styled(
            format!(" poll:{}t ", app.status_poll_interval),
            Style::default().fg(C_INFO),
        )
    } else {
        Span::styled(" not connected ", Style::default().fg(C_DISC))
    };

    let content = vec![
        Line::from(vec![
            Span::styled("  State:  ", Style::default().fg(C_INFO)),
            Span::styled(
                state_label,
                Style::default()
                    .fg(state_colour)
                    .add_modifier(Modifier::BOLD),
            ),
            poll_span,
        ]),
        Line::from(vec![
            Span::styled("  Feed:   ", Style::default().fg(C_INFO)),
            Span::styled(
                format!("{:>7} mm/min", feed_str),
                Style::default().fg(C_TITLE),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Speed:  ", Style::default().fg(C_INFO)),
            Span::styled(format!("{:>7} S", speed_str), Style::default().fg(C_TITLE)),
        ]),
        Line::from(vec![
            Span::styled("  Buffer: ", Style::default().fg(C_INFO)),
            Span::styled(format!("{:>7}", buf_str), Style::default().fg(C_TITLE)),
            Span::styled("  Ln:", Style::default().fg(C_INFO)),
            Span::styled(ln_str, Style::default().fg(C_TITLE)),
        ]),
        Line::from(vec![
            Span::styled("  Limits: ", Style::default().fg(C_INFO)),
            pin_x,
            Span::raw(" "),
            pin_y,
            Span::raw(" "),
            pin_z,
            Span::raw("  Probe: "),
            pin_p,
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  ?: poll  ~: resume  !: hold",
            Style::default().fg(C_INFO),
        )]),
        Line::from(vec![Span::styled(
            "  $X: unlock  $H: home  Ctrl+X: reset",
            Style::default().fg(C_INFO),
        )]),
    ];

    let block = Block::default()
        .title(Span::styled(
            " Machine Status ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_INACT));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

// ── Position panel ────────────────────────────────────────────────────────────

fn render_ctrl_position(app: &App, frame: &mut Frame, area: Rect) {
    let wpos = app.grbl_status.as_ref().and_then(|s| s.work_pos());
    let mpos = app.grbl_status.as_ref().and_then(|s| s.mpos);
    let wco = app.grbl_status.as_ref().and_then(|s| s.wco);

    let fmt_pos = |p: Option<crate::grbl::Pos3>| -> Vec<Line<'static>> {
        match p {
            Some(pos) => vec![
                Line::from(vec![
                    Span::styled("  X: ", Style::default().fg(C_INFO)),
                    Span::styled(
                        format!("{:>10.3} mm", pos.x),
                        Style::default().fg(C_HL).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Y: ", Style::default().fg(C_INFO)),
                    Span::styled(
                        format!("{:>10.3} mm", pos.y),
                        Style::default().fg(C_HL).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Z: ", Style::default().fg(C_INFO)),
                    Span::styled(
                        format!("{:>10.3} mm", pos.z),
                        Style::default().fg(C_HL).add_modifier(Modifier::BOLD),
                    ),
                ]),
            ],
            None => vec![Line::from(Span::styled(
                "  ---.--- mm",
                Style::default().fg(C_INFO),
            ))],
        }
    };

    // Split vertically: work pos top, machine pos bottom, WCO hint
    let splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // work pos
            Constraint::Length(4), // machine pos (no header = compact)
            Constraint::Min(0),    // WCO + probe
        ])
        .split(area);

    // Work position
    {
        let mut lines = vec![Line::from("")];
        lines.extend(fmt_pos(wpos));
        let block = Block::default()
            .title(Span::styled(
                " Work Position ",
                Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_INACT));
        frame.render_widget(Paragraph::new(lines).block(block), splits[0]);
    }

    // Machine position (compact, no title)
    {
        let wco_str = wco
            .map(|w| format!("WCO X{:.3} Y{:.3} Z{:.3}", w.x, w.y, w.z))
            .unwrap_or_else(|| "WCO: --".into());
        let mpos_str = mpos
            .map(|m| format!("MPos X{:.3} Y{:.3} Z{:.3}", m.x, m.y, m.z))
            .unwrap_or_else(|| "MPos: --".into());

        let probe_str = app.last_probe.as_ref().map(|(pos, ok)| {
            format!(
                "PRB X{:.3} Y{:.3} Z{:.3} {}",
                pos.x,
                pos.y,
                pos.z,
                if *ok { "OK" } else { "FAIL" }
            )
        });

        let mut lines = vec![
            Line::from(Span::styled(
                format!("  {}", mpos_str),
                Style::default().fg(C_INFO),
            )),
            Line::from(Span::styled(
                format!("  {}", wco_str),
                Style::default().fg(C_INFO),
            )),
        ];
        if let Some(ps) = probe_str {
            lines.push(Line::from(Span::styled(
                format!("  {}", ps),
                Style::default().fg(C_WARN),
            )));
        }
        let gc = app.gc_state.as_deref().unwrap_or("GC: --");
        lines.push(Line::from(Span::styled(
            format!("  GC: {}", gc),
            Style::default().fg(C_INFO),
        )));

        let block = Block::default()
            .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_INACT));
        frame.render_widget(Paragraph::new(lines).block(block), splits[1]);
    }

    // Remaining space: key hints
    {
        let lines = vec![
            Line::from(Span::styled(
                "  G28/G30: go to stored pos",
                Style::default().fg(C_INFO),
            )),
            Line::from(Span::styled(
                "  G10 L20 P1 X0 Y0 Z0: zero WCS",
                Style::default().fg(C_INFO),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), splits[2]);
    }
}

// ── Jog pad ───────────────────────────────────────────────────────────────────

fn render_ctrl_jog(app: &App, frame: &mut Frame, area: Rect) {
    use crate::app::ControlFocus;
    use crate::grbl::JOG_STEPS;

    let focused =
        app.control_focus == ControlFocus::Jog && app.active_tab == crate::app::ActiveTab::Control;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    let step = app.jog_step_mm();
    let step_str = if step < 1.0 {
        format!("{:.3} mm", step)
    } else {
        format!("{:.1} mm", step)
    };

    // Step selector dots
    let step_dots: Vec<Span> = JOG_STEPS
        .iter()
        .enumerate()
        .flat_map(|(i, _)| {
            let dot = if i == app.jog_step_idx { "●" } else { "○" };
            let colour = if i == app.jog_step_idx { C_HL } else { C_INFO };
            vec![
                Span::styled(dot, Style::default().fg(colour)),
                Span::raw(" "),
            ]
        })
        .collect();

    // Arrow-key jog pad visual
    // Layout:
    //   [Z+]         [Y+]
    //   [Z-]  [X-]  [●]  [X+]
    //              [Y-]
    let btn = |label: &'static str, active: bool| -> Span<'static> {
        if active {
            Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(C_HL)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(label, Style::default().fg(C_TITLE).bg(Color::DarkGray))
        }
    };

    let connected = app.mode == crate::app::AppMode::Connected;

    let content = vec![
        Line::from(vec![
            Span::styled("  Step: ", Style::default().fg(C_INFO)),
            Span::styled(
                step_str,
                Style::default().fg(C_HL).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
        ]),
        Line::from({
            let mut s = vec![Span::styled("  ", Style::default())];
            s.extend(step_dots);
            s
        }),
        Line::from(Span::styled(
            format!("  Feed: {:.0} mm/min", app.jog_feedrate),
            Style::default().fg(C_INFO),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            btn("[Z+]", connected),
            Span::raw("          "),
            btn("[Y+]", connected),
        ]),
        Line::from(vec![
            Span::raw("  "),
            btn("[Z-]", connected),
            Span::raw("  "),
            btn("[X-]", connected),
            Span::raw("  "),
            Span::styled(" ◎ ", Style::default().fg(C_INFO)),
            Span::raw("  "),
            btn("[X+]", connected),
        ]),
        Line::from(vec![Span::raw("            "), btn("[Y-]", connected)]),
        Line::from(""),
        Line::from(Span::styled(
            "  ←→↑↓:XY  PgUp/Dn:Z  +/-:step",
            Style::default().fg(C_INFO),
        )),
        Line::from(Span::styled(
            "  0x85 (Esc): cancel jog",
            Style::default().fg(C_INFO),
        )),
        Line::from(Span::styled(
            "  Tab: switch focus",
            Style::default().fg(C_INFO),
        )),
    ];

    let block = Block::default()
        .title(Span::styled(
            " Jog ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

// ── Overrides panel ───────────────────────────────────────────────────────────

fn render_ctrl_overrides(app: &App, frame: &mut Frame, area: Rect) {
    use crate::app::ControlFocus;

    let focused = app.control_focus == ControlFocus::Overrides
        && app.active_tab == crate::app::ActiveTab::Control;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    // Read live values from GRBL status if available, else fall back to local
    let (feed_ov, rapid_ov, spindle_ov) = app
        .grbl_status
        .as_ref()
        .and_then(|s| s.overrides)
        .map(|ov| (ov.feed, ov.rapid, ov.spindle))
        .unwrap_or((app.override_feed, app.override_rapid, app.override_spindle));

    let bar = |pct: u8, width: usize| -> String {
        let filled = ((pct as usize).saturating_mul(width)) / 200; // 200% = full bar
        let filled = filled.min(width);
        let empty = width.saturating_sub(filled);
        format!("{}{}", "█".repeat(filled), "░".repeat(empty))
    };

    let bar_w = area.width.saturating_sub(22) as usize;
    let bar_w = bar_w.clamp(4, 30);

    let bar_colour = |pct: u8| -> Color {
        if pct < 80 || pct > 120 {
            C_WARN
        } else {
            C_RECV
        }
    };

    let content = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Feed    ", Style::default().fg(C_INFO)),
            Span::styled(
                format!("{:>3}% ", feed_ov),
                Style::default()
                    .fg(bar_colour(feed_ov))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                bar(feed_ov, bar_w),
                Style::default().fg(bar_colour(feed_ov)),
            ),
        ]),
        Line::from(Span::styled(
            "         ↑/↓: -10/+10%  ←/→: -1/+1%",
            Style::default().fg(C_INFO),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Rapid   ", Style::default().fg(C_INFO)),
            Span::styled(
                format!("{:>3}% ", rapid_ov),
                Style::default()
                    .fg(bar_colour(rapid_ov))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                bar(rapid_ov, bar_w),
                Style::default().fg(bar_colour(rapid_ov)),
            ),
        ]),
        Line::from(Span::styled(
            "         r/R: cycle 25/50/100%",
            Style::default().fg(C_INFO),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Laser   ", Style::default().fg(C_INFO)),
            Span::styled(
                format!("{:>3}% ", spindle_ov),
                Style::default()
                    .fg(bar_colour(spindle_ov))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                bar(spindle_ov, bar_w),
                Style::default().fg(bar_colour(spindle_ov)),
            ),
        ]),
        Line::from(Span::styled(
            "         PgUp/Dn: +10/-10%  s: reset 100%",
            Style::default().fg(C_INFO),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  0: reset all overrides to 100%",
            Style::default().fg(C_INFO),
        )),
        Line::from(Span::styled(
            "  Tab: switch focus",
            Style::default().fg(C_INFO),
        )),
    ];

    let block = Block::default()
        .title(Span::styled(
            " Feed / Rapid / Laser Overrides ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

// ── Settings list ─────────────────────────────────────────────────────────────

fn render_ctrl_settings(app: &mut App, frame: &mut Frame, area: Rect) {
    use crate::app::ControlFocus;

    let focused = app.control_focus == ControlFocus::Settings
        && app.active_tab == crate::app::ActiveTab::Control;
    let bc = if focused {
        C_BORDER_ACT
    } else {
        C_BORDER_INACT
    };

    let block = Block::default()
        .title(Span::styled(
            " GRBL Settings ($) ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(bc));

    if app.grbl_settings.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No settings received yet.",
                Style::default().fg(C_INFO),
            )),
            Line::from(Span::styled(
                "  Press '$' to request settings from GRBL.",
                Style::default().fg(C_INFO),
            )),
        ])
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app
        .grbl_settings
        .iter()
        .map(|(k, v)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<6}", k),
                    Style::default().fg(C_HL).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" = ", Style::default().fg(C_INFO)),
                Span::styled(v.clone(), Style::default().fg(C_TITLE)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.settings_list_state);
}

// ── Status bar ────────────────────────────────────────────────────────────────

// ═══════════════════════════════════════════════════════════════════════════════
// Settings tab
// ═══════════════════════════════════════════════════════════════════════════════

fn render_settings_tab(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Machine & Conversion Settings ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_ACT));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split: field list on left, help/description panel on right
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(36)])
        .split(inner);

    render_settings_fields(app, frame, cols[0]);
    render_settings_help(app, frame, cols[1]);
}

fn render_settings_fields(app: &App, frame: &mut Frame, area: Rect) {
    let field_count = MachineSettings::field_count();
    let in_edit = app.settings_edit_buf.is_some();

    let mut items: Vec<Line> = (0..field_count)
        .map(|i| {
            let name = MachineSettings::FIELD_NAMES[i];
            let value = if in_edit && i == app.settings_selected {
                // Show the live edit buffer
                let buf = app.settings_edit_buf.as_deref().unwrap_or("");
                format!("{:<22} : {}_", name, buf)
            } else {
                let val = app.machine_settings.field_value(i);
                format!("{:<22} : {}", name, val)
            };

            let is_sel = i == app.settings_selected;
            let style = if is_sel && in_edit {
                Style::default()
                    .fg(Color::Black)
                    .bg(C_HL)
                    .add_modifier(Modifier::BOLD)
            } else if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(C_TAB_UNSEL)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(C_TITLE)
            };

            Line::from(Span::styled(value, style))
        })
        .collect();

    // Validation error row
    if let Some(err) = &app.settings_edit_error {
        items.push(Line::from(Span::styled(
            format!("  ✗ {}", err),
            Style::default().fg(C_ERR).add_modifier(Modifier::BOLD),
        )));
    }

    // Scroll so selected row stays visible
    let visible_height = area.height as usize;
    let offset = if app.settings_selected >= visible_height {
        app.settings_selected - visible_height + 1
    } else {
        0
    };

    let visible_items: Vec<ListItem> = items.into_iter().skip(offset).map(ListItem::new).collect();

    let list = List::new(visible_items);
    frame.render_widget(list, area);
}

fn render_settings_help(app: &App, frame: &mut Frame, area: Rect) {
    let idx = app.settings_selected;
    let description: &str = match idx {
        0 => {
            "GCode sent before the job starts.\nTypically sets units, distance mode\nand turns the laser on.\nExample: G90 G21 M4 S1000"
        }
        1 => {
            "GCode sent after the job ends.\nTypically turns the laser off\nand stops the program.\nExample: M5 M2"
        }
        2 => "Maximum X travel of the machine\nin millimetres.\nUsed to validate job extents.",
        3 => "Maximum Y travel of the machine\nin millimetres.\nUsed to validate job extents.",
        4 => {
            "Maximum machine speed in mm/min.\nUsed as a ceiling when validating\nthe feedrate setting."
        }
        5 => {
            "Maximum laser power (S-word).\nGRBL default is 1000.\nSet $30 on the controller to match."
        }
        6 => "Laser-on feedrate in mm/min.\nApplied to all G1 cutting moves.\nMust be > 0.",
        7 => {
            "Bézier curve tolerance in mm.\nSmaller = more GCode lines,\nbetter curve accuracy.\nTypical: 0.01 – 0.5"
        }
        8 => "Dots-per-inch assumed for SVG\npixel / point / pica units.\nStandard web DPI is 96.",
        9 => "Laser power written into the\nbegin sequence (S word).\nRange: 0 – Max laser power.",
        10 => "X offset in mm applied to the\nwhole job origin.\n0 = start at machine origin.",
        11 => "Y offset in mm applied to the\nwhole job origin.\n0 = start at machine origin.",
        12 => {
            "Emit G2/G3 arc commands instead\nof linearising curves.\nRequires GRBL firmware support.\nToggle with Enter or Space."
        }
        13 => "Prepend line numbers (N-words)\nto every GCode line.\nToggle with Enter or Space.",
        14 => "Append XOR checksums to every\nGCode line.\nToggle with Enter or Space.",
        _ => "",
    };

    let in_edit = app.settings_edit_buf.is_some();
    let keys = if in_edit {
        " Enter:save  Esc:cancel  Backspace:delete"
    } else {
        " ↑↓:navigate  Enter/e:edit  Space:toggle  r:reset"
    };

    let block = Block::default()
        .title(Span::styled(" Field Info ", Style::default().fg(C_TITLE)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER_INACT));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(description)
            .style(Style::default().fg(C_INFO))
            .wrap(Wrap { trim: false }),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(keys).style(Style::default().fg(C_SENT)),
        rows[1],
    );
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    // Left: context hint or status message
    let left_text = if let Some(msg) = &app.status_message {
        msg.as_str()
    } else {
        match app.active_tab {
            ActiveTab::Connect => match (app.mode, app.focused) {
                (AppMode::Disconnected, FocusedPane::SerialList) => {
                    "F1-F5:tabs  ↑↓:ports  Enter:connect  b:baud  r:refresh  ?:help  q:quit"
                }
                (AppMode::Connected, FocusedPane::SerialList) => {
                    "F1-F5:tabs  d:disconnect  Tab:focus  ?:help  q:quit"
                }
                (_, FocusedPane::Console) => {
                    "F1-F5:tabs  ↑↓/PgUp/Dn:scroll  End:follow  Tab:focus  q:quit"
                }
                (_, FocusedPane::CommandInput) => {
                    "F1-F5:tabs  Enter:send  ↑↓:history  Tab:focus  Esc:clear  q:quit"
                }
            },
            ActiveTab::Control => {
                use crate::app::ControlFocus;
                match app.control_focus {
                    ControlFocus::Jog => {
                        "F1-F5:tabs  Tab:focus  ←→↑↓:jog XY  PgUp/Dn:jog Z  +/-:step  ?:poll  p:auto-poll  ~:resume  !:hold  q:quit"
                    }
                    ControlFocus::Overrides => {
                        "F1-F5:tabs  Tab:focus  ↑↓:feed±10%  ←→:feed±1%  f:feed100%  r:rapid100%  PgUp/Dn:laser±10%  s:laser100%  q:quit"
                    }
                    ControlFocus::Settings => {
                        "F1-F5:tabs  Tab:focus  ↑↓:navigate  r:refresh($$)  q:quit"
                    }
                }
            }
            ActiveTab::GCode => {
                "F1-F5:tabs  o:open SVG  c:convert  s:save  g:send to GRBL  ↑↓/PgUp/Dn:scroll  q:quit"
            }
            ActiveTab::Preview => "F1-F5:tabs  p:render preview  q:quit",
            ActiveTab::Settings => {
                if app.settings_edit_buf.is_some() {
                    "Enter:save  Esc:cancel  Backspace:delete char"
                } else {
                    "F1-F5:tabs  ↑↓/j/k:navigate  Enter/e:edit  Space:toggle bool  r:reset defaults  q:quit"
                }
            }
        }
    };

    // Right: connection indicator
    let (right_text, right_colour) = match &app.connected_port {
        Some(port) => (
            format!(" ● {} @ {} ", port, app.selected_baud().label()),
            C_CONN,
        ),
        None => (" ○ Disconnected ".to_string(), C_DISC),
    };

    let right_w = right_text.len() as u16;
    let left_w = area.width.saturating_sub(right_w);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_w), Constraint::Length(right_w)])
        .split(area);

    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", left_text),
            Style::default().fg(Color::White),
        ))
        .style(Style::default().bg(C_STATUS_BG)),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(Span::styled(
            right_text,
            Style::default()
                .fg(right_colour)
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Right)
        .style(Style::default().bg(C_STATUS_BG)),
        chunks[1],
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Help overlay
// ═══════════════════════════════════════════════════════════════════════════════

pub fn render_help_overlay(frame: &mut Frame) {
    let area = frame.area();
    let width = 60u16.min(area.width);
    let height = 58u16.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            " Help – Larris GRBL Laser Terminal ",
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(C_HL));

    let help = vec![
        Line::from(Span::styled(
            "  GLOBAL",
            Style::default().fg(C_HL).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "  F1  Connect tab    F2  Control tab",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  F3  GCode tab      F4  Preview tab",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  F5  Settings tab",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Tab          Cycle pane / control focus",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  ?          Toggle this help overlay",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  q / Ctrl-C Quit",
            Style::default().fg(C_TITLE),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  CONNECT TAB",
            Style::default().fg(C_HL).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "  ↑ / ↓      Navigate port list",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Enter      Connect to selected port",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  b          Open baud-rate selector",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  r          Refresh port list",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  d (conn)   Disconnect from current port",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Console:  ↑↓/PgUp/PgDn scroll, End → follow",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Input:    type + Enter to send, ↑↓ for history",
            Style::default().fg(C_TITLE),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  GCODE TAB",
            Style::default().fg(C_HL).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "  o          Open SVG file (file picker)",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  c          Convert loaded SVG to GCode",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  s          Save GCode to file",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  g          Stream GCode to connected GRBL machine",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  ↑↓/PgUp/Dn Scroll GCode text",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Home/End   Jump to start / end of GCode",
            Style::default().fg(C_TITLE),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  CONTROL TAB  (Tab cycles: Jog → Overrides → Settings)",
            Style::default().fg(C_HL).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "  [Jog]  ←→↑↓ XY jog   PgUp/Dn Z jog   +/- step size",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  [Jog]  ~ resume  ! feed-hold  ^X soft-reset  $H home",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  [Jog]  ? poll status   p toggle auto-poll   $$ settings",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  [Ovr]  ↑↓ feed±10%  ←→ feed±1%  f/r/s reset to 100%",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  [Ovr]  PgUp/Dn laser±10%  5=rapid 50%  2=rapid 25%",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  [Set]  ↑↓ navigate   r refresh ($$)",
            Style::default().fg(C_TITLE),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  SETTINGS TAB",
            Style::default().fg(C_HL).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "  ↑↓ / j/k   Navigate fields",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Enter / e  Edit selected field",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  Space      Toggle boolean fields",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  r          Reset all to defaults",
            Style::default().fg(C_TITLE),
        )),
        Line::from(Span::styled(
            "  [edit] Enter:save  Esc:cancel",
            Style::default().fg(C_TITLE),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  PREVIEW TAB",
            Style::default().fg(C_HL).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(Span::styled(
            "  p          Render toolpath preview image",
            Style::default().fg(C_TITLE),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Press ? to close",
            Style::default().fg(C_INFO),
        )),
    ];

    frame.render_widget(
        Paragraph::new(help).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}
