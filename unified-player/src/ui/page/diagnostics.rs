use crate::config;
use crate::state::{PageState, SharedState, UIStateGuard, WorkspaceHit};
use crate::ui::utils::{self, construct_and_render_block, to_bidi_string};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

pub fn render_logs_page(frame: &mut Frame, state: &SharedState, ui: &mut UIStateGuard, rect: Rect) {
    let theme = ui.theme.clone();
    let rect = {
        let content = super::render_workspace_page_layout(frame, state, ui, rect);
        super::workspace_content_frame(frame, ui, content, "Diagnostics")
    };

    let health = state.diagnostics.health_snapshot();
    let filter = state.diagnostics.filter_snapshot();
    let incidents = state.diagnostics.incident_states();
    let follow_reference = match ui.current_page() {
        PageState::Logs { state } => state.follow_reference.clone(),
        _ => return,
    };
    let mut operations = state.diagnostics.recent_operations(8);
    if let Some(timeline) = follow_reference
        .as_deref()
        .and_then(|reference| state.diagnostics.timeline(reference))
    {
        if !operations
            .iter()
            .any(|operation| operation.reference == timeline.reference)
        {
            operations.push(timeline);
        }
    }
    let mut rows = crate::observability::diagnostic_rows(&health, filter, &incidents, &operations);
    let youtube_route = state
        .player
        .read()
        .youtube_playback
        .as_ref()
        .map(|playback| playback.route.clone());
    crate::observability::append_youtube_playback_route_row(&mut rows, youtube_route.as_ref());
    #[cfg(feature = "private-capture")]
    let rows = {
        let mut rows = rows;
        rows.push(crate::observability::private_capture_row(
            &state.private_capture_operator_snapshot(),
        ));
        rows
    };

    let (selected, follow) = {
        let PageState::Logs { state: page } = ui.current_page_mut() else {
            unreachable!("logs page changed while rendering")
        };
        page.synchronize(&rows);
        (page.selected_row.clone(), page.follow_reference.clone())
    };
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();

    let header_line = diagnostics_header_line(&theme, &health, &incidents);
    let header_height = Paragraph::new(header_line.clone())
        .wrap(Wrap { trim: false })
        .line_count(rect.width.max(1))
        .clamp(1, 3) as u16;
    let chunks =
        Layout::vertical([Constraint::Length(header_height), Constraint::Fill(1)]).split(rect);
    render_diagnostics_header(frame, header_line, chunks[0]);

    let panels = ui.layout_policy().diagnostics_panes(chunks[1]);
    let overview_rect = construct_and_render_block(
        "Overview",
        &theme,
        ratatui::widgets::Borders::ALL,
        frame,
        panels[0],
    );
    let inspector_rect = construct_and_render_block(
        "Inspector",
        &theme,
        ratatui::widgets::Borders::ALL,
        frame,
        panels[1],
    );
    for panel in [overview_rect, inspector_rect] {
        frame.render_widget(
            ratatui::widgets::Block::default().style(theme.workspace_panel()),
            panel,
        );
    }

    let PageState::Logs { state: page } = ui.current_page_mut() else {
        unreachable!("logs page changed while rendering")
    };
    render_diagnostic_overview(
        frame,
        &theme,
        &rows,
        follow.as_deref(),
        focused_overflow,
        focused_phase,
        overview_rect,
        &mut page.list,
    );
    record_diagnostic_row_hits(ui, &rows, overview_rect);
    render_diagnostic_inspector(
        frame,
        &theme,
        &rows,
        selected.as_ref(),
        &health,
        &incidents,
        &operations,
        inspector_rect,
    );
}

