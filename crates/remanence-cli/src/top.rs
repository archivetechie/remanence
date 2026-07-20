#[cfg(feature = "tui")]
use std::io::{self, Write};
#[cfg(feature = "tui")]
use std::time::Duration;

use crate::{bytes_to_uuid_text, connect_daemon, daemon_runtime, drive_status_name, status_error};
#[cfg(feature = "tui")]
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
#[cfg(feature = "tui")]
use crossterm::execute;
#[cfg(feature = "tui")]
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
#[cfg(feature = "tui")]
use ratatui::backend::CrosstermBackend;
#[cfg(feature = "tui")]
use ratatui::layout::{Constraint, Direction, Layout, Rect};
#[cfg(feature = "tui")]
use ratatui::prelude::{Frame, Terminal};
#[cfg(feature = "tui")]
use ratatui::style::{Color, Modifier, Style};
#[cfg(feature = "tui")]
use ratatui::text::{Line, Span};
#[cfg(feature = "tui")]
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
#[cfg(feature = "tui")]
use remanence_api::pb;
#[cfg(feature = "tui")]
#[derive(Clone, Debug, Default)]
struct TopState {
    live: Option<pb::GetLiveStatusResponse>,
    selected_library: usize,
    show_slots: bool,
    paused: bool,
    help: bool,
}

#[cfg(feature = "tui")]
#[derive(Debug, Default)]
struct TerminalCleanupGuard {
    raw_mode_enabled: bool,
    alternate_screen_entered: bool,
}

#[cfg(feature = "tui")]
impl TerminalCleanupGuard {
    fn new() -> Self {
        Self::default()
    }

    fn mark_raw_mode_enabled(&mut self) {
        self.raw_mode_enabled = true;
    }

    fn mark_alternate_screen_entered(&mut self) {
        self.alternate_screen_entered = true;
    }

    #[cfg(test)]
    fn needs_cleanup(&self) -> bool {
        self.raw_mode_enabled || self.alternate_screen_entered
    }
}

#[cfg(feature = "tui")]
impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        if self.alternate_screen_entered {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
        if self.raw_mode_enabled {
            let _ = disable_raw_mode();
        }
    }
}

#[cfg(feature = "tui")]
pub(crate) fn run_top_tui(
    endpoint: &str,
    _out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::process::ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(crate::DaemonClientError::from)?;
            let mut library_client = pb::library_service_client::LibraryServiceClient::new(channel);

            let mut state = TopState {
                show_slots: true,
                ..Default::default()
            };

            let mut cleanup_guard = TerminalCleanupGuard::new();
            enable_raw_mode().map_err(|error| {
                crate::DaemonClientError::client(format!("enable raw mode: {error}"))
            })?;
            cleanup_guard.mark_raw_mode_enabled();
            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen).map_err(|error| {
                crate::DaemonClientError::client(format!("enter alternate screen: {error}"))
            })?;
            cleanup_guard.mark_alternate_screen_entered();
            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend).map_err(|error| {
                crate::DaemonClientError::client(format!("create terminal: {error}"))
            })?;

            let loop_result = async {
                loop {
                    if !state.paused {
                        refresh_top_state(&mut library_client, &mut state).await?;
                    }
                    terminal
                        .draw(|frame| render_top(frame, &state))
                        .map_err(|error| {
                            crate::DaemonClientError::client(format!("draw top view: {error}"))
                        })?;

                    if event::poll(Duration::from_millis(250)).map_err(|error| {
                        crate::DaemonClientError::client(format!("poll terminal event: {error}"))
                    })? {
                        if let Event::Key(key) = event::read().map_err(|error| {
                            crate::DaemonClientError::client(format!(
                                "read terminal event: {error}"
                            ))
                        })? {
                            if key.kind != KeyEventKind::Press {
                                continue;
                            }
                            match key.code {
                                KeyCode::Char('q') => break,
                                KeyCode::Char('p') | KeyCode::Char(' ') => {
                                    state.paused = !state.paused
                                }
                                KeyCode::Char('s') => state.show_slots = !state.show_slots,
                                KeyCode::Char('l') => {
                                    if let Some(live) = state.live.as_ref() {
                                        if !live.libraries.is_empty() {
                                            state.selected_library =
                                                (state.selected_library + 1) % live.libraries.len();
                                        }
                                    }
                                }
                                KeyCode::Char('?') => state.help = !state.help,
                                KeyCode::Esc => state.help = false,
                                _ => {}
                            }
                        }
                    }
                }
                Ok::<(), crate::DaemonClientError>(())
            }
            .await;
            loop_result
        })
    });
    crate::finish_daemon_client_result(result, false, err)
}

#[cfg(feature = "tui")]
async fn refresh_top_state(
    library_client: &mut pb::library_service_client::LibraryServiceClient<
        tonic::transport::Channel,
    >,
    state: &mut TopState,
) -> Result<(), crate::DaemonClientError> {
    let live = library_client
        .get_live_status(pb::GetLiveStatusRequest {})
        .await
        .map_err(status_error)?
        .into_inner();
    if state.selected_library >= live.libraries.len() {
        state.selected_library = 0;
    }
    state.live = Some(live);
    Ok(())
}

