#[tokio::test]
async fn browser_request_auth_uses_saved_cookie_regardless_of_file_age() {
    use std::{
        fs::{File, FileTimes},
        path::Path,
        time::{Duration, SystemTime},
    };

    let directory = tempfile::tempdir().unwrap();
    let cookie_path = directory.path().join("youtube-cookie.txt");
    std::fs::write(&cookie_path, "SAPISID=fixture-session; LOGIN_INFO=fixture").unwrap();
    File::options()
        .write(true)
        .open(&cookie_path)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(24 * 60 * 60)),
        )
        .unwrap();

    let auth = load_request_auth(
        crate::config::YouTubeMusicAuthType::Browser,
        &cookie_path,
        Path::new("unused-oauth-token"),
        Some(1),
    )
    .await
    .unwrap();

    assert!(matches!(auth, super::RequestAuth::Browser(_)));
}

#[test]
fn android_vr_requests_keep_browser_identity_out_but_accept_scoped_player_tokens() {
    let auth = super::browser_request_auth_at("SAPISID=fixture-session", 1).unwrap();
    let client = super::PlayerClient::android_vr();
    let request_auth = client.auth_for_request(&auth);
    assert!(matches!(&request_auth, super::RequestAuth::None));
    assert_eq!(
        client.player_po_token(Some("fixture-player-token")),
        Some("fixture-player-token")
    );
    let request = super::build_player_request(
        &reqwest::Client::new(),
        reqwest::Url::parse("https://www.youtube.com/youtubei/v1/player?key=fixture").unwrap(),
        "fixture-video",
        &request_auth,
        &client,
        Some("fixture-player-token"),
        Some("fixture-visitor"),
    )
    .unwrap();
    assert!(request.headers().get(reqwest::header::COOKIE).is_none());
    assert!(request
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .is_none());
    assert!(request.headers().get(reqwest::header::ORIGIN).is_none());
    assert_eq!(request.headers()["x-goog-visitor-id"], "fixture-visitor");
    let body = request.body().and_then(reqwest::Body::as_bytes).unwrap();
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        body["serviceIntegrityDimensions"]["poToken"],
        "fixture-player-token"
    );
}

#[test]
fn visionos_requests_are_public_and_ignore_all_auth_and_proof_tokens() {
    let browser_auth = super::browser_request_auth_at("SAPISID=fixture-session", 1).unwrap();
    let bearer_auth = super::RequestAuth::Bearer("fixture-oauth-token".to_owned());
    let client = super::PlayerClient::visionos();
    assert!(matches!(
        client.auth_for_request(&browser_auth),
        super::RequestAuth::None
    ));
    assert!(matches!(
        client.auth_for_request(&bearer_auth),
        super::RequestAuth::None
    ));
    assert_eq!(
        client.auth_scope(&browser_auth),
        super::format::PlayerAttemptAuthScope::Public
    );

    let material = super::PoTokenMaterial::from_text(
        r#"{
            "player": "global-player",
            "gvs": "global-gvs",
            "clients": {
                "VISIONOS": {"player": "vision-player", "gvs": "vision-gvs"}
            }
        }"#,
    );
    assert_eq!(material.player_for(&client), None);
    assert_eq!(material.gvs_for(&client), None);
    assert_eq!(client.player_po_token(Some("direct-player-token")), None);

    let request = super::build_player_request(
        &reqwest::Client::new(),
        reqwest::Url::parse("https://www.youtube.com/youtubei/v1/player?key=fixture").unwrap(),
        "fixture-video",
        &browser_auth,
        &client,
        Some("direct-player-token"),
        Some("fixture-visitor"),
    )
    .unwrap();
    for header in [
        reqwest::header::COOKIE,
        reqwest::header::AUTHORIZATION,
        reqwest::header::ORIGIN,
    ] {
        assert!(request.headers().get(header).is_none());
    }
    assert_eq!(request.headers()["x-youtube-client-name"], "101");
    assert_eq!(request.headers()["x-youtube-client-version"], "1.02");
    assert_eq!(
        request.headers()[reqwest::header::USER_AGENT],
        super::player::VISIONOS_USER_AGENT
    );
    assert_eq!(request.headers()["x-goog-visitor-id"], "fixture-visitor");

    let body = request.body().and_then(reqwest::Body::as_bytes).unwrap();
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    let context = &body["context"]["client"];
    assert_eq!(context["clientName"], "VISIONOS");
    assert_eq!(context["clientVersion"], "1.02");
    assert_eq!(context["deviceMake"], "Apple");
    assert_eq!(context["deviceModel"], "RealityDevice17,1");
    assert_eq!(context["osName"], "visionOS");
    assert_eq!(context["osVersion"], "26.5.23O471");
    assert!(body.get("serviceIntegrityDimensions").is_none());
}

