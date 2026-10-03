use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::ActiveProvider;

const ACCOUNT_REGISTRY_FILE: &str = "accounts.toml";
const ACCOUNT_ROOT: &str = "accounts";
const SPOTIFY_TOKEN_FILE: &str = "web-token.json";
const SPOTIFY_CREDENTIALS_FILE: &str = "credentials.json";
const YOUTUBE_COOKIE_FILE: &str = "cookie.txt";
const YOUTUBE_BROWSER_PATH_FILE: &str = "browser-path.txt";
const YOUTUBE_BROWSER_PROFILE_DIR: &str = "browser-profile";
const CANONICAL_YOUTUBE_PROFILE_DIR: &str = "browser-profile";

/// Safe account metadata. Provider credentials remain in the provider-owned
/// files and are never serialized into this registry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccountRecord {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccountRegistry {
    #[serde(default)]
    pub spotify: Vec<AccountRecord>,
    #[serde(default)]
    pub youtube_music: Vec<AccountRecord>,
    #[serde(default)]
    pub active_spotify: Option<String>,
    #[serde(default)]
    pub active_youtube_music: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSummary {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub ready: bool,
}

impl AccountRegistry {
    pub fn load(config_folder: &Path) -> Result<Self> {
        let path = config_folder.join(ACCOUNT_REGISTRY_FILE);
        match fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).context("parse account registry"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("read account registry"),
        }
    }

    pub fn save(&self, config_folder: &Path) -> Result<()> {
        fs::create_dir_all(config_folder).context("create account registry directory")?;
        let content = toml::to_string_pretty(self).context("serialize account registry")?;
        let path = config_folder.join(ACCOUNT_REGISTRY_FILE);
        let temporary = path.with_extension("toml.tmp");
        {
            let mut file = fs::File::create(&temporary).context("create account registry temp")?;
            file.write_all(content.as_bytes())
                .context("write account registry temp")?;
            file.sync_all().ok();
        }
        fs::rename(&temporary, &path)
            .or_else(|_| fs::copy(&temporary, &path).and_then(|_| fs::remove_file(&temporary)))
            .context("replace account registry")
    }

    /// Bootstrap metadata for an existing one-account configuration. This is
    /// intentionally a migration of file ownership, not a second auth model.
    pub fn bootstrap(
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<Self> {
        let mut registry = Self::load(config_folder)?;
        let mut changed = false;

        if registry.spotify.is_empty() && spotify_credentials_are_present(cache_folder) {
            let record = registry.add_metadata(ActiveProvider::Spotify, None);
            registry
                .snapshot_current(
                    ActiveProvider::Spotify,
                    &record.id,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                )
                .context("migrate existing Spotify account")?;
            changed = true;
        }

        if registry.youtube_music.is_empty() && youtube_cookie_path.is_file() {
            let record = registry.add_metadata(ActiveProvider::YouTubeMusic, None);
            registry
                .snapshot_current(
                    ActiveProvider::YouTubeMusic,
                    &record.id,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                )
                .context("migrate existing YouTube Music account")?;
            changed = true;
        }

        changed |= registry.ensure_active_defaults();
        if changed {
            registry.save(config_folder)?;
        }

        // A registry created by an earlier run is authoritative for the
        // canonical provider files. Do not delete a legacy file when the slot
        // is incomplete; the next validation can report it as missing.
        if let Some(id) = registry.active_id(ActiveProvider::Spotify) {
            if registry.account_ready(
                ActiveProvider::Spotify,
                id,
                config_folder,
                cache_folder,
                youtube_cookie_path,
            ) {
                registry.activate_files(
                    ActiveProvider::Spotify,
                    id,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                )?;
            }
        }
        if let Some(id) = registry.active_id(ActiveProvider::YouTubeMusic) {
            if registry.account_ready(
                ActiveProvider::YouTubeMusic,
                id,
                config_folder,
                cache_folder,
                youtube_cookie_path,
            ) {
                registry.activate_files(
                    ActiveProvider::YouTubeMusic,
                    id,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                )?;
            }
        }

        Ok(registry)
    }

    pub fn summaries(
        &self,
        provider: ActiveProvider,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Vec<AccountSummary> {
        let active_id = self.active_id(provider);
        self.records(provider)
            .iter()
            .map(|record| AccountSummary {
                id: record.id.clone(),
                label: record.label.clone(),
                active: active_id == Some(record.id.as_str()),
                ready: self.account_ready(
                    provider,
                    &record.id,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                ),
            })
            .collect()
    }

    pub fn labels(&self, provider: ActiveProvider) -> Vec<String> {
        self.records(provider)
            .iter()
            .map(|record| record.label.clone())
            .collect()
    }

    pub fn active_label(&self, provider: ActiveProvider) -> Option<&str> {
        let id = self.active_id(provider)?;
        self.records(provider)
            .iter()
            .find(|record| record.id == id)
            .map(|record| record.label.as_str())
    }

    pub fn active_id(&self, provider: ActiveProvider) -> Option<&str> {
        match provider {
            ActiveProvider::Spotify => self.active_spotify.as_deref(),
            ActiveProvider::YouTubeMusic => self.active_youtube_music.as_deref(),
        }
    }

    pub fn record(&self, provider: ActiveProvider, id: &str) -> Option<&AccountRecord> {
        self.records(provider).iter().find(|record| record.id == id)
    }

    pub fn add_metadata(&mut self, provider: ActiveProvider, label: Option<&str>) -> AccountRecord {
        let records = self.records_mut(provider);
        // Never recycle a slot after removal: credential snapshots are keyed
        // by this id, so reuse could silently select another account's files.
        let index = records
            .iter()
            .filter_map(|record| record.id.rsplit('-').next()?.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let id = format!("{}-{}", provider_slug(provider), index);
        let record = AccountRecord {
            id,
            label: sanitize_label(label, provider, index),
        };
        records.push(record.clone());
        self.set_active_id(provider, Some(record.id.clone()));
        record
    }

    pub fn ensure_active_defaults(&mut self) -> bool {
        let mut changed = false;
        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            if self.active_id(provider).is_none() {
                let first = self
                    .records(provider)
                    .first()
                    .map(|record| record.id.clone());
                if first.is_some() {
                    self.set_active_id(provider, first);
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn select(&mut self, provider: ActiveProvider, id: &str) -> Result<()> {
        anyhow::ensure!(
            self.record(provider, id).is_some(),
            "the requested account is not registered"
        );
        self.set_active_id(provider, Some(id.to_string()));
        Ok(())
    }

    pub fn remove_metadata(&mut self, provider: ActiveProvider, id: &str) -> Result<()> {
        let records = self.records_mut(provider);
        let Some(position) = records.iter().position(|record| record.id == id) else {
            anyhow::bail!("the requested account is not registered");
        };
        records.remove(position);
        let active = self.active_id(provider).map(str::to_owned);
        if active.as_deref() == Some(id) {
            let next = self
                .records(provider)
                .first()
                .map(|record| record.id.clone());
            self.set_active_id(provider, next);
        }
        Ok(())
    }

    pub fn register_current(
        &mut self,
        provider: ActiveProvider,
        label: Option<&str>,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<AccountRecord> {
        let record = self.add_metadata(provider, label);
        if let Err(error) = self.snapshot_current(
            provider,
            &record.id,
            config_folder,
            cache_folder,
            youtube_cookie_path,
        ) {
            let _ = self.remove_metadata(provider, &record.id);
            return Err(error);
        }
        if !self.account_ready(
            provider,
            &record.id,
            config_folder,
            cache_folder,
            youtube_cookie_path,
        ) {
            self.remove_metadata(provider, &record.id)?;
            anyhow::bail!("the authenticated account did not produce a complete session");
        }
        self.save(config_folder)?;
        Ok(record)
    }

    /// Update the active slot after a provider re-authentication, creating the
    /// first slot when a legacy configuration has no account metadata yet.
    pub fn refresh_or_register_current(
        &mut self,
        provider: ActiveProvider,
        label: Option<&str>,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<AccountRecord> {
        if let Some(id) = self.active_id(provider).map(str::to_owned) {
            self.snapshot_current(
                provider,
                &id,
                config_folder,
                cache_folder,
                youtube_cookie_path,
            )?;
            self.save(config_folder)?;
            return self
                .record(provider, &id)
                .cloned()
                .context("the active account is not registered");
        }
        self.register_current(
            provider,
            label,
            config_folder,
            cache_folder,
            youtube_cookie_path,
        )
    }

    pub fn activate(
        &mut self,
        provider: ActiveProvider,
        id: &str,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<bool> {
        anyhow::ensure!(
            self.account_ready(
                provider,
                id,
                config_folder,
                cache_folder,
                youtube_cookie_path
            ),
            "the selected account has no complete saved session"
        );
        let previous = self.active_id(provider).map(str::to_owned);
        self.activate_files(
            provider,
            id,
            config_folder,
            cache_folder,
            youtube_cookie_path,
        )?;
        self.select(provider, id)?;
        if let Err(error) = self.save(config_folder) {
            self.set_active_id(provider, previous.clone());
            if let Some(previous) = previous {
                let _ = self.activate_files(
                    provider,
                    &previous,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                );
            }
            return Err(error);
        }
        Ok(previous.as_deref() != Some(id))
    }

    pub fn remove(
        &mut self,
        provider: ActiveProvider,
        id: &str,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<()> {
        anyhow::ensure!(
            self.record(provider, id).is_some(),
            "the requested account is not registered"
        );
        let was_active = self.active_id(provider) == Some(id);
        self.remove_metadata(provider, id)?;
        remove_account_files(
            provider,
            id,
            config_folder,
            cache_folder,
            youtube_cookie_path,
        )?;
        if was_active {
            if let Some(next) = self.active_id(provider).map(str::to_owned) {
                self.activate_files(
                    provider,
                    &next,
                    config_folder,
                    cache_folder,
                    youtube_cookie_path,
                )?;
            } else {
                remove_canonical_files(provider, cache_folder, youtube_cookie_path)?;
            }
        }
        self.save(config_folder)
    }

    pub fn snapshot_current(
        &self,
        provider: ActiveProvider,
        id: &str,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<()> {
        let slot = slot_path(config_folder, provider, id)?;
        fs::create_dir_all(&slot).context("create account session slot")?;
        match provider {
            ActiveProvider::Spotify => {
                copy_required(
                    &cache_folder.join("user_client_token.json"),
                    &slot.join(SPOTIFY_TOKEN_FILE),
                )?;
                replace_file_optional(
                    &cache_folder.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE),
                    &slot.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE),
                )?;
                copy_required(
                    &cache_folder.join("credentials.json"),
                    &slot.join(SPOTIFY_CREDENTIALS_FILE),
                )?;
            }
            ActiveProvider::YouTubeMusic => {
                copy_required(youtube_cookie_path, &slot.join(YOUTUBE_COOKIE_FILE))?;
                copy_optional(
                    &config_folder
                        .join("youtube")
                        .join(YOUTUBE_BROWSER_PATH_FILE),
                    &slot.join(YOUTUBE_BROWSER_PATH_FILE),
                )?;
                copy_directory_optional(
                    &config_folder
                        .join("youtube")
                        .join(CANONICAL_YOUTUBE_PROFILE_DIR),
                    &slot.join(YOUTUBE_BROWSER_PROFILE_DIR),
                )?;
            }
        }
        Ok(())
    }

    fn activate_files(
        &self,
        provider: ActiveProvider,
        id: &str,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> Result<()> {
        let slot = slot_path(config_folder, provider, id)?;
        match provider {
            ActiveProvider::Spotify => {
                copy_required(
                    &slot.join(SPOTIFY_TOKEN_FILE),
                    &cache_folder.join("user_client_token.json"),
                )?;
                // A slot saved before clients were recorded restores no
                // client, so its token is not reused until signed in again.
                replace_file_optional(
                    &slot.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE),
                    &cache_folder.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE),
                )?;
                copy_required(
                    &slot.join(SPOTIFY_CREDENTIALS_FILE),
                    &cache_folder.join("credentials.json"),
                )?;
            }
            ActiveProvider::YouTubeMusic => {
                copy_required(&slot.join(YOUTUBE_COOKIE_FILE), youtube_cookie_path)?;
                replace_file_optional(
                    &slot.join(YOUTUBE_BROWSER_PATH_FILE),
                    &config_folder
                        .join("youtube")
                        .join(YOUTUBE_BROWSER_PATH_FILE),
                )?;
                replace_directory_optional(
                    &slot.join(YOUTUBE_BROWSER_PROFILE_DIR),
                    &config_folder
                        .join("youtube")
                        .join(CANONICAL_YOUTUBE_PROFILE_DIR),
                )?;
            }
        }
        Ok(())
    }

    fn account_ready(
        &self,
        provider: ActiveProvider,
        id: &str,
        config_folder: &Path,
        cache_folder: &Path,
        youtube_cookie_path: &Path,
    ) -> bool {
        let Ok(slot) = slot_path(config_folder, provider, id) else {
            return false;
        };
        match provider {
            ActiveProvider::Spotify => {
                slot.join(SPOTIFY_TOKEN_FILE).is_file()
                    && slot.join(SPOTIFY_CREDENTIALS_FILE).is_file()
            }
            ActiveProvider::YouTubeMusic => {
                let _ = (cache_folder, youtube_cookie_path);
                slot.join(YOUTUBE_COOKIE_FILE).is_file()
            }
        }
    }

    fn records(&self, provider: ActiveProvider) -> &Vec<AccountRecord> {
        match provider {
            ActiveProvider::Spotify => &self.spotify,
            ActiveProvider::YouTubeMusic => &self.youtube_music,
        }
    }

    fn records_mut(&mut self, provider: ActiveProvider) -> &mut Vec<AccountRecord> {
        match provider {
            ActiveProvider::Spotify => &mut self.spotify,
            ActiveProvider::YouTubeMusic => &mut self.youtube_music,
        }
    }

    fn set_active_id(&mut self, provider: ActiveProvider, id: Option<String>) {
        match provider {
            ActiveProvider::Spotify => self.active_spotify = id,
            ActiveProvider::YouTubeMusic => self.active_youtube_music = id,
        }
    }
}

fn provider_slug(provider: ActiveProvider) -> &'static str {
    match provider {
        ActiveProvider::Spotify => "spotify",
        ActiveProvider::YouTubeMusic => "youtube",
    }
}

fn sanitize_label(label: Option<&str>, provider: ActiveProvider, index: usize) -> String {
    let sanitized = label
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if sanitized.is_empty() {
        format!("{} account {index}", provider.title())
    } else {
        sanitized
            .chars()
            .take(48)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}

fn spotify_credentials_are_present(cache_folder: &Path) -> bool {
    cache_folder.join("user_client_token.json").is_file()
        && cache_folder.join("credentials.json").is_file()
}

fn slot_path(config_folder: &Path, provider: ActiveProvider, id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !id.is_empty()
            && id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-'),
        "invalid account identifier"
    );
    Ok(config_folder
        .join(ACCOUNT_ROOT)
        .join(provider_slug(provider))
        .join(id))
}

fn copy_required(source: &Path, destination: &Path) -> Result<()> {
    anyhow::ensure!(source.is_file(), "the account session file is missing");
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).context("create account session file directory")?;
    }
    fs::copy(source, destination).context("copy account session file")?;
    Ok(())
}

fn copy_optional(source: &Path, destination: &Path) -> Result<()> {
    if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).context("create optional account file directory")?;
        }
        fs::copy(source, destination).context("copy optional account file")?;
    }
    Ok(())
}

fn copy_directory_optional(source: &Path, destination: &Path) -> Result<()> {
    if source.is_dir() {
        copy_directory(source, destination)?;
    }
    Ok(())
}

fn replace_directory_optional(source: &Path, destination: &Path) -> Result<()> {
    if destination.is_dir() {
        fs::remove_dir_all(destination).context("remove previous browser profile")?;
    }
    if !source.is_dir() {
        return Ok(());
    }
    copy_directory(source, destination)
}

fn replace_file_optional(source: &Path, destination: &Path) -> Result<()> {
    if destination.is_file() {
        fs::remove_file(destination).context("remove previous optional account file")?;
    }
    copy_optional(source, destination)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).context("create browser profile directory")?;
    for entry in fs::read_dir(source).context("read browser profile directory")? {
        let entry = entry.context("read browser profile entry")?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if source_path.is_file() {
            fs::copy(&source_path, &destination_path).context("copy browser profile file")?;
        }
    }
    Ok(())
}

