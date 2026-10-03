use std::borrow::Cow;

/// formats a time duration into a "{minutes}:{seconds}" format
pub fn format_duration(duration: &chrono::Duration) -> String {
    let secs = duration.num_seconds();
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn map_join<T, F>(v: &[T], f: F, sep: &str) -> String
where
    F: Fn(&T) -> &str,
{
    v.iter().map(f).fold(String::new(), |x, y| {
        if x.is_empty() {
            x + y
        } else {
            x + sep + y
        }
    })
}

#[allow(dead_code)]
pub fn get_track_album_image_url(track: &rspotify::model::FullTrack) -> Option<&str> {
    if track.album.images.is_empty() {
        None
    } else {
        Some(&track.album.images[0].url)
    }
}

#[allow(dead_code)]
pub fn get_episode_show_image_url(episode: &rspotify::model::FullEpisode) -> Option<&str> {
    if episode.show.images.is_empty() {
        None
    } else {
        Some(&episode.show.images[0].url)
    }
}

pub fn parse_uri(uri: &str) -> Cow<'_, str> {
    let parts = uri.split(':').collect::<Vec<_>>();
    // The below URI probably has a format of `spotify:user:{user_id}:{type}:{id}`,
    // but `rspotify` library expects to receive an URI of format `spotify:{type}:{id}`.
    // We have to modify the URI to a corresponding format.
    // See: https://github.com/aome510/spotify-player/issues/57#issuecomment-1160868626
    if parts.len() == 5 {
        Cow::Owned([parts[0], parts[3], parts[4]].join(":"))
    } else {
        Cow::Borrowed(uri)
    }
}

#[cfg(feature = "fzf")]
use fuzzy_matcher::skim::SkimMatcherV2;

#[cfg(feature = "fzf")]
pub fn fuzzy_search_items<'a, T: std::fmt::Display>(items: &'a [T], query: &str) -> Vec<&'a T> {
    let matcher = SkimMatcherV2::default();
    let mut result = items
        .iter()
        .filter_map(|t| {
            matcher
                .fuzzy(&t.to_string(), query, false)
                .map(|(score, _)| (t, score))
        })
        .collect::<Vec<_>>();

    result.sort_by(|(_, a), (_, b)| b.cmp(a));
    result.into_iter().map(|(t, _)| t).collect::<Vec<_>>()
}

#[cfg(feature = "fzf")]
pub fn fuzzy_search_item_indices<T, F>(items: &[T], query: &str, label: F) -> Vec<usize>
where
    F: Fn(&T, &mut String),
{
    let matcher = SkimMatcherV2::default();
    let mut label_text = String::new();
    let mut result = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            label_text.clear();
            label(item, &mut label_text);
            matcher
                .fuzzy(&label_text, query, false)
                .map(|(score, _)| (index, score))
        })
        .collect::<Vec<_>>();

    result.sort_by(|(_, a), (_, b)| b.cmp(a));
    result.into_iter().map(|(index, _)| index).collect()
}

/// Get a list of items filtered by a search query.
pub fn filtered_items_from_query<'a, T: std::fmt::Display>(
    query: &str,
    items: &'a [T],
) -> Vec<&'a T> {
    let query = query.to_lowercase();

    #[cfg(feature = "fzf")]
    return fuzzy_search_items(items, &query);

    #[cfg(not(feature = "fzf"))]
    if !query_has_terms(&query) {
        return items.iter().collect();
    }

    #[cfg(not(feature = "fzf"))]
    let terms = query.split(' ').filter(|term| !term.is_empty());

    #[cfg(not(feature = "fzf"))]
    items
        .iter()
        .filter(|t| {
            let text = lowercased_display(*t);
            terms.clone().all(|term| text.contains(term))
        })
        .collect::<Vec<_>>()
}

/// Count items matching a search query without allocating a borrowed-item
/// projection.
pub fn filtered_item_count_from_query<T: std::fmt::Display>(query: &str, items: &[T]) -> usize {
    let query = query.to_lowercase();

    #[cfg(feature = "fzf")]
    {
        let matcher = SkimMatcherV2::default();
        items
            .iter()
            .filter(|item| matcher.fuzzy(&item.to_string(), &query, false).is_some())
            .count()
    }

    #[cfg(not(feature = "fzf"))]
    if !query_has_terms(&query) {
        return items.len();
    }

    #[cfg(not(feature = "fzf"))]
    let terms = query.split(' ').filter(|term| !term.is_empty());

    #[cfg(not(feature = "fzf"))]
    items
        .iter()
        .filter(|item| {
            let text = lowercased_display(*item);
            terms.clone().all(|term| text.contains(term))
        })
        .count()
}

#[cfg(not(feature = "fzf"))]
fn query_has_terms(query: &str) -> bool {
    query.split(' ').any(|term| !term.is_empty())
}

/// Lowercase a formatted item without paying for a second allocation on the
/// common ASCII path. The Unicode path intentionally keeps Rust's full
/// lowercase mapping so search behavior does not change for non-ASCII labels.
#[cfg(not(feature = "fzf"))]
fn lowercased_display<T: std::fmt::Display>(item: &T) -> String {
    let mut text = item.to_string();
    if text.is_ascii() {
        text.make_ascii_lowercase();
        text
    } else {
        text.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::{filtered_item_count_from_query, filtered_items_from_query};

    #[cfg(not(feature = "fzf"))]
    #[test]
    fn count_matches_the_projection_without_allocating_it() {
        let items = ["Blue sky", "Blue ocean", "Green sky"];

        assert_eq!(
            filtered_item_count_from_query("blue sky", &items),
            filtered_items_from_query("blue sky", &items).len()
        );
        assert_eq!(filtered_item_count_from_query("", &items), items.len());
        assert_eq!(filtered_item_count_from_query("  ", &items), items.len());
    }

    #[cfg(feature = "fzf")]
    #[test]
    fn fuzzy_count_matches_the_projection_for_a_nonempty_query() {
        let items = ["Blue sky", "Blue ocean", "Green sky"];
        let query = "blue sky";

        assert_eq!(
            filtered_item_count_from_query(query, &items),
            filtered_items_from_query(query, &items).len()
        );
    }

    #[cfg(not(feature = "fzf"))]
    #[test]
    fn filtering_keeps_unicode_lowercase_behavior() {
        let items = ["İstanbul", "Izmir", "Berlin"];

        assert_eq!(
            filtered_items_from_query("İSTANBUL", &items)
                .into_iter()
                .copied()
                .collect::<Vec<_>>(),
            vec!["İstanbul"]
        );
        assert_eq!(filtered_item_count_from_query("İSTANBUL", &items), 1);
    }

    #[cfg(not(feature = "fzf"))]
    #[test]
    fn whitespace_only_queries_skip_item_formatting() {
        use std::fmt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountedLabel<'a>(&'a AtomicUsize, &'a str);

        impl fmt::Display for CountedLabel<'_> {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fetch_add(1, Ordering::Relaxed);
                formatter.write_str(self.1)
            }
        }

        let formatted = AtomicUsize::new(0);
        let items = [
            CountedLabel(&formatted, "one"),
            CountedLabel(&formatted, "two"),
        ];

        assert_eq!(filtered_items_from_query("  ", &items).len(), items.len());
        assert_eq!(filtered_item_count_from_query("  ", &items), items.len());
        assert_eq!(formatted.load(Ordering::Relaxed), 0);
    }
}