fn record_diagnostic_row_hits(
    ui: &mut UIStateGuard,
    rows: &[crate::observability::DiagnosticRow],
    rect: Rect,
) {
    let list_offset = match ui.current_page() {
        PageState::Logs { state } => state.list.offset(),
        _ => return,
    };
    let mut previous_section = None;
    let row_heights = rows
        .iter()
        .map(|row| {
            let section = diagnostic_section(&row.id);
            let height = if previous_section == Some(section) {
                1
            } else {
                2
            };
            previous_section = Some(section);
            height
        })
        .collect::<Vec<_>>();
    let mut y = rect.y;
    for (index, &height) in row_heights.iter().enumerate().skip(list_offset) {
        let height = height as u16;
        if y >= rect.bottom() {
            break;
        }
        let summary_y = y.saturating_add(height.saturating_sub(1));
        if summary_y < rect.bottom() {
            ui.workspace_hits.push((
                Rect::new(rect.x, summary_y, rect.width, 1),
                WorkspaceHit::DiagnosticRow(index),
            ));
        }
        y = y.saturating_add(height);
    }
}

fn diagnostic_section(id: &crate::observability::DiagnosticRowId) -> &'static str {
    use crate::observability::DiagnosticRowId as Row;
    match id {
        Row::ComponentLoading
        | Row::Component(_)
        | Row::Worker(_)
        | Row::WorkersEmpty
        | Row::Logging => "HEALTH",
        Row::Operation(_) | Row::OperationsEmpty => "OPERATIONS",
        Row::Incident(_) | Row::IncidentsEmpty => "INCIDENTS",
        Row::PlaybackRoute => "PLAYBACK",
        Row::Filter | Row::Support | Row::Performance => "TOOLS",
        #[cfg(feature = "private-capture")]
        Row::PrivateCapture => "TOOLS",
    }
}

fn diagnostic_status_style(
    theme: &config::Theme,
    severity: crate::observability::Severity,
) -> Style {
    match severity {
        crate::observability::Severity::Error => theme.workspace_status_error(),
        crate::observability::Severity::Warn => theme.workspace_status_warning(),
        crate::observability::Severity::Trace
        | crate::observability::Severity::Debug
        | crate::observability::Severity::Info => theme.workspace_status_info(),
    }
}

fn diagnostic_status_marker(severity: crate::observability::Severity) -> &'static str {
    match severity {
        crate::observability::Severity::Error => "[ERR]",
        crate::observability::Severity::Warn => "[WARN]",
        _ => "[OK]",
    }
}

fn diagnostic_short_label(label: &str) -> &str {
    label
        .split_once(" / ")
        .map_or(label, |(_, remainder)| remainder)
}

fn diagnostic_detail_text(text: &str) -> String {
    to_bidi_string(text)
}

fn diagnostic_overview_summary(
    prefix: &str,
    severity: crate::observability::Severity,
    label: &str,
    summary: &str,
) -> String {
    format!(
        "{prefix}{} {} | {}",
        diagnostic_status_marker(severity),
        to_bidi_string(diagnostic_short_label(label)),
        to_bidi_string(summary),
    )
}

fn render_diagnostics_header(frame: &mut Frame, line: Line<'_>, rect: Rect) {
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), rect);
}

fn diagnostics_header_line<'a>(
    theme: &config::Theme,
    health: &'a crate::observability::HealthSnapshot,
    incidents: &'a [(crate::observability::IncidentSummary, bool)],
) -> Line<'a> {
    let has_unacknowledged_incident = incidents.iter().any(|(_, acknowledged)| !acknowledged);
    let has_degraded_health = health.logging_status != crate::observability::HealthStatus::Healthy
        || health.components.iter().any(|component| {
            !matches!(
                component.status,
                crate::observability::HealthStatus::Healthy
                    | crate::observability::HealthStatus::Busy
            )
        });
    let (status, status_style) = if has_unacknowledged_incident || has_degraded_health {
        ("DEGRADED", theme.workspace_status_warning())
    } else if health.active_operation.is_some() {
        ("BUSY", theme.workspace_status_busy())
    } else {
        ("HEALTHY", theme.workspace_status_success())
    };
    let provider = health
        .ui
        .as_ref()
        .map_or("unknown", |snapshot| snapshot.provider.as_str());
    let unacknowledged = incidents
        .iter()
        .filter(|(_, acknowledged)| !acknowledged)
        .count();
    let reason = diagnostic_health_reason(health, incidents);
    Line::from(vec![
        Span::styled("Status ", theme.workspace_secondary_text()),
        Span::styled(status, status_style),
        Span::raw(": "),
        Span::styled(reason, theme.workspace_status_info()),
        Span::raw("   Provider "),
        Span::styled(provider, theme.workspace_status_success()),
        Span::raw("   Logging "),
        Span::styled(
            health.logging_status.label(),
            diagnostic_status_style(theme, crate::observability::Severity::Info),
        ),
        Span::raw(format!(
            "   Incidents {} ({} open)",
            incidents.len(),
            unacknowledged
        )),
    ])
}