fn remove_account_files(
    provider: ActiveProvider,
    id: &str,
    config_folder: &Path,
    _cache_folder: &Path,
    _youtube_cookie_path: &Path,
) -> Result<()> {
    let slot = slot_path(config_folder, provider, id)?;
    if slot.exists() {
        fs::remove_dir_all(slot).context("remove account session slot")?;
    }
    Ok(())
}

pub(crate) fn remove_canonical_files(
    provider: ActiveProvider,
    cache_folder: &Path,
    youtube_cookie_path: &Path,
) -> Result<()> {
    let paths: Vec<PathBuf> = match provider {
        ActiveProvider::Spotify => vec![
            cache_folder.join("user_client_token.json"),
            cache_folder.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE),
            cache_folder.join("credentials.json"),
        ],
        ActiveProvider::YouTubeMusic => vec![youtube_cookie_path.to_path_buf()],
    };
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove canonical account session file"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let cache = root.path().join("cache");
        let cookie = config.join("youtube").join("cookie.txt");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(cookie.parent().unwrap()).unwrap();
        (root, config, cache, cookie)
    }

    #[test]
    fn fresh_registry_has_no_active_accounts() {
        let registry = AccountRegistry::default();
        assert_eq!(registry.active_id(ActiveProvider::Spotify), None);
        assert!(registry.labels(ActiveProvider::YouTubeMusic).is_empty());
    }

    #[test]
    fn partial_saved_account_is_visible_but_not_ready() {
        let (_root, config, cache, cookie) = folders();
        let mut registry = AccountRegistry::default();
        registry.add_metadata(ActiveProvider::Spotify, Some("Partial"));
        let summaries = registry.summaries(ActiveProvider::Spotify, &config, &cache, &cookie);
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].active);
        assert!(!summaries[0].ready);
    }

    #[test]
    fn missing_youtube_browser_auth_is_not_reported_as_ready() {
        let (_root, config, cache, cookie) = folders();
        let mut registry = AccountRegistry::default();
        registry.add_metadata(ActiveProvider::YouTubeMusic, Some("Browser account"));
        let summaries = registry.summaries(ActiveProvider::YouTubeMusic, &config, &cache, &cookie);
        assert_eq!(summaries[0].active, true);
        assert_eq!(summaries[0].ready, false);
    }

    #[test]
    fn labels_are_safe_and_bounded() {
        let mut registry = AccountRegistry::default();
        let record = registry.add_metadata(
            ActiveProvider::Spotify,
            Some("  Main\naccount\twith a very long label that should be bounded "),
        );
        assert!(record
            .label
            .starts_with("Main account with a very long label"));
        assert!(!record.label.chars().any(char::is_control));
        assert!(record.label.chars().count() <= 48);
    }

    #[test]
    fn adding_after_removal_does_not_reuse_a_credential_slot() {
        let mut registry = AccountRegistry::default();
        let first = registry.add_metadata(ActiveProvider::Spotify, Some("First"));
        let second = registry.add_metadata(ActiveProvider::Spotify, Some("Second"));
        registry
            .remove_metadata(ActiveProvider::Spotify, &first.id)
            .unwrap();
        let third = registry.add_metadata(ActiveProvider::Spotify, Some("Third"));

        assert_eq!(second.id, "spotify-2");
        assert_eq!(third.id, "spotify-3");
    }

    #[test]
    fn registry_round_trip_contains_metadata_but_no_credential_values() {
        let (_root, config, _cache, _cookie) = folders();
        let mut registry = AccountRegistry::default();
        registry.add_metadata(ActiveProvider::Spotify, Some("Main"));
        registry.save(&config).unwrap();
        let restored = AccountRegistry::load(&config).unwrap();
        assert_eq!(restored, registry);
        let content = fs::read_to_string(config.join(ACCOUNT_REGISTRY_FILE)).unwrap();
        assert!(content.contains("Main"));
        assert!(!content.contains("token-value"));
        assert!(!content.contains("cookie-value"));
    }

    #[test]
    fn bootstrap_migrates_existing_sessions_once() {
        let (_root, config, cache, cookie) = folders();
        fs::write(cache.join("user_client_token.json"), "token-value").unwrap();
        fs::write(cache.join("credentials.json"), "credentials-value").unwrap();
        fs::write(&cookie, "cookie-value").unwrap();

        let registry = AccountRegistry::bootstrap(&config, &cache, &cookie).unwrap();
        assert_eq!(
            registry.labels(ActiveProvider::Spotify),
            vec!["Spotify account 1"]
        );
        assert_eq!(
            registry.labels(ActiveProvider::YouTubeMusic),
            vec!["YouTube Music account 1"]
        );
        let spotify_slot = slot_path(
            &config,
            ActiveProvider::Spotify,
            registry.active_id(ActiveProvider::Spotify).unwrap(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(spotify_slot.join(SPOTIFY_TOKEN_FILE)).unwrap(),
            "token-value"
        );
        assert_eq!(
            fs::read_to_string(spotify_slot.join(SPOTIFY_CREDENTIALS_FILE)).unwrap(),
            "credentials-value"
        );
    }

    #[test]
    fn first_browser_session_migration_does_not_create_a_duplicate_account() {
        let (_root, config, cache, cookie) = folders();
        fs::write(&cookie, "cookie-value").unwrap();
        let mut registry = AccountRegistry::bootstrap(&config, &cache, &cookie).unwrap();
        registry
            .refresh_or_register_current(
                ActiveProvider::YouTubeMusic,
                None,
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        assert_eq!(registry.youtube_music.len(), 1);
    }

    #[test]
    fn switching_restores_the_selected_provider_slot_and_persists_active_id() {
        let (_root, config, cache, cookie) = folders();
        fs::write(cache.join("user_client_token.json"), "one-token").unwrap();
        fs::write(cache.join("credentials.json"), "one-creds").unwrap();
        let mut registry = AccountRegistry::default();
        let first = registry.register_current(
            ActiveProvider::Spotify,
            Some("One"),
            &config,
            &cache,
            &cookie,
        );
        assert!(first.is_ok());

        fs::write(cache.join("user_client_token.json"), "two-token").unwrap();
        fs::write(cache.join("credentials.json"), "two-creds").unwrap();
        let second = registry
            .register_current(
                ActiveProvider::Spotify,
                Some("Two"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        registry
            .activate(
                ActiveProvider::Spotify,
                &first.unwrap().id,
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(cache.join("user_client_token.json")).unwrap(),
            "one-token"
        );
        assert_eq!(registry.active_label(ActiveProvider::Spotify), Some("One"));
        assert!(registry
            .record(ActiveProvider::Spotify, &second.id)
            .is_some());
    }

    #[test]
    fn refreshing_current_spotify_account_updates_the_active_slot() {
        let (_root, config, cache, cookie) = folders();
        fs::write(cache.join("user_client_token.json"), "old-token").unwrap();
        fs::write(cache.join("credentials.json"), "creds").unwrap();
        let mut registry = AccountRegistry::default();
        let account = registry
            .register_current(
                ActiveProvider::Spotify,
                Some("Main"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();

        fs::write(cache.join("user_client_token.json"), "new-token").unwrap();
        registry
            .refresh_or_register_current(
                ActiveProvider::Spotify,
                Some("Renamed"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();

        let slot = slot_path(&config, ActiveProvider::Spotify, &account.id).unwrap();
        assert_eq!(
            fs::read_to_string(slot.join(SPOTIFY_TOKEN_FILE)).unwrap(),
            "new-token"
        );
        assert_eq!(registry.active_label(ActiveProvider::Spotify), Some("Main"));
    }

    #[test]
    fn removing_last_account_clears_canonical_files() {
        let (_root, config, cache, cookie) = folders();
        fs::write(cache.join("user_client_token.json"), "token").unwrap();
        fs::write(cache.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE), "client").unwrap();
        fs::write(cache.join("credentials.json"), "creds").unwrap();
        let mut registry = AccountRegistry::default();
        let account = registry
            .register_current(ActiveProvider::Spotify, None, &config, &cache, &cookie)
            .unwrap();
        registry
            .remove(
                ActiveProvider::Spotify,
                &account.id,
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        assert!(!cache.join("user_client_token.json").exists());
        assert!(!cache.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE).exists());
        assert!(!cache.join("credentials.json").exists());
        assert!(registry.spotify.is_empty());
    }

    #[test]
    fn switching_accounts_carries_each_tokens_client() {
        let (_root, config, cache, cookie) = folders();
        let client_file = cache.join(crate::auth::SPOTIFY_TOKEN_CLIENT_FILE);
        fs::write(cache.join("user_client_token.json"), "one-token").unwrap();
        fs::write(&client_file, "client-a").unwrap();
        fs::write(cache.join("credentials.json"), "one-creds").unwrap();
        let mut registry = AccountRegistry::default();
        let first = registry
            .register_current(ActiveProvider::Spotify, None, &config, &cache, &cookie)
            .unwrap();

        // A token cached before clients were recorded.
        fs::write(cache.join("user_client_token.json"), "two-token").unwrap();
        fs::remove_file(&client_file).unwrap();
        fs::write(cache.join("credentials.json"), "two-creds").unwrap();
        let second = registry
            .register_current(ActiveProvider::Spotify, None, &config, &cache, &cookie)
            .unwrap();

        registry
            .activate(ActiveProvider::Spotify, &first.id, &config, &cache, &cookie)
            .unwrap();
        assert_eq!(fs::read_to_string(&client_file).unwrap(), "client-a");

        registry
            .activate(
                ActiveProvider::Spotify,
                &second.id,
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        assert!(
            !client_file.exists(),
            "the first account's client must not stay with the second token"
        );
    }

    #[test]
    fn restart_restores_active_account_and_keeps_other_slot() {
        let (_root, config, cache, cookie) = folders();
        fs::write(cache.join("user_client_token.json"), "one-token").unwrap();
        fs::write(cache.join("credentials.json"), "one-creds").unwrap();
        let mut registry = AccountRegistry::default();
        let first = registry
            .register_current(
                ActiveProvider::Spotify,
                Some("One"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        fs::write(cache.join("user_client_token.json"), "two-token").unwrap();
        fs::write(cache.join("credentials.json"), "two-creds").unwrap();
        let second = registry
            .register_current(
                ActiveProvider::Spotify,
                Some("Two"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        registry
            .activate(ActiveProvider::Spotify, &first.id, &config, &cache, &cookie)
            .unwrap();

        let restored = AccountRegistry::bootstrap(&config, &cache, &cookie).unwrap();
        assert_eq!(
            restored.active_id(ActiveProvider::Spotify),
            Some(first.id.as_str())
        );
        assert_eq!(restored.active_label(ActiveProvider::Spotify), Some("One"));
        assert!(restored
            .record(ActiveProvider::Spotify, &second.id)
            .is_some());
        assert_eq!(
            fs::read_to_string(cache.join("user_client_token.json")).unwrap(),
            "one-token"
        );
    }

    #[test]
    fn replacing_youtube_session_removes_old_optional_browser_state() {
        let (_root, config, cache, cookie) = folders();
        let browser_dir = config.join("youtube").join(CANONICAL_YOUTUBE_PROFILE_DIR);
        fs::create_dir_all(&browser_dir).unwrap();
        fs::write(browser_dir.join("old"), "old-profile").unwrap();
        fs::write(
            config.join("youtube").join(YOUTUBE_BROWSER_PATH_FILE),
            "old-browser",
        )
        .unwrap();
        fs::write(&cookie, "cookie-one").unwrap();
        let mut registry = AccountRegistry::default();
        let first = registry
            .register_current(
                ActiveProvider::YouTubeMusic,
                Some("One"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        fs::remove_file(config.join("youtube").join(YOUTUBE_BROWSER_PATH_FILE)).unwrap();
        fs::remove_dir_all(&browser_dir).unwrap();
        fs::write(&cookie, "cookie-two").unwrap();
        let second = registry
            .register_current(
                ActiveProvider::YouTubeMusic,
                Some("Two"),
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        registry
            .activate(
                ActiveProvider::YouTubeMusic,
                &second.id,
                &config,
                &cache,
                &cookie,
            )
            .unwrap();
        assert!(!config
            .join("youtube")
            .join(YOUTUBE_BROWSER_PATH_FILE)
            .exists());
        assert!(!config
            .join("youtube")
            .join(CANONICAL_YOUTUBE_PROFILE_DIR)
            .exists());
        assert!(registry
            .record(ActiveProvider::YouTubeMusic, &first.id)
            .is_some());
    }
}