#[test]
fn public_clients_do_not_require_unused_configured_auth() {
    let visionos = super::PlayerClient::visionos();
    assert!(!visionos.requires_configured_auth(crate::config::YouTubeMusicAuthType::Browser));
    assert!(!visionos.requires_configured_auth(crate::config::YouTubeMusicAuthType::OAuth));

    let android_vr = super::PlayerClient::android_vr();
    assert!(!android_vr.requires_configured_auth(crate::config::YouTubeMusicAuthType::Browser));
    assert!(android_vr.requires_configured_auth(crate::config::YouTubeMusicAuthType::OAuth));

    let web = super::PlayerClient::web(
        "2.fixture-web".to_owned(),
        super::PlayerVersionSource::Fallback,
    );
    assert!(web.requires_configured_auth(crate::config::YouTubeMusicAuthType::Browser));
    assert!(web.requires_configured_auth(crate::config::YouTubeMusicAuthType::OAuth));
    assert!(!web.requires_configured_auth(crate::config::YouTubeMusicAuthType::Unauthenticated));
}

#[cfg(feature = "private-capture")]
#[test]
fn public_android_login_required_is_provider_unavailable_not_account_auth() {
    let response = super::PlayerResponse {
        playability_status: super::PlayabilityStatus {
            status: "LOGIN_REQUIRED".to_owned(),
            reason: Some("Sign in to confirm you are not a bot".to_owned()),
        },
        streaming_data: None,
        assets: None,
        http_status: 200,
    };
    let inspection = super::inspect_player_attempt(
        &super::PlayerClient::android_vr(),
        super::PlayerAttemptAuthScope::Public,
        false,
        &response,
        crate::config::YouTubePlaybackQuality::High,
    );
    assert_eq!(inspection.playability_status, "LOGIN_REQUIRED");
    assert_eq!(inspection.selection, "provider_unavailable");
    assert_eq!(inspection.error_category, None);

    let response: super::PlayerResponse =
        serde_json::from_str(r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED"}}"#).unwrap();
    assert_eq!(
        super::playable_formats_for_attempt(response, super::PlayerAttemptAuthScope::Public,)
            .unwrap_err()
            .kind,
        super::AudioSourceErrorKind::Unavailable
    );
    let response: super::PlayerResponse =
        serde_json::from_str(r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED"}}"#).unwrap();
    assert_eq!(
        super::playable_formats_for_attempt(response, super::PlayerAttemptAuthScope::Account,)
            .unwrap_err()
            .kind,
        super::AudioSourceErrorKind::Authentication
    );
}

#[test]
fn proof_tokens_keep_player_and_gvs_scopes_separate() {
    let material = super::PoTokenMaterial::from_text(
        r#"{
            "player": "player-default",
            "gvs": "gvs-default",
            "clients": {
                "WEB_REMIX": {"player": "remix-player", "gvs": "remix-gvs"},
                "ANDROID_VR": {"player": "vr-player", "gvs": "vr-gvs"}
            }
        }"#,
    );
    let remix = super::PlayerClient::web_music(
        "1.fixture-remix".to_owned(),
        super::PlayerVersionSource::Fallback,
    );
    let tv = super::PlayerClient::tv(
        "5.fixture-tv".to_owned(),
        super::PlayerVersionSource::Fallback,
    );
    let android = super::PlayerClient::android_vr();
    assert_eq!(material.player_for(&remix), Some("remix-player"));
    assert_eq!(material.gvs_for(&remix), Some("remix-gvs"));
    assert_eq!(material.player_for(&tv), Some("player-default"));
    assert_eq!(material.gvs_for(&tv), Some("gvs-default"));
    assert_eq!(material.player_for(&android), Some("vr-player"));
    assert_eq!(material.gvs_for(&android), Some("vr-gvs"));
    assert!(material.has_scoped_playback_token(&android));
    assert_eq!(material.legacy_player, None);
    let legacy = super::PoTokenMaterial::from_text("legacy-player-token");
    assert_eq!(legacy.player_for(&tv), Some("legacy-player-token"));
    assert_eq!(legacy.gvs_for(&tv), None);
    assert_eq!(legacy.player_for(&android), None);
    assert_eq!(legacy.gvs_for(&android), None);
    assert!(!legacy.has_scoped_playback_token(&android));
}

#[test]
fn player_response_copy_budget_accepts_the_exact_limit_and_rejects_one_more_byte() {
    let mut exact = Vec::new();
    assert!(append_player_response_chunk(
        &mut exact,
        &vec![b'x'; PLAYER_RESPONSE_COPY_LIMIT]
    ));
    assert_eq!(exact.len(), PLAYER_RESPONSE_COPY_LIMIT);

    assert!(!append_player_response_chunk(&mut exact, b"x"));
    assert_eq!(exact.len(), PLAYER_RESPONSE_COPY_LIMIT);

    let mut crossing = vec![b'x'; PLAYER_RESPONSE_COPY_LIMIT - 1];
    assert!(!append_player_response_chunk(&mut crossing, b"yz"));
    assert_eq!(crossing.len(), PLAYER_RESPONSE_COPY_LIMIT);
    assert_eq!(crossing.last(), Some(&b'y'));
}

#[test]
fn player_response_fixtures_classify_provider_failures() {
    let cases = [
        (
            include_str!("../../fixtures/player_login_required.json"),
            AudioSourceErrorKind::Authentication,
        ),
        (
            include_str!("../../fixtures/player_age_required.json"),
            AudioSourceErrorKind::ConsentAgeOrRegion,
        ),
        (
            include_str!("../../fixtures/player_unavailable.json"),
            AudioSourceErrorKind::Unavailable,
        ),
        (
            include_str!("../../fixtures/player_missing_streaming_data.json"),
            AudioSourceErrorKind::Contract,
        ),
    ];

    for (fixture, expected) in cases {
        let response: PlayerResponse = serde_json::from_str(fixture).unwrap();
        let error = playable_formats(response).unwrap_err();
        assert_eq!(error.kind, expected);
    }
}

#[test]
fn browser_auth_signs_the_cookie_without_exposing_it() {
    assert_eq!(
        cookie_value("SID=one; SAPISID=sample-sapisid; X=two", "SAPISID"),
        Some("sample-sapisid")
    );
    assert_eq!(
        sapisid_authorization("sample-sapisid", 123),
        "SAPISIDHASH 123_9f656697f2f8403ce20dffb37175246584e6a017"
    );
    let RequestAuth::Browser(headers) =
        browser_request_auth("SAPISID=sample-sapisid; VISITOR_INFO1_LIVE=visitor").unwrap()
    else {
        panic!("expected browser playback authentication");
    };
    assert_eq!(headers[reqwest::header::ORIGIN], "https://www.youtube.com");
    assert_eq!(headers["x-goog-authuser"], "0");
    assert_eq!(headers["x-goog-visitor-id"], "visitor");
    assert!(headers[reqwest::header::AUTHORIZATION]
        .to_str()
        .unwrap()
        .starts_with("SAPISIDHASH "));
}

#[test]
fn derives_the_cookie_capable_downgraded_tv_client_version() {
    let page = r#"{"INNERTUBE_CONTEXT_CLIENT_VERSION":"7.20260708.13.02"}"#;
    assert_eq!(parse_tv_client_version(page).as_deref(), Some("5.20260708"));
    assert_eq!(parse_tv_client_version("{}"), None);
}

#[test]
fn extracts_the_player_signature_timestamp_without_retaining_page_data() {
    assert_eq!(parse_signature_timestamp(r#"{"STS": 20413}"#), Some(20413));
    assert_eq!(
        parse_signature_timestamp(r#"{"signatureTimestamp": 20414}"#),
        Some(20414)
    );
    assert_eq!(parse_signature_timestamp(r#"{"STS": 123}"#), None);
}

#[test]
fn extracts_guest_visitor_data_from_webpage_configuration() {
    assert_eq!(
        parse_visitor_data(r#"ytcfg.set({"VISITOR_DATA":"guest\u003dvalue"});"#).as_deref(),
        Some("guest=value")
    );
    assert_eq!(
        parse_visitor_data(r#"{"responseContext":{"visitorData":"response-visitor"}}"#)
            .as_deref(),
        Some("response-visitor")
    );
    assert_eq!(parse_visitor_data(r#"{"visitorData":""}"#), None);
    assert_eq!(parse_visitor_data(r#"{"visitorData":42}"#), None);
}

#[cfg(feature = "private-capture")]
#[test]
fn accepts_current_web_client_versions_and_rejects_other_profiles() {
    let page = r#"{"INNERTUBE_CONTEXT_CLIENT_VERSION":"2.20260708.00.00"}"#;
    assert_eq!(
        parse_innertube_client_version(page, "2."),
        Some("2.20260708.00.00".to_owned())
    );
    assert_eq!(parse_innertube_client_version(page, "1."), None);
}

#[cfg(feature = "private-capture")]
#[test]
fn routes_web_music_to_its_host_without_rewriting_fixture_endpoints() {
    let resolver = fixture_resolver(
        reqwest::Url::parse("https://www.youtube.com/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let web_music =
        PlayerClient::web_music("1.fixture-remix".to_owned(), PlayerVersionSource::Fallback);
    assert_eq!(
        resolver
            .player_endpoint_for_client_for_test(&web_music)
            .host_str(),
        Some("music.youtube.com")
    );

    let fixture_resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:9/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    assert_eq!(
        fixture_resolver
            .player_endpoint_for_client_for_test(&web_music)
            .host_str(),
        Some("127.0.0.1")
    );
}

#[cfg(feature = "private-capture")]
#[test]
fn web_music_player_requests_use_the_music_origin() {
    let auth = browser_request_auth("SAPISID=fixture-session").unwrap();
    let client =
        PlayerClient::web_music("1.fixture-remix".to_owned(), PlayerVersionSource::Fallback);
    let request = build_player_request(
        &reqwest::Client::new(),
        reqwest::Url::parse("https://music.youtube.com/youtubei/v1/player?key=fixture").unwrap(),
        "fixture-video",
        &auth,
        &client,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        request.headers()[reqwest::header::ORIGIN],
        YOUTUBE_MUSIC_ORIGIN
    );
    assert_eq!(request.headers()["x-origin"], YOUTUBE_MUSIC_ORIGIN);
}