fn diagnostic_health_reason(
    health: &crate::observability::HealthSnapshot,
    incidents: &[(crate::observability::IncidentSummary, bool)],
) -> String {
    let open_incidents = incidents
        .iter()
        .filter(|(_, acknowledged)| !acknowledged)
        .count();
    if open_incidents > 0 {
        return format!(
            "{} open incident{}",
            open_incidents,
            if open_incidents == 1 { "" } else { "s" }
        );
    }

    if health.logging_status != crate::observability::HealthStatus::Healthy {
        return format!("logging {}", health.logging_status.label());
    }

    let unknown_components = health
        .components
        .iter()
        .filter(|component| component.status == crate::observability::HealthStatus::Unknown)
        .count();
    if unknown_components > 0 {
        return format!(
            "{} health check{} unknown",
            unknown_components,
            if unknown_components == 1 { "" } else { "s" }
        );
    }

    let attention_components = health
        .components
        .iter()
        .filter(|component| {
            !matches!(
                component.status,
                crate::observability::HealthStatus::Healthy
                    | crate::observability::HealthStatus::Busy
            )
        })
        .count();
    if attention_components > 0 {
        return format!(
            "{} health check{} need attention",
            attention_components,
            if attention_components == 1 { "" } else { "s" }
        );
    }

    if health.active_operation.is_some() {
        return "operation active".to_owned();
    }

    "all checks nominal".to_owned()
}

fn render_diagnostic_overview(
    frame: &mut Frame,
    theme: &config::Theme,
    rows: &[crate::observability::DiagnosticRow],
    follow: Option<&str>,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
    rect: Rect,
    list_state: &mut ListState,
) {
    // Section headers occupy an extra terminal line inside their first row, so
    // use visual row heights rather than treating every item as one line.
    let mut previous_section = None;
    let row_heights = rows
        .iter()
        .map(|row| {
            let section = diagnostic_section(&row.id);
            let height = if previous_section == Some(section) {
                1
            } else {
                2
            };
            previous_section = Some(section);
            height
        })
        .collect::<Vec<_>>();
    utils::adjust_multiline_list_offset(list_state, &row_heights, rect.height);
    let row_width = rect.width.saturating_sub(2) as usize;
    let mut previous_section = None;
    let items = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let section = diagnostic_section(&row.id);
            let mut lines = Vec::with_capacity(2);
            if previous_section != Some(section) {
                let section_count = rows
                    .iter()
                    .filter(|candidate| diagnostic_section(&candidate.id) == section)
                    .count();
                lines.push(Line::styled(
                    utils::bounded_text(&format!("{section}  ({section_count})"), row_width),
                    theme.workspace_status_success(),
                ));
                previous_section = Some(section);
            }
            let followed = matches!(
                &row.id,
                crate::observability::DiagnosticRowId::Operation(reference)
                    if follow == Some(reference.as_str())
            );
            let prefix = if followed { "* " } else { "  " };
            let style = if row.acknowledged {
                theme.workspace_disabled()
            } else {
                diagnostic_status_style(theme, row.severity)
            };
            let summary =
                diagnostic_overview_summary(prefix, row.severity, &row.label, &row.summary);
            let summary = if list_state.selected() == Some(index) {
                utils::focused_overflow_text(&summary, row_width, focused_overflow, focused_phase)
            } else {
                utils::bounded_text(&summary, row_width)
            };
            lines.push(Line::styled(summary, style));
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_style(theme.workspace_selection_active())
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, rect, list_state);
}

