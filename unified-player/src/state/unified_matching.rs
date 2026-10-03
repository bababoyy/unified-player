//! Conservative metadata matching for provider-neutral playlist projection.
//!
//! A provider ID is authoritative when it is already present. This module is
//! only for an explicit, opt-in resolution step: it scores denormalized local
//! metadata against a bounded set of `YouTube` search results and accepts a
//! result only when it is both strong and unambiguous.

use std::collections::HashSet;

use super::{UnifiedPlaylistItem, YouTubeTrack};

const MIN_MATCH_SCORE: u16 = 65;
const MIN_SCORE_MARGIN: u16 = 10;

/// Return the highest-confidence `YouTube` candidate for a local item.
pub fn best_youtube_match<'a>(
    item: &UnifiedPlaylistItem,
    candidates: &'a [YouTubeTrack],
) -> Option<(&'a YouTubeTrack, u16)> {
    let mut ranked = candidates
        .iter()
        .filter_map(|candidate| Some((candidate, score_youtube_match(item, candidate)?)))
        .collect::<Vec<_>>();
    let exact = ranked
        .iter()
        .copied()
        .filter(|(candidate, _)| {
            normalize_tokens(&item.title) == normalize_tokens(&candidate.name)
                && normalize_tokens(&item.artists) == normalize_tokens(&candidate.artists)
        })
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        // Exact title/artist metadata is stronger evidence than an annotated
        // upload or a near-title result. Ignore weaker rows before applying
        // the ambiguity margin so a remote duplicate can be reused safely.
        ranked = exact;
    }
    ranked.sort_by(|(left_track, left_score), (right_track, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_track.is_video.cmp(&right_track.is_video))
            .then_with(|| left_track.id.cmp(&right_track.id))
    });

    let (best, score) = ranked.first().copied()?;
    let second_score = ranked.get(1).map_or(0, |(_, score)| *score);
    let exact_metadata = ranked.get(1).is_some_and(|(second, _)| {
        normalize_tokens(&best.name) == normalize_tokens(&second.name)
            && normalize_tokens(&best.artists) == normalize_tokens(&second.artists)
    });
    // A local duration gives us a useful discriminator even when YouTube
    // returns duplicate song rows from multiple result sections. Without it,
    // retain the conservative margin requirement for same-type ties.
    let same_metadata =
        exact_metadata && (best.is_video != ranked[1].0.is_video || item.duration_ms.is_some());
    (score >= MIN_MATCH_SCORE
        && (score.saturating_sub(second_score) >= MIN_SCORE_MARGIN || same_metadata))
        .then_some((best, score))
}

/// Score one candidate, returning `None` when title/artist evidence is weak.
pub fn score_youtube_match(item: &UnifiedPlaylistItem, candidate: &YouTubeTrack) -> Option<u16> {
    let title_score = text_score(&item.title, &candidate.name, 55);
    let artist_score = text_score(&item.artists, &candidate.artists, 30);
    if title_score < 30 || artist_score < 10 {
        return None;
    }
    Some(title_score + artist_score + duration_score(item.duration_ms, &candidate.duration))
}

fn text_score(expected: &str, actual: &str, maximum: u16) -> u16 {
    let expected = normalize_tokens(expected);
    let actual = normalize_tokens(actual);
    if expected.is_empty() || actual.is_empty() {
        return 0;
    }
    if expected == actual {
        return maximum;
    }
    if expected.is_subset(&actual) || actual.is_subset(&expected) {
        return maximum * 4 / 5;
    }
    let overlap = expected.intersection(&actual).count() as u16;
    let union = expected.union(&actual).count() as u16;
    if overlap == 0 || union == 0 {
        0
    } else {
        maximum * overlap / union
    }
}

