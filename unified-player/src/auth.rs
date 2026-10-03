use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
};

use crate::config;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use librespot_core::{authentication::Credentials, cache::Cache, Session};
use reqwest::Url;
use rspotify::clients::{BaseClient as _, OAuthClient as _};
use sha2::{Digest as _, Sha256};

pub const SPOTIFY_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
pub const NCSPOT_CLIENT_ID: &str = "d420a117a32841c2b3474932e49fb54b";

/// Beside the Web API token cache: the client ID that token was issued to.
pub const SPOTIFY_TOKEN_CLIENT_FILE: &str = "user_client_token.client_id";

const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

// based on https://developer.spotify.com/documentation/web-api/concepts/scopes#list-of-scopes
pub const OAUTH_SCOPES: &[&str] = &[
    // Spotify Connect
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    // Playback
    "app-remote-control",
    "streaming",
    // Playlists
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    // Follow
    "user-follow-modify",
    "user-follow-read",
    // Listening History
    "user-read-playback-position",
    "user-top-read",
    "user-read-recently-played",
    // Library
    "user-library-modify",
    "user-library-read",
    // Users
    // Required by Spotify's current-user profile endpoint (`GET /me`).
    "user-read-private",
    "user-personalized",
];

/// Presentation callbacks supplied by a surface that owns OAuth interaction.
pub trait AuthPrompt: Send + Sync {
    fn show_authorize_url(&self, auth_url: &str);
    fn read_redirect_url(&self) -> Result<String>;
}

/// Controls which presentation surface may handle OAuth interaction.
///
/// The terminal is owned by the UI runtime. Backend callers use
/// [`AuthInteraction::NonInteractive`] so authentication can only use the
/// loopback callback and otherwise returns an error for the UI to present.
#[derive(Clone, Copy)]
pub enum AuthInteraction<'a> {
    /// A presentation surface owns stdin/stdout for this flow.
    Prompt(&'a dyn AuthPrompt),
    /// A UI/backend flow must not read stdin or write directly to the terminal.
    NonInteractive,
}

impl<'a> AuthInteraction<'a> {
    fn prompt_ref(self) -> Option<&'a dyn AuthPrompt> {
        match self {
            Self::Prompt(prompt) => Some(prompt),
            Self::NonInteractive => None,
        }
    }
}

#[derive(Clone)]
pub struct AuthConfig {
    pub cache: Cache,
    pub login_redirect_uri: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            cache: Cache::new(None::<String>, None, None, None).unwrap(),
            login_redirect_uri: "http://127.0.0.1:8989/login".to_string(),
        }
    }
}

impl AuthConfig {
    /// Create a `librespot::Session` from authentication configs
    pub fn session(&self) -> Session {
        let session_config = config::get_config().app_config.session_config();
        Session::new(session_config, Some(self.cache.clone()))
    }

    pub fn new(configs: &config::Configs) -> Result<AuthConfig> {
        let audio_cache_folder = if configs.app_config.device.audio_cache {
            Some(configs.cache_folder.join("audio"))
        } else {
            None
        };

        let cache = Cache::new(
            Some(configs.cache_folder.clone()),
            None,
            audio_cache_folder,
            None,
        )?;

        Ok(AuthConfig {
            cache,
            login_redirect_uri: configs.app_config.login_redirect_uri.clone(),
        })
    }
}

/// Spotify Web API client that preserves the existing refresh token when
/// Spotify rotates only the access token.
///
/// Spotify explicitly allows refresh responses to omit `refresh_token`; in
/// that case clients must continue using the previous refresh token. Rspotify
/// 0.16.1 replaces the complete token with the refresh response, which makes
/// the next application session unable to refresh. This wrapper keeps
/// rspotify's endpoint implementations while correcting that token merge.
#[derive(Clone, Debug, Default)]
pub struct SpotifyWebApiClient {
    inner: rspotify::AuthCodePkceSpotify,
    cache_owner: Option<(std::sync::Arc<tokio::sync::RwLock<u64>>, u64)>,
}

impl SpotifyWebApiClient {
    pub fn new(inner: rspotify::AuthCodePkceSpotify) -> Self {
        Self {
            inner,
            cache_owner: None,
        }
    }