fn render_diagnostic_inspector(
    frame: &mut Frame,
    theme: &config::Theme,
    rows: &[crate::observability::DiagnosticRow],
    selected: Option<&crate::observability::DiagnosticRowId>,
    health: &crate::observability::HealthSnapshot,
    incidents: &[(crate::observability::IncidentSummary, bool)],
    operations: &[crate::observability::OperationTimeline],
    rect: Rect,
) {
    let Some(selected) = selected else {
        frame.render_widget(
            Paragraph::new("Select a diagnostic row to inspect.")
                .style(theme.workspace_secondary_text()),
            rect,
        );
        return;
    };
    let Some(row) = rows.iter().find(|row| &row.id == selected) else {
        return;
    };
    let mut lines = vec![Line::styled(
        diagnostic_detail_text(&row.label),
        diagnostic_status_style(theme, row.severity),
    )];
    lines.push(Line::raw(String::new()));
    match selected {
        crate::observability::DiagnosticRowId::Incident(reference) => {
            if let Some((incident, acknowledged)) = incidents
                .iter()
                .find(|(incident, _)| incident.safe_reference() == reference)
            {
                lines.extend(incident.render_lines().into_iter().map(Line::raw));
                lines.push(Line::raw(format!(
                    "State: {}   Occurrences: {}",
                    if *acknowledged {
                        "acknowledged"
                    } else {
                        "open"
                    },
                    incident.occurrence_count
                )));
            }
        }
        crate::observability::DiagnosticRowId::Component(component) => {
            if let Some(item) = health
                .components
                .iter()
                .find(|item| item.component == *component)
            {
                lines.push(Line::raw(format!("Status: {}", item.status.label())));
                lines.push(Line::raw(format!(
                    "Fact: {}",
                    diagnostic_detail_text(&item.fact)
                )));
                lines.push(Line::raw(format!("Updated: +{}ms", item.updated_uptime_ms)));
            }
        }
        crate::observability::DiagnosticRowId::Worker(worker) => {
            if let Some(item) = health.workers.iter().find(|item| &item.worker == worker) {
                lines.push(Line::raw(format!("Status: {}", item.status.label())));
                lines.push(Line::raw(format!("Updated: +{}ms", item.updated_uptime_ms)));
            }
        }
        crate::observability::DiagnosticRowId::Operation(reference) => {
            if let Some(operation) = operations
                .iter()
                .find(|operation| &operation.reference == reference)
            {
                lines.push(Line::raw(format!("Reference: {}", operation.reference)));
                lines.push(Line::raw(format!(
                    "Outcome: {}",
                    operation
                        .outcome
                        .map_or("running", crate::observability::outcome_label)
                )));
                lines.push(Line::raw(String::new()));
                lines.extend(
                    operation
                        .entries
                        .iter()
                        .map(|entry| Line::raw(entry.render())),
                );
            }
        }
        _ => lines.push(Line::raw(diagnostic_detail_text(&row.summary))),
    }
    // Keep the inspector's detail payload intact and let ratatui wrap it to
    // the panel width. The overview is intentionally bounded; the inspector
    // is where the complete diagnostic value should remain readable.
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
}

#[cfg(test)]
mod diagnostics_page_tests {
    use super::{
        diagnostic_detail_text, diagnostic_health_reason, diagnostic_overview_summary,
        diagnostic_section, diagnostic_short_label, diagnostic_status_style,
        diagnostics_header_line, render_diagnostic_inspector, render_diagnostic_overview,
    };
    use crate::observability::{
        Component, DiagnosticEvent, DiagnosticRow, DiagnosticRowId, DynamicFilterSnapshot,
        EventCode, EventName, HealthRegistry, HealthStatus, IncidentSummary, OperationContext,
        OperationOutcome, OperationSource, Severity, WriterHealth, WriterState,
    };
    use crate::ui::utils::to_bidi_string;
    use ratatui::{
        backend::TestBackend,
        widgets::{ListState, Paragraph, Wrap},
        Terminal,
    };
    use std::time::Instant;

    fn snapshot(
        registry: &HealthRegistry,
        writer_state: WriterState,
        dropped_events: u64,
    ) -> crate::observability::HealthSnapshot {
        registry.snapshot(WriterHealth {
            state: writer_state,
            dropped_events,
            files_created: 0,
            bytes_written: 0,
        })
    }

