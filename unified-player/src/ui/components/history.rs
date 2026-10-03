//! Shared journal and session-history row projections.

use std::collections::BTreeSet;

use chrono_humanize::HumanTime;

use crate::{
    state::{JournalList, SessionEntry, TrackJournalEntry},
    ui::utils::{bounded_text, to_bidi_string},
};

/// Provider-neutral fields shared by journal and session-history rows.
///
/// The surrounding surface owns its provider-specific columns (ratings,
/// notes, selection markers, and actions), while this projection keeps the
/// identity and temporal metadata consistent everywhere the row appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRowProjection {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) time: String,
    provider_kind: Option<String>,
    duration: Option<String>,
}

impl HistoryRowProjection {
    pub(crate) fn from_session(entry: &SessionEntry) -> Self {
        Self {
            title: to_bidi_string(&entry.title),
            artist: to_bidi_string(&entry.artists),
            time: session_time(entry.started_at),
            provider_kind: Some(format!(
                "{}/{}",
                provider_label(entry),
                media_kind_label(entry)
            )),
            duration: Some(entry.duration_ms.map_or_else(
                || "?:??".to_owned(),
                |millis| format!("{}:{:02}", millis / 60_000, (millis / 1_000) % 60),
            )),
        }
    }

    pub(crate) fn from_journal(entry: &TrackJournalEntry) -> Self {
        Self {
            title: to_bidi_string(&entry.track.display_name()),
            artist: to_bidi_string(&entry.track.artists_info()),
            time: updated_date(entry.updated_at),
            provider_kind: None,
            duration: None,
        }
    }

    pub(crate) fn session_label(&self, marker: &str) -> String {
        format!(
            "{marker}{}  {} • {} • {} • {}",
            self.provider_kind.as_deref().unwrap_or_default(),
            self.title,
            self.artist,
            self.duration.as_deref().unwrap_or_default(),
            self.time
        )
    }
}

/// Project session history once for both the full page and action-oriented
/// popup flows. The source iterator preserves the caller's newest-first
/// ordering and the selected set uses those same visible indices.
pub(crate) fn session_history_items_with_selection<'a>(
    entries: impl Iterator<Item = &'a SessionEntry>,
    maximum_width: usize,
    selected: &BTreeSet<usize>,
) -> Vec<(String, bool)> {
    entries
        .enumerate()
        .map(|(index, entry)| {
            let row = HistoryRowProjection::from_session(entry);
            let marker = session_history_marker(index, selected);
            (
                bounded_text(&row.session_label(marker), maximum_width),
                false,
            )
        })
        .collect()
}

pub(crate) fn session_history_marker(index: usize, selected: &BTreeSet<usize>) -> &'static str {
    if selected.is_empty() {
        ""
    } else if selected.contains(&index) {
        "[x] "
    } else {
        "[ ] "
    }
}

/// Shared projection for the Journal Lists page and its list-selection popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JournalListRowProjection {
    pub(crate) name: String,
    pub(crate) track_count: usize,
    pub(crate) updated: String,
}

impl JournalListRowProjection {
    pub(crate) fn from_list(list: &JournalList) -> Self {
        Self {
            name: to_bidi_string(&list.name),
            track_count: list.track_count(),
            updated: updated_date(list.updated_at),
        }
    }

    pub(crate) fn page_label(&self) -> String {
        format!(
            "{} | {} tracks | {}",
            self.name, self.track_count, self.updated
        )
    }

    pub(crate) fn popup_label(&self) -> String {
        format!("{} ({})", self.name, self.track_count)
    }
}

pub(crate) fn updated_date(updated_at: u64) -> String {
    chrono::DateTime::from_timestamp(updated_at as i64, 0)
        .map(|time| time.format("%b %d").to_string())
        .unwrap_or_default()
}

fn session_time(started_at: u64) -> String {
    chrono::DateTime::from_timestamp(started_at as i64, 0).map_or_else(
        || "unknown time".to_owned(),
        |time| HumanTime::from(time).to_string(),
    )
}

fn provider_label(entry: &SessionEntry) -> &'static str {
    match entry.media_id.provider {
        crate::state::Provider::Spotify => "Spotify",
        crate::state::Provider::YouTubeMusic => "YouTube",
    }
}

fn media_kind_label(entry: &SessionEntry) -> &'static str {
    match entry.media_id.kind {
        crate::state::MediaKind::Track => "track",
        crate::state::MediaKind::Video => "video",
        crate::state::MediaKind::Episode => "episode",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        session_history_items_with_selection, updated_date, HistoryRowProjection,
        JournalListRowProjection,
    };
    use crate::state::{JournalList, MediaId, MediaKind, Provider, SessionEntry};
    use std::collections::BTreeSet;

    fn entry(id: &str, title: &str) -> SessionEntry {
        SessionEntry {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: id.to_owned(),
            },
            title: title.to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration_ms: Some(61_000),
            started_at: 1,
        }
    }

    #[test]
    fn session_projection_preserves_newest_first_order_and_width_bound() {
        let entries = [entry("new", "A very long title"), entry("old", "Older")];
        let rows = session_history_items_with_selection(entries.iter(), 24, &BTreeSet::new());
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0.starts_with("Spotify/track"));
        assert!(!rows[0].0.contains("Older"));
        assert!(rows[1].0.contains("Older"));
        assert!(rows.iter().all(|(row, _)| row.chars().count() <= 24));
    }

    #[test]
    fn session_projection_keeps_provider_metadata_and_selection_marker() {
        let bidi_entry = entry("selected", "Mix שלום");
        let row = HistoryRowProjection::from_session(&bidi_entry);
        assert!(row
            .title
            .contains(&crate::ui::utils::to_bidi_string("Mix שלום")));
        let selected_entry = entry("selected", "Selected");
        let rows = session_history_items_with_selection(
            std::iter::once(&selected_entry),
            32,
            &BTreeSet::from([0usize]),
        );
        assert!(rows[0].0.starts_with("[x] Spotify/track"));
    }

    #[test]
    fn journal_list_projection_shares_page_and_popup_identity() {
        let list = JournalList {
            id: "id".to_owned(),
            name: "Mix שלום".to_owned(),
            track_uris: vec!["one".to_owned(), "two".to_owned()],
            created_at: 1,
            updated_at: 1,
        };
        let row = JournalListRowProjection::from_list(&list);
        assert!(row.page_label().contains("2 tracks"));
        assert!(row
            .popup_label()
            .contains(&crate::ui::utils::to_bidi_string("Mix שלום")));
        assert_eq!(row.updated, updated_date(1));
    }
}
