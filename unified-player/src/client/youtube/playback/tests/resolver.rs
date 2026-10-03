#[tokio::test]
async fn browser_client_policy_tries_visionos_first_and_keeps_existing_fallbacks() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );
    resolver.seed_client_versions_for_test(
        "5.fixture-tv".to_owned(),
        (
            "2.fixture-web".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
        (
            "1.fixture-remix".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
    );

    let clients = resolver
        .player_client_candidates_for_test(super::PlayerClientPlan::Playback)
        .await;
    assert_eq!(
        clients
            .iter()
            .map(|client| client.source_name)
            .collect::<Vec<_>>(),
        ["VISIONOS", "TVHTML5", "WEB", "WEB_REMIX", "ANDROID_VR"]
    );
}

#[tokio::test]
async fn non_browser_client_policy_also_tries_visionos_first() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::OAuth,
    );

    let clients = resolver
        .player_client_candidates_for_test(super::PlayerClientPlan::Playback)
        .await;
    assert_eq!(
        clients
            .iter()
            .map(|client| client.source_name)
            .collect::<Vec<_>>(),
        ["VISIONOS", "ANDROID_VR"]
    );
}

#[tokio::test]
async fn forced_probe_clients_do_not_cross_native_routes() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );
    resolver.seed_client_versions_for_test(
        "5.fixture-tv".to_owned(),
        (
            "2.fixture-web".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
        (
            "1.fixture-remix".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
    );

    for (client, expected) in [
        (YouTubeProbeClient::Tv, "TVHTML5"),
        (YouTubeProbeClient::Web, "WEB"),
        (YouTubeProbeClient::WebRemix, "WEB_REMIX"),
        (YouTubeProbeClient::AndroidVr, "ANDROID_VR"),
        (YouTubeProbeClient::VisionOs, "VISIONOS"),
    ] {
        assert_eq!(
            resolver.probe_client_labels_for_test(client).await,
            [expected]
        );
    }
}

#[test]
fn automatic_android_vr_requires_client_scoped_proof_material() {
    let android = super::PlayerClient::android_vr();
    let global_only = PoTokenMaterial::from_text(
        r#"{"player":"global-player","gvs":"global-gvs"}"#,
    );
    let scoped = PoTokenMaterial::from_text(
        r#"{"clients":{"ANDROID_VR":{"gvs":"vr-gvs"}}}"#,
    );

    assert!(!automatic_player_client_enabled(&android, &global_only));
    assert!(automatic_player_client_enabled(&android, &scoped));
    assert!(automatic_player_client_enabled(
        &super::PlayerClient::web(
            "2.fixture-web".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
        &PoTokenMaterial::default(),
    ));
}

#[test]
fn learned_android_vr_remains_behind_visionos() {
    let scoped = PoTokenMaterial::from_text(
        r#"{"clients":{"ANDROID_VR":{"gvs":"vr-gvs"}}}"#,
    );
    let clients = automatic_player_clients(
        vec![
            super::PlayerClient::visionos(),
            super::PlayerClient::tv(
                "5.fixture-tv".to_owned(),
                super::PlayerVersionSource::Fallback,
            ),
            super::PlayerClient::web(
                "2.fixture-web".to_owned(),
                super::PlayerVersionSource::Fallback,
            ),
            super::PlayerClient::android_vr(),
        ],
        &scoped,
        true,
    );

    assert_eq!(
        clients
            .iter()
            .map(|client| client.source_name)
            .collect::<Vec<_>>(),
        ["VISIONOS", "ANDROID_VR", "TVHTML5", "WEB"]
    );
}

#[test]
fn playback_route_reports_the_browser_fallback_ladder() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );

    let route = resolver.playback_route();
    assert_eq!(
        route.order,
        [
            "VISIONOS",
            "TVHTML5",
            "WEB",
            "WEB_REMIX",
            "ANDROID_VR",
            "WEB_MUSIC_BROWSER_SESSION",
        ]
    );
    assert_eq!(route.selected, None);
    assert!(!route.learned_public_android_vr);
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn inspection_plan_reuses_playback_clients_and_adds_safari_probe() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );
    resolver.seed_client_versions_for_test(
        "5.fixture-tv".to_owned(),
        (
            "2.fixture-web".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
        (
            "1.fixture-remix".to_owned(),
            super::PlayerVersionSource::Fallback,
        ),
    );

    let clients = resolver
        .player_client_candidates_for_test(super::PlayerClientPlan::Inspection)
        .await;
    assert_eq!(
        clients
            .iter()
            .map(|client| client.source_name)
            .collect::<Vec<_>>(),
        [
            "VISIONOS",
            "TVHTML5",
            "WEB_SAFARI",
            "WEB",
            "WEB_REMIX",
            "ANDROID_VR"
        ]
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn native_resolution_falls_through_to_the_viable_music_client() {
    let (endpoint, request_bodies, server) = serve_player_response_sequence(vec![
        (
            200,
            include_bytes!("../../fixtures/player_unavailable.json"),
        ),
        (
            200,
            include_bytes!("../../fixtures/player_unavailable.json"),
        ),
        (
            200,
            include_bytes!("../../fixtures/player_cipher_only_browser_fallback.json"),
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("youtube-cookie.txt");
    std::fs::write(&cookie_path, "SAPISID=fixture; LOGIN_INFO=fixture").unwrap();
    let mut resolver = fixture_resolver(endpoint, crate::config::YouTubeMusicAuthType::Browser);
    resolver.set_auth_paths_for_test(directory.path().to_owned(), cookie_path);
    resolver.set_playback_clients_for_test(vec![
        PlayerClient::tv("5.fixture-tv".to_owned(), PlayerVersionSource::Fallback),
        PlayerClient::web("2.fixture-web".to_owned(), PlayerVersionSource::Fallback),
        PlayerClient::web_music("1.fixture-remix".to_owned(), PlayerVersionSource::Fallback),
    ]);

    let error = resolver
        .resolve(
            "fixture-client-fallback",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Authentication);
    server.join().unwrap();

    let request_clients = request_bodies
        .try_iter()
        .map(|request_body| {
            serde_json::from_slice::<serde_json::Value>(&request_body).unwrap()["context"]["client"]
                ["clientName"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(request_clients, ["TVHTML5", "WEB", "WEB_REMIX"]);
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn native_only_probe_stops_before_browser_session_fallback() {
    let (endpoint, _request_bodies, server) = serve_player_response_sequence(vec![
        (
            200,
            include_bytes!("../../fixtures/player_unavailable.json"),
        ),
        (
            200,
            include_bytes!("../../fixtures/player_unavailable.json"),
        ),
        (
            200,
            include_bytes!("../../fixtures/player_cipher_only_browser_fallback.json"),
        ),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("youtube-cookie.txt");
    std::fs::write(&cookie_path, "SAPISID=fixture; LOGIN_INFO=fixture").unwrap();
    let mut resolver = fixture_resolver(endpoint, crate::config::YouTubeMusicAuthType::Browser);
    resolver.set_auth_paths_for_test(directory.path().to_owned(), cookie_path);
    resolver.set_playback_clients_for_test(vec![
        PlayerClient::tv("5.fixture-tv".to_owned(), PlayerVersionSource::Fallback),
        PlayerClient::web("2.fixture-web".to_owned(), PlayerVersionSource::Fallback),
        PlayerClient::web_music("1.fixture-remix".to_owned(), PlayerVersionSource::Fallback),
    ]);

    let (result, browser_attempted, _, _, _) = resolver
        .resolve_for_probe(
            "fixture-native-only-probe",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
            false,
            YouTubeProbeClient::Auto,
        )
        .await;

    assert_eq!(result.unwrap_err().kind, AudioSourceErrorKind::Decipher);
    assert!(!browser_attempted);
    server.join().unwrap();
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn media_forbidden_learns_the_bootstrapped_public_android_vr_route() {
    fn direct_response(url: &reqwest::Url) -> &'static [u8] {
        Box::leak(
            serde_json::to_vec(&serde_json::json!({
                "playabilityStatus": {"status": "OK"},
                "streamingData": {
                    "adaptiveFormats": [{
                        "itag": 140,
                        "url": url.as_str(),
                        "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                        "bitrate": 128000,
                        "contentLength": "1024",
                        "approxDurationMs": "5000"
                    }]
                }
            }))
            .unwrap()
            .into_boxed_slice(),
        )
    }

    let (forbidden_url, forbidden_server) =
        serve_one_media_probe(403, None, Duration::ZERO);
    let (first_public_url, first_public_server) = serve_one_media_probe(
        206,
        Some("bytes 512-1023/1024"),
        Duration::ZERO,
    );
    let (second_public_url, second_public_server) = serve_one_media_probe(
        206,
        Some("bytes 512-1023/1024"),
        Duration::ZERO,
    );
    let (authenticated_url, authenticated_server) = serve_one_media_probe(
        206,
        Some("bytes 512-1023/1024"),
        Duration::ZERO,
    );
    let (endpoint, request_bodies, player_server) = serve_player_response_sequence(vec![
        (200, direct_response(&forbidden_url)),
        (200, direct_response(&first_public_url)),
        (200, direct_response(&second_public_url)),
        (
            200,
            include_bytes!("../../fixtures/player_unavailable.json"),
        ),
        (200, direct_response(&authenticated_url)),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("youtube-cookie.txt");
    std::fs::write(&cookie_path, "SAPISID=fixture; LOGIN_INFO=fixture").unwrap();
    let mut resolver = fixture_resolver(endpoint, crate::config::YouTubeMusicAuthType::Browser);
    resolver.set_auth_paths_for_test(directory.path().to_owned(), cookie_path);
    resolver.seed_guest_visitor_data_for_test("fixture-visitor".to_owned());
    resolver.set_playback_clients_for_test(vec![
        PlayerClient::web_music(
            "1.fixture-remix".to_owned(),
            PlayerVersionSource::Fallback,
        ),
        PlayerClient::android_vr(),
    ]);

    let source = resolver
        .resolve(
            "fixture-public-transport-fallback",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(source.source_client, "ANDROID_VR");
    assert_eq!(source.url, first_public_url);

    let learned_source = resolver
        .resolve(
            "fixture-learned-public-route",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(learned_source.source_client, "ANDROID_VR");
    assert_eq!(learned_source.url, second_public_url);

    let authenticated_source = resolver
        .resolve(
            "fixture-account-route-restored",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(authenticated_source.source_client, "WEB_REMIX");
    assert_eq!(authenticated_source.url, authenticated_url);

    player_server.join().unwrap();
    forbidden_server.join().unwrap();
    first_public_server.join().unwrap();
    second_public_server.join().unwrap();
    authenticated_server.join().unwrap();
    let request_clients = request_bodies
        .try_iter()
        .map(|request_body| {
            serde_json::from_slice::<serde_json::Value>(&request_body).unwrap()["context"]
                ["client"]["clientName"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        request_clients,
        [
            "WEB_REMIX",
            "ANDROID_VR",
            "ANDROID_VR",
            "ANDROID_VR",
            "WEB_REMIX"
        ]
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn browser_auth_web_remix_uses_native_ranged_transport() {
    let (media_url, user_agent_matches, media_server) =
        serve_media_probe_requiring_user_agent(super::player::WEB_USER_AGENT);
    let response_body = Box::leak(
        serde_json::to_vec(&serde_json::json!({
            "playabilityStatus": {"status": "OK"},
            "streamingData": {
                "adaptiveFormats": [{
                    "itag": 140,
                    "url": media_url.as_str(),
                    "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                    "bitrate": 128000,
                    "contentLength": "1024",
                    "approxDurationMs": "5000"
                }]
            }
        }))
        .unwrap()
        .into_boxed_slice(),
    );
    let (endpoint, request_bodies, player_server) =
        serve_player_response_sequence(vec![(200, response_body)]);
    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("youtube-cookie.txt");
    std::fs::write(&cookie_path, "SAPISID=fixture; LOGIN_INFO=fixture").unwrap();
    let mut resolver = fixture_resolver(endpoint, crate::config::YouTubeMusicAuthType::Browser);
    resolver.set_auth_paths_for_test(directory.path().to_owned(), cookie_path);
    resolver.set_playback_clients_for_test(vec![PlayerClient::web_music(
        "1.fixture-remix".to_owned(),
        PlayerVersionSource::Fallback,
    )]);

    let source = resolver
        .resolve(
            "fixture-web-remix-native",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(source.source_client, "WEB_REMIX");
    assert!(user_agent_matches.recv_timeout(Duration::from_secs(1)).unwrap());
    assert_eq!(request_bodies.try_iter().count(), 1);
    player_server.join().unwrap();
    media_server.join().unwrap();
}

#[tokio::test]
async fn resolver_honors_pre_cancelled_requests_without_network_io() {
    let resolver = fixture_resolver(
        reqwest::Url::parse(super::PLAYER_ENDPOINT).unwrap(),
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let err = resolver
        .resolve(
            "cancelled",
            crate::config::YouTubePlaybackQuality::High,
            cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, AudioSourceErrorKind::Cancelled);
    assert!(err.to_string().contains("cancelled"));
}

#[tokio::test]
async fn forced_visionos_probe_does_not_require_browser_credentials() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let (result, browser_attempted, _, _, attempts) = resolver
        .resolve_for_probe(
            "fixture-public-without-browser-auth",
            crate::config::YouTubePlaybackQuality::High,
            cancellation,
            false,
            YouTubeProbeClient::VisionOs,
        )
        .await;

    assert_eq!(result.unwrap_err().kind, AudioSourceErrorKind::Cancelled);
    assert!(!browser_attempted);
    assert!(attempts.iter().any(|attempt| {
        attempt.stage == "player"
            && attempt.client == "VISIONOS"
            && attempt.result == "cancelled"
    }));
}

#[tokio::test]
#[ignore = "network contract smoke test"]
async fn native_resolver_opens_a_decodable_public_audio_stream() {
    let resolver = fixture_resolver(
        reqwest::Url::parse(super::PLAYER_ENDPOINT).unwrap(),
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let cancellation = CancellationToken::new();
    let resolved = resolver
        .resolve(
            "dQw4w9WgXcQ",
            crate::config::YouTubePlaybackQuality::High,
            cancellation.clone(),
        )
        .await
        .unwrap();
    assert!(resolved.mime_type.starts_with("audio/mp4"));
    let _decoded = open_decoded_source(&resolved, Duration::ZERO, 16 * 1024 * 1024, cancellation)
        .await
        .unwrap();
}