    fn lines(
        snapshot: &crate::observability::HealthSnapshot,
        incidents: &[IncidentSummary],
    ) -> String {
        let incidents = incidents
            .iter()
            .cloned()
            .map(|incident| (incident, false))
            .collect::<Vec<_>>();
        crate::observability::diagnostic_rows(
            snapshot,
            DynamicFilterSnapshot {
                level: Severity::Info,
                temporary: false,
                remaining_seconds: 0,
                available: true,
            },
            &incidents,
            &[],
        )
        .into_iter()
        .map(|row| format!("{}: {}", row.label, row.summary))
        .collect::<Vec<_>>()
        .join("\n")
    }

    #[test]
    fn diagnostics_workspace_uses_grouped_labels() {
        assert_eq!(
            diagnostic_short_label("Incident / I-ab12cd34 / REQUEST_FAILED"),
            "I-ab12cd34 / REQUEST_FAILED"
        );
        assert_eq!(
            diagnostic_short_label("Support / reviewable bundle"),
            "reviewable bundle"
        );
        assert_eq!(
            diagnostic_section(&crate::observability::DiagnosticRowId::Logging),
            "HEALTH"
        );
        assert_eq!(
            diagnostic_section(&crate::observability::DiagnosticRowId::Incident(
                "I-ab12cd34".to_owned()
            )),
            "INCIDENTS"
        );
        assert_eq!(
            diagnostic_section(&crate::observability::DiagnosticRowId::PlaybackRoute),
            "PLAYBACK"
        );
    }

    #[test]
    fn workspace_diagnostics_use_semantic_status_roles() {
        let theme = crate::config::Theme::default();

        assert_eq!(
            diagnostic_status_style(&theme, Severity::Info),
            theme.workspace_status_info()
        );
        assert_eq!(
            diagnostic_status_style(&theme, Severity::Warn),
            theme.workspace_status_warning()
        );
        assert_eq!(
            diagnostic_status_style(&theme, Severity::Error),
            theme.workspace_status_error()
        );
    }

    #[test]
    fn diagnostics_overview_projects_dynamic_labels_and_summaries_for_bidi_text() {
        let summary =
            diagnostic_overview_summary("  ", Severity::Info, "Operation / שלום", "completed עולם");
        assert!(summary.contains(&to_bidi_string("שלום")));
        assert!(summary.contains(&to_bidi_string("completed עולם")));
    }

    #[test]
    fn diagnostics_inspector_projects_detail_text_for_bidi_text() {
        assert_eq!(
            diagnostic_detail_text("Route שלום -> WEB"),
            to_bidi_string("Route שלום -> WEB")
        );
    }

    #[test]
    fn diagnostics_header_wraps_with_the_panel_width() {
        let registry = HealthRegistry::default();
        let health = snapshot(&registry, WriterState::Healthy, 0);
        let line = diagnostics_header_line(&crate::config::Theme::default(), &health, &[]);
        let wrapped = Paragraph::new(line).wrap(Wrap { trim: false });
        assert!(wrapped.line_count(24) > 1);
    }

    #[test]
    fn diagnostics_header_explains_degraded_health_without_incidents() {
        let registry = HealthRegistry::default();
        registry.set_component(Component::Browser, HealthStatus::Unknown, "awaiting", 0);
        registry.set_component(Component::Audio, HealthStatus::Unknown, "awaiting", 0);
        let health = snapshot(&registry, WriterState::Healthy, 0);

        assert_eq!(
            diagnostic_health_reason(&health, &[]),
            "2 health checks unknown"
        );
    }

    #[test]
    fn diagnostics_inspector_wraps_long_route_details_without_ellipsis() {
        let registry = HealthRegistry::default();
        let health = snapshot(&registry, WriterState::Healthy, 0);
        let rows = vec![DiagnosticRow {
            id: DiagnosticRowId::PlaybackRoute,
            label: "Playback / YouTube route".to_owned(),
            summary: "route (learned): ANDROID_VR -> TVHTML5 -> WEB -> WEB_REMIX -> WEB_MUSIC_BROWSER_SESSION | selected: WEB_REMIX".to_owned(),
            severity: Severity::Info,
            acknowledged: false,
        }];
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| {
                render_diagnostic_inspector(
                    frame,
                    &crate::config::Theme::default(),
                    &rows,
                    Some(&DiagnosticRowId::PlaybackRoute),
                    &health,
                    &[],
                    &[],
                    frame.area(),
                );
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("WEB_MUSIC_BROWSER_SESSION"));
        assert!(!rendered.contains("WEB_MUSIC_BROWSER_SESS..."));
    }