    pub(crate) fn with_cache_owner(
        mut self,
        epoch: std::sync::Arc<tokio::sync::RwLock<u64>>,
        generation: u64,
    ) -> Self {
        self.cache_owner = Some((epoch, generation));
        self
    }

    /// Record which client the cached token belongs to, next to the cache.
    fn write_token_client(&self) -> std::io::Result<()> {
        let config = self.inner.get_config();
        if !config.token_cached {
            return Ok(());
        }
        std::fs::write(
            config.cache_path.with_file_name(SPOTIFY_TOKEN_CLIENT_FILE),
            &self.inner.get_creds().id,
        )
    }

    pub fn get_authorize_url(
        &mut self,
        verifier_bytes: Option<usize>,
    ) -> rspotify::ClientResult<String> {
        self.inner.get_authorize_url(verifier_bytes)
    }
}

/// Whether the Web API token cached in `cache_folder` was issued to
/// `client_id`. A token cached without a recorded client does not match, so it
/// is never reused under a client it may not belong to.
pub fn cached_token_matches_client(cache_folder: &std::path::Path, client_id: &str) -> bool {
    std::fs::read_to_string(cache_folder.join(SPOTIFY_TOKEN_CLIENT_FILE))
        .is_ok_and(|saved| saved.trim() == client_id)
}

fn retain_previous_refresh_token(
    previous_refresh_token: Option<String>,
    refreshed: &mut rspotify::Token,
) {
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = previous_refresh_token;
    }
}

/// Whether a Web API token is cached for the configured client. A client ID
/// command is not run here; sign-in checks the client it returns.
pub fn web_api_token_cached(configs: &config::Configs) -> bool {
    configs
        .cache_folder
        .join("user_client_token.json")
        .is_file()
        && (configs.app_config.client_id_command.is_some()
            || cached_token_matches_client(&configs.cache_folder, &configs.app_config.client_id))
}