fn normalize_tokens(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

fn duration_score(expected_ms: Option<u64>, actual: &str) -> u16 {
    let Some(expected_ms) = expected_ms else {
        return 0;
    };
    let Some(actual_ms) = parse_duration_ms(actual) else {
        return 0;
    };
    match expected_ms.abs_diff(actual_ms) {
        0..=1_500 => 20,
        1_501..=5_000 => 12,
        5_001..=15_000 => 5,
        _ => 0,
    }
}

fn parse_duration_ms(value: &str) -> Option<u64> {
    let mut seconds = 0_u64;
    let mut saw_component = false;
    for component in value.trim().split(':') {
        if component.trim().is_empty() {
            return None;
        }
        let value = component.trim().parse::<u64>().ok()?;
        seconds = seconds.checked_mul(60)?.checked_add(value)?;
        saw_component = true;
    }
    saw_component.then_some(seconds.saturating_mul(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MediaId, MediaKind, Provider};

    fn item(title: &str, artists: &str, duration_ms: Option<u64>) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "spotify-id".to_string(),
            },
            title: title.to_string(),
            artists: artists.to_string(),
            duration_ms,
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        }
    }

    fn candidate(id: &str, title: &str, artists: &str, duration: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_string(),
            name: title.to_string(),
            artists: artists.to_string(),
            album: None,
            duration: duration.to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    #[test]
    fn exact_metadata_wins_over_annotated_upload() {
        let item = item("Want Some More", "Nicki Minaj", Some(229_000));
        let candidates = vec![
            candidate("official", "Want Some More", "Nicki Minaj", "3:49"),
            candidate(
                "upload",
                "Want Some More (Official Audio)",
                "Nicki Minaj",
                "3:49",
            ),
        ];
        let (matched, score) = best_youtube_match(&item, &candidates).unwrap();
        assert_eq!(matched.id, "official");
        assert!(score >= MIN_MATCH_SCORE);
    }

    #[test]
    fn exact_metadata_ignores_a_near_title_search_result() {
        let item = item("Bad At Love", "Halsey", Some(236_000));
        let candidates = vec![
            candidate("exact", "Bad At Love", "Halsey", "3:56"),
            candidate("annotated", "Bad At Love (Live)", "Halsey", "3:56"),
        ];
        let (matched, _) = best_youtube_match(&item, &candidates).unwrap();
        assert_eq!(matched.id, "exact");
    }

    #[test]
    fn unrelated_artist_is_rejected() {
        let item = item("Want Some More", "Nicki Minaj", Some(229_000));
        let candidates = vec![candidate(
            "wrong",
            "Want Some More",
            "Another Artist",
            "3:49",
        )];
        assert!(best_youtube_match(&item, &candidates).is_none());
    }

    #[test]
    fn near_tie_is_rejected_to_avoid_guessing() {
        let item = item("Want Some More", "Nicki Minaj", None);
        let candidates = vec![
            candidate("a", "Want Some More", "Nicki Minaj", "3:49"),
            candidate("b", "Want Some More", "Nicki Minaj", "3:48"),
        ];
        assert!(best_youtube_match(&item, &candidates).is_none());
    }

    #[test]
    fn exact_song_and_video_duplicates_choose_the_song() {
        let item = item("Bad At Love", "Halsey", None);
        let candidates = vec![
            candidate("video", "Bad At Love", "Halsey", "3:56"),
            candidate("song", "Bad At Love", "Halsey", "3:56"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, mut track)| {
            track.is_video = index == 0;
            track
        })
        .collect::<Vec<_>>();
        let (matched, _) = best_youtube_match(&item, &candidates).unwrap();
        assert_eq!(matched.id, "song");
        assert!(!matched.is_video);
    }

    #[test]
    fn exact_metadata_with_duration_allows_same_type_duplicate() {
        let item = item("Bad At Love", "Halsey", Some(236_000));
        let candidates = vec![
            candidate("first", "Bad At Love", "Halsey", "3:56"),
            candidate("second", "Bad At Love", "Halsey", "3:56"),
        ];
        assert!(best_youtube_match(&item, &candidates).is_some());
    }

    #[test]
    fn duration_parser_supports_hour_components() {
        assert_eq!(parse_duration_ms("1:02:03"), Some(3_723_000));
        assert_eq!(parse_duration_ms(""), None);
        assert_eq!(parse_duration_ms("3:xx"), None);
    }
}
