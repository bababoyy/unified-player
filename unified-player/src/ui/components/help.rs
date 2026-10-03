//! Shared command-help view model.

use crate::{config::ResolvedBinding, ui::utils::to_bidi_string};

/// One command-help row shared by the full page and popup presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandHelpRow {
    pub(crate) binding: String,
    pub(crate) shortcut: String,
    pub(crate) description: String,
}

impl CommandHelpRow {
    pub(crate) fn compact_line(&self) -> String {
        format!("{} {}: {}", self.shortcut, self.binding, self.description)
    }
}

/// Project resolved keymap entries into the shared, bidi-safe help rows.
pub(crate) fn project_command_help_rows<'a, I>(bindings: I) -> Vec<CommandHelpRow>
where
    I: IntoIterator<Item = &'a ResolvedBinding>,
{
    bindings
        .into_iter()
        .map(|binding| CommandHelpRow {
            binding: to_bidi_string(binding.label()),
            shortcut: binding.key_sequence.to_string(),
            description: command_help_description(&binding.description()),
        })
        .collect()
}

fn command_help_description(description: &str) -> String {
    to_bidi_string(description)
}

#[cfg(test)]
mod tests {
    use super::project_command_help_rows;
    use crate::config::KeymapConfig;

    #[test]
    fn help_projection_keeps_shared_compact_and_tabular_fields() {
        let bindings = KeymapConfig::default().resolved_bindings();
        let rows = project_command_help_rows(bindings.iter().take(1));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].shortcut, bindings[0].key_sequence.to_string());
        assert!(rows[0].compact_line().contains(&rows[0].binding));
        assert!(rows[0].compact_line().contains(&rows[0].description));
    }

    #[test]
    fn help_projection_bidi_safe_labels_are_shared_by_both_presentations() {
        let binding = KeymapConfig::default()
            .resolved_bindings()
            .into_iter()
            .next()
            .expect("default keymap has a help binding");
        let rows = project_command_help_rows([&binding]);
        assert_eq!(
            rows[0].binding,
            crate::ui::utils::to_bidi_string(binding.label())
        );
        assert!(!rows[0].binding.starts_with("Command:"));
        assert!(!rows[0].shortcut.starts_with('['));
        assert!(!rows[0].description.is_empty());
        assert!(rows[0].compact_line().contains(&rows[0].shortcut));
    }
}