/// Return only coarse cached-auth readiness for the first-use UI.
///
/// Credential values stay inside the provider cache owner. The welcome flow
/// only needs to know whether an existing developer session can be attempted;
/// Spotify Premium is confirmed later from the authenticated user response.
pub fn cached_spotify_auth_snapshot(configs: &config::Configs) -> config::SpotifyAuthSnapshot {
    let session_ready = AuthConfig::new(configs)
        .ok()
        .and_then(|auth_config| auth_config.cache.credentials())
        .is_some()
        && web_api_token_cached(configs)
        && config::SetupState::load(&configs.config_folder)
            .is_ok_and(|setup| !setup.spotify_reauthentication_required);

    config::SpotifyAuthSnapshot {
        session_ready,
        premium: config::SpotifyPremiumStatus::Unknown,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CachedTokenAction {
    Reuse,
    Refresh,
    Reauthenticate,
}

fn cached_token_action(token: &rspotify::Token) -> CachedTokenAction {
    if token.refresh_token.is_none() {
        CachedTokenAction::Reauthenticate
    } else if token.is_expired() {
        CachedTokenAction::Refresh
    } else {
        CachedTokenAction::Reuse
    }
}

#[maybe_async::maybe_async]
impl rspotify::clients::BaseClient for SpotifyWebApiClient {
    fn get_http(&self) -> &rspotify::http::HttpClient {
        self.inner.get_http()
    }

    fn get_token(&self) -> std::sync::Arc<rspotify::sync::Mutex<Option<rspotify::Token>>> {
        self.inner.get_token()
    }

    fn get_creds(&self) -> &rspotify::Credentials {
        self.inner.get_creds()
    }

    fn get_config(&self) -> &rspotify::Config {
        self.inner.get_config()
    }

    async fn write_token_cache(&self) -> rspotify::ClientResult<()> {
        // Hold ownership through the write: replacing the active client waits for
        // a current write, and retired snapshots cannot overwrite its successor.
        let owner = match &self.cache_owner {
            Some((epoch, generation)) => {
                let current = epoch.read().await;
                if *current != *generation {
                    return Err(rspotify::ClientError::InvalidToken);
                }
                Some(current)
            }
            None => None,
        };
        let mut result = self.inner.write_token_cache().await;
        if result.is_ok() {
            result = self.write_token_client().map_err(rspotify::ClientError::Io);
        }
        drop(owner);
        result
    }

    async fn refetch_token(&self) -> rspotify::ClientResult<Option<rspotify::Token>> {
        let previous_refresh_token = self
            .get_token()
            .lock()
            .await
            .unwrap()
            .as_ref()
            .and_then(|token| token.refresh_token.clone());
        let mut refreshed = self.inner.refetch_token().await?;
        if let Some(ref mut token) = refreshed {
            retain_previous_refresh_token(previous_refresh_token, token);
        }
        Ok(refreshed)
    }
}

#[maybe_async::maybe_async]
impl rspotify::clients::OAuthClient for SpotifyWebApiClient {
    fn get_oauth(&self) -> &rspotify::OAuth {
        self.inner.get_oauth()
    }

    async fn request_token(&self, code: &str) -> rspotify::ClientResult<()> {
        self.inner.request_token(code).await
    }
}

/// Get Spotify credentials while making terminal ownership explicit.
pub fn get_creds_with_interaction(
    auth_config: &AuthConfig,
    reauth: bool,
    use_cached: bool,
    interaction: AuthInteraction<'_>,
) -> Result<Credentials> {
    let creds = if use_cached {
        auth_config.cache.credentials()
    } else {
        None
    };

    Ok(match creds {
        None => {
            let msg = "No cached credentials found, please authenticate the application first.";
            if reauth {
                let access_token = get_oauth_access_token(
                    SPOTIFY_CLIENT_ID,
                    &auth_config.login_redirect_uri,
                    OAUTH_SCOPES,
                    interaction,
                )?;
                let credentials = Credentials::with_access_token(access_token);
                auth_config.cache.save_credentials(&credentials);
                credentials
            } else {
                anyhow::bail!(msg);
            }
        }
        Some(creds) => {
            tracing::info!("Using cached credentials");
            creds
        }
    })
}

/// Authenticate the user-provided (Web API) client using the authorization code with PKCE flow.
///
/// This mirrors `rspotify`'s `prompt_for_token` (reusing/refreshing a cached token when possible),
/// but replaces its callback listener with [`obtain_auth_code`], which is robust against stray
/// browser requests on the callback port (see [`listen_for_auth_code`]).
///
/// When `force` is set, any cached token is ignored and a fresh interactive authorization flow is
/// always run. This is used by the `authenticate` CLI command to re-authenticate on demand.
pub async fn prompt_for_user_token(client: &mut SpotifyWebApiClient, force: bool) -> Result<()> {
    prompt_for_user_token_with_interaction(client, force, AuthInteraction::NonInteractive).await
}

/// Authenticate the user-provided client without taking ownership of stdin/stdout.
pub async fn prompt_for_user_token_with_interaction(
    client: &mut SpotifyWebApiClient,
    force: bool,
    interaction: AuthInteraction<'_>,
) -> Result<()> {
    let configs = config::get_config();
    let client_changed =
        spotify_client_reauthorization_required(&configs.config_folder, &client.get_creds().id)?;
    let cache_folder = client
        .get_config()
        .cache_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let token_matches_client = cached_token_matches_client(&cache_folder, &client.get_creds().id);
    if !token_matches_client && !force && client.get_config().cache_path.is_file() {
        tracing::warn!(
            "Cached Spotify Web API token was not issued to the configured client; starting interactive authentication"
        );
    }
    // A changed application cannot reuse the previous application's cached token.
    if !force && !client_changed && token_matches_client {
        match client.read_token_cache(true).await {
            Ok(Some(token)) => {
                match cached_token_action(&token) {
                    CachedTokenAction::Reuse => {
                        *client.get_token().lock().await.unwrap() = Some(token);
                        return Ok(());
                    }
                    CachedTokenAction::Refresh => {
                        *client.get_token().lock().await.unwrap() = Some(token);
                        match client.refetch_token().await {
                            Ok(Some(refreshed)) => {
                                *client.get_token().lock().await.unwrap() = Some(refreshed);
                                client
                                    .write_token_cache()
                                    .await
                                    .context("write refreshed token to cache")?;
                                return Ok(());
                            }
                            Ok(None) => tracing::warn!(
                                "Cached Spotify Web API token did not refresh; starting interactive reauthentication"
                            ),
                            Err(_) => tracing::warn!(
                                "Unable to refresh cached Spotify Web API authentication; starting interactive reauthentication"
                            ),
                        }
                    }
                    CachedTokenAction::Reauthenticate => tracing::warn!(
                        "Cached Spotify Web API token cannot be renewed; starting interactive reauthentication"
                    ),
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                diagnostic = %crate::observability::safe_error(
                    crate::observability::DiagnosticCode::SPOTIFY_TOKEN_CACHE_READ_FAILED,
                    crate::observability::ErrorCategory::Storage,
                    &error,
                ),
                "Unable to read Spotify Web API token cache; interactive authentication may be required"
            ),
        }
    }

    // No usable cached token: run the interactive authorization code flow.
    // `get_authorize_url` also generates and stores the PKCE verifier used by `request_token`.
    let url = client
        .get_authorize_url(None)
        .context("get authorize URL for user-provided client")?;
    let code = obtain_auth_code(&url, &client.get_oauth().redirect_uri, interaction)?;
    client
        .request_token(&code)
        .await
        .context("exchange auth code for token (user-provided client)")?;
    // `request_token` only updates the in-memory token. Persist the fresh
    // token so the next startup can reuse or refresh it instead of opening
    // the authorization URL again.
    // The user may have edited setup while the browser approval was pending.
    let client_changed =
        spotify_client_reauthorization_required(&configs.config_folder, &client.get_creds().id)?
            || client_changed;
    client
        .write_token_cache()
        .await
        .context("write Spotify Web API token cache")?;

    if client_changed {
        let mut setup = config::SetupState::load(&configs.config_folder)?;
        setup.spotify_reauthentication_required = false;
        setup.save(&configs.config_folder)?;
    }

    Ok(())
}

fn spotify_client_reauthorization_required(
    config_folder: &std::path::Path,
    runtime_id: &str,
) -> Result<bool> {
    let setup = config::SetupState::load(config_folder)?;
    if !setup.spotify_reauthentication_required {
        return Ok(false);
    }
    let saved = config::AppConfig::new(config_folder)?;
    anyhow::ensure!(
        saved.get_client_id()? == runtime_id,
        "The Spotify client choice changed. Use Apply & sign in in Welcome and retry."
    );
    Ok(true)
}

/// Run the authorization code with PKCE flow for `librespot` and return an access token.
fn get_oauth_access_token(
    client_id: &str,
    redirect_uri: &str,
    scopes: &[&str],
    interaction: AuthInteraction<'_>,
) -> Result<String> {
    let pkce = Pkce::new_random();
    let state = random_url_safe(16);
    let auth_url = build_authorize_url(client_id, redirect_uri, scopes, &pkce.challenge, &state)?;

    let code = obtain_auth_code(auth_url.as_str(), redirect_uri, interaction)?;
    exchange_code_for_token(client_id, redirect_uri, &code, &pkce.verifier)
}

/// Open the authorization URL in a browser and obtain the auth `code` from the redirect.
///
/// If `redirect_uri` is an HTTP loopback address with a port, a local server collects the code
/// automatically; otherwise the owning presentation callback supplies the redirect URL.
fn obtain_auth_code(
    auth_url: &str,
    redirect_uri: &str,
    interaction: AuthInteraction<'_>,
) -> Result<String> {
    open::that_in_background(auth_url);
    if let Some(prompt) = interaction.prompt_ref() {
        prompt.show_authorize_url(auth_url);
    }

    if let Some(addr) = redirect_socket_address(redirect_uri) {
        listen_for_auth_code(addr)
    } else {
        let Some(prompt) = interaction.prompt_ref() else {
            anyhow::bail!(
                "OAuth redirect requires interactive input; configure an HTTP loopback redirect"
            );
        };
        let redirect = prompt.read_redirect_url()?;
        code_from_redirect(redirect.trim())
            .context("no auth code found in the provided redirect URL")
    }
}

/// Spawn a local HTTP server that waits for the OAuth redirect and returns the auth `code`.
///
/// Browsers commonly prefetch resources such as `/favicon.ico` or
/// `/apple-touch-icon-precomposed.png` from the callback server. Unlike the listeners shipped by
/// `librespot-oauth` and `rspotify` — which treat the *first* incoming connection as the redirect
/// and therefore fail with "Auth code param not found" when a prefetch arrives first — this server
/// ignores any request that does not carry an auth `code` and keeps listening until the real
/// redirect arrives.
fn listen_for_auth_code(addr: SocketAddr) -> Result<String> {
    let listener =
        TcpListener::bind(addr).with_context(|| format!("bind OAuth callback server to {addr}"))?;
    tracing::info!("OAuth callback server is listening on the configured loopback address");

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                tracing::warn!(
                    diagnostic = %crate::observability::safe_error(
                        crate::observability::DiagnosticCode::OAUTH_CALLBACK_ACCEPT_FAILED,
                        crate::observability::ErrorCategory::NetworkUnavailable,
                        &err,
                    ),
                    "Failed to accept an OAuth callback connection"
                );
                continue;
            }
        };

        let mut request_line = String::new();
        if let Err(err) = BufReader::new(&stream).read_line(&mut request_line) {
            tracing::warn!(
                diagnostic = %crate::observability::safe_error(
                    crate::observability::DiagnosticCode::OAUTH_CALLBACK_READ_FAILED,
                    crate::observability::ErrorCategory::NetworkUnavailable,
                    &err,
                ),
                "Failed to read an OAuth callback request"
            );
            continue;
        }

        // The request line looks like `GET /login?code=...&state=... HTTP/1.1`.
        let request_target = request_line.split_whitespace().nth(1).unwrap_or_default();
        if let Some(code) = code_from_redirect(request_target) {
            respond(
                &mut stream,
                "200 OK",
                "Authentication successful! You can close this tab and return to the app.",
            );
            return Ok(code);
        }

        // Ignore stray requests (e.g. favicon / apple-touch-icon prefetches) that don't carry an
        // auth code, and keep listening for the real redirect.
        tracing::debug!("Ignoring OAuth callback request without an authorization code");
        respond(&mut stream, "404 Not Found", "");
    }

    anyhow::bail!("OAuth callback server stopped before receiving an auth code");
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(err) = stream.write_all(response.as_bytes()) {
        tracing::warn!(
            diagnostic = %crate::observability::safe_error(
                crate::observability::DiagnosticCode::OAUTH_CALLBACK_WRITE_FAILED,
                crate::observability::ErrorCategory::NetworkUnavailable,
                &err,
            ),
            "Failed to write an OAuth callback response"
        );
    }
}