    #[test]
    fn diagnostics_overview_keeps_the_selected_row_inside_the_panel() {
        let rows = (0..8)
            .map(|index| DiagnosticRow {
                id: DiagnosticRowId::Operation(format!("I-{index}")),
                label: format!("Operation / I-{index}"),
                summary: "completed".to_owned(),
                severity: Severity::Info,
                acknowledged: false,
            })
            .collect::<Vec<_>>();
        let mut list_state = ListState::default();
        list_state.select(Some(rows.len() - 1));
        let mut terminal = Terminal::new(TestBackend::new(36, 4)).unwrap();

        terminal
            .draw(|frame| {
                render_diagnostic_overview(
                    frame,
                    &crate::config::Theme::default(),
                    &rows,
                    None,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    frame.area(),
                    &mut list_state,
                );
            })
            .unwrap();

        assert_eq!(list_state.selected(), Some(rows.len() - 1));
        assert!(list_state.offset() > 0);
    }

    #[test]
    fn diagnostics_overview_scrolls_for_section_header_height() {
        let rows = vec![
            DiagnosticRow {
                id: DiagnosticRowId::Logging,
                label: "Logging".to_owned(),
                summary: "healthy".to_owned(),
                severity: Severity::Info,
                acknowledged: false,
            },
            DiagnosticRow {
                id: DiagnosticRowId::Operation("I-1".to_owned()),
                label: "Operation / I-1".to_owned(),
                summary: "completed".to_owned(),
                severity: Severity::Info,
                acknowledged: false,
            },
            DiagnosticRow {
                id: DiagnosticRowId::Operation("I-2".to_owned()),
                label: "Operation / I-2".to_owned(),
                summary: "completed".to_owned(),
                severity: Severity::Info,
                acknowledged: false,
            },
            DiagnosticRow {
                id: DiagnosticRowId::Incident("I-3".to_owned()),
                label: "Incident / I-3".to_owned(),
                summary: "failed".to_owned(),
                severity: Severity::Warn,
                acknowledged: false,
            },
        ];
        let mut list_state = ListState::default();
        list_state.select(Some(3));
        let mut terminal = Terminal::new(TestBackend::new(36, 4)).unwrap();

        terminal
            .draw(|frame| {
                render_diagnostic_overview(
                    frame,
                    &crate::config::Theme::default(),
                    &rows,
                    None,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    frame.area(),
                    &mut list_state,
                );
            })
            .unwrap();

        assert_eq!(list_state.offset(), 2);
    }

    #[test]
    fn diagnostics_page_explains_loading_empty_and_logging_recovery() {
        let registry = HealthRegistry::default();
        let loading = lines(&snapshot(&registry, WriterState::Starting, 0), &[]);
        assert!(loading.contains("Components / loading: awaiting first runtime health update"));
        assert!(loading.contains("Workers / empty: no worker transitions retained"));
        assert!(loading.contains("Operations / idle: no operation evidence retained"));
        assert!(loading.contains("Incidents / empty: no significant incidents in this run"));

        registry.set_component(
            Component::Runtime,
            crate::observability::HealthStatus::Healthy,
            "supervised",
            0,
        );

        let degraded = lines(&snapshot(&registry, WriterState::Degraded, 3), &[]);
        assert!(degraded.contains("Component / runtime: healthy / supervised"));
        assert!(degraded.contains("Logging / writer: degraded / dropped=3"));
        assert!(degraded.contains("Workers / empty: no worker transitions retained"));

        let mut worker = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::WORKER_TRANSITION,
            EventCode::WORKER_TRANSITION,
            Severity::Info,
            Component::Runtime,
            "Worker lifecycle changed",
        );
        worker.fields.worker = Some("client-handler".to_owned());
        worker.fields.state = Some("started".to_owned());
        registry.observe(&worker);

