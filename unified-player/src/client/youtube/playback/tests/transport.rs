#[test]
fn media_redirect_candidates_extract_allowlisted_videoplayback_urls() {
    let body = b"fixture-prefix\0https://rr4---sn-4g5edndk.googlevideo.com/videoplayback?expire=1&sig=fixture\0";

    let candidates = media_redirect_candidates(body);

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].host_str(),
        Some("rr4---sn-4g5edndk.googlevideo.com")
    );
    assert_eq!(candidates[0].path(), "/videoplayback");
}

#[test]
fn media_redirect_candidates_reject_unallowlisted_or_non_media_urls() {
    let body = b"https://example.com/videoplayback?sig=bad\0https://rr4.googlevideo.com/watch?v=not-media\0";

    assert!(media_redirect_candidates(body).is_empty());
}

#[test]
fn media_probe_response_class_preserves_range_and_redirect_outcomes() {
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::OK, true, false),
        "http_200_embedded_redirect"
    );
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::OK, false, false),
        "http_200_range_contract"
    );
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::PARTIAL_CONTENT, false, true),
        "http_206_valid_range"
    );
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::PARTIAL_CONTENT, false, false),
        "http_206_range_contract"
    );
}

#[test]
fn media_probe_response_class_coarsens_unexpected_http_statuses() {
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::FORBIDDEN, false, false),
        "http_403_forbidden"
    );
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::TOO_MANY_REQUESTS, false, false),
        "http_4xx"
    );
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::BAD_GATEWAY, false, false),
        "http_5xx"
    );
    assert_eq!(
        media_probe_response_class(reqwest::StatusCode::FOUND, false, false),
        "http_other"
    );
}

#[test]
fn decoder_media_probe_outcome_requires_a_valid_range() {
    assert_eq!(
        decoder_media_probe_outcome(reqwest::StatusCode::PARTIAL_CONTENT, true),
        ("success", None)
    );
    assert_eq!(
        decoder_media_probe_outcome(reqwest::StatusCode::FORBIDDEN, false),
        ("error", Some("media_forbidden"))
    );
    assert_eq!(
        decoder_media_probe_outcome(reqwest::StatusCode::OK, false),
        ("error", Some("media_range_contract"))
    );
}

#[test]
fn transport_diagnostic_display_contains_only_safe_statuses() {
    let diagnostic = MediaTransportDiagnostic::new_for_test(
        [403, 200, 200, 200, 206, 403],
        [true, true, true],
        true,
    );

    assert_eq!(
        diagnostic.to_string(),
        "request-only-current=403 browser=200 exact=200 raw-current-headers=200 sanitized-browser-headers=206 current=403 captured-query={ump:true,srfvp:true,range:true} player-required-browser=true"
    );
}

#[test]
fn exact_replay_leaves_transport_headers_to_reqwest() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HOST,
        reqwest::header::HeaderValue::from_static("r1.googlevideo.com"),
    );
    headers.insert(
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::HeaderValue::from_static("0"),
    );
    headers.insert(
        reqwest::header::CONNECTION,
        reqwest::header::HeaderValue::from_static("keep-alive"),
    );
    headers.insert(
        reqwest::header::COOKIE,
        reqwest::header::HeaderValue::from_static("fixture=session"),
    );

    let replay = replayable_browser_headers(headers);

    assert!(!replay.contains_key(reqwest::header::HOST));
    assert!(!replay.contains_key(reqwest::header::CONTENT_LENGTH));
    assert!(!replay.contains_key(reqwest::header::CONNECTION));
    assert_eq!(replay[reqwest::header::COOKIE], "fixture=session");
}
#[test]
fn browser_transport_is_used_only_for_recoverable_native_failures() {
    let browser = browser_request_auth("SAPISID=session; SID=signed-in").unwrap();
    let cipher = AudioSourceError::new(AudioSourceErrorKind::Decipher, "ciphered");
    let forbidden = AudioSourceError::new(AudioSourceErrorKind::MediaForbidden, "media forbidden");
    let authentication = AudioSourceError::new(AudioSourceErrorKind::Authentication, "expired");
    assert!(should_use_browser_session_transport(&browser, &cipher));
    assert!(should_retry_browser_session_transport(&browser, &forbidden));
    assert!(!should_use_browser_session_transport(
        &browser,
        &authentication
    ));
    assert!(!should_retry_browser_session_transport(
        &browser,
        &authentication
    ));
    assert!(!should_use_browser_session_transport(
        &RequestAuth::None,
        &cipher
    ));
    assert!(!should_retry_browser_session_transport(
        &RequestAuth::None,
        &forbidden
    ));
    assert!(!should_use_browser_session_transport(
        &RequestAuth::Bearer("token".to_string()),
        &cipher
    ));
    assert!(!should_retry_browser_session_transport(
        &RequestAuth::Bearer("token".to_string()),
        &forbidden
    ));
}

#[test]
fn bounded_media_ranges_keep_the_total_resource_length() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 0-4095/987654"),
    );
    assert_eq!(media_response_length(&headers), Some(987_654));
    assert_eq!(
        bounded_media_range_end(0, None),
        MEDIA_RANGE_CHUNK_BYTES - 1
    );
    assert_eq!(bounded_media_range_end(100, Some(199)), 199);
    assert_eq!(
        bounded_media_range_end(100, Some(u64::MAX)),
        100 + MEDIA_RANGE_CHUNK_BYTES - 1
    );
}

#[test]
fn media_ranges_use_the_standard_http_header() {
    assert_eq!(
        media_range_header(1_048_576, 1_114_111),
        "bytes=1048576-1114111"
    );
}

#[test]
fn diagnostic_chunk_override_changes_only_the_range_bound() {
    assert_eq!(
        bounded_media_range_end_for_chunk(0, None, 10 * 1024 * 1024),
        10 * 1024 * 1024 - 1
    );
    assert_eq!(
        bounded_media_range_end_for_chunk(100, Some(u64::MAX), 10 * 1024 * 1024),
        100 + 10 * 1024 * 1024 - 1
    );
}

#[test]
fn media_url_failover_candidates_follow_the_advertised_google_video_nodes() {
    let url = reqwest::Url::parse(
        "https://rr3---sn-primary.googlevideo.com/videoplayback?mn=sn-primary%2Csn-backup%2Csn-backup",
    )
    .unwrap();

    let candidates = media_url_failover_candidates(&url);

    assert_eq!(
        candidates
            .iter()
            .filter_map(|candidate| candidate.host_str())
            .collect::<Vec<_>>(),
        ["rr3---sn-backup.googlevideo.com"]
    );
}

#[test]
fn media_url_failover_does_not_invent_hosts_without_a_google_video_route() {
    let url = reqwest::Url::parse(
        "https://r1.googlevideo.com/videoplayback?mn=sn-primary%2Csn-backup",
    )
    .unwrap();

    assert!(media_url_failover_candidates(&url).is_empty());
}

#[test]
fn media_url_failover_preserves_fully_qualified_nodes() {
    let url = reqwest::Url::parse(
        "https://rr3---sn-primary.googlevideo.com/videoplayback?mn=rr4---sn-backup.googlevideo.com",
    )
    .unwrap();

    assert_eq!(
        media_url_failover_candidates(&url)[0].host_str(),
        Some("rr4---sn-backup.googlevideo.com")
    );
}

#[test]
fn media_probe_targets_distinguish_advertised_and_embedded_failover() {
    assert_eq!(media_probe_target(0, 2), "primary");
    assert_eq!(media_probe_target(1, 2), "advertised_failover");
    assert_eq!(media_probe_target(2, 2), "embedded_redirect");
}
