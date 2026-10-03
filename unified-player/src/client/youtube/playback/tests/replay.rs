#[cfg(feature = "private-capture")]
static PROVIDER_REPLAY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(feature = "private-capture")]
struct ProviderReplayClock(u64);

#[cfg(feature = "private-capture")]
impl crate::developer_capture::ReplayClock for ProviderReplayClock {
    fn unix_ms(&self) -> u64 {
        self.0
    }
}

#[cfg(feature = "private-capture")]
struct ProviderReplayPendingTimer;

#[cfg(feature = "private-capture")]
impl crate::developer_capture::ReplayTimer for ProviderReplayPendingTimer {
    fn wait<'a>(&'a self, _duration: Duration) -> crate::developer_capture::ReplayFuture<'a, ()> {
        Box::pin(std::future::pending())
    }
}

#[cfg(feature = "private-capture")]
fn provider_replay_recipe(
    status: u16,
    body: &[u8],
    auth: crate::developer_capture::ReplayAuthKind,
    proof_token_present: bool,
    client: crate::developer_capture::ReplayPlayerClient,
    quality: crate::developer_capture::ReplayQualityPolicy,
    now_unix_ms: u64,
) -> crate::developer_capture::ReplayRecipeV1 {
    use crate::developer_capture::{
        CapturePurpose, CaptureRef, ExchangeRef, ReplayClientVersionPolicy, ReplayCredentialPolicy,
        ReplayRecipeParts, SensitiveBytes, SensitiveString,
    };

    crate::developer_capture::ReplayRecipeV1::new(ReplayRecipeParts {
        parent_capture_ref: CaptureRef::from_bytes([0x31; 16]),
        source_exchange_ref: ExchangeRef::from_bytes([0x32; 8]),
        source_purpose: CapturePurpose::InteractivePlayback,
        created_unix_ms: now_unix_ms.saturating_sub(1_000),
        expires_unix_ms: now_unix_ms.saturating_add(1_000),
        media_id: SensitiveString::new("fixture_replay_media".to_owned()),
        player_http_status: status,
        player_response: SensitiveBytes::new(body.to_vec()),
        response_complete: true,
        client,
        auth,
        proof_token_present,
        client_version_policy: ReplayClientVersionPolicy::CurrentCompatible,
        credential_policy: ReplayCredentialPolicy::CurrentConfigured,
        quality,
        expected_decision: super::replay_player_decision(
            status,
            body,
            auth,
            proof_token_present,
            client,
            quality,
        ),
    })
    .unwrap()
}

#[cfg(feature = "private-capture")]
fn fixture_fresh_replay_adapter(
    endpoint: reqwest::Url,
    auth_type: crate::config::YouTubeMusicAuthType,
    cookie_path: std::path::PathBuf,
    po_token_path: Option<std::path::PathBuf>,
) -> super::YouTubeFreshReplayAdapter {
    super::YouTubeFreshReplayAdapter::new_for_test(
        endpoint,
        auth_type,
        cookie_path,
        po_token_path,
    )
}