        let recovered = lines(&snapshot(&registry, WriterState::Healthy, 0), &[]);
        assert!(recovered.contains("Component / runtime: healthy / supervised"));
        assert!(recovered.contains("Worker / client handler: healthy"));
        assert!(recovered.contains("Logging / writer: healthy / dropped=0"));
    }

    #[test]
    fn diagnostics_page_distinguishes_running_and_each_terminal_state() {
        let registry = HealthRegistry::default();
        let accepted = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_ACCEPTED,
            EventCode::REQUEST_ACCEPTED,
            Severity::Debug,
            Component::Scheduler,
            "Request accepted",
        )
        .with_operation(OperationContext::new("playback", OperationSource::Terminal));
        registry.observe(&accepted);
        assert!(lines(&snapshot(&registry, WriterState::Healthy, 0), &[]).contains("Operation /"));
        assert!(lines(&snapshot(&registry, WriterState::Healthy, 0), &[])
            .contains("start playback: running / queued"));

        for outcome in [
            OperationOutcome::Error,
            OperationOutcome::Superseded,
            OperationOutcome::Success,
        ] {
            let mut completed = accepted.clone();
            completed.event_name = "request.completed".to_owned();
            completed.event_code = EventCode::REQUEST_COMPLETED.as_str().to_owned();
            completed.fields.outcome = Some(outcome);
            registry.observe(&completed);
            let rendered = lines(&snapshot(&registry, WriterState::Healthy, 0), &[]);
            assert!(rendered.contains(crate::observability::outcome_label(outcome)));
        }
    }

    #[test]
    fn diagnostics_page_renders_actionable_failure_without_private_activity() {
        let registry = HealthRegistry::default();
        let incident = IncidentSummary {
            reference: "I-ab12cd34".to_owned(),
            event_code: "REQUEST_COMPLETED".to_owned(),
            operation: "Start playback".to_owned(),
            provider: None,
            impact: "Playback may be affected; the application remains available".to_owned(),
            cause: "The operation is currently unavailable".to_owned(),
            retryable: true,
            next_action: "Retry the operation; use its incident reference if it repeats".to_owned(),
            component: Component::YoutubeMusic,
            component_health: "degraded".to_owned(),
            occurrence_count: 1,
        };
        let rendered = lines(
            &snapshot(&registry, WriterState::Healthy, 0),
            std::slice::from_ref(&incident),
        );
        let detail = incident.render_lines().join("\n");
        for expected in ["I-ab12cd34", "REQUEST_COMPLETED", "Start playback"] {
            assert!(rendered.contains(expected));
        }
        for expected in [
            "Start playback failed",
            "Impact:",
            "Cause:",
            "Retryable: yes",
            "Next:",
            "YoutubeMusic=degraded",
        ] {
            assert!(detail.contains(expected));
        }
        for forbidden in ["private title", "lyrics", "https://", "video-id"] {
            assert!(!rendered.contains(forbidden));
            assert!(!detail.contains(forbidden));
        }
    }

    #[test]
    fn ordinary_failure_messages_are_privacy_safe() {
        for message in [
            crate::state::YOUTUBE_CONTEXT_ERROR_MESSAGE,
            crate::state::YOUTUBE_LIBRARY_ERROR_MESSAGE,
            crate::state::SETTINGS_RELOAD_ERROR_MESSAGE,
            crate::state::SETTINGS_SAVE_ERROR_MESSAGE,
            crate::state::UNIFIED_PLAYLIST_ERROR_MESSAGE,
            crate::state::UNIFIED_PLAYLIST_ERROR_NEXT_ACTION,
        ] {
            for forbidden in ["https://", "video-id", "private title", "lyrics", "C:\\"] {
                assert!(!message.contains(forbidden), "message leaked {forbidden}");
            }
        }
        for message in [
            crate::state::LYRICS_PLAYBACK_UNAVAILABLE_MESSAGE,
            crate::state::LYRICS_PLAYBACK_UNAVAILABLE_NEXT_ACTION,
        ] {
            for forbidden in ["https://", "video-id", "private title", "C:\\"] {
                assert!(!message.contains(forbidden), "message leaked {forbidden}");
            }
        }
    }
}