#[cfg(feature = "tui")]
fn render_top(frame: &mut Frame<'_>, state: &TopState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Min(7),
            Constraint::Length(if state.help { 4 } else { 2 }),
        ])
        .split(area);

    render_header(frame, chunks[0], state);
    render_pinned_band(frame, chunks[1], state);
    if state.show_slots {
        render_slot_grid(frame, chunks[2], state);
    } else {
        render_collapsed_slots(frame, chunks[2], state);
    }
    render_footer(frame, chunks[3], state);
}

#[cfg(feature = "tui")]
fn render_header(frame: &mut Frame<'_>, area: Rect, state: &TopState) {
    let title = match &state.live {
        Some(live) => format!(
            "rem top  snapshot {}  daemon {}",
            live.snapshot_at_utc, live.daemon_epoch
        ),
        None => "rem top".to_string(),
    };
    // One bordered row: borders consume 2 of the 3 header rows, so PAUSED
    // must live on the title line or real ratatui clips it.
    let mut spans = vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if state.paused {
        spans.push(Span::styled(
            "   ── PAUSED ──",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let paragraph =
        Paragraph::new(vec![Line::from(spans)]).block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

#[cfg(feature = "tui")]
fn render_pinned_band(frame: &mut Frame<'_>, area: Rect, state: &TopState) {
    let Some(live) = state.live.as_ref() else {
        let paragraph = Paragraph::new("waiting for daemon...")
            .block(Block::default().title("Live").borders(Borders::ALL));
        frame.render_widget(paragraph, area);
        return;
    };
    let alarms = if live.alarms.is_empty() {
        "no open alarms".to_string()
    } else {
        live.alarms
            .iter()
            .map(|alarm| format!("{} [{}]", alarm.condition_key, alarm.state))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let library = live.libraries.get(
        state
            .selected_library
            .min(live.libraries.len().saturating_sub(1)),
    );
    let rows = library
        .map(|library| {
            library
                .drives
                .iter()
                .map(|drive| {
                    let barcode = if drive.loaded_tape_barcode.is_empty() {
                        "-"
                    } else {
                        drive.loaded_tape_barcode.as_str()
                    };
                    let badges = drive_badges(drive);
                    Row::new(vec![
                        Cell::from(format!("{:04x}", drive.element_address)),
                        Cell::from(drive.drive_serial.clone()),
                        Cell::from(barcode.to_string()),
                        Cell::from(mount_age_label(drive)),
                        Cell::from(format!(
                            "{} {}",
                            drive_state_glyph(drive.status),
                            drive_status_name(drive.status)
                        )),
                        Cell::from(format!(
                            "{:.1}",
                            drive.tape_io_window_feed_bytes_per_second as f64 / 1_048_576.0
                        )),
                        Cell::from(format!(
                            "{:.1}",
                            drive.tape_io_effective_feed_bytes_per_second as f64 / 1_048_576.0
                        )),
                        Cell::from(badges),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Min(8),
        ],
    )
    .header(Row::new(vec![
        Cell::from("bay"),
        Cell::from("serial"),
        Cell::from("barcode"),
        Cell::from("age"),
        Cell::from("state"),
        Cell::from("win MB/s"),
        Cell::from("avg MB/s"),
        Cell::from("badges"),
    ]))
    .block(
        Block::default()
            .title(format!("alarms: {alarms}"))
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
}

#[cfg(feature = "tui")]
fn render_slot_grid(frame: &mut Frame<'_>, area: Rect, state: &TopState) {
    let Some(live) = state.live.as_ref() else {
        return;
    };
    let library = live.libraries.get(
        state
            .selected_library
            .min(live.libraries.len().saturating_sub(1)),
    );
    let text = library
        .map(|library| {
            let mut lines = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    "slots for {}",
                    library
                        .library
                        .as_ref()
                        .map(|lib| lib.library_serial.as_str())
                        .unwrap_or("<unknown>")
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for slot in &library.slots {
                lines.push(Line::from(format!(
                    "{:04x}  {}  {}",
                    slot.element_address,
                    if slot.voltag.is_empty() {
                        "-"
                    } else {
                        slot.voltag.as_str()
                    },
                    if slot.tape_uuid.is_empty() {
                        "-".to_string()
                    } else {
                        bytes_to_uuid_text(&slot.tape_uuid)
                    }
                )));
            }
            lines
        })
        .unwrap_or_else(|| vec![Line::from("no library selected")]);
    let paragraph = Paragraph::new(text)
        .block(Block::default().title("Slot Grid").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

#[cfg(feature = "tui")]
fn render_collapsed_slots(frame: &mut Frame<'_>, area: Rect, _state: &TopState) {
    let paragraph = Paragraph::new("slot grid collapsed")
        .block(Block::default().title("Slot Grid").borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

#[cfg(feature = "tui")]
fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TopState) {
    let text = if state.help {
        vec![
            Line::from("q quit   l next library   s toggle slots   p pause   ? help"),
            Line::from("drive detail stays in `rem drive show`"),
        ]
    } else {
        vec![Line::from(
            "q quit   l next library   s toggle slots   p pause   ? help",
        )]
    };
    let paragraph = Paragraph::new(text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

#[cfg(feature = "tui")]
fn drive_state_glyph(value: i32) -> &'static str {
    match value {
        1 => "I",
        2 => "L",
        3 => "B",
        4 => "U",
        5 => "C",
        6 => "F",
        _ => "?",
    }
}

#[cfg(feature = "tui")]
fn drive_badges(drive: &pb::Drive) -> String {
    let mut badges = Vec::new();
    if drive.fenced {
        badges.push("fenced");
    }
    if !drive.cleaning_due.is_empty() && drive.cleaning_due != "none" {
        badges.push(drive.cleaning_due.as_str());
    }
    badges.extend(drive.active_alert_names.iter().map(String::as_str));
    if badges.is_empty() {
        "-".to_string()
    } else {
        badges.join(",")
    }
}

#[cfg(feature = "tui")]
fn mount_age_label(drive: &pb::Drive) -> String {
    if drive.loaded_tape_barcode.is_empty() {
        return "-".to_string();
    }
    let seconds = drive.mount_age_seconds;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

#[cfg(feature = "tui")]
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use uuid::Uuid;

    fn sample_response() -> pb::GetLiveStatusResponse {
        pb::GetLiveStatusResponse {
            libraries: vec![pb::LibraryState {
                library: Some(pb::Library {
                    library_serial: "MAINLIB".to_string(),
                    vendor: "HPE".to_string(),
                    product: "MSL".to_string(),
                    product_revision: "6.40".to_string(),
                    library_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
                }),
                drives: vec![pb::Drive {
                    element_address: 0x0100,
                    drive_serial: "DRV-01".to_string(),
                    host_device_path: "/dev/sg1".to_string(),
                    vendor: "HPE".to_string(),
                    product: "LTO".to_string(),
                    loaded_tape_uuid: Uuid::from_u128(2).as_bytes().to_vec(),
                    status: pb::drive::Status::DriveStatusCleaning as i32,
                    drive_uuid: Uuid::from_u128(3).as_bytes().to_vec(),
                    cleaning_due: "now".to_string(),
                    fenced: true,
                    lifetime_read_bytes: 1_048_576,
                    lifetime_write_bytes: 2_097_152,
                    counter_epoch: 42,
                    session_id: Uuid::from_u128(4).as_bytes().to_vec(),
                    active_alert_names: vec!["cleaning".to_string()],
                    loaded_tape_barcode: "CLN001".to_string(),
                    mount_age_seconds: 83,
                    tape_io_window_feed_bytes_per_second: 304 * 1024 * 1024,
                    tape_io_effective_feed_bytes_per_second: 13 * 1024 * 1024,
                    ..Default::default()
                }],
                slots: vec![pb::Slot {
                    element_address: 0x0200,
                    voltag: "CLN001".to_string(),
                    tape_uuid: Uuid::from_u128(2).as_bytes().to_vec(),
                }],
                import_export_ports: Vec::new(),
                last_inventory_at: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                managed: "rem".to_string(),
            }],
            operations: vec![pb::OperationRef {
                operation_id: Uuid::from_u128(5).as_bytes().to_vec(),
            }],
            alarms: vec![pb::Alarm {
                alarm_id: 1,
                condition_key: "kind:scope".to_string(),
                kind: "kind".to_string(),
                severity: "warning".to_string(),
                state: "open".to_string(),
                first_seen_utc: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                last_seen_utc: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                acked_by: String::new(),
                acked_at_utc: None,
                detail: String::new(),
            }],
            snapshot_at_utc: "2026-07-04T00:00:00Z".to_string(),
            daemon_epoch: 17,
            drive_assignments: Vec::new(),
        }
    }

    #[test]
    fn renders_pinned_band_and_paused_banner() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let state = TopState {
            live: Some(sample_response()),
            show_slots: true,
            paused: true,
            help: true,
            ..Default::default()
        };
        terminal
            .draw(|frame| render_top(frame, &state))
            .expect("draw");
        let buffer = terminal.backend_mut().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("PAUSED"));
        assert!(text.contains("win MB/s"));
        assert!(text.contains("avg MB/s"));
        assert!(text.contains("DRV-01"));
        assert!(text.contains("CLN001"));
        assert!(text.contains("1m23s"));
    }

    #[test]
    fn terminal_cleanup_guard_tracks_entered_modes() {
        let mut guard = TerminalCleanupGuard::new();
        assert!(!guard.needs_cleanup());
        guard.mark_raw_mode_enabled();
        assert!(guard.needs_cleanup());
        guard.mark_alternate_screen_entered();
        assert!(guard.needs_cleanup());
    }

    #[test]
    fn mount_age_labels_loaded_and_empty_bays() {
        let drive = &sample_response().libraries[0].drives[0];
        assert_eq!(mount_age_label(drive), "1m23s");
        assert_eq!(mount_age_label(&pb::Drive::default()), "-");
    }
}
