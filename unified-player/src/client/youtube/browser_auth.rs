use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const MUSIC_URL: &str = "https://music.youtube.com/";
// Export after YouTube's non-rotating endpoint instead of the active music tab.
const COOKIE_STABILIZATION_URL: &str = "https://www.youtube.com/robots.txt";
const COOKIE_STABILIZATION_DELAY: Duration = Duration::from_millis(750);
const DEVTOOLS_START_TIMEOUT: Duration = Duration::from_secs(30);
const DEVTOOLS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const BROWSER_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const BROWSER_SIGN_IN_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PLAYBACK_CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);
const PLAYBACK_BROWSER_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const NEW_ACCOUNT_PROFILE_PREFIX: &str = "account-login-";
#[cfg(target_os = "windows")]
pub(super) const BROWSER_MEDIA_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
#[cfg(target_os = "macos")]
pub(super) const BROWSER_MEDIA_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(super) const BROWSER_MEDIA_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
// Chrome permits only one process to own a user-data directory. Refresh and
// playback share the active profile, while account-add sign-in gets a fresh
// profile, so browser lifetimes must still be mutually exclusive.
static BROWSER_PROFILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PLAYBACK_BROWSER: std::sync::Mutex<Option<PlaybackBrowserProcess>> =
    std::sync::Mutex::new(None);
static PLAYBACK_BROWSER_GENERATION: AtomicU64 = AtomicU64::new(0);
static NEW_ACCOUNT_PROFILE_ID: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PLAYBACK_BROWSER_LAUNCHES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "private-capture")]
pub(super) struct FreshReplayBrowserProfileGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

#[cfg(feature = "private-capture")]
pub(super) fn try_lock_browser_profile_for_fresh_replay() -> Option<FreshReplayBrowserProfileGuard>
{
    BROWSER_PROFILE_LOCK
        .try_lock()
        .ok()
        .map(|guard| FreshReplayBrowserProfileGuard { _guard: guard })
}

struct PlaybackBrowserProcess {
    child: Child,
    debug_port: u16,
    websocket_url: Option<String>,
    generation: u64,
    last_used: Instant,
}

#[cfg_attr(not(feature = "private-capture"), allow(dead_code))]
pub(super) struct BrowserMediaCapture {
    pub(super) original_url: reqwest::Url,
    pub(super) sanitized_url: reqwest::Url,
    pub(super) request_headers: reqwest::header::HeaderMap,
    pub(super) response_status: Option<u16>,
    pub(super) redirect_count: u8,
    pub(super) from_disk_cache: bool,
    pub(super) from_service_worker: bool,
}

#[cfg(feature = "private-capture")]
#[derive(Clone)]
struct BrowserPrivateEvidence {
    capture: crate::developer_capture::CaptureSession,
    exchange_ref: crate::developer_capture::ExchangeRef,
}

#[cfg(feature = "private-capture")]
const BROWSER_PLAYER_EXCHANGE_CAPACITY: usize = 8;
#[cfg(feature = "private-capture")]
const MAX_DEVTOOLS_REQUEST_ID_BYTES: usize = 256;
#[cfg(feature = "private-capture")]
const MAX_DEVTOOLS_URL_BYTES: usize = 64 * 1024;
#[cfg(feature = "private-capture")]
const MAX_DEVTOOLS_HEADER_COUNT: usize = 256;
#[cfg(feature = "private-capture")]
const MAX_DEVTOOLS_HEADER_BYTES: usize = 256 * 1024;

#[cfg(feature = "private-capture")]
#[allow(clippy::struct_excessive_bools)]
struct PendingBrowserPlayerExchange {
    request_id: String,
    request: reqwest::Request,
    request_complete: bool,
    response_status: Option<reqwest::StatusCode>,
    response_url: Option<reqwest::Url>,
    response_headers: reqwest::header::HeaderMap,
    response_complete: bool,
    started_at: Instant,
    attempt: u8,
    redirected: bool,
    from_disk_cache: bool,
    from_service_worker: bool,
}

#[cfg(feature = "private-capture")]
#[derive(Default)]
struct BrowserPlayerExchangeTracker {
    pending: VecDeque<PendingBrowserPlayerExchange>,
    pending_extra_headers: VecDeque<(String, reqwest::header::HeaderMap)>,
    observed: usize,
    dropped: u64,
}

#[derive(Clone, Copy)]
enum BrowserCaptureMode {
    RequestOnly,
    AwaitResponse,
}

impl BrowserCaptureMode {
    const fn awaits_media_response(self) -> bool {
        matches!(self, Self::AwaitResponse)
    }

    const fn label(self) -> &'static str {
        match self {
            Self::RequestOnly => "request_only",
            Self::AwaitResponse => "await_response",
        }
    }
}

pub struct BrowserLoginSession {
    child: Child,
    debug_port: u16,
    executable: PathBuf,
    profile_path: PathBuf,
    temporary_profile: bool,
    profile_promoted: bool,
    browser_stopped: bool,
    _profile_guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for BrowserLoginSession {
    fn drop(&mut self) {
        if self.temporary_profile && !self.profile_promoted && self.browser_stopped {
            let _ = std::fs::remove_dir_all(&self.profile_path);
        }
    }
}

impl BrowserLoginSession {
    pub fn profile_path(&self) -> &Path {
        &self.profile_path
    }

    pub fn promote_to_active_profile(&mut self, config_folder: &Path) -> Result<()> {
        anyhow::ensure!(
            self.temporary_profile,
            "the browser session is already using the active YouTube profile"
        );
        promote_profile(
            &self.profile_path,
            &profile_path(config_folder),
            &self.executable,
            self.browser_stopped,
        )?;
        self.profile_promoted = true;
        Ok(())
    }

    pub async fn wait_for_sign_in_and_save(
        &mut self,
        cookie_path: &Path,
        close_browser: bool,
        cancellation: &CancellationToken,
    ) -> Result<Option<usize>> {
        let result = tokio::time::timeout(BROWSER_SIGN_IN_TIMEOUT, async {
            loop {
                if self
                    .child
                    .try_wait()
                    .context("check dedicated YouTube sign-in browser")?
                    .is_some()
                {
                    return Ok(None);
                }
                let websocket_url = wait_for_browser_target(self.debug_port).await?;
                let cookies = tokio::select! {
                    () = cancellation.cancelled() => return Ok(None),
                    cookies = capture_youtube_cookies(&websocket_url) => cookies?,
                };
                if signed_in_cookie_header(cookies)?.is_some() {
                    let page_websocket_url =
                        wait_for_page_target(self.debug_port, false, &mut self.child).await?;
                    let cookies = tokio::select! {
                        () = cancellation.cancelled() => return Ok(None),
                        cookies = capture_stable_youtube_cookies(&websocket_url, &page_websocket_url) => cookies?,
                    };
                    let Some(cookie_header) = signed_in_cookie_header(cookies)? else {
                        continue;
                    };
                    let cookie_count = cookie_header.split(';').count();
                    persist_cookie(cookie_path, &cookie_header)?;
                    remember_browser_path(&self.profile_path, &self.executable)?;
                    return Ok(Some(cookie_count));
                }
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(None),
                    () = tokio::time::sleep(BROWSER_SIGN_IN_POLL_INTERVAL) => {}
                }
            }
        })
        .await
        .context("timed out after five minutes waiting for YouTube Music sign-in")
        .and_then(|result| result);
        if close_browser {
            if let Ok(websocket_url) = wait_for_browser_target(self.debug_port).await {
                let _ = close_devtools_browser(&websocket_url).await;
            }
            terminate_browser(&mut self.child).await;
            self.browser_stopped = true;
        }
        result
    }
}

pub async fn begin_login_for_new_account(
    config_folder: &Path,
    executable_override: Option<PathBuf>,
    headless: bool,
) -> Result<BrowserLoginSession> {
    let profile = fresh_account_profile_path(config_folder)?;
    match begin_login_at_profile(
        config_folder,
        executable_override,
        headless,
        profile.clone(),
        true,
    )
    .await
    {
        Ok(session) => Ok(session),
        Err(error) => {
            let _ = std::fs::remove_dir_all(profile);
            Err(error)
        }
    }
}

async fn begin_login_at_profile(
    config_folder: &Path,
    executable_override: Option<PathBuf>,
    headless: bool,
    profile_path: PathBuf,
    temporary_profile: bool,
) -> Result<BrowserLoginSession> {
    let profile_guard = BROWSER_PROFILE_LOCK.lock().await;
    shutdown_playback_browser_locked().await;
    let executable = resolve_browser_executable(config_folder, executable_override)?;
    std::fs::create_dir_all(&profile_path).context("create dedicated YouTube browser profile")?;
    let port = available_local_port()?;
    let mut command = Command::new(&executable);
    command
        .arg(format!("--user-data-dir={}", profile_path.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--new-window")
        .arg(MUSIC_URL)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if headless {
        command.arg("--headless=new");
    }
    let mut child = command
        .spawn()
        .context("launch dedicated YouTube sign-in browser")?;
    match wait_for_page_target(port, headless, &mut child).await {
        Ok(_) => {}
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    }
    if headless {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(BrowserLoginSession {
        child,
        debug_port: port,
        executable,
        profile_path,
        temporary_profile,
        profile_promoted: false,
        browser_stopped: false,
        _profile_guard: profile_guard,
    })
}

fn fresh_account_profile_path(config_folder: &Path) -> Result<PathBuf> {
    let youtube_folder = config_folder.join("youtube");
    std::fs::create_dir_all(&youtube_folder)
        .context("create YouTube account sign-in profile folder")?;
    for _ in 0..8 {
        let ordinal = NEW_ACCOUNT_PROFILE_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile =
            youtube_folder.join(format!("{NEW_ACCOUNT_PROFILE_PREFIX}{timestamp}-{ordinal}"));
        match std::fs::create_dir(&profile) {
            Ok(()) => return Ok(profile),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("create temporary YouTube account sign-in profile")
            }
        }
    }
    anyhow::bail!("could not allocate a temporary YouTube account sign-in profile")
}

fn promote_profile(
    profile: &Path,
    active_profile: &Path,
    executable: &Path,
    browser_stopped: bool,
) -> Result<()> {
    anyhow::ensure!(
        profile.is_dir(),
        "temporary YouTube account profile is missing"
    );
    if active_profile.is_dir() {
        std::fs::remove_dir_all(active_profile)
            .context("remove previous YouTube browser profile")?;
    } else if active_profile.exists() {
        std::fs::remove_file(active_profile).context("remove previous YouTube browser profile")?;
    }
    if browser_stopped {
        std::fs::rename(profile, active_profile).context("activate new YouTube browser profile")?;
    } else {
        copy_profile_directory(profile, active_profile)?;
    }
    remember_browser_path(active_profile, executable)?;
    if browser_stopped && profile.exists() {
        // The browser has released the temporary profile, so do not leave a
        // second credential-bearing profile behind after promotion.
        std::fs::remove_dir_all(profile).context("remove temporary YouTube account profile")?;
    }
    Ok(())
}

fn copy_profile_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination).context("create active YouTube browser profile")?;
    for entry in std::fs::read_dir(source).context("read temporary YouTube browser profile")? {
        let entry = entry.context("read temporary YouTube browser profile entry")?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_profile_directory(&source_path, &destination_path)?;
        } else if source_path.is_file() {
            std::fs::copy(&source_path, &destination_path)
                .context("copy temporary YouTube browser profile file")?;
        }
    }
    Ok(())
}

fn profile_path(config_folder: &Path) -> PathBuf {
    config_folder.join("youtube").join("browser-profile")
}

fn browser_path_file(config_folder: &Path) -> PathBuf {
    config_folder.join("youtube").join("browser-path.txt")
}

fn available_local_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .context("reserve a local browser debugging port")?;
    Ok(listener.local_addr()?.port())
}

pub(crate) fn resolve_browser_executable(
    config_folder: &Path,
    executable_override: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(candidate) = executable_override {
        return which::which(candidate).context(
            "Selected browser is missing or not executable; choose another browser path.",
        );
    }
    read_remembered_browser_path(config_folder)
        .into_iter()
        .chain(platform_browser_candidates())
        .find_map(|candidate| which::which(candidate).ok())
        .context("No supported browser found. Choose a browser path or import cookies in Welcome.")
}