#[cfg(feature = "private-capture")]
#[test]
fn offline_replay_uses_the_current_pure_parser_and_selector() {
    use crate::developer_capture::{
        run_offline_replay, CaptureRef, OfflineReplayOutcome, ReplayAuthKind, ReplayPlayerClient,
        ReplayQualityPolicy, ReplaySelectionOutcome, ReplayTerminalOutcome,
    };

    let response = br#"{
        "playabilityStatus":{"status":"OK"},
        "streamingData":{"adaptiveFormats":[
            {"itag":140,"url":"https://r1.googlevideo.com/videoplayback?a=1","mimeType":"audio/mp4; codecs=\"mp4a.40.2\"","bitrate":129000},
            {"itag":139,"url":"https://r1.googlevideo.com/videoplayback?a=2","mimeType":"audio/mp4; codecs=\"mp4a.40.5\"","bitrate":49000}
        ]}
    }"#;
    let now = 1_700_000_000_000;
    let high = provider_replay_recipe(
        200,
        response,
        ReplayAuthKind::None,
        false,
        ReplayPlayerClient::AndroidVr,
        ReplayQualityPolicy::High,
        now,
    );
    let high_result = run_offline_replay(
        &high,
        CaptureRef::from_bytes([0x41; 16]),
        &super::YouTubeOfflineReplayAdapter,
    )
    .unwrap();
    assert_eq!(
        high_result.terminal().outcome(),
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Reproduced)
    );
    assert_eq!(
        high_result.observed_decision().unwrap().selection,
        ReplaySelectionOutcome::Selected { itag: 140 }
    );

    let saver = provider_replay_recipe(
        200,
        response,
        ReplayAuthKind::None,
        false,
        ReplayPlayerClient::AndroidVr,
        ReplayQualityPolicy::DataSaver,
        now,
    );
    let saver_result = run_offline_replay(
        &saver,
        CaptureRef::from_bytes([0x42; 16]),
        &super::YouTubeOfflineReplayAdapter,
    )
    .unwrap();
    assert_eq!(
        saver_result.observed_decision().unwrap().selection,
        ReplaySelectionOutcome::Selected { itag: 139 }
    );

    let proof = super::replay_player_decision(
        403,
        b"{}",
        ReplayAuthKind::Browser,
        true,
        ReplayPlayerClient::TvHtml5,
        ReplayQualityPolicy::High,
    );
    assert_eq!(
        proof.provider,
        crate::developer_capture::ReplayProviderOutcome::ProofToken
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn fresh_replay_uses_current_material_and_exactly_one_fixed_post() {
    use std::sync::atomic::Ordering;

    use crate::developer_capture::{
        run_fresh_replay, CaptureRef, FreshReplayOutcome, FreshReplayPolicy, ReplayAuthKind,
        ReplayPlayerClient, ReplayQualityPolicy, ReplayTerminalOutcome,
    };

    let _test_guard = PROVIDER_REPLAY_TEST_LOCK.lock().await;
    let now = 1_700_000_000_000;
    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("current-cookie.txt");
    let proof_path = directory.path().join("current-proof.txt");
    std::fs::write(
        &cookie_path,
        "SAPISID=current-sapisid; VISITOR_INFO1_LIVE=current-visitor",
    )
    .unwrap();
    std::fs::write(&proof_path, "current-proof-token").unwrap();
    let (endpoint, request, requests, server) =
        serve_counted_replay_response(403, b"{}", Duration::ZERO);
    let adapter = fixture_fresh_replay_adapter(
        endpoint,
        crate::config::YouTubeMusicAuthType::Browser,
        cookie_path,
        Some(proof_path),
    );
    let recipe = provider_replay_recipe(
        403,
        b"{}",
        ReplayAuthKind::Browser,
        true,
        ReplayPlayerClient::TvHtml5,
        ReplayQualityPolicy::High,
        now,
    );
    let result = run_fresh_replay(
        &recipe,
        CaptureRef::from_bytes([0x43; 16]),
        &adapter,
        &ProviderReplayClock(now),
        &ProviderReplayPendingTimer,
        &CancellationToken::new(),
        FreshReplayPolicy::default(),
    )
    .await
    .unwrap();
    let request = request.recv_timeout(Duration::from_secs(1)).unwrap();
    server.join().unwrap();

    assert_eq!(
        result.terminal().outcome(),
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Reproduced)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let request_text = String::from_utf8(request).unwrap();
    let expected_auth = super::sapisid_authorization("current-sapisid", now / 1_000);
    assert!(request_text.contains("POST /youtubei/v1/player?key=fixture HTTP/1.1"));
    assert!(request_text.contains("SAPISID=current-sapisid"));
    assert!(request_text.contains(&expected_auth));
    assert!(request_text.contains("current-proof-token"));
    assert!(request_text.contains("\"clientVersion\":\"5.20231114\""));
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn fresh_replay_never_follows_a_provider_redirect() {
    use std::sync::atomic::Ordering;

    use crate::developer_capture::{
        run_fresh_replay, CaptureRef, FreshReplayPolicy, ReplayAuthKind, ReplayPlayerClient,
        ReplayQualityPolicy,
    };

    let _test_guard = PROVIDER_REPLAY_TEST_LOCK.lock().await;
    let now = 1_700_000_000_000;
    let (endpoint, first_requests, second_requests, server) = serve_replay_redirect();
    let adapter = fixture_fresh_replay_adapter(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
        std::path::PathBuf::new(),
        None,
    );
    let recipe = provider_replay_recipe(
        403,
        b"{}",
        ReplayAuthKind::None,
        false,
        ReplayPlayerClient::AndroidVr,
        ReplayQualityPolicy::High,
        now,
    );
    let result = run_fresh_replay(
        &recipe,
        CaptureRef::from_bytes([0x47; 16]),
        &adapter,
        &ProviderReplayClock(now),
        &ProviderReplayPendingTimer,
        &CancellationToken::new(),
        FreshReplayPolicy::default(),
    )
    .await
    .unwrap();
    server.join().unwrap();

    assert_eq!(result.terminal_count(), 1);
    assert_eq!(first_requests.load(Ordering::SeqCst), 1);
    assert_eq!(second_requests.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn fresh_replay_cancellation_drops_the_only_in_flight_post() {
    use std::sync::atomic::Ordering;

    use crate::developer_capture::{
        run_fresh_replay, CaptureRef, FreshReplayOutcome, FreshReplayPolicy, ReplayAuthKind,
        ReplayPlayerClient, ReplayQualityPolicy, ReplayTerminalOutcome,
    };

    let _test_guard = PROVIDER_REPLAY_TEST_LOCK.lock().await;
    let now = 1_700_000_000_000;
    let (endpoint, accepted, requests, server) =
        serve_counted_replay_response(403, b"{}", Duration::from_millis(250));
    let adapter = fixture_fresh_replay_adapter(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
        std::path::PathBuf::new(),
        None,
    );
    let recipe = provider_replay_recipe(
        403,
        b"{}",
        ReplayAuthKind::None,
        false,
        ReplayPlayerClient::AndroidVr,
        ReplayQualityPolicy::High,
        now,
    );
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    let cancel_thread = std::thread::spawn(move || {
        accepted.recv_timeout(Duration::from_secs(1)).unwrap();
        cancel.cancel();
    });
    let result = run_fresh_replay(
        &recipe,
        CaptureRef::from_bytes([0x44; 16]),
        &adapter,
        &ProviderReplayClock(now),
        &ProviderReplayPendingTimer,
        &cancellation,
        FreshReplayPolicy::default(),
    )
    .await
    .unwrap();
    cancel_thread.join().unwrap();
    server.join().unwrap();

    assert_eq!(
        result.terminal().outcome(),
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Cancelled)
    );
    assert_eq!(result.terminal_count(), 1);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn fresh_replay_rejects_browser_profile_contention_before_current_material() {
    use crate::developer_capture::{
        run_fresh_replay, CaptureRef, FreshReplayOutcome, FreshReplayPolicy, ReplayAuthKind,
        ReplayPlayerClient, ReplayQualityPolicy, ReplayTerminalOutcome,
    };

    let _test_guard = PROVIDER_REPLAY_TEST_LOCK.lock().await;
    let profile_guard =
        crate::client::youtube::browser_auth::try_lock_browser_profile_for_fresh_replay().unwrap();
    let now = 1_700_000_000_000;
    let adapter = fixture_fresh_replay_adapter(
        reqwest::Url::parse("https://www.youtube.com/youtubei/v1/player").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
        std::path::PathBuf::from("missing-private-cookie"),
        None,
    );
    let recipe = provider_replay_recipe(
        403,
        b"{}",
        ReplayAuthKind::Browser,
        false,
        ReplayPlayerClient::AndroidVr,
        ReplayQualityPolicy::High,
        now,
    );
    let result = run_fresh_replay(
        &recipe,
        CaptureRef::from_bytes([0x45; 16]),
        &adapter,
        &ProviderReplayClock(now),
        &ProviderReplayPendingTimer,
        &CancellationToken::new(),
        FreshReplayPolicy::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        result.terminal().outcome(),
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::BrowserContended)
    );
    assert_eq!(result.terminal_count(), 1);
    drop(profile_guard);
    assert!(
        crate::client::youtube::browser_auth::try_lock_browser_profile_for_fresh_replay().is_some()
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn fresh_replay_holds_browser_profile_lock_through_the_only_post() {
    use std::sync::atomic::Ordering;

    use crate::developer_capture::{
        run_fresh_replay, CaptureRef, FreshReplayOutcome, FreshReplayPolicy, ReplayAuthKind,
        ReplayPlayerClient, ReplayQualityPolicy, ReplayTerminalOutcome,
    };

    let _test_guard = PROVIDER_REPLAY_TEST_LOCK.lock().await;
    let now = 1_700_000_000_000;
    let (endpoint, request, requests, server) =
        serve_counted_replay_response(403, b"{}", Duration::from_millis(250));
    let adapter = fixture_fresh_replay_adapter(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
        std::path::PathBuf::new(),
        None,
    );
    let recipe = provider_replay_recipe(
        403,
        b"{}",
        ReplayAuthKind::None,
        false,
        ReplayPlayerClient::AndroidVr,
        ReplayQualityPolicy::High,
        now,
    );
    let (inspection_tx, inspection_rx) = std::sync::mpsc::channel();
    let inspector = std::thread::spawn(move || {
        request.recv_timeout(Duration::from_secs(1)).unwrap();
        inspection_tx
            .send(
                crate::client::youtube::browser_auth::try_lock_browser_profile_for_fresh_replay()
                    .is_none(),
            )
            .unwrap();
    });

    let result = run_fresh_replay(
        &recipe,
        CaptureRef::from_bytes([0x46; 16]),
        &adapter,
        &ProviderReplayClock(now),
        &ProviderReplayPendingTimer,
        &CancellationToken::new(),
        FreshReplayPolicy::default(),
    )
    .await
    .unwrap();
    inspector.join().unwrap();
    server.join().unwrap();

    assert!(inspection_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.terminal().outcome(),
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Reproduced)
    );
    assert!(
        crate::client::youtube::browser_auth::try_lock_browser_profile_for_fresh_replay().is_some()
    );
}