/// Extract the `code` query parameter from a redirect, accepting either a full URL or a bare
/// request target (e.g. `/login?code=...`).
fn code_from_redirect(redirect: &str) -> Option<String> {
    let url = Url::parse(redirect)
        .or_else(|_| Url::parse(&format!("http://localhost{redirect}")))
        .ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, code)| code.into_owned())
}

/// Resolve the loopback socket address that an `http://host:port/...` redirect URI listens on.
fn redirect_socket_address(redirect_uri: &str) -> Option<SocketAddr> {
    let url = match Url::parse(redirect_uri) {
        Ok(url) if url.scheme() == "http" && url.port().is_some() => url,
        _ => return None,
    };
    url.socket_addrs(|| None).ok()?.into_iter().next()
}

fn build_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    scopes: &[&str],
    challenge: &str,
    state: &str,
) -> Result<Url> {
    let scope = scopes.join(" ");
    Url::parse_with_params(
        SPOTIFY_AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", scope.as_str()),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge),
            ("state", state),
        ],
    )
    .context("build Spotify authorize URL")
}

/// Exchange an authorization code for an access token via Spotify's token endpoint.
fn exchange_code_for_token(
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        access_token: String,
    }

    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];

    // `reqwest::blocking` spins up its own runtime, which panics if constructed on a thread that is
    // already running one. `get_creds` may be called from within the async client task, so perform
    // the exchange on a dedicated thread.
    std::thread::scope(|s| {
        s.spawn(|| {
            let token = reqwest::blocking::Client::new()
                .post(SPOTIFY_TOKEN_URL)
                .form(&params)
                .send()
                .context("send token exchange request")?
                .error_for_status()
                .context("token exchange request failed")?
                .json::<TokenResponse>()
                .context("parse token exchange response")?;
            Ok(token.access_token)
        })
        .join()
        .map_err(|_| anyhow::anyhow!("token exchange thread panicked"))?
    })
}