pub(crate) fn save_browser_choice(config_folder: &Path, candidate: PathBuf) -> Result<PathBuf> {
    let executable = resolve_browser_executable(config_folder, Some(candidate))?;
    let profile = profile_path(config_folder);
    std::fs::create_dir_all(profile.parent().context("browser folder unavailable")?)?;
    remember_browser_path(&profile, &executable)?;
    Ok(executable)
}

fn read_remembered_browser_path(config_folder: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(browser_path_file(config_folder))
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty())
}

fn remember_browser_path(profile_path: &Path, executable: &Path) -> Result<()> {
    let youtube_folder = profile_path
        .parent()
        .context("dedicated browser profile has no parent folder")?;
    let path = youtube_folder.join("browser-path.txt");
    let value = executable.to_string_lossy();
    atomicwrites::AtomicFile::new(&path, atomicwrites::AllowOverwrite)
        .write(|file| std::io::Write::write_all(file, value.as_bytes()))
        .context("remember YouTube browser executable")?;
    super::restrict_token_permissions(&path)
}

fn platform_browser_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for (environment, suffix) in [
            ("PROGRAMFILES", "Google/Chrome/Application/chrome.exe"),
            ("PROGRAMFILES(X86)", "Google/Chrome/Application/chrome.exe"),
            ("LOCALAPPDATA", "Google/Chrome/Application/chrome.exe"),
            ("LOCALAPPDATA", "imput/Helium/Application/helium.exe"),
            ("LOCALAPPDATA", "imput/Helium/Application/chrome.exe"),
            ("PROGRAMFILES", "imput/Helium/Application/helium.exe"),
            ("PROGRAMFILES", "imput/Helium/Application/chrome.exe"),
            ("PROGRAMFILES", "Microsoft/Edge/Application/msedge.exe"),
            ("PROGRAMFILES(X86)", "Microsoft/Edge/Application/msedge.exe"),
        ] {
            if let Some(root) = std::env::var_os(environment) {
                candidates.push(PathBuf::from(root).join(suffix));
            }
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        PathBuf::from("/Applications/Helium.app/Contents/MacOS/Helium"),
        PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
    ]);
    candidates.extend(
        [
            "helium",
            "helium-browser",
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevtoolsTarget {
    #[serde(rename = "type")]
    target_type: String,
    url: String,
    web_socket_debugger_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevtoolsVersion {
    web_socket_debugger_url: String,
}

async fn wait_for_browser_target(port: u16) -> Result<String> {
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}/json/version");
    let deadline = tokio::time::Instant::now() + DEVTOOLS_START_TIMEOUT;
    loop {
        if let Ok(response) = client.get(&endpoint).send().await {
            if let Ok(version) = response.json::<DevtoolsVersion>().await {
                return Ok(version.web_socket_debugger_url);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("the dedicated sign-in browser did not expose its control endpoint within 30 seconds");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_page_target(
    port: u16,
    require_music_page: bool,
    child: &mut Child,
) -> Result<String> {
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}/json/list");
    let deadline = tokio::time::Instant::now() + DEVTOOLS_START_TIMEOUT;
    loop {
        if let Ok(response) = client.get(&endpoint).send().await {
            if let Ok(targets) = response.json::<Vec<DevtoolsTarget>>().await {
                if let Some(url) = targets.into_iter().find_map(|target| {
                    (target.target_type == "page"
                        && (!require_music_page || target.url.starts_with(MUSIC_URL)))
                    .then_some(target.web_socket_debugger_url)
                    .flatten()
                }) {
                    return Ok(url);
                }
            }
        }
        if let Some(status) = child
            .try_wait()
            .context("check dedicated YouTube sign-in browser startup")?
        {
            anyhow::bail!(
                "the dedicated sign-in browser exited during startup ({status}); another browser may already own its profile"
            );
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("the dedicated sign-in browser did not expose a page within 30 seconds");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[derive(Clone, Debug, Deserialize)]
struct DevtoolsCookie {
    name: String,
    value: String,
    domain: String,
}

#[derive(Deserialize)]
struct CookieResponse {
    id: u64,
    result: Option<CookieResult>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CookieResult {
    cookies: Vec<DevtoolsCookie>,
}

async fn capture_youtube_cookies(websocket_url: &str) -> Result<Vec<DevtoolsCookie>> {
    tokio::time::timeout(DEVTOOLS_COMMAND_TIMEOUT, async {
        let (mut socket, _) = tokio_tungstenite::connect_async(websocket_url)
            .await
            .map_err(|_| anyhow::anyhow!("connect to the dedicated sign-in browser"))?;
        socket
            .send(Message::Text(
                serde_json::json!({ "id": 1, "method": "Storage.getCookies" })
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| {
                anyhow::anyhow!("request YouTube cookies from the dedicated sign-in browser")
            })?;
        while let Some(message) = socket.next().await {
            let message =
                message.map_err(|_| anyhow::anyhow!("read dedicated browser response"))?;
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(response) = serde_json::from_str::<CookieResponse>(&text) else {
                continue;
            };
            if response.id != 1 {
                continue;
            }
            if response.error.is_some() {
                anyhow::bail!("dedicated browser refused cookie export");
            }
            return response
                .result
                .map(|result| result.cookies)
                .context("dedicated browser returned no cookie result");
        }
        anyhow::bail!("dedicated browser closed before returning YouTube cookies")
    })
    .await
    .context("timed out while importing the dedicated browser session")?
}

async fn capture_stable_youtube_cookies(
    cookie_websocket_url: &str,
    page_websocket_url: &str,
) -> Result<Vec<DevtoolsCookie>> {
    navigate_to_cookie_stabilization_page(page_websocket_url).await?;
    tokio::time::sleep(COOKIE_STABILIZATION_DELAY).await;
    capture_youtube_cookies(cookie_websocket_url).await
}

async fn navigate_to_cookie_stabilization_page(websocket_url: &str) -> Result<()> {
    let (socket, _) = tokio_tungstenite::connect_async(websocket_url)
        .await
        .map_err(|_| anyhow::anyhow!("connect to the dedicated sign-in browser"))?;
    let mut dispatcher = DevtoolsDispatcher::new(socket);
    send_devtools_command(
        &mut dispatcher,
        1,
        "Page.navigate",
        serde_json::json!({ "url": COOKIE_STABILIZATION_URL }),
        None,
    )
    .await
    .context("navigate the dedicated browser to the stable YouTube cookie page")?;
    Ok(())
}

fn signed_in_cookie_header(cookies: Vec<DevtoolsCookie>) -> Result<Option<String>> {
    let mut selected = BTreeMap::<String, (bool, String)>::new();
    for cookie in cookies {
        let domain = cookie.domain.trim_start_matches('.');
        if domain != "youtube.com" && !domain.ends_with(".youtube.com") {
            continue;
        }
        anyhow::ensure!(
            !cookie.name.contains([';', '\r', '\n']) && !cookie.value.contains([';', '\r', '\n']),
            "dedicated browser returned an invalid YouTube cookie"
        );
        let shared_domain = domain == "youtube.com";
        let entry = selected
            .entry(cookie.name)
            .or_insert_with(|| (shared_domain, cookie.value.clone()));
        if shared_domain && !entry.0 {
            *entry = (true, cookie.value);
        }
    }
    let has_sapisid = selected.contains_key("SAPISID")
        || selected.contains_key("__Secure-3PAPISID")
        || selected.contains_key("__Secure-1PAPISID");
    let has_login = selected.contains_key("LOGIN_INFO") || selected.contains_key("SID");
    if !has_sapisid || !has_login {
        return Ok(None);
    }
    Ok(Some(
        selected
            .into_iter()
            .map(|(name, (_, value))| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

fn persist_cookie(path: &Path, cookie: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create YouTube credential folder")?;
    }
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write(|file| std::io::Write::write_all(file, cookie.as_bytes()))
        .context("persist YouTube browser session")?;
    super::restrict_token_permissions(path)
}

async fn close_devtools_browser(websocket_url: &str) -> Result<()> {
    tokio::time::timeout(DEVTOOLS_COMMAND_TIMEOUT, async {
        let (mut socket, _) = tokio_tungstenite::connect_async(websocket_url)
            .await
            .map_err(|_| anyhow::anyhow!("reconnect to the dedicated sign-in browser"))?;
        socket
            .send(Message::Text(
                serde_json::json!({ "id": 2, "method": "Browser.close" })
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| anyhow::anyhow!("close the dedicated sign-in browser"))
    })
    .await
    .context("timed out while closing the dedicated sign-in browser")?
}

type DevtoolsSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const DEVTOOLS_PENDING_REPLY_CAPACITY: usize = 16;
const DEVTOOLS_EVENT_CAPACITY: usize = 256;

#[derive(Default)]
struct DevtoolsInbox {
    replies: VecDeque<(u64, serde_json::Value)>,
    events: VecDeque<serde_json::Value>,
    dropped_replies: u64,
    dropped_events: u64,
}

impl DevtoolsInbox {
    fn route(&mut self, message: serde_json::Value) {
        if let Some(id) = message.get("id").and_then(serde_json::Value::as_u64) {
            if let Some(position) = self
                .replies
                .iter()
                .position(|(pending_id, _)| *pending_id == id)
            {
                self.replies.remove(position);
            }
            if self.replies.len() == DEVTOOLS_PENDING_REPLY_CAPACITY {
                self.replies.pop_front();
                self.dropped_replies = self.dropped_replies.saturating_add(1);
            }
            self.replies.push_back((id, message));
        } else if message
            .get("method")
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            if self.events.len() == DEVTOOLS_EVENT_CAPACITY {
                self.events.pop_front();
                self.dropped_events = self.dropped_events.saturating_add(1);
            }
            self.events.push_back(message);
        }
    }

    fn take_reply(&mut self, id: u64) -> Option<serde_json::Value> {
        let position = self
            .replies
            .iter()
            .position(|(pending_id, _)| *pending_id == id)?;
        self.replies.remove(position).map(|(_, reply)| reply)
    }

    fn take_event(&mut self) -> Option<serde_json::Value> {
        self.events.pop_front()
    }

    const fn dropped_messages(&self) -> (u64, u64) {
        (self.dropped_replies, self.dropped_events)
    }
}

struct DevtoolsDispatcher {
    socket: DevtoolsSocket,
    inbox: DevtoolsInbox,
}

impl DevtoolsDispatcher {
    fn new(socket: DevtoolsSocket) -> Self {
        Self {
            socket,
            inbox: DevtoolsInbox::default(),
        }
    }

    async fn send(
        &mut self,
        id: u64,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
    ) -> Result<()> {
        let mut command = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            command["sessionId"] = serde_json::Value::String(session_id.to_string());
        }
        self.socket
            .send(Message::Text(command.to_string().into()))
            .await
            .map_err(|_| anyhow::anyhow!("send {method} to the dedicated browser"))
    }

    async fn command(
        &mut self,
        id: u64,
        method: &str,
        params: serde_json::Value,
        session_id: Option<&str>,
    ) -> Result<serde_json::Value> {
        self.send(id, method, params, session_id).await?;
        tokio::time::timeout(DEVTOOLS_COMMAND_TIMEOUT, async {
            loop {
                if let Some(reply) = self.inbox.take_reply(id) {
                    return devtools_command_result(method, &reply);
                }
                self.pump_message()
                    .await
                    .with_context(|| format!("run {method} in the dedicated browser"))?;
            }
        })
        .await
        .with_context(|| format!("timed out while running {method} in the dedicated browser"))?
    }

    async fn next_event(&mut self) -> Result<serde_json::Value> {
        loop {
            if let Some(event) = self.inbox.take_event() {
                return Ok(event);
            }
            self.pump_message().await?;
        }
    }

    async fn pump_message(&mut self) -> Result<()> {
        let Some(message) = self.socket.next().await else {
            anyhow::bail!("dedicated browser control channel closed unexpectedly");
        };
        let message =
            message.map_err(|_| anyhow::anyhow!("dedicated browser control channel failed"))?;
        let Message::Text(text) = message else {
            return Ok(());
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Ok(());
        };
        self.inbox.route(message);
        Ok(())
    }

    const fn dropped_messages(&self) -> (u64, u64) {
        self.inbox.dropped_messages()
    }
}

fn devtools_command_result(
    method: &str,
    response: &serde_json::Value,
) -> Result<serde_json::Value> {
    if response.get("error").is_some() {
        anyhow::bail!("dedicated browser refused {method}");
    }
    response
        .get("result")
        .cloned()
        .context("dedicated browser returned no command result")
}

async fn send_devtools_command(
    dispatcher: &mut DevtoolsDispatcher,
    id: u64,
    method: &str,
    params: serde_json::Value,
    session_id: Option<&str>,
) -> Result<serde_json::Value> {
    dispatcher.command(id, method, params, session_id).await
}

pub(super) async fn capture_playback_url(
    config_folder: &Path,
    video_id: &str,
    itag: u64,
    content_length: Option<u64>,
    cancellation: &CancellationToken,
    close_after_capture: bool,
) -> Result<reqwest::Url> {
    Ok(capture_playback_media(
        config_folder,
        video_id,
        itag,
        content_length,
        cancellation,
        close_after_capture,
        BrowserCaptureMode::RequestOnly,
        #[cfg(feature = "private-capture")]
        None,
    )
    .await?
    .sanitized_url)
}

#[cfg(feature = "private-capture")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn capture_playback_url_with_private_evidence(
    config_folder: &Path,
    video_id: &str,
    itag: u64,
    content_length: Option<u64>,
    cancellation: &CancellationToken,
    close_after_capture: bool,
    capture: crate::developer_capture::CaptureSession,
    exchange_ref: crate::developer_capture::ExchangeRef,
) -> Result<reqwest::Url> {
    Ok(capture_playback_media(
        config_folder,
        video_id,
        itag,
        content_length,
        cancellation,
        close_after_capture,
        BrowserCaptureMode::RequestOnly,
        Some(BrowserPrivateEvidence {
            capture,
            exchange_ref,
        }),
    )
    .await?
    .sanitized_url)
}

pub(super) async fn capture_playback_diagnostic(
    config_folder: &Path,
    video_id: &str,
    itag: u64,
    content_length: Option<u64>,
    cancellation: &CancellationToken,
) -> Result<BrowserMediaCapture> {
    capture_playback_media(
        config_folder,
        video_id,
        itag,
        content_length,
        cancellation,
        true,
        BrowserCaptureMode::AwaitResponse,
        #[cfg(feature = "private-capture")]
        None,
    )
    .await
}

fn browser_capture_outcome(
    cancellation: &CancellationToken,
    result: &Result<BrowserMediaCapture>,
    elapsed: Duration,
) -> (crate::observability::OperationOutcome, &'static str) {
    if result.is_ok() {
        (
            crate::observability::OperationOutcome::Success,
            "media_captured",
        )
    } else if cancellation.is_cancelled() {
        (
            crate::observability::OperationOutcome::Cancelled,
            "cancelled",
        )
    } else if elapsed >= PLAYBACK_CAPTURE_TIMEOUT {
        (crate::observability::OperationOutcome::Timeout, "timeout")
    } else {
        (
            crate::observability::OperationOutcome::Error,
            "capture_error",
        )
    }
}

fn browser_stage(
    stage: &'static str,
    elapsed: Option<Duration>,
    outcome: Option<crate::observability::OperationOutcome>,
    phase: &'static str,
    status_class: &'static str,
    error_type: Option<&'static str>,
) {
    crate::observability::operation_stage_detail(
        crate::observability::Component::Browser,
        stage,
        elapsed,
        outcome,
        Some(phase),
        Some(status_class),
        error_type,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
async fn capture_playback_media(
    config_folder: &Path,
    video_id: &str,
    itag: u64,
    content_length: Option<u64>,
    cancellation: &CancellationToken,
    close_after_capture: bool,
    mode: BrowserCaptureMode,
    #[cfg(feature = "private-capture")] private_evidence: Option<BrowserPrivateEvidence>,
) -> Result<BrowserMediaCapture> {
    let capture_started = Instant::now();
    browser_stage(
        "youtube_browser_capture",
        Some(Duration::ZERO),
        None,
        mode.label(),
        "started",
        None,
    );
    let _capture_guard = tokio::select! {
        () = cancellation.cancelled() => {
            browser_stage(
                "youtube_browser_capture",
                Some(capture_started.elapsed()),
                Some(crate::observability::OperationOutcome::Cancelled),
                mode.label(),
                "cancelled",
                Some("cancelled"),
            );
            anyhow::bail!("YouTube browser playback capture was cancelled");
        }
        guard = BROWSER_PROFILE_LOCK.lock() => guard,
    };
    anyhow::ensure!(
        !video_id.is_empty()
            && video_id.len() <= 64
            && video_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid YouTube media identifier"
    );
    let profile_path = profile_path(config_folder);
    if !profile_path.is_dir() {
        browser_stage(
            "youtube_browser_profile",
            Some(capture_started.elapsed()),
            Some(crate::observability::OperationOutcome::Error),
            "active",
            "missing",
            Some("storage"),
        );
        anyhow::bail!(
            "the dedicated YouTube browser profile is missing; run `unified-player youtube browser-login`"
        );
    }
    browser_stage(
        "youtube_browser_profile",
        Some(capture_started.elapsed()),
        Some(crate::observability::OperationOutcome::Success),
        "active",
        "ready",
        None,
    );
    let acquire_started = Instant::now();
    let (websocket_url, generation, reused) =
        match acquire_playback_browser(config_folder, &profile_path, cancellation).await {
            Ok(browser) => {
                browser_stage(
                    "youtube_browser_process",
                    Some(acquire_started.elapsed()),
                    Some(crate::observability::OperationOutcome::Success),
                    if browser.2 { "reused" } else { "started" },
                    "ready",
                    None,
                );
                browser
            }
            Err(error) => {
                browser_stage(
                    "youtube_browser_process",
                    Some(acquire_started.elapsed()),
                    Some(if cancellation.is_cancelled() {
                        crate::observability::OperationOutcome::Cancelled
                    } else {
                        crate::observability::OperationOutcome::Error
                    }),
                    "acquire",
                    if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "unavailable"
                    },
                    Some(if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "resource"
                    }),
                );
                crate::observability::set_component_health(
                    crate::observability::Component::Browser,
                    crate::observability::HealthStatus::Degraded,
                    "playback-browser-unavailable",
                );
                return Err(error);
            }
        };
    crate::observability::set_component_health(
        crate::observability::Component::Browser,
        crate::observability::HealthStatus::Healthy,
        if reused {
            "session-reused"
        } else {
            "session-started"
        },
    );
    tracing::debug!(
        reused,
        idle_timeout_seconds = PLAYBACK_BROWSER_IDLE_TIMEOUT.as_secs(),
        "Using bounded YouTube playback browser session"
    );
    let result = capture_playback_media_from_browser(
        &websocket_url,
        video_id,
        itag,
        content_length,
        cancellation,
        mode,
        #[cfg(feature = "private-capture")]
        private_evidence.as_ref(),
    )
    .await;
    let (capture_outcome, capture_status) =
        browser_capture_outcome(cancellation, &result, capture_started.elapsed());
    browser_stage(
        "youtube_browser_capture",
        Some(capture_started.elapsed()),
        Some(capture_outcome),
        mode.label(),
        capture_status,
        None,
    );
    if close_after_capture || (result.is_err() && !cancellation.is_cancelled()) {
        shutdown_playback_browser_generation_locked(generation).await;
    } else {
        touch_playback_browser(generation);
        schedule_playback_browser_idle_shutdown(generation);
    }
    result
}

async fn acquire_playback_browser(
    config_folder: &Path,
    profile_path: &Path,
    cancellation: &CancellationToken,
) -> Result<(String, u64, bool)> {
    let generation = PLAYBACK_BROWSER_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let existing = {
        let mut browser = PLAYBACK_BROWSER
            .lock()
            .expect("YouTube playback browser mutex poisoned");
        let alive = browser.as_mut().is_some_and(|process| {
            process
                .child
                .try_wait()
                .is_ok_and(|status| status.is_none())
        });
        if !alive {
            *browser = None;
        }
        browser.as_mut().map(|process| {
            process.generation = generation;
            process.last_used = Instant::now();
            (process.debug_port, process.websocket_url.clone(), true)
        })
    };
    let (port, known_websocket_url, reused) = if let Some(existing) = existing {
        existing
    } else {
        let executable = resolve_browser_executable(config_folder, None)?;
        let port = available_local_port()?;
        let mut command = Command::new(&executable);
        command
            .arg(format!("--user-data-dir={}", profile_path.display()))
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg("--headless=new")
            .arg("--no-startup-window")
            .arg("--mute-audio")
            .arg("--autoplay-policy=no-user-gesture-required")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        suppress_browser_window(&mut command);
        let child = command
            .spawn()
            .context("launch the dedicated YouTube playback browser")?;
        browser_stage(
            "youtube_browser_process",
            Some(Duration::ZERO),
            None,
            "started",
            "spawned",
            None,
        );
        #[cfg(test)]
        PLAYBACK_BROWSER_LAUNCHES.fetch_add(1, Ordering::SeqCst);
        *PLAYBACK_BROWSER
            .lock()
            .expect("YouTube playback browser mutex poisoned") = Some(PlaybackBrowserProcess {
            child,
            debug_port: port,
            websocket_url: None,
            generation,
            last_used: Instant::now(),
        });
        (port, None, false)
    };

    let websocket_url = if let Some(url) = known_websocket_url {
        browser_stage(
            "youtube_browser_cdp_endpoint",
            Some(Duration::ZERO),
            Some(crate::observability::OperationOutcome::Success),
            "reused",
            "ready",
            None,
        );
        url
    } else {
        let endpoint_started = Instant::now();
        let result = tokio::select! {
            () = cancellation.cancelled() => {
                Err(anyhow::anyhow!("YouTube browser playback capture was cancelled"))
            }
            result = wait_for_browser_target(port) => result,
        };
        let url = match result {
            Ok(url) => {
                browser_stage(
                    "youtube_browser_cdp_endpoint",
                    Some(endpoint_started.elapsed()),
                    Some(crate::observability::OperationOutcome::Success),
                    "started",
                    "ready",
                    None,
                );
                url
            }
            Err(err) => {
                browser_stage(
                    "youtube_browser_cdp_endpoint",
                    Some(endpoint_started.elapsed()),
                    Some(if cancellation.is_cancelled() {
                        crate::observability::OperationOutcome::Cancelled
                    } else {
                        crate::observability::OperationOutcome::Error
                    }),
                    "started",
                    if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "unavailable"
                    },
                    Some(if cancellation.is_cancelled() {
                        "cancelled"
                    } else {
                        "network"
                    }),
                );
                shutdown_playback_browser_generation_locked(generation).await;
                return Err(err);
            }
        };
        if let Some(process) = PLAYBACK_BROWSER
            .lock()
            .expect("YouTube playback browser mutex poisoned")
            .as_mut()
            .filter(|process| process.generation == generation)
        {
            process.websocket_url = Some(url.clone());
        }
        url
    };
    Ok((websocket_url, generation, reused))
}

fn touch_playback_browser(generation: u64) {
    if let Some(process) = PLAYBACK_BROWSER
        .lock()
        .expect("YouTube playback browser mutex poisoned")
        .as_mut()
        .filter(|process| process.generation == generation)
    {
        process.last_used = Instant::now();
    }
}

fn schedule_playback_browser_idle_shutdown(generation: u64) {
    tokio::task::spawn(async move {
        tokio::time::sleep(PLAYBACK_BROWSER_IDLE_TIMEOUT).await;
        let _capture_guard = BROWSER_PROFILE_LOCK.lock().await;
        let should_shutdown = PLAYBACK_BROWSER
            .lock()
            .expect("YouTube playback browser mutex poisoned")
            .as_ref()
            .is_some_and(|process| {
                process.generation == generation
                    && process.last_used.elapsed() >= PLAYBACK_BROWSER_IDLE_TIMEOUT
            });
        if should_shutdown {
            tracing::debug!("Closing idle YouTube playback browser session");
            shutdown_playback_browser_generation_locked(generation).await;
        }
    });
}

async fn shutdown_playback_browser_generation_locked(generation: u64) {
    let process = {
        let mut browser = PLAYBACK_BROWSER
            .lock()
            .expect("YouTube playback browser mutex poisoned");
        if browser
            .as_ref()
            .is_some_and(|process| process.generation == generation)
        {
            browser.take()
        } else {
            None
        }
    };
    if let Some(mut process) = process {
        if let Some(websocket_url) = process.websocket_url.as_deref() {
            let _ = close_devtools_browser(websocket_url).await;
        }
        terminate_browser(&mut process.child).await;
        crate::observability::set_component_health(
            crate::observability::Component::Browser,
            crate::observability::HealthStatus::Stopped,
            "session-closed",
        );
    }
}

pub(crate) async fn shutdown_playback_browser() {
    let _profile_guard = BROWSER_PROFILE_LOCK.lock().await;
    shutdown_playback_browser_locked().await;
}

async fn shutdown_playback_browser_locked() {
    let process = PLAYBACK_BROWSER
        .lock()
        .expect("YouTube playback browser mutex poisoned")
        .take();
    if let Some(mut process) = process {
        if let Some(websocket_url) = process.websocket_url.as_deref() {
            let _ = close_devtools_browser(websocket_url).await;
        }
        terminate_browser(&mut process.child).await;
        crate::observability::set_component_health(
            crate::observability::Component::Browser,
            crate::observability::HealthStatus::Stopped,
            "session-closed",
        );
    }
}

#[cfg(test)]
fn shutdown_playback_browser_now() {
    let process = PLAYBACK_BROWSER
        .lock()
        .expect("YouTube playback browser mutex poisoned")
        .take();
    if let Some(mut process) = process {
        let _ = process.child.kill();
        let _ = process.child.wait();
    }
}

#[cfg(test)]
fn playback_browser_process_id() -> Option<u32> {
    PLAYBACK_BROWSER
        .lock()
        .expect("YouTube playback browser mutex poisoned")
        .as_ref()
        .map(|process| process.child.id())
}

#[cfg(test)]
fn playback_browser_debug_port() -> Option<u16> {
    PLAYBACK_BROWSER
        .lock()
        .expect("YouTube playback browser mutex poisoned")
        .as_ref()
        .map(|process| process.debug_port)
}

fn suppress_browser_window(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(target_os = "windows"))]
    let _ = command;
}

async fn capture_playback_media_from_browser(
    websocket_url: &str,
    video_id: &str,
    itag: u64,
    content_length: Option<u64>,
    cancellation: &CancellationToken,
    mode: BrowserCaptureMode,
    #[cfg(feature = "private-capture")] private_evidence: Option<&BrowserPrivateEvidence>,
) -> Result<BrowserMediaCapture> {
    let cdp_started = Instant::now();
    let (socket, _) = if let Ok(connection) = tokio_tungstenite::connect_async(websocket_url).await
    {
        browser_stage(
            "youtube_browser_cdp_connect",
            Some(cdp_started.elapsed()),
            Some(crate::observability::OperationOutcome::Success),
            "websocket",
            "connected",
            None,
        );
        connection
    } else {
        browser_stage(
            "youtube_browser_cdp_connect",
            Some(cdp_started.elapsed()),
            Some(crate::observability::OperationOutcome::Error),
            "websocket",
            "connect_failed",
            Some("network"),
        );
        return Err(anyhow::anyhow!(
            "connect to the dedicated YouTube playback browser"
        ));
    };
    let mut dispatcher = DevtoolsDispatcher::new(socket);
    let mut target_id = None;
    let result = async {
        let target = send_devtools_command(
            &mut dispatcher,
            1,
            "Target.createTarget",
            serde_json::json!({ "url": "about:blank" }),
            None,
        )
        .await?;
        let created_target_id = target
            .get("targetId")
            .and_then(serde_json::Value::as_str)
            .context("dedicated browser returned no playback target")?
            .to_string();
        target_id = Some(created_target_id.clone());
        let attached = send_devtools_command(
            &mut dispatcher,
            2,
            "Target.attachToTarget",
            serde_json::json!({ "targetId": &created_target_id, "flatten": true }),
            None,
        )
        .await?;
        let session_id = attached
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .context("dedicated browser returned no playback session")?
            .to_string();
        browser_stage(
            "youtube_browser_cdp_session",
            Some(Duration::ZERO),
            Some(crate::observability::OperationOutcome::Success),
            "target",
            "attached",
            None,
        );
        send_devtools_command(
            &mut dispatcher,
            3,
            "Network.enable",
            serde_json::json!({}),
            Some(&session_id),
        )
        .await?;
        let browser_user_agent = browser_user_agent(&mut dispatcher).await;
        send_devtools_command(
            &mut dispatcher,
            4,
            "Network.setUserAgentOverride",
            serde_json::json!({
                "userAgent": browser_user_agent,
                "platform": browser_user_agent_platform(),
            }),
            Some(&session_id),
        )
        .await?;
        let mut page_url = reqwest::Url::parse(MUSIC_URL).expect("static YouTube Music URL");
        page_url.set_path("watch");
        page_url.query_pairs_mut().append_pair("v", video_id);
        send_devtools_command(
            &mut dispatcher,
            5,
            "Page.navigate",
            serde_json::json!({ "url": page_url.as_str() }),
            Some(&session_id),
        )
        .await?;
        browser_stage(
            "youtube_browser_navigation",
            Some(Duration::ZERO),
            Some(crate::observability::OperationOutcome::Success),
            "watch",
            "requested",
            None,
        );

        match tokio::time::timeout(
            PLAYBACK_CAPTURE_TIMEOUT,
            wait_for_browser_media_request(
                &mut dispatcher,
                &session_id,
                itag,
                content_length,
                cancellation,
                mode,
                #[cfg(feature = "private-capture")]
                private_evidence,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                browser_stage(
                    "youtube_browser_media",
                    Some(PLAYBACK_CAPTURE_TIMEOUT),
                    Some(crate::observability::OperationOutcome::Timeout),
                    mode.label(),
                    "timeout",
                    Some("network"),
                );
                Err(anyhow::Error::new(err).context(
                    "timed out waiting for the signed-in YouTube Music player to authorize audio",
                ))
            }
        }
    }
    .await;
    if let Some(target_id) = target_id {
        let _ = send_devtools_command(
            &mut dispatcher,
            7,
            "Target.closeTarget",
            serde_json::json!({ "targetId": target_id }),
            None,
        )
        .await;
    }
    let (dropped_replies, dropped_events) = dispatcher.dropped_messages();
    #[cfg(feature = "private-capture")]
    note_dispatcher_overflow(private_evidence, dropped_replies, dropped_events);
    if dropped_replies != 0 || dropped_events != 0 {
        tracing::debug!(
            dropped_replies,
            dropped_events,
            "Dedicated browser control dispatcher reached a bounded queue"
        );
    }
    result
}

async fn browser_user_agent(dispatcher: &mut DevtoolsDispatcher) -> String {
    let Ok(version) = send_devtools_command(
        dispatcher,
        8,
        "Browser.getVersion",
        serde_json::json!({}),
        None,
    )
    .await
    else {
        return BROWSER_MEDIA_USER_AGENT.to_owned();
    };
    let Some(user_agent) = version.get("userAgent").and_then(serde_json::Value::as_str) else {
        return BROWSER_MEDIA_USER_AGENT.to_owned();
    };
    let user_agent = user_agent.replace("HeadlessChrome/", "Chrome/");
    if user_agent.contains("Chrome/") {
        user_agent
    } else {
        BROWSER_MEDIA_USER_AGENT.to_owned()
    }
}

#[cfg(feature = "private-capture")]
fn note_dispatcher_overflow(
    evidence: Option<&BrowserPrivateEvidence>,
    dropped_replies: u64,
    dropped_events: u64,
) {
    if dropped_replies == 0 && dropped_events == 0 {
        return;
    }
    if let Some(evidence) = evidence {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::QueueCapacity);
    }
}

#[cfg(feature = "private-capture")]
fn browser_player_request_matches(method: &str, url: &reqwest::Url) -> bool {
    method == "POST"
        && url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("music.youtube.com" | "www.youtube.com" | "youtube.com")
        )
        && url.path() == "/youtubei/v1/player"
}

#[cfg(feature = "private-capture")]
impl BrowserPlayerExchangeTracker {
    fn queue_extra_headers(&mut self, request_id: &str, headers: &serde_json::Value) {
        if request_id.len() > MAX_DEVTOOLS_REQUEST_ID_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let Some(headers) = devtools_headers_bounded(headers) else {
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.request_id == request_id)
        {
            if !extend_header_map_bounded(pending.request.headers_mut(), &headers) {
                pending.request_complete = false;
                self.dropped = self.dropped.saturating_add(1);
            }
            return;
        }
        if self.pending_extra_headers.len() == BROWSER_PLAYER_EXCHANGE_CAPACITY * 2 {
            self.pending_extra_headers.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.pending_extra_headers
            .push_back((request_id.to_owned(), headers));
    }

    fn observe_request(&mut self, event: &serde_json::Value, body_limit: usize) {
        let Some(request_id) = event
            .pointer("/params/requestId")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        if request_id.len() > MAX_DEVTOOLS_REQUEST_ID_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let Some(method) = event
            .pointer("/params/request/method")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        if method.len() > 16 {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let Some(raw_url) = event
            .pointer("/params/request/url")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        if raw_url.len() > MAX_DEVTOOLS_URL_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let Ok(url) = reqwest::Url::parse(raw_url) else {
            return;
        };
        if !browser_player_request_matches(method, &url) {
            return;
        }
        let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
            return;
        };
        let mut request = reqwest::Request::new(method, url);
        let mut request_complete = true;
        if let Some(headers) = event.pointer("/params/request/headers") {
            if let Some(headers) = devtools_headers_bounded(headers) {
                if !extend_header_map_bounded(request.headers_mut(), &headers) {
                    request_complete = false;
                    self.dropped = self.dropped.saturating_add(1);
                }
            } else {
                request_complete = false;
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        if let Some(position) = self
            .pending_extra_headers
            .iter()
            .position(|(pending_id, _)| pending_id == request_id)
        {
            if let Some((_, headers)) = self.pending_extra_headers.remove(position) {
                if !extend_header_map_bounded(request.headers_mut(), &headers) {
                    request_complete = false;
                    self.dropped = self.dropped.saturating_add(1);
                }
            }
        }
        if let Some(body) = event
            .pointer("/params/request/postData")
            .and_then(serde_json::Value::as_str)
        {
            if body.len() <= body_limit {
                *request.body_mut() = Some(reqwest::Body::from(body.to_owned()));
            } else {
                request_complete = false;
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        let redirected = event.pointer("/params/redirectResponse").is_some();
        if let Some(position) = self
            .pending
            .iter()
            .position(|pending| pending.request_id == request_id)
        {
            self.pending.remove(position);
            self.dropped = self.dropped.saturating_add(1);
        }
        if self.pending.len() == BROWSER_PLAYER_EXCHANGE_CAPACITY {
            self.pending.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.observed = self.observed.saturating_add(1);
        self.pending.push_back(PendingBrowserPlayerExchange {
            request_id: request_id.to_owned(),
            request,
            request_complete,
            response_status: None,
            response_url: None,
            response_headers: reqwest::header::HeaderMap::new(),
            response_complete: true,
            started_at: Instant::now(),
            attempt: u8::try_from(self.observed).unwrap_or(u8::MAX),
            redirected,
            from_disk_cache: false,
            from_service_worker: false,
        });
    }

    fn observe_response(&mut self, event: &serde_json::Value) {
        let Some(request_id) = event
            .pointer("/params/requestId")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.request_id == request_id)
        else {
            return;
        };
        pending.response_status = event
            .pointer("/params/response/status")
            .and_then(serde_json::Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .and_then(|status| reqwest::StatusCode::from_u16(status).ok());
        pending.response_url = event
            .pointer("/params/response/url")
            .and_then(serde_json::Value::as_str)
            .filter(|url| url.len() <= MAX_DEVTOOLS_URL_BYTES)
            .and_then(|url| reqwest::Url::parse(url).ok());
        if event
            .pointer("/params/response/url")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|url| url.len() > MAX_DEVTOOLS_URL_BYTES)
        {
            pending.response_complete = false;
            self.dropped = self.dropped.saturating_add(1);
        }
        if let Some(headers) = event.pointer("/params/response/headers") {
            if let Some(headers) = devtools_headers_bounded(headers) {
                pending.response_headers = headers;
            } else {
                pending.response_complete = false;
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        pending.from_disk_cache = event
            .pointer("/params/response/fromDiskCache")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        pending.from_service_worker = event
            .pointer("/params/response/fromServiceWorker")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    }

    fn take(&mut self, request_id: &str) -> Option<PendingBrowserPlayerExchange> {
        let position = self
            .pending
            .iter()
            .position(|pending| pending.request_id == request_id)?;
        self.pending.remove(position)
    }
}

#[cfg(feature = "private-capture")]
fn record_browser_player_failure(
    evidence: &BrowserPrivateEvidence,
    exchange: &PendingBrowserPlayerExchange,
    category: &'static str,
) {
    record_browser_player_request(evidence, exchange);
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::STAGE,
            "browser_player",
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CATEGORY,
            category,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::REQUEST_ORDINAL,
            u64::from(exchange.attempt),
        ),
    ];
    if let Ok(payload) = crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::NetworkFailure,
        &fields,
    ) {
        let _ = evidence.capture.record_with_context(
            Some(evidence.exchange_ref),
            crate::developer_capture::EndpointRole::BrowserPlayer,
            crate::developer_capture::ProviderClientKind::WebRemix,
            crate::developer_capture::TransportKind::BrowserCdp,
            exchange.attempt,
            crate::developer_capture::CaptureRecordKind::NetworkFailure,
            payload,
        );
    } else {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
    }
}

#[cfg(feature = "private-capture")]
fn record_browser_player_request(
    evidence: &BrowserPrivateEvidence,
    exchange: &PendingBrowserPlayerExchange,
) {
    let limit = evidence
        .capture
        .body_limit(crate::developer_capture::CaptureRecordKind::HttpRequest);
    match crate::developer_capture::encode_private_http_request_bounded(&exchange.request, limit) {
        Ok((payload, complete)) => {
            if !complete || !exchange.request_complete {
                evidence
                    .capture
                    .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
            }
            evidence.capture.mark_credential_values_present();
            let _ = evidence.capture.record_with_context(
                Some(evidence.exchange_ref),
                crate::developer_capture::EndpointRole::BrowserPlayer,
                crate::developer_capture::ProviderClientKind::WebRemix,
                crate::developer_capture::TransportKind::BrowserCdp,
                exchange.attempt,
                crate::developer_capture::CaptureRecordKind::HttpRequest,
                payload,
            );
        }
        Err(_) => {
            evidence
                .capture
                .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
fn record_browser_player_response(
    evidence: &BrowserPrivateEvidence,
    exchange: &PendingBrowserPlayerExchange,
    body: &[u8],
    body_complete: bool,
) {
    record_browser_player_request(evidence, exchange);
    let (Some(status), Some(url)) = (exchange.response_status, exchange.response_url.as_ref())
    else {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        return;
    };
    let limit = evidence
        .capture
        .body_limit(crate::developer_capture::CaptureRecordKind::HttpResponse);
    match crate::developer_capture::encode_private_http_response_bounded(
        status,
        url,
        &exchange.response_headers,
        body,
        exchange.started_at.elapsed(),
        body_complete && exchange.response_complete,
        exchange.redirected,
        limit,
    ) {
        Ok((payload, complete)) => {
            if !complete {
                evidence
                    .capture
                    .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
            }
            evidence.capture.mark_credential_values_present();
            let _ = evidence.capture.record_with_context(
                Some(evidence.exchange_ref),
                crate::developer_capture::EndpointRole::BrowserPlayer,
                crate::developer_capture::ProviderClientKind::WebRemix,
                crate::developer_capture::TransportKind::BrowserCdp,
                exchange.attempt,
                crate::developer_capture::CaptureRecordKind::HttpResponse,
                payload,
            );
            let fields = [
                crate::developer_capture::PrivateField::u64(
                    crate::developer_capture::private_field::REQUEST_ORDINAL,
                    u64::from(exchange.attempt),
                ),
                crate::developer_capture::PrivateField::boolean(
                    crate::developer_capture::private_field::FROM_DISK_CACHE,
                    exchange.from_disk_cache,
                ),
                crate::developer_capture::PrivateField::boolean(
                    crate::developer_capture::private_field::FROM_SERVICE_WORKER,
                    exchange.from_service_worker,
                ),
            ];
            if let Ok(payload) = crate::developer_capture::encode_private_fields(
                crate::developer_capture::PrivatePayloadKind::BrowserExchange,
                &fields,
            ) {
                let _ = evidence.capture.record_with_context(
                    Some(evidence.exchange_ref),
                    crate::developer_capture::EndpointRole::BrowserPlayer,
                    crate::developer_capture::ProviderClientKind::WebRemix,
                    crate::developer_capture::TransportKind::BrowserCdp,
                    exchange.attempt,
                    crate::developer_capture::CaptureRecordKind::BrowserExchange,
                    payload,
                );
            }
        }
        Err(_) => {
            evidence
                .capture
                .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
fn observe_browser_player_event(
    tracker: &mut BrowserPlayerExchangeTracker,
    event: &serde_json::Value,
    evidence: &BrowserPrivateEvidence,
) {
    let method = event.get("method").and_then(serde_json::Value::as_str);
    let request_id = event
        .pointer("/params/requestId")
        .and_then(serde_json::Value::as_str);
    match method {
        Some("Network.requestWillBeSentExtraInfo") => {
            if let (Some(request_id), Some(headers)) =
                (request_id, event.pointer("/params/headers"))
            {
                tracker.queue_extra_headers(request_id, headers);
            }
        }
        Some("Network.requestWillBeSent") => tracker.observe_request(
            event,
            evidence
                .capture
                .body_limit(crate::developer_capture::CaptureRecordKind::HttpRequest),
        ),
        Some("Network.responseReceived") => tracker.observe_response(event),
        Some("Network.loadingFinished") => {
            let Some(request_id) = request_id else {
                return;
            };
            let Some(exchange) = tracker.take(request_id) else {
                return;
            };
            // Request-only playback must return on the media request itself. Fetching a
            // response body here would synchronously delay that event, so retain the
            // response metadata already present in CDP and mark the body incomplete.
            record_browser_player_response(evidence, &exchange, &[], false);
        }
        Some("Network.loadingFailed") => {
            let Some(request_id) = request_id else {
                return;
            };
            if let Some(exchange) = tracker.take(request_id) {
                record_browser_player_failure(evidence, &exchange, "loading_failed");
            }
        }
        _ => {}
    }
    if tracker.dropped != 0 {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::QueueCapacity);
    }
}

#[cfg(feature = "private-capture")]
fn flush_pending_browser_players(
    tracker: &mut BrowserPlayerExchangeTracker,
    evidence: &BrowserPrivateEvidence,
) {
    while let Some(exchange) = tracker.pending.pop_front() {
        if exchange.response_status.is_some() {
            record_browser_player_response(evidence, &exchange, &[], false);
        } else {
            record_browser_player_failure(evidence, &exchange, "capture_ended");
        }
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
    }
}

#[cfg(feature = "private-capture")]
fn record_browser_media_evidence(
    evidence: &BrowserPrivateEvidence,
    capture: &BrowserMediaCapture,
    attempt: u8,
) {
    if capture.original_url.as_str().len() > MAX_DEVTOOLS_URL_BYTES
        || !private_headers_within_limits(&capture.request_headers)
    {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        return;
    }
    let Ok(headers) = crate::developer_capture::encode_private_headers(&capture.request_headers)
    else {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        return;
    };
    let mut fields = vec![
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::METHOD,
            "GET",
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::URL,
            capture.original_url.as_str(),
        ),
        crate::developer_capture::PrivateField::bytes(
            crate::developer_capture::private_field::HEADERS,
            &headers,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::REDIRECT_COUNT,
            u64::from(capture.redirect_count),
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::FROM_DISK_CACHE,
            capture.from_disk_cache,
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::FROM_SERVICE_WORKER,
            capture.from_service_worker,
        ),
    ];
    if let Some(status) = capture.response_status {
        fields.push(crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::STATUS,
            u64::from(status),
        ));
    }
    match crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::MediaProbe,
        &fields,
    ) {
        Ok(payload) => {
            evidence.capture.mark_credential_values_present();
            let _ = evidence.capture.record_with_context(
                Some(evidence.exchange_ref),
                crate::developer_capture::EndpointRole::BrowserMedia,
                crate::developer_capture::ProviderClientKind::WebRemix,
                crate::developer_capture::TransportKind::BrowserCdp,
                attempt,
                crate::developer_capture::CaptureRecordKind::MediaProbe,
                payload,
            );
        }
        Err(_) => {
            evidence
                .capture
                .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
fn record_browser_media_failure(evidence: &BrowserPrivateEvidence, attempt: u8) {
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::STAGE,
            "browser_media",
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CATEGORY,
            "loading_failed",
        ),
    ];
    if let Ok(payload) = crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::NetworkFailure,
        &fields,
    ) {
        let _ = evidence.capture.record_with_context(
            Some(evidence.exchange_ref),
            crate::developer_capture::EndpointRole::BrowserMedia,
            crate::developer_capture::ProviderClientKind::WebRemix,
            crate::developer_capture::TransportKind::BrowserCdp,
            attempt,
            crate::developer_capture::CaptureRecordKind::NetworkFailure,
            payload,
        );
    }
}

async fn wait_for_browser_media_request(
    dispatcher: &mut DevtoolsDispatcher,
    session_id: &str,
    itag: u64,
    content_length: Option<u64>,
    cancellation: &CancellationToken,
    mode: BrowserCaptureMode,
    #[cfg(feature = "private-capture")] private_evidence: Option<&BrowserPrivateEvidence>,
) -> Result<BrowserMediaCapture> {
    let nudge_at = tokio::time::Instant::now() + Duration::from_secs(7);
    let mut nudged = false;
    let mut pending_extra_headers = VecDeque::<(String, serde_json::Value)>::new();
    let mut capture: Option<(String, reqwest::Url, reqwest::header::HeaderMap, u8)> = None;
    #[cfg(feature = "private-capture")]
    let mut player_tracker = BrowserPlayerExchangeTracker::default();
    #[cfg(feature = "private-capture")]
    let private_headers_required = private_evidence.is_some();
    #[cfg(not(feature = "private-capture"))]
    let private_headers_required = false;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => {
                browser_stage(
                    "youtube_browser_media",
                    Some(Duration::ZERO),
                    Some(crate::observability::OperationOutcome::Cancelled),
                    mode.label(),
                    "cancelled",
                    Some("cancelled"),
                );
                anyhow::bail!("YouTube browser playback capture was cancelled");
            }
            () = tokio::time::sleep_until(nudge_at), if !nudged => {
                nudged = true;
                let nudge_result = dispatcher
                    .send(
                        9,
                        "Runtime.evaluate",
                        serde_json::json!({
                        "expression": "(()=>{const v=document.querySelector('video');if(v){v.muted=true;void v.play();}})()"
                        }),
                        Some(session_id),
                    )
                    .await;
                match nudge_result {
                    Ok(()) => {
                        browser_stage(
                            "youtube_browser_nudge",
                            Some(Duration::from_secs(7)),
                            Some(crate::observability::OperationOutcome::Success),
                            mode.label(),
                            "sent",
                            None,
                        );
                    }
                    Err(error) => {
                        browser_stage(
                            "youtube_browser_nudge",
                            Some(Duration::from_secs(7)),
                            Some(if cancellation.is_cancelled() {
                                crate::observability::OperationOutcome::Cancelled
                            } else {
                                crate::observability::OperationOutcome::Error
                            }),
                            mode.label(),
                            if cancellation.is_cancelled() {
                                "cancelled"
                            } else {
                                "send_failed"
                            },
                            Some(if cancellation.is_cancelled() {
                                "cancelled"
                            } else {
                                "network"
                            }),
                        );
                        return Err(error.context("start muted playback in the dedicated browser"));
                    }
                }
            }
            event = dispatcher.next_event() => {
                let event = match event {
                    Ok(event) => event,
                    Err(error) => {
                        browser_stage(
                            "youtube_browser_media",
                            Some(Duration::ZERO),
                            Some(if cancellation.is_cancelled() {
                                crate::observability::OperationOutcome::Cancelled
                            } else {
                                crate::observability::OperationOutcome::Error
                            }),
                            mode.label(),
                            if cancellation.is_cancelled() {
                                "cancelled"
                            } else {
                                "event_read_failed"
                            },
                            Some(if cancellation.is_cancelled() {
                                "cancelled"
                            } else {
                                "network"
                            }),
                        );
                        return Err(error.context("read dedicated playback browser event"));
                    }
                };
                if event.get("sessionId").and_then(serde_json::Value::as_str) != Some(session_id) {
                    continue;
                }
                #[cfg(feature = "private-capture")]
                if let Some(private_evidence) = private_evidence {
                    observe_browser_player_event(
                        &mut player_tracker,
                        &event,
                        private_evidence,
                    );
                }
                let method = event.get("method").and_then(serde_json::Value::as_str);
                let request_id = event
                    .pointer("/params/requestId")
                    .and_then(serde_json::Value::as_str);
                match method {
                    Some("Network.requestWillBeSentExtraInfo") => {
                        if matches!(mode, BrowserCaptureMode::RequestOnly)
                            && !private_headers_required
                        {
                            continue;
                        }
                        let Some(request_id) = request_id else {
                            continue;
                        };
                        let Some(headers) = event.pointer("/params/headers") else {
                            continue;
                        };
                        if let Some((capture_id, _, request_headers, _)) = capture.as_mut() {
                            if capture_id == request_id {
                                extend_devtools_headers(request_headers, headers);
                            }
                        } else {
                            if pending_extra_headers.len() == 32 {
                                pending_extra_headers.pop_front();
                            }
                            pending_extra_headers
                                .push_back((request_id.to_string(), headers.clone()));
                        }
                    }
                    Some("Network.requestWillBeSent") if capture.is_none() => {
                        let Some(request_id) = request_id else {
                            continue;
                        };
                        let Some(raw_url) = event
                            .pointer("/params/request/url")
                            .and_then(serde_json::Value::as_str)
                        else {
                            continue;
                        };
                        let Ok(url) = reqwest::Url::parse(raw_url) else {
                            continue;
                        };
                        if !browser_media_request_matches(&url, itag, content_length) {
                            continue;
                        }
                        let mut request_headers = reqwest::header::HeaderMap::new();
                        if let Some(headers) = event.pointer("/params/request/headers") {
                            extend_devtools_headers(&mut request_headers, headers);
                        }
                        if let Some(position) = pending_extra_headers
                            .iter()
                            .position(|(pending_id, _)| pending_id == request_id)
                        {
                            let (_, headers) = pending_extra_headers
                                .remove(position)
                                .expect("located pending browser header entry");
                            extend_devtools_headers(&mut request_headers, &headers);
                        }
                        let redirect_count = u8::from(event.pointer("/params/redirectResponse").is_some());
                        if !mode.awaits_media_response() {
                            let result = match browser_media_capture(
                                url,
                                request_headers,
                                None,
                                redirect_count,
                                false,
                                false,
                            ) {
                                Ok(result) => result,
                                Err(error) => {
                                    browser_stage(
                                        "youtube_browser_media",
                                        Some(Duration::ZERO),
                                        Some(crate::observability::OperationOutcome::Error),
                                        mode.label(),
                                        "capture_rejected",
                                        Some("contract"),
                                    );
                                    return Err(error);
                                }
                            };
                            browser_stage(
                                "youtube_browser_media",
                                Some(Duration::ZERO),
                                Some(crate::observability::OperationOutcome::Success),
                                mode.label(),
                                "request_matched",
                                None,
                            );
                            #[cfg(feature = "private-capture")]
                            if let Some(private_evidence) = private_evidence {
                                flush_pending_browser_players(
                                    &mut player_tracker,
                                    private_evidence,
                                );
                                record_browser_media_evidence(private_evidence, &result, 1);
                            }
                            return Ok(result);
                        }
                        browser_stage(
                            "youtube_browser_media",
                            Some(Duration::ZERO),
                            Some(crate::observability::OperationOutcome::Success),
                            mode.label(),
                            "request_matched",
                            None,
                        );
                        capture = Some((request_id.to_string(), url, request_headers, redirect_count));
                    }
                    Some("Network.responseReceived") => {
                        let Some((capture_id, _, _, _)) = capture.as_ref() else {
                            continue;
                        };
                        if request_id != Some(capture_id.as_str()) {
                            continue;
                        }
                        let status = event
                            .pointer("/params/response/status")
                            .and_then(serde_json::Value::as_u64)
                            .and_then(|status| u16::try_from(status).ok())
                            .context("dedicated browser media response had no HTTP status")?;
                        let from_disk_cache = event
                            .pointer("/params/response/fromDiskCache")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let from_service_worker = event
                            .pointer("/params/response/fromServiceWorker")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let (_, url, request_headers, redirect_count) = capture.take().unwrap();
                        let result = match browser_media_capture(
                            url,
                            request_headers,
                            Some(status),
                            redirect_count,
                            from_disk_cache,
                            from_service_worker,
                        ) {
                            Ok(result) => result,
                            Err(error) => {
                                browser_stage(
                                    "youtube_browser_media",
                                    Some(Duration::ZERO),
                                    Some(crate::observability::OperationOutcome::Error),
                                    mode.label(),
                                    "capture_rejected",
                                    Some("contract"),
                                );
                                return Err(error);
                            }
                        };
                        browser_stage(
                            "youtube_browser_media",
                            Some(Duration::ZERO),
                            Some(crate::observability::OperationOutcome::Success),
                            mode.label(),
                            "response_received",
                            None,
                        );
                        #[cfg(feature = "private-capture")]
                        if let Some(private_evidence) = private_evidence {
                            flush_pending_browser_players(&mut player_tracker, private_evidence);
                            record_browser_media_evidence(private_evidence, &result, 1);
                        }
                        return Ok(result);
                    }
                    Some("Network.loadingFailed")
                        if capture.as_ref().is_some_and(|(capture_id, _, _, _)| {
                            request_id == Some(capture_id.as_str())
                        }) =>
                    {
                        #[cfg(feature = "private-capture")]
                        if let Some(private_evidence) = private_evidence {
                            if let Some((_, url, request_headers, redirect_count)) = capture.take() {
                                if let Ok(result) = browser_media_capture(
                                    url,
                                    request_headers,
                                    None,
                                    redirect_count,
                                    false,
                                    false,
                                ) {
                                    flush_pending_browser_players(
                                        &mut player_tracker,
                                        private_evidence,
                                    );
                                    record_browser_media_evidence(private_evidence, &result, 1);
                                }
                            }
                            record_browser_media_failure(private_evidence, 1);
                        }
                        browser_stage(
                            "youtube_browser_media",
                            Some(Duration::ZERO),
                            Some(crate::observability::OperationOutcome::Error),
                            mode.label(),
                            "loading_failed",
                            Some("network"),
                        );
                        anyhow::bail!(
                            "dedicated browser media request failed before an HTTP response"
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

fn browser_media_capture(
    original_url: reqwest::Url,
    request_headers: reqwest::header::HeaderMap,
    response_status: Option<u16>,
    redirect_count: u8,
    from_disk_cache: bool,
    from_service_worker: bool,
) -> Result<BrowserMediaCapture> {
    let sanitized_url = sanitize_browser_media_url(original_url.clone())?;
    Ok(BrowserMediaCapture {
        original_url,
        sanitized_url,
        request_headers,
        response_status,
        redirect_count,
        from_disk_cache,
        from_service_worker,
    })
}

#[cfg(feature = "private-capture")]
fn devtools_headers_bounded(headers: &serde_json::Value) -> Option<reqwest::header::HeaderMap> {
    let Some(headers) = headers.as_object() else {
        return Some(reqwest::header::HeaderMap::new());
    };
    if headers.len() > MAX_DEVTOOLS_HEADER_COUNT {
        return None;
    }
    let mut total_bytes = 0_usize;
    let mut decoded = reqwest::header::HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let Some(value) = value.as_str() else {
            continue;
        };
        total_bytes = total_bytes
            .checked_add(name.len())?
            .checked_add(value.len())?;
        if total_bytes > MAX_DEVTOOLS_HEADER_BYTES {
            return None;
        }
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(value) else {
            continue;
        };
        decoded.insert(name, value);
    }
    Some(decoded)
}

fn extend_devtools_headers(target: &mut reqwest::header::HeaderMap, headers: &serde_json::Value) {
    let Some(headers) = headers.as_object() else {
        return;
    };
    for (name, value) in headers {
        let Some(value) = value.as_str() else {
            continue;
        };
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(value) else {
            continue;
        };
        target.insert(name, value);
    }
}

#[cfg(feature = "private-capture")]
fn extend_header_map_bounded(
    target: &mut reqwest::header::HeaderMap,
    headers: &reqwest::header::HeaderMap,
) -> bool {
    let Some((target_count, target_bytes)) = private_header_size(target) else {
        return false;
    };
    let Some((additional_count, additional_bytes)) = private_header_size(headers) else {
        return false;
    };
    let Some(combined_count) = target_count.checked_add(additional_count) else {
        return false;
    };
    let Some(combined_bytes) = target_bytes.checked_add(additional_bytes) else {
        return false;
    };
    if combined_count > MAX_DEVTOOLS_HEADER_COUNT || combined_bytes > MAX_DEVTOOLS_HEADER_BYTES {
        return false;
    }
    for (name, value) in headers {
        target.insert(name, value.clone());
    }
    true
}

#[cfg(feature = "private-capture")]
fn private_headers_within_limits(headers: &reqwest::header::HeaderMap) -> bool {
    private_header_size(headers).is_some_and(|(count, bytes)| {
        count <= MAX_DEVTOOLS_HEADER_COUNT && bytes <= MAX_DEVTOOLS_HEADER_BYTES
    })
}

#[cfg(feature = "private-capture")]
fn private_header_size(headers: &reqwest::header::HeaderMap) -> Option<(usize, usize)> {
    let mut bytes = 0_usize;
    for (name, value) in headers {
        bytes = bytes
            .checked_add(name.as_str().len())?
            .checked_add(value.as_bytes().len())?;
    }
    Some((headers.len(), bytes))
}

const fn browser_user_agent_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    }
}

fn browser_media_request_matches(
    url: &reqwest::Url,
    itag: u64,
    content_length: Option<u64>,
) -> bool {
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !(host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
    {
        return false;
    }
    let query = url.query_pairs().collect::<BTreeMap<_, _>>();
    let is_mp4_audio = query
        .get("mime")
        .is_some_and(|value| value.starts_with("audio/mp4"));
    if !is_mp4_audio {
        return false;
    }
    let is_expected_format = query.get("itag").and_then(|value| value.parse().ok()) == Some(itag)
        && content_length.is_none_or(|expected| {
            query.get("clen").and_then(|value| value.parse().ok()) == Some(expected)
        });
    // The browser is free to choose another AAC itag than the Innertube
    // response's preferred target. Capture any MP4/AAC request rather than
    // waiting forever for an itag the browser will never request.
    is_expected_format
        || query
            .get("itag")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some()
}

fn sanitize_browser_media_url(mut url: reqwest::Url) -> Result<reqwest::Url> {
    let retained = url
        .query_pairs()
        .filter(|(key, _)| !matches!(key.as_ref(), "ump" | "srfvp" | "range" | "rn" | "rbuf"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.query_pairs_mut().clear().extend_pairs(retained);
    anyhow::ensure!(
        url.query_pairs().any(|(key, _)| key == "pot"),
        "the signed-in YouTube player returned audio without a playback-origin token"
    );
    Ok(url)
}

async fn terminate_browser(child: &mut Child) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        browser_capture_outcome, browser_media_capture, browser_media_request_matches,
        capture_playback_url, devtools_command_result, extend_devtools_headers,
        fresh_account_profile_path, playback_browser_debug_port, playback_browser_process_id,
        profile_path, promote_profile, sanitize_browser_media_url, shutdown_playback_browser,
        shutdown_playback_browser_now, signed_in_cookie_header, BrowserCaptureMode, DevtoolsCookie,
        DevtoolsInbox, DevtoolsTarget, DEVTOOLS_EVENT_CAPACITY, DEVTOOLS_PENDING_REPLY_CAPACITY,
        NEW_ACCOUNT_PROFILE_PREFIX, PLAYBACK_BROWSER_IDLE_TIMEOUT, PLAYBACK_BROWSER_LAUNCHES,
        PLAYBACK_CAPTURE_TIMEOUT,
    };
    #[cfg(feature = "private-capture")]
    use super::{
        browser_player_request_matches, devtools_headers_bounded, extend_header_map_bounded,
        note_dispatcher_overflow, observe_browser_player_event, private_headers_within_limits,
        BrowserPlayerExchangeTracker, BrowserPrivateEvidence, BROWSER_PLAYER_EXCHANGE_CAPACITY,
        MAX_DEVTOOLS_HEADER_BYTES, MAX_DEVTOOLS_HEADER_COUNT, MAX_DEVTOOLS_REQUEST_ID_BYTES,
    };

    #[test]
    fn welcome_browser_candidates_include_helium() {
        assert!(super::platform_browser_candidates().contains(&std::path::PathBuf::from("helium")));
    }

    #[test]
    fn welcome_browser_override_is_validated_and_remembered() {
        let folder = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let selected = super::save_browser_choice(folder.path(), executable.clone()).unwrap();
        assert_eq!(selected, executable);
        assert_eq!(
            super::resolve_browser_executable(folder.path(), None).unwrap(),
            executable
        );
        assert!(super::resolve_browser_executable(
            folder.path(),
            Some(folder.path().join("missing-browser"))
        )
        .is_err());
        #[cfg(unix)]
        {
            let not_executable = folder.path().join("not-executable");
            std::fs::write(&not_executable, "text").unwrap();
            assert!(super::save_browser_choice(folder.path(), not_executable).is_err());
            assert_eq!(
                super::resolve_browser_executable(folder.path(), None).unwrap(),
                executable
            );
        }
    }

    fn cookie(name: &str, value: &str, domain: &str) -> DevtoolsCookie {
        DevtoolsCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
        }
    }

    #[test]
    fn browser_capture_outcome_distinguishes_success_timeout_and_cancellation() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let success = browser_media_capture(
            reqwest::Url::parse(
                "https://r1.googlevideo.com/videoplayback?itag=140&mime=audio%2Fmp4&pot=proof",
            )
            .unwrap(),
            reqwest::header::HeaderMap::new(),
            Some(200),
            0,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            browser_capture_outcome(&cancellation, &Ok(success), Duration::ZERO),
            (
                crate::observability::OperationOutcome::Success,
                "media_captured"
            )
        );
        assert_eq!(
            browser_capture_outcome(
                &cancellation,
                &Err(anyhow::anyhow!("fixture timeout")),
                PLAYBACK_CAPTURE_TIMEOUT,
            ),
            (crate::observability::OperationOutcome::Timeout, "timeout")
        );

        cancellation.cancel();
        assert_eq!(
            browser_capture_outcome(
                &cancellation,
                &Err(anyhow::anyhow!("fixture cancellation")),
                Duration::ZERO,
            ),
            (
                crate::observability::OperationOutcome::Cancelled,
                "cancelled"
            )
        );
    }

    #[test]
    fn browser_capture_modes_have_stable_diagnostic_labels() {
        assert_eq!(BrowserCaptureMode::RequestOnly.label(), "request_only");
        assert_eq!(BrowserCaptureMode::AwaitResponse.label(), "await_response");
    }

    #[test]
    fn devtools_inbox_retains_interleaved_events_and_out_of_order_replies() {
        let mut inbox = DevtoolsInbox::default();
        let event = serde_json::json!({
            "method": "Network.requestWillBeSent",
            "sessionId": "fixture-session",
            "params": { "requestId": "fixture-request" }
        });
        inbox.route(serde_json::json!({ "id": 41, "result": { "first": true } }));
        inbox.route(event.clone());
        inbox.route(serde_json::json!({ "id": 42, "result": { "second": true } }));

        assert_eq!(
            inbox.take_reply(42).unwrap()["result"]["second"],
            serde_json::Value::Bool(true)
        );
        assert_eq!(inbox.take_event(), Some(event));
        assert_eq!(
            inbox.take_reply(41).unwrap()["result"]["first"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn devtools_inbox_bounds_replies_and_events() {
        let mut inbox = DevtoolsInbox::default();
        for id in 0..u64::try_from(DEVTOOLS_PENDING_REPLY_CAPACITY + 3).unwrap() {
            inbox.route(serde_json::json!({ "id": id, "result": {} }));
        }
        for ordinal in 0..DEVTOOLS_EVENT_CAPACITY + 5 {
            inbox.route(serde_json::json!({
                "method": "Network.fixture",
                "params": { "ordinal": ordinal }
            }));
        }

        assert_eq!(inbox.replies.len(), DEVTOOLS_PENDING_REPLY_CAPACITY);
        assert_eq!(inbox.events.len(), DEVTOOLS_EVENT_CAPACITY);
        assert_eq!(inbox.dropped_messages(), (3, 5));
        assert!(inbox.take_reply(0).is_none());
        assert_eq!(
            inbox.take_event().unwrap()["params"]["ordinal"],
            serde_json::json!(5)
        );
    }

    #[test]
    fn request_only_mode_never_waits_for_a_media_response() {
        assert!(!BrowserCaptureMode::RequestOnly.awaits_media_response());
        assert!(BrowserCaptureMode::AwaitResponse.awaits_media_response());
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn dispatcher_overflow_marks_the_active_capture_incomplete() {
        use crate::developer_capture::{
            CaptureCompleteness, CaptureLimits, CapturePassphrase, CapturePurpose, ExchangeRef,
            SafeOperationRef,
        };

        let directory = tempfile::tempdir().unwrap();
        let (handle, _worker, _) = crate::developer_capture::prepare_runtime(
            &directory.path().join("vault"),
            CaptureLimits::default(),
        )
        .unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(
                CapturePassphrase::new("dispatcher overflow fixture passphrase".to_owned())
                    .unwrap(),
            )
            .unwrap();
        let capture = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([21; 4]),
            )
            .unwrap()
            .unwrap();
        let evidence = BrowserPrivateEvidence {
            capture,
            exchange_ref: ExchangeRef::from_bytes([22; 8]),
        };

        note_dispatcher_overflow(Some(&evidence), 0, 0);
        assert_eq!(handle.snapshot().completeness, CaptureCompleteness::Pending);
        note_dispatcher_overflow(Some(&evidence), 0, 1);
        assert_eq!(
            handle.snapshot().completeness,
            CaptureCompleteness::Incomplete
        );
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn completed_player_event_uses_available_metadata_without_a_body_command() {
        use crate::developer_capture::{
            CaptureCompleteness, CaptureLimits, CapturePassphrase, CapturePurpose, ExchangeRef,
            SafeOperationRef,
        };

        let directory = tempfile::tempdir().unwrap();
        let (handle, _worker, _) = crate::developer_capture::prepare_runtime(
            &directory.path().join("vault"),
            CaptureLimits::default(),
        )
        .unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(
                CapturePassphrase::new("metadata-only player fixture passphrase".to_owned())
                    .unwrap(),
            )
            .unwrap();
        let evidence = BrowserPrivateEvidence {
            capture: handle
                .claim(
                    CapturePurpose::InteractivePlayback,
                    SafeOperationRef::from_bytes([23; 4]),
                )
                .unwrap()
                .unwrap(),
            exchange_ref: ExchangeRef::from_bytes([24; 8]),
        };
        let mut tracker = BrowserPlayerExchangeTracker::default();
        let request_id = "player-fixture";
        observe_browser_player_event(
            &mut tracker,
            &serde_json::json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": request_id,
                    "request": {
                        "method": "POST",
                        "url": "https://music.youtube.com/youtubei/v1/player?key=fixture",
                        "headers": { "Content-Type": "application/json" },
                        "postData": "{\"videoId\":\"private-fixture\"}"
                    }
                }
            }),
            &evidence,
        );
        observe_browser_player_event(
            &mut tracker,
            &serde_json::json!({
                "method": "Network.responseReceived",
                "params": {
                    "requestId": request_id,
                    "response": {
                        "status": 200,
                        "url": "https://music.youtube.com/youtubei/v1/player?key=fixture",
                        "headers": { "Content-Type": "application/json" }
                    }
                }
            }),
            &evidence,
        );
        observe_browser_player_event(
            &mut tracker,
            &serde_json::json!({
                "method": "Network.loadingFinished",
                "params": { "requestId": request_id }
            }),
            &evidence,
        );

        assert!(tracker.pending.is_empty());
        assert_eq!(
            handle.snapshot().completeness,
            CaptureCompleteness::Incomplete
        );
    }

    #[test]
    fn devtools_command_errors_never_expose_browser_payloads() {
        let private_error = "fixture-private-cdp-error";
        let error = devtools_command_result(
            "Network.enable",
            &serde_json::json!({
                "id": 7,
                "error": { "code": -32000, "message": private_error }
            }),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "dedicated browser refused Network.enable"
        );
        assert!(!error.to_string().contains(private_error));
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn browser_player_tracker_matches_only_exact_read_only_player_posts() {
        let player = reqwest::Url::parse(
            "https://music.youtube.com/youtubei/v1/player?key=fixture-private-key",
        )
        .unwrap();
        let unrelated = reqwest::Url::parse(
            "https://music.youtube.com/youtubei/v1/browse?key=fixture-private-key",
        )
        .unwrap();

        assert!(browser_player_request_matches("POST", &player));
        assert!(!browser_player_request_matches("GET", &player));
        assert!(!browser_player_request_matches("POST", &unrelated));
        assert!(!browser_player_request_matches(
            "POST",
            &reqwest::Url::parse("https://example.invalid/youtubei/v1/player").unwrap()
        ));
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn browser_player_tracker_bounds_several_correlated_calls_and_headers() {
        let mut tracker = BrowserPlayerExchangeTracker::default();
        for ordinal in 0..BROWSER_PLAYER_EXCHANGE_CAPACITY + 3 {
            let request_id = format!("fixture-{ordinal}");
            tracker.queue_extra_headers(
                &request_id,
                &serde_json::json!({ "Cookie": format!("private-cookie-{ordinal}") }),
            );
            tracker.observe_request(
                &serde_json::json!({
                    "params": {
                        "requestId": request_id,
                        "request": {
                            "method": "POST",
                            "url": "https://music.youtube.com/youtubei/v1/player?key=fixture",
                            "headers": { "Content-Type": "application/json" },
                            "postData": format!("{{\"videoId\":\"private-{ordinal}\"}}")
                        }
                    }
                }),
                1024,
            );
        }

        assert_eq!(tracker.pending.len(), BROWSER_PLAYER_EXCHANGE_CAPACITY);
        assert_eq!(tracker.dropped, 3);
        let retained = tracker.pending.front().unwrap();
        assert_eq!(retained.attempt, 4);
        assert!(retained
            .request
            .headers()
            .contains_key(reqwest::header::COOKIE));

        let request_id = retained.request_id.clone();
        tracker.observe_response(&serde_json::json!({
            "params": {
                "requestId": request_id,
                "response": {
                    "status": 403,
                    "url": "https://music.youtube.com/youtubei/v1/player?key=fixture",
                    "headers": { "Content-Type": "application/json" },
                    "fromDiskCache": true,
                    "fromServiceWorker": false
                }
            }
        }));
        let retained = tracker.take(&request_id).unwrap();
        assert_eq!(
            retained.response_status,
            Some(reqwest::StatusCode::FORBIDDEN)
        );
        assert!(retained.from_disk_cache);
        assert!(!retained.from_service_worker);
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn browser_player_tracker_rejects_unbounded_cdp_fields_before_cloning() {
        let mut tracker = BrowserPlayerExchangeTracker::default();
        let oversized_body = "x".repeat(1025);
        tracker.observe_request(
            &serde_json::json!({
                "params": {
                    "requestId": "fixture-bounded",
                    "request": {
                        "method": "POST",
                        "url": "https://music.youtube.com/youtubei/v1/player",
                        "headers": { "Content-Type": "application/json" },
                        "postData": oversized_body
                    }
                }
            }),
            1024,
        );

        let retained = tracker.pending.front().unwrap();
        assert!(!retained.request_complete);
        assert!(retained.request.body().is_none());
        assert_eq!(tracker.dropped, 1);

        let oversized_id = "x".repeat(MAX_DEVTOOLS_REQUEST_ID_BYTES + 1);
        tracker.queue_extra_headers(
            &oversized_id,
            &serde_json::json!({ "Cookie": "private-cookie" }),
        );
        assert!(tracker.pending_extra_headers.is_empty());
        assert_eq!(tracker.dropped, 2);
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_cdp_header_limits_accept_exact_boundaries_and_reject_overflow() {
        let header_name = "x";
        let exact_value = "a".repeat(MAX_DEVTOOLS_HEADER_BYTES - header_name.len());
        let exact =
            devtools_headers_bounded(&serde_json::json!({ (header_name): exact_value })).unwrap();
        assert!(private_headers_within_limits(&exact));

        let oversized_value = "a".repeat(MAX_DEVTOOLS_HEADER_BYTES - header_name.len() + 1);
        assert!(
            devtools_headers_bounded(&serde_json::json!({ (header_name): oversized_value }))
                .is_none()
        );

        let mut exact_count = serde_json::Map::new();
        for index in 0..MAX_DEVTOOLS_HEADER_COUNT {
            exact_count.insert(format!("x-{index}"), serde_json::json!("v"));
        }
        assert!(devtools_headers_bounded(&exact_count.clone().into()).is_some());
        exact_count.insert("x-overflow".to_owned(), serde_json::json!("v"));
        assert!(devtools_headers_bounded(&exact_count.into()).is_none());
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_cdp_header_merging_cannot_exceed_the_aggregate_budget() {
        let mut target = reqwest::header::HeaderMap::new();
        target.insert(
            "x-first",
            reqwest::header::HeaderValue::from_bytes(&vec![
                b'a';
                MAX_DEVTOOLS_HEADER_BYTES / 2
                    - "x-first".len()
            ])
            .unwrap(),
        );
        let mut exact = reqwest::header::HeaderMap::new();
        exact.insert(
            "x-second",
            reqwest::header::HeaderValue::from_bytes(&vec![
                b'b';
                MAX_DEVTOOLS_HEADER_BYTES / 2
                    - "x-second".len()
            ])
            .unwrap(),
        );
        assert!(extend_header_map_bounded(&mut target, &exact));
        assert!(private_headers_within_limits(&target));

        let mut overflow = reqwest::header::HeaderMap::new();
        overflow.insert("x-overflow", reqwest::header::HeaderValue::from_static("v"));
        assert!(!extend_header_map_bounded(&mut target, &overflow));
        assert!(!target.contains_key("x-overflow"));
    }

    #[test]
    fn serializes_only_signed_in_youtube_cookies() {
        let header = signed_in_cookie_header(vec![
            cookie("SAPISID", "shared", ".youtube.com"),
            cookie("SAPISID", "host", "music.youtube.com"),
            cookie("LOGIN_INFO", "login", ".youtube.com"),
            cookie("SID", "google", ".google.com"),
        ])
        .unwrap()
        .unwrap();
        assert!(header.contains("SAPISID=shared"));
        assert!(header.contains("LOGIN_INFO=login"));
        assert!(!header.contains("host"));
        assert!(!header.contains("google"));
    }

    #[test]
    fn ignores_a_browser_that_is_not_signed_in() {
        let header = signed_in_cookie_header(vec![cookie(
            "VISITOR_INFO1_LIVE",
            "visitor",
            ".youtube.com",
        )])
        .unwrap();
        assert!(header.is_none());
    }

    #[test]
    fn waits_until_both_identity_and_youtube_session_cookies_exist() {
        assert!(
            signed_in_cookie_header(vec![cookie("SAPISID", "identity", ".youtube.com")])
                .unwrap()
                .is_none()
        );
        assert!(
            signed_in_cookie_header(vec![cookie("LOGIN_INFO", "session", ".youtube.com")])
                .unwrap()
                .is_none()
        );

        let header = signed_in_cookie_header(vec![
            cookie("SAPISID", "identity", ".youtube.com"),
            cookie("LOGIN_INFO", "session", ".youtube.com"),
        ])
        .unwrap();
        assert!(header.is_some());
    }

    #[test]
    fn new_account_sign_in_uses_a_profile_separate_from_active_profile() {
        let directory = tempfile::tempdir().unwrap();
        let active = profile_path(directory.path());
        std::fs::create_dir_all(&active).unwrap();
        let fresh = fresh_account_profile_path(directory.path()).unwrap();

        assert_ne!(fresh, active);
        assert!(fresh.starts_with(directory.path().join("youtube")));
        assert!(fresh.is_dir());
        assert!(fresh
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(NEW_ACCOUNT_PROFILE_PREFIX));
    }

    #[test]
    fn new_account_profile_promotion_replaces_only_the_active_profile() {
        let directory = tempfile::tempdir().unwrap();
        let active = profile_path(directory.path());
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(active.join("account.txt"), "old").unwrap();
        let fresh = fresh_account_profile_path(directory.path()).unwrap();
        std::fs::write(fresh.join("account.txt"), "new").unwrap();

        promote_profile(&fresh, &active, Path::new("fixture-browser.exe"), true).unwrap();

        assert!(!fresh.exists());
        assert_eq!(
            std::fs::read_to_string(active.join("account.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("youtube/browser-path.txt")).unwrap(),
            "fixture-browser.exe"
        );
    }

    #[test]
    fn open_browser_promotion_copies_without_deleting_the_live_profile() {
        let directory = tempfile::tempdir().unwrap();
        let active = profile_path(directory.path());
        std::fs::create_dir_all(&active).unwrap();
        let fresh = fresh_account_profile_path(directory.path()).unwrap();
        std::fs::write(fresh.join("account.txt"), "new").unwrap();

        promote_profile(&fresh, &active, Path::new("fixture-browser.exe"), false).unwrap();

        assert!(fresh.is_dir());
        assert_eq!(
            std::fs::read_to_string(active.join("account.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn browser_media_capture_keeps_the_token_but_removes_sabr_ranges() {
        let url = reqwest::Url::parse(
            "https://r1.googlevideo.com/videoplayback?itag=140&mime=audio%2Fmp4&clen=3274344&pot=proof&ump=1&srfvp=1&range=0-65535&rn=1&rbuf=0&sig=signed",
        )
        .unwrap();
        assert!(browser_media_request_matches(&url, 140, Some(3_274_344)));

        let sanitized = sanitize_browser_media_url(url).unwrap();
        let query = sanitized.query().unwrap();
        assert!(query.contains("pot=proof"));
        assert!(query.contains("sig=signed"));
        for removed in ["ump=", "srfvp=", "range=", "rn=", "rbuf="] {
            assert!(!query.contains(removed));
        }
    }

    #[test]
    fn browser_media_capture_accepts_the_codec_selected_by_chrome() {
        let url = reqwest::Url::parse(
            "https://r1.googlevideo.com/videoplayback?itag=141&mime=audio%2Fmp4&clen=3274344&pot=proof&sig=signed",
        )
        .unwrap();

        assert!(browser_media_request_matches(&url, 140, Some(3_274_344)));
    }

    #[test]
    fn browser_media_capture_rejects_video_requests() {
        let url = reqwest::Url::parse(
            "https://r1.googlevideo.com/videoplayback?itag=247&mime=video%2Fwebm&clen=3274344&pot=proof&sig=signed",
        )
        .unwrap();

        assert!(!browser_media_request_matches(&url, 140, Some(3_274_344)));
    }

    #[test]
    fn diagnostic_capture_keeps_headers_in_memory_and_reports_status() {
        let mut headers = reqwest::header::HeaderMap::new();
        extend_devtools_headers(
            &mut headers,
            &serde_json::json!({
                ":authority": "r1.googlevideo.com",
                "Cookie": "fixture=session",
                "User-Agent": "fixture-agent"
            }),
        );
        let capture = browser_media_capture(
            reqwest::Url::parse(
                "https://r1.googlevideo.com/videoplayback?itag=140&mime=audio%2Fmp4&clen=3274344&pot=proof&ump=1&range=0-65535&sig=signed",
            )
            .unwrap(),
            headers,
            Some(200),
            0,
            false,
            false,
        )
        .unwrap();

        assert_eq!(capture.response_status, Some(200));
        assert_eq!(
            capture.request_headers[reqwest::header::COOKIE],
            "fixture=session"
        );
        assert!(!capture.sanitized_url.query().unwrap().contains("range="));
        assert!(!capture.request_headers.contains_key(":authority"));
    }

    #[tokio::test]
    #[ignore = "live signed-in browser reuse and idle-shutdown contract"]
    async fn warm_playback_browser_is_reused_then_exits_when_idle() {
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                shutdown_playback_browser_now();
            }
        }
        let _cleanup = Cleanup;
        let launches_before = PLAYBACK_BROWSER_LAUNCHES.load(std::sync::atomic::Ordering::SeqCst);
        let config_folder = crate::config::get_config_folder_path().unwrap();
        let cancellation = tokio_util::sync::CancellationToken::new();
        capture_playback_url(
            &config_folder,
            "5NAlXos329A",
            140,
            Some(3_274_344),
            &cancellation,
            false,
        )
        .await
        .unwrap();
        let first_pid = playback_browser_process_id().unwrap();
        assert_no_playback_page_targets().await;
        capture_playback_url(
            &config_folder,
            "5NAlXos329A",
            140,
            Some(3_274_344),
            &cancellation,
            false,
        )
        .await
        .unwrap();
        assert_eq!(playback_browser_process_id(), Some(first_pid));
        assert_no_playback_page_targets().await;
        assert_eq!(
            PLAYBACK_BROWSER_LAUNCHES.load(std::sync::atomic::Ordering::SeqCst) - launches_before,
            1
        );

        tokio::time::sleep(PLAYBACK_BROWSER_IDLE_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(playback_browser_process_id(), None);
        shutdown_playback_browser().await;
    }

    #[tokio::test]
    #[ignore = "live current-plus-prefetch browser lifetime contract"]
    async fn prefetch_capture_reuses_then_closes_the_browser() {
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                shutdown_playback_browser_now();
            }
        }
        let _cleanup = Cleanup;
        let launches_before = PLAYBACK_BROWSER_LAUNCHES.load(std::sync::atomic::Ordering::SeqCst);
        let config_folder = crate::config::get_config_folder_path().unwrap();
        let cancellation = tokio_util::sync::CancellationToken::new();
        capture_playback_url(
            &config_folder,
            "5NAlXos329A",
            140,
            Some(3_274_344),
            &cancellation,
            false,
        )
        .await
        .unwrap();
        let current_pid = playback_browser_process_id().unwrap();
        capture_playback_url(
            &config_folder,
            "5NAlXos329A",
            140,
            Some(3_274_344),
            &cancellation,
            true,
        )
        .await
        .unwrap();
        assert!(current_pid > 0);
        assert_eq!(
            PLAYBACK_BROWSER_LAUNCHES.load(std::sync::atomic::Ordering::SeqCst) - launches_before,
            1
        );
        assert_eq!(playback_browser_process_id(), None);
    }

    async fn assert_no_playback_page_targets() {
        let port = playback_browser_debug_port().unwrap();
        let endpoint = format!("http://127.0.0.1:{port}/json/list");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let targets = reqwest::get(&endpoint)
                .await
                .unwrap()
                .json::<Vec<DevtoolsTarget>>()
                .await
                .unwrap();
            let playback_pages = targets
                .iter()
                .filter(|target| {
                    target.target_type == "page" && target.url.starts_with(super::MUSIC_URL)
                })
                .map(|target| target.url.as_str())
                .collect::<Vec<_>>();
            if playback_pages.is_empty() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "temporary YouTube Music targets remained open: {playback_pages:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    use std::path::Path;
    use std::time::Duration;
}
