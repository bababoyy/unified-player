#[test]
fn audio_source_error_kinds_project_to_stable_safe_categories() {
    use crate::observability::ErrorCategory;

    let cases = [
        (
            AudioSourceErrorKind::Authentication,
            ErrorCategory::Authentication,
        ),
        (
            AudioSourceErrorKind::ConsentAgeOrRegion,
            ErrorCategory::ConsentAgeRegion,
        ),
        (
            AudioSourceErrorKind::Unavailable,
            ErrorCategory::ProviderUnavailable,
        ),
        (AudioSourceErrorKind::ProofToken, ErrorCategory::ProofToken),
        (AudioSourceErrorKind::Decipher, ErrorCategory::Decipher),
        (
            AudioSourceErrorKind::RateLimited,
            ErrorCategory::RateLimited,
        ),
        (AudioSourceErrorKind::Network, ErrorCategory::Network),
        (AudioSourceErrorKind::Cancelled, ErrorCategory::Cancelled),
        (AudioSourceErrorKind::Contract, ErrorCategory::Contract),
        (
            AudioSourceErrorKind::UnsupportedFormat,
            ErrorCategory::UnsupportedFormat,
        ),
        (
            AudioSourceErrorKind::MediaForbidden,
            ErrorCategory::MediaForbidden,
        ),
        (
            AudioSourceErrorKind::MediaRangeContract,
            ErrorCategory::MediaRangeContract,
        ),
    ];

    for (kind, expected) in cases {
        assert_eq!(kind.diagnostic_category(), expected);
    }
    assert_eq!(
        super::diagnostic_category_inventory(),
        "authentication,consent_age_region,provider_unavailable,proof_token,decipher,rate_limited,network,cancelled,contract,unsupported_format,media_forbidden,media_range_contract"
    );
}

#[test]
fn resolved_source_debug_redacts_url_and_headers() {
    let mut selected = select_audio_format(
        vec![format(
            Some("https://r1.googlevideo.com/videoplayback?expire=300&secret=value"),
            "audio/mp4; codecs=\"mp4a.40.2\"",
            130_000,
        )],
        crate::config::YouTubePlaybackQuality::High,
        "TEST",
    )
    .unwrap();
    selected.required_headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_static("Bearer secret"),
    );
    let debug = format!("{selected:?}");
    assert!(!debug.contains("secret=value"));
    assert!(!debug.contains("Bearer secret"));
    assert!(debug.contains("redacted"));
}

#[test]
fn native_player_sources_carry_their_player_user_agent() {
    for (source_client, expected_user_agent) in [
        ("WEB", super::player::WEB_USER_AGENT),
        ("WEB_REMIX", super::player::WEB_USER_AGENT),
        ("VISIONOS", super::player::VISIONOS_USER_AGENT),
    ] {
        let mut source = select_audio_format(
            vec![format(
                Some("https://r1.googlevideo.com/videoplayback?expire=300&sig=fixture"),
                "audio/mp4; codecs=\"mp4a.40.2\"",
                130_000,
            )],
            crate::config::YouTubePlaybackQuality::High,
            source_client,
        )
        .unwrap();

        super::transport::prepare_resolved_source(&mut source, "fixture-video");

        assert_eq!(
            source.required_headers[reqwest::header::USER_AGENT],
            expected_user_agent
        );
    }
}

#[test]
fn audio_source_error_has_a_fixed_message_and_no_private_source_chain() {
    use std::error::Error as _;

    let error = AudioSourceError::new(
        AudioSourceErrorKind::Network,
        "request YouTube playback source",
    );
    let rendered = format!("{error:?}\n{error}");

    assert!(rendered.contains("request YouTube playback source"));
    assert!(!rendered.contains("credential="));
    assert!(!rendered.contains("fixture-secret"));
    assert!(error.source().is_none());
}