/// A PKCE code verifier/challenge pair (RFC 7636).
struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    fn new_random() -> Self {
        let verifier = random_url_safe(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// Generate a random URL-safe (base64url, no padding) string from `n` random bytes.
fn random_url_safe(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::fill(bytes.as_mut_slice());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod test {
    #[tokio::test]
    async fn retired_spotify_client_cannot_overwrite_new_token_cache() {
        use rspotify::clients::BaseClient;
        let folder = tempfile::tempdir().unwrap();
        let cache = folder.path().join("token.json");
        let epoch = std::sync::Arc::new(tokio::sync::RwLock::new(0));
        let make_client = |generation, token: &str| {
            let inner = rspotify::AuthCodePkceSpotify::with_config(
                rspotify::Credentials::default(),
                rspotify::OAuth::default(),
                rspotify::Config {
                    token_cached: true,
                    cache_path: cache.clone(),
                    ..Default::default()
                },
            );
            (
                super::SpotifyWebApiClient::new(inner).with_cache_owner(epoch.clone(), generation),
                rspotify::Token {
                    access_token: token.to_owned(),
                    ..Default::default()
                },
            )
        };
        let (old, token) = make_client(0, "old-test-token");
        *old.get_token().lock().await.unwrap() = Some(token);
        old.write_token_cache().await.unwrap();
        *epoch.write().await = 1;
        let (current, token) = make_client(1, "current-test-token");
        *current.get_token().lock().await.unwrap() = Some(token);
        current.write_token_cache().await.unwrap();
        let saved = std::fs::read(&cache).unwrap();
        assert!(old.write_token_cache().await.is_err());
        assert_eq!(std::fs::read(&cache).unwrap(), saved);
    }

    #[tokio::test]
    async fn a_cached_token_is_tied_to_the_client_it_was_issued_to() {
        use rspotify::clients::BaseClient;
        let folder = tempfile::tempdir().unwrap();
        let inner = rspotify::AuthCodePkceSpotify::with_config(
            rspotify::Credentials::new_pkce("client-a"),
            rspotify::OAuth::default(),
            rspotify::Config {
                token_cached: true,
                cache_path: folder.path().join("user_client_token.json"),
                ..Default::default()
            },
        );
        let client = super::SpotifyWebApiClient::new(inner);
        assert!(!super::cached_token_matches_client(
            folder.path(),
            "client-a"
        ));

        *client.get_token().lock().await.unwrap() = Some(rspotify::Token::default());
        client.write_token_cache().await.unwrap();
        assert!(super::cached_token_matches_client(
            folder.path(),
            "client-a"
        ));
        assert!(!super::cached_token_matches_client(
            folder.path(),
            "client-b"
        ));

        std::fs::remove_file(folder.path().join(super::SPOTIFY_TOKEN_CLIENT_FILE)).unwrap();
        assert!(
            !super::cached_token_matches_client(folder.path(), "client-a"),
            "a token without a recorded client is not reused"
        );
    }

    #[test]
    fn welcome_client_change_requires_matching_client_and_fresh_auth() {
        let folder = tempfile::tempdir().unwrap();
        let custom = "0123456789abcdef0123456789abcdef";
        crate::config::save_welcome_spotify_client(folder.path(), custom).unwrap();
        assert!(super::spotify_client_reauthorization_required(
            folder.path(),
            super::NCSPOT_CLIENT_ID
        )
        .is_err());
        assert!(super::spotify_client_reauthorization_required(folder.path(), custom).unwrap());
        // An unrelated setup status must never bypass the application change.
        let mut setup = crate::config::SetupState::load(folder.path()).unwrap();
        setup.status = crate::config::SetupStatus::Skipped;
        setup.save(folder.path()).unwrap();
        assert!(super::spotify_client_reauthorization_required(folder.path(), custom).unwrap());
        setup.spotify_reauthentication_required = false;
        setup.save(folder.path()).unwrap();
        assert!(!super::spotify_client_reauthorization_required(folder.path(), custom).unwrap());
    }

    use super::*;

    fn test_cache() -> (Cache, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "unified-player-auth-test-{}-{}",
            std::process::id(),
            random_url_safe(8)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let cache = Cache::new(Some(path.clone()), None, None, None).unwrap();
        (cache, path)
    }

    #[test]
    fn interactive_credentials_are_persisted_for_the_next_launch() {
        let (cache, path) = test_cache();
        let credentials = Credentials::with_access_token("access-token");

        cache.save_credentials(&credentials);

        assert_eq!(cache.credentials(), Some(credentials));
        std::fs::remove_dir_all(path).unwrap();
    }

    fn web_api_token(refresh_token: Option<&str>) -> rspotify::Token {
        rspotify::Token {
            access_token: "access-token".to_owned(),
            expires_in: chrono::Duration::hours(1),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            scopes: std::collections::HashSet::new(),
            refresh_token: refresh_token.map(str::to_owned),
        }
    }

    #[test]
    fn refresh_response_without_rotation_keeps_previous_refresh_token() {
        let mut refreshed = web_api_token(None);

        retain_previous_refresh_token(Some("previous-refresh-token".to_owned()), &mut refreshed);

        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("previous-refresh-token")
        );
    }

    #[test]
    fn rotated_refresh_token_takes_precedence() {
        let mut refreshed = web_api_token(Some("rotated-refresh-token"));

        retain_previous_refresh_token(Some("previous-refresh-token".to_owned()), &mut refreshed);

        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("rotated-refresh-token")
        );
    }

    #[test]
    fn missing_refresh_tokens_remain_missing() {
        let mut refreshed = web_api_token(None);

        retain_previous_refresh_token(None, &mut refreshed);

        assert!(refreshed.refresh_token.is_none());
    }

    #[test]
    fn reusable_cached_token_is_accepted() {
        let token = web_api_token(Some("refresh-token"));

        assert_eq!(cached_token_action(&token), CachedTokenAction::Reuse);
    }

    #[test]
    fn expired_cached_token_is_refreshed() {
        let mut token = web_api_token(Some("refresh-token"));
        token.expires_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));

        assert_eq!(cached_token_action(&token), CachedTokenAction::Refresh);
    }

    #[test]
    fn cached_token_without_refresh_token_requires_reauthentication() {
        let fresh = web_api_token(None);
        let mut expired = web_api_token(None);
        expired.expires_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));

        assert_eq!(
            cached_token_action(&fresh),
            CachedTokenAction::Reauthenticate
        );
        assert_eq!(
            cached_token_action(&expired),
            CachedTokenAction::Reauthenticate
        );
    }

    #[test]
    fn code_from_redirect_extracts_code() {
        // Bare request target (as read from the HTTP request line) and full URL both work.
        assert_eq!(
            code_from_redirect("/login?code=abc123&state=xyz").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            code_from_redirect("http://127.0.0.1:8989/login?code=abc123&state=xyz").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn code_from_redirect_ignores_stray_requests() {
        // The exact request that previously broke authentication: a browser prefetch with no code.
        assert_eq!(
            code_from_redirect("/apple-touch-icon-precomposed.png"),
            None
        );
        assert_eq!(code_from_redirect("/favicon.ico"), None);
        assert_eq!(code_from_redirect("/login"), None);
    }

    #[test]
    fn redirect_socket_address_requires_http_and_port() {
        assert!(redirect_socket_address("http://127.0.0.1:8989/login").is_some());
        // No port / non-http schemes fall back to the stdin flow.
        assert!(redirect_socket_address("http://127.0.0.1/login").is_none());
        assert!(redirect_socket_address("https://127.0.0.1:8989/login").is_none());
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // Verifier/challenge pair from RFC 7636, Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
