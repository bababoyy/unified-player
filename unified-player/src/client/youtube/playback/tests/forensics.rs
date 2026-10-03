#[cfg(feature = "private-capture")]
#[test]
#[ignore = "local Phase 7 performance evidence"]
fn disabled_private_capture_hook_stays_below_one_microsecond_median() {
    const SAMPLE_COUNT: usize = 101;
    const CALLS_PER_SAMPLE: u128 = 1_000;

    let client = super::PlayerClient::android_vr();
    let headers = reqwest::header::HeaderMap::new();
    let exchange_ref = crate::developer_capture::ExchangeRef::from_bytes([0x71; 8]);
    let capture: Option<&crate::developer_capture::CaptureSession> = None;
    for _ in 0..1_000 {
        super::capture_player_http_response(
            std::hint::black_box(capture),
            exchange_ref,
            &client,
            reqwest::StatusCode::OK,
            None,
            &headers,
            &[],
            Duration::ZERO,
            true,
            false,
        );
    }

    let mut samples_ns = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let started = std::time::Instant::now();
        for _ in 0..CALLS_PER_SAMPLE {
            super::capture_player_http_response(
                std::hint::black_box(capture),
                exchange_ref,
                &client,
                reqwest::StatusCode::OK,
                None,
                &headers,
                &[],
                Duration::ZERO,
                true,
                false,
            );
        }
        samples_ns.push(started.elapsed().as_nanos() / CALLS_PER_SAMPLE);
    }
    samples_ns.sort_unstable();
    let median_ns = samples_ns[SAMPLE_COUNT / 2];
    eprintln!("phase7.performance.disabled_hook_median_ns={median_ns}");
    assert!(
        median_ns < 1_000,
        "disabled private capture hook median was {median_ns} ns"
    );
}

#[cfg(feature = "private-capture")]
#[test]
fn native_capture_headers_accept_exact_limits_and_reject_overflow() {
    let mut exact = reqwest::header::HeaderMap::new();
    exact.insert(
        "x",
        reqwest::header::HeaderValue::from_bytes(&vec![
            b'a';
            super::PRIVATE_CAPTURE_HEADER_BYTES_LIMIT
                - 1
        ])
        .unwrap(),
    );
    assert!(super::bounded_private_header_map(&exact).is_some());

    exact.insert(
        "x",
        reqwest::header::HeaderValue::from_bytes(&vec![
            b'a';
            super::PRIVATE_CAPTURE_HEADER_BYTES_LIMIT
        ])
        .unwrap(),
    );
    assert!(super::bounded_private_header_map(&exact).is_none());
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn developer_inspection_reports_player_facts_without_playback_io() {
    let (endpoint, request_body, server) =
        serve_one_player_response(200, include_bytes!("../../fixtures/player_direct.json"));
    let resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let report = resolver
        .inspect_player_response(
            "fixture-direct",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.join().unwrap();
    let request: serde_json::Value =
        serde_json::from_slice(&request_body.recv().unwrap()).expect("player request is JSON");
    assert_eq!(request["videoId"], "fixture-direct");
    assert_eq!(request["context"]["client"]["hl"], "en");
    assert_eq!(request["context"]["client"]["gl"], "US");
    assert_eq!(request["context"]["client"]["timeZone"], "UTC");
    assert_eq!(request["context"]["client"]["utcOffsetMinutes"], 0);
    assert_eq!(
        request["playbackContext"]["contentPlaybackContext"]["html5Preference"],
        "HTML5_PREF_WANTS"
    );
    assert_eq!(report.video_id, "fixture-direct");
    assert_eq!(report.auth_kind, "none");
    assert_eq!(report.http_status, 200);
    assert_eq!(report.playability_status, "OK");
    assert_eq!(report.adaptive_format_count, 1);
    assert_eq!(report.direct_audio_count, 1);
    assert_eq!(report.cipher_audio_count, 0);
    assert_eq!(report.selected_audio_itag, Some(140));
    assert_eq!(report.selection, "native_selected");
    let rendered = serde_json::to_string(&report).unwrap();
    assert!(!rendered.contains("googlevideo"));
    assert!(!rendered.contains("expire="));

    let (endpoint, request_body, server) = serve_one_player_response(
        200,
        include_bytes!("../../fixtures/player_cipher_only.json"),
    );
    let resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let report = resolver
        .inspect_player_response(
            "fixture-cipher",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.join().unwrap();
    let _ = request_body.recv().unwrap();
    assert_eq!(report.direct_audio_count, 0);
    assert_eq!(report.cipher_audio_count, 1);
    assert_eq!(report.selected_audio_itag, None);
    assert_eq!(report.selection, "decipher");

    let (endpoint, request_body, server) = serve_one_player_response(
        200,
        include_bytes!("../../fixtures/player_unavailable.json"),
    );
    let resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let report = resolver
        .inspect_player_response(
            "fixture-unavailable",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.join().unwrap();
    let _ = request_body.recv().unwrap();
    assert_eq!(report.playability_status, "UNPLAYABLE");
    assert_eq!(
        report.playability_reason.as_deref(),
        Some("Media unavailable")
    );
    assert_eq!(report.selection, "provider_unavailable");
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn developer_inspection_probes_the_browser_client_matrix_in_order() {
    let (endpoint, request_bodies, server) = serve_player_response_sequence(vec![
        (
            200,
            include_bytes!("../../fixtures/player_unavailable.json"),
        ),
        (200, include_bytes!("../../fixtures/player_direct.json")),
        (
            200,
            include_bytes!("../../fixtures/player_cipher_only.json"),
        ),
    ]);
    let mut resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    resolver.set_inspection_clients_for_test(vec![
        PlayerClient::tv("5.fixture-tv".to_owned(), PlayerVersionSource::Fallback),
        PlayerClient::web("2.fixture-web".to_owned(), PlayerVersionSource::Discovered),
        PlayerClient::web_music(
            "1.fixture-remix".to_owned(),
            PlayerVersionSource::Discovered,
        ),
    ]);

    let report = resolver
        .inspect_player_response(
            "fixture-matrix",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.join().unwrap();

    let mut request_clients = Vec::new();
    for request_body in request_bodies.try_iter() {
        let request: serde_json::Value =
            serde_json::from_slice(&request_body).expect("player request is JSON");
        request_clients.push(
            request["context"]["client"]["clientName"]
                .as_str()
                .expect("client name")
                .to_owned(),
        );
    }
    assert_eq!(request_clients, ["TVHTML5", "WEB", "WEB_REMIX"]);
    assert_eq!(report.client.context_name, "TVHTML5");
    assert_eq!(report.client_matrix.len(), 3);
    assert_eq!(report.client_matrix[0].selection, "provider_unavailable");
    assert_eq!(report.client_matrix[1].selection, "native_selected");
    assert_eq!(report.client_matrix[1].selected_audio_itag, Some(140));
    assert_eq!(report.client_matrix[2].selection, "decipher");
    let rendered = serde_json::to_string(&report).unwrap();
    assert!(!rendered.contains("googlevideo"));
    assert!(!rendered.contains("expire="));
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn developer_inspection_reports_auth_failure_as_matrix_facts() {
    let mut resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );
    resolver.set_inspection_clients_for_test(vec![
        PlayerClient::tv("5.fixture-tv".to_owned(), PlayerVersionSource::Fallback),
        PlayerClient::web("2.fixture-web".to_owned(), PlayerVersionSource::Discovered),
        PlayerClient::web_music(
            "1.fixture-remix".to_owned(),
            PlayerVersionSource::Discovered,
        ),
    ]);

    let report = resolver
        .inspect_player_response(
            "fixture-auth-failure",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(report.auth_kind, "browser");
    assert_eq!(report.client_matrix.len(), 3);
    assert!(report.client_matrix.iter().all(|attempt| {
        attempt.playability_status == "request_error"
            && attempt.selection == "authentication"
            && attempt.error_category.as_deref() == Some("authentication")
    }));
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn player_transport_matrix_preserves_fixed_error_categories() {
    let cases: &[(u16, &'static [u8], AudioSourceErrorKind)] = &[
        (401, b"{}", AudioSourceErrorKind::Unavailable),
        (403, b"{}", AudioSourceErrorKind::Unavailable),
        (429, b"{}", AudioSourceErrorKind::RateLimited),
        (500, b"{}", AudioSourceErrorKind::Network),
        (200, b"", AudioSourceErrorKind::Contract),
        (200, b"{", AudioSourceErrorKind::Contract),
        (200, b"\xff", AudioSourceErrorKind::Contract),
        (
            200,
            include_bytes!("../../fixtures/player_login_required.json"),
            AudioSourceErrorKind::Unavailable,
        ),
        (
            200,
            include_bytes!("../../fixtures/player_age_required.json"),
            AudioSourceErrorKind::ConsentAgeOrRegion,
        ),
        (
            200,
            include_bytes!("../../fixtures/player_missing_streaming_data.json"),
            AudioSourceErrorKind::Contract,
        ),
    ];

    for &(status, body, expected) in cases {
        let (endpoint, _, server) = serve_one_player_response(status, body);
        let resolver = fixture_resolver(
            endpoint,
            crate::config::YouTubeMusicAuthType::Unauthenticated,
        );
        let error = resolver
            .resolve(
                "fixture-transport-media",
                crate::config::YouTubePlaybackQuality::High,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        server.join().unwrap();
        assert_eq!(
            error.kind, expected,
            "unexpected category for HTTP {status}"
        );
    }

    let (endpoint, server) = serve_closed_player_connection();
    let resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let error = resolver
        .resolve(
            "fixture-closed-connection",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    server.join().unwrap();
    assert_eq!(error.kind, AudioSourceErrorKind::Network);

    let (endpoint, server) =
        serve_redirected_player_response(include_bytes!("../../fixtures/player_unavailable.json"));
    let resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let error = resolver
        .resolve(
            "fixture-redirected-media",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    server.join().unwrap();
    assert_eq!(error.kind, AudioSourceErrorKind::Unavailable);

    let (endpoint, head_received, server) =
        serve_delayed_player_body(include_bytes!("../../fixtures/player_unavailable.json"));
    let mut resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    resolver.set_http_client_for_test(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap(),
    );
    let error = resolver
        .resolve(
            "fixture-timeout-media",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    head_received.recv_timeout(Duration::from_secs(1)).unwrap();
    server.join().unwrap();
    assert_eq!(error.kind, AudioSourceErrorKind::Network);
}

#[cfg(feature = "private-capture")]
async fn wait_for_capture_state(
    handle: &crate::developer_capture::CaptureHandle,
    expected: crate::developer_capture::SafeCaptureState,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while handle.snapshot().state != expected {
        if std::time::Instant::now() >= deadline {
            panic!(
                "private capture did not reach {expected:?}; final snapshot: {:?}",
                handle.snapshot()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn media_source_commits_success_or_unrecovered_failure_after_acceptance() {
    use crate::developer_capture::{
        CaptureLimits, CapturePassphrase, CapturePurpose, CaptureRecordKind, CaptureStore,
        EndpointRole, ExchangeRef, PrivateField, PrivatePayloadKind, ProviderClientKind,
        SafeCaptureState, SafeOperationRef, TransportKind,
    };

    for (seed, inject_failure, expected_terminal) in
        [(18_u8, false, "success"), (28_u8, true, "failed")]
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits::default();
        let (handle, worker, _) = crate::developer_capture::prepare_runtime(&root, limits).unwrap();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
        handle.request_arm().unwrap();
        handle
            .accept_consent(
                CapturePassphrase::new("media lifecycle fixture passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([seed; 4]),
            )
            .unwrap()
            .unwrap();
        let capture_ref = session.capture_ref();
        let request_payload = crate::developer_capture::encode_private_fields(
            PrivatePayloadKind::Operation,
            &[PrivateField::text(
                crate::developer_capture::private_field::STAGE,
                "fixture_player_request",
            )],
        )
        .unwrap();
        assert_eq!(
            session.record_with_context(
                Some(ExchangeRef::from_bytes([seed.saturating_add(1); 8])),
                EndpointRole::PlayerApi,
                ProviderClientKind::TvHtml5,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpRequest,
                request_payload,
            ),
            crate::developer_capture::RecordOutcome::Accepted
        );

        let evidence = super::MediaPrivateEvidence::new(
            session,
            ExchangeRef::from_bytes([seed.saturating_add(2); 8]),
        );
        if inject_failure {
            super::record_media_failure(&evidence, 2, "followup_range", "media_forbidden");
        }
        let lifecycle = super::MediaCaptureLifecycle::new(evidence, CancellationToken::new());
        let media_capture = super::MediaCaptureHandle::new(lifecycle.clone());
        let source = rodio::buffer::SamplesBuffer::new(1, 44_100, vec![0.0_f32]);
        let mut source = super::CapturedMediaSource::new(Box::new(source), lifecycle);

        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), None);
        assert_eq!(handle.snapshot().state, SafeCaptureState::Capturing);
        media_capture.record_audio_output("completed", Duration::ZERO);
        media_capture.commit();
        wait_for_capture_state(&handle, SafeCaptureState::Ready).await;
        drop(source);
        shutdown.cancel();
        worker_thread.join().unwrap().unwrap();

        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let capture = store
            .read_private(
                capture_ref,
                &CapturePassphrase::new("media lifecycle fixture passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        assert_eq!(
            capture
                .records()
                .iter()
                .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
                .count(),
            1
        );
        let terminal = capture
            .records()
            .iter()
            .find(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
            .unwrap();
        let terminal = crate::developer_capture::decode_private_fields(terminal.payload()).unwrap();
        assert_eq!(
            terminal.field_bytes(crate::developer_capture::private_field::OUTCOME),
            Some(expected_terminal.as_bytes())
        );
        assert!(capture.records().iter().any(|record| {
            if record.kind() != CaptureRecordKind::DecodeStage {
                return false;
            }
            let payload =
                crate::developer_capture::decode_private_fields(record.payload()).unwrap();
            payload.field_bytes(crate::developer_capture::private_field::STAGE)
                == Some(b"media_range_retention")
                && payload.field_u64(crate::developer_capture::private_field::DROPPED_COUNT)
                    == Some(0)
        }));
    }
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn media_range_capture_retains_bounded_metadata_and_zero_body_bytes() {
    use crate::developer_capture::{
        private_field, CaptureLimits, CapturePassphrase, CapturePurpose, CaptureRecordKind,
        CaptureStore, ExchangeRef, PrivatePayloadKind, SafeCaptureState, SafeOperationRef,
        SafeTerminalCategory,
    };
    use futures::StreamExt as _;
    use stream_download::http::{Client as _, ClientResponse as _};

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("vault");
    let limits = CaptureLimits::default();
    let (handle, worker, _) = crate::developer_capture::prepare_runtime(&root, limits).unwrap();
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
    handle.request_arm().unwrap();
    handle
        .accept_consent(
            CapturePassphrase::new("media metadata fixture passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let session = handle
        .claim(
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([12; 4]),
        )
        .unwrap()
        .unwrap();
    let capture_ref = session.capture_ref();
    let evidence =
        super::MediaPrivateEvidence::new(session.clone(), ExchangeRef::from_bytes([13; 8]));
    let client =
        super::MediaHttpClient::new(reqwest::header::HeaderMap::new(), Some(evidence)).unwrap();
    let (url, server) = serve_media_range_responses(2);
    let redacted = super::RedactedMediaUrl(url);

    let first = client.get(&redacted).await.unwrap();
    let mut first_stream = first.stream();
    assert!(!first_stream.next().await.unwrap().unwrap().is_empty());
    let second = client.get_range(&redacted, 8, Some(15)).await.unwrap();
    let mut second_stream = second.stream();
    assert!(!second_stream.next().await.unwrap().unwrap().is_empty());
    server.join().unwrap();
    session.finish(SafeTerminalCategory::Success);
    wait_for_capture_state(&handle, SafeCaptureState::Incomplete).await;
    shutdown.cancel();
    worker_thread.join().unwrap().unwrap();

    let (store, _) = CaptureStore::open(&root, limits).unwrap();
    let capture = store
        .read_private(
            capture_ref,
            &CapturePassphrase::new("media metadata fixture passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let probes = capture
        .records()
        .iter()
        .filter(|record| record.kind() == CaptureRecordKind::MediaProbe)
        .collect::<Vec<_>>();
    assert_eq!(probes.len(), 2);
    for probe in probes {
        let payload = crate::developer_capture::decode_private_fields(probe.payload()).unwrap();
        assert_eq!(payload.kind(), PrivatePayloadKind::MediaProbe);
        assert_eq!(payload.field_u64(private_field::STATUS), Some(206));
        assert_eq!(
            payload.field_bool(private_field::CONTENT_RANGE_PRESENT),
            Some(true)
        );
        assert!(payload.field_bytes(private_field::URL).is_some());
        assert!(payload.field_bytes(private_field::BODY).is_none());
    }
    assert_eq!(
        capture
            .records()
            .iter()
            .filter(|record| record.kind() == CaptureRecordKind::DecodeStage)
            .count(),
        1
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn media_continuation_matrix_distinguishes_contract_and_transport_failures() {
    use crate::developer_capture::ExchangeRef;

    let endpoint = reqwest::Url::parse("http://127.0.0.1:9/youtubei/v1/player").unwrap();
    let mut resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let cases = [
        (206, Some("bytes 512-1023/1024"), None),
        (403, None, Some(AudioSourceErrorKind::MediaForbidden)),
        (200, None, Some(AudioSourceErrorKind::MediaRangeContract)),
        (
            206,
            Some("bytes 0-511/1024"),
            Some(AudioSourceErrorKind::MediaRangeContract),
        ),
    ];
    for (status, content_range, expected_error) in cases {
        let (url, server) = serve_one_media_probe(status, content_range, Duration::ZERO);
        let result = resolver
            .verify_media_continuation_for_test(
                &media_probe_source(url),
                &CancellationToken::new(),
                None,
                ExchangeRef::from_bytes([14; 8]),
            )
            .await;
        server.join().unwrap();
        assert_eq!(result.err().map(|error| error.kind), expected_error);
    }

    let (url, server) = serve_redirected_media_probe();
    resolver
        .verify_media_continuation_for_test(
            &media_probe_source(url),
            &CancellationToken::new(),
            None,
            ExchangeRef::from_bytes([15; 8]),
        )
        .await
        .unwrap();
    server.join().unwrap();

    let (url, server) = serve_embedded_redirect_media_probe();
    let result = resolver
        .verify_media_continuation_for_test(
            &media_probe_source(url),
            &CancellationToken::new(),
            None,
            ExchangeRef::from_bytes([18; 8]),
        )
        .await;
    server.join().unwrap();
    result.unwrap();

    let (url, server) =
        serve_one_media_probe(206, Some("bytes 512-1023/1024"), Duration::from_millis(250));
    resolver.set_http_client_for_test(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(25))
            .build()
            .unwrap(),
    );
    let error = resolver
        .verify_media_continuation_for_test(
            &media_probe_source(url),
            &CancellationToken::new(),
            None,
            ExchangeRef::from_bytes([16; 8]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Network);
    server.join().unwrap();

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = resolver
        .verify_media_continuation_for_test(
            &media_probe_source(reqwest::Url::parse("http://127.0.0.1:9/videoplayback").unwrap()),
            &cancellation,
            None,
            ExchangeRef::from_bytes([17; 8]),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Cancelled);
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn cancellation_after_response_headers_retains_one_incomplete_response_and_terminal() {
    use crate::developer_capture::{
        private_field, CaptureLimits, CapturePassphrase, CapturePurpose, CaptureRecordKind,
        CaptureStore, SafeCaptureState, SafeOperationRef, SafeTerminalCategory,
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("vault");
    let limits = CaptureLimits::default();
    let (handle, worker, _) = crate::developer_capture::prepare_runtime(&root, limits).unwrap();
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
    handle.request_arm().unwrap();
    handle
        .accept_consent(
            CapturePassphrase::new("post-head cancellation passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let session = handle
        .claim(
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([6; 4]),
        )
        .unwrap()
        .unwrap();
    let capture_ref = session.capture_ref();
    let (endpoint, body_release, server_thread) =
        serve_blocked_player_body(include_bytes!("../../fixtures/player_unavailable.json"));
    let response_headers = std::sync::Arc::new(tokio::sync::Notify::new());
    let mut resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    resolver.set_response_headers_observer_for_test(response_headers.clone());
    let cancellation = CancellationToken::new();
    let cancel_after_head = cancellation.clone();
    let cancel_task = tokio::spawn(async move {
        response_headers.notified().await;
        cancel_after_head.cancel();
    });
    let error = resolver
        .resolve_with_capture(
            "fixture-cancelled-media",
            crate::config::YouTubePlaybackQuality::High,
            cancellation,
            session.clone(),
        )
        .await
        .unwrap_err();
    drop(body_release);
    assert_eq!(error.kind, AudioSourceErrorKind::Cancelled);
    session.finish(SafeTerminalCategory::Cancelled);
    cancel_task.await.unwrap();
    server_thread.join().unwrap();
    wait_for_capture_state(&handle, SafeCaptureState::Incomplete).await;
    shutdown.cancel();
    worker_thread.join().unwrap().unwrap();

    let (store, _) = CaptureStore::open(&root, limits).unwrap();
    let capture = store
        .read_private(
            capture_ref,
            &CapturePassphrase::new("post-head cancellation passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let responses = capture
        .records()
        .iter()
        .filter(|record| record.kind() == CaptureRecordKind::HttpResponse)
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 1);
    let response = crate::developer_capture::decode_private_fields(responses[0].payload()).unwrap();
    assert_eq!(response.field_u64(private_field::STATUS), Some(200));
    assert_eq!(
        response.field_bool(private_field::BODY_COMPLETE),
        Some(false)
    );
    assert_eq!(response.field_bytes(private_field::BODY), Some(&[][..]));
    assert_eq!(
        capture
            .records()
            .iter()
            .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
            .count(),
        1
    );
    assert_eq!(
        capture.records().last().map(|record| record.kind()),
        Some(CaptureRecordKind::TerminalOutcome)
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn authentication_failure_before_http_retains_typed_failure_and_one_terminal() {
    use crate::developer_capture::{
        CaptureLimits, CapturePassphrase, CapturePurpose, CaptureRecordKind, CaptureStore,
        SafeCaptureState, SafeOperationRef, SafeTerminalCategory,
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("vault");
    let limits = CaptureLimits::default();
    let (handle, worker, _) = crate::developer_capture::prepare_runtime(&root, limits).unwrap();
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
    handle.request_arm().unwrap();
    handle
        .accept_consent(CapturePassphrase::new("pre-http failure passphrase".to_owned()).unwrap())
        .unwrap();
    let session = handle
        .claim(
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([7; 4]),
        )
        .unwrap()
        .unwrap();
    let capture_ref = session.capture_ref();
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:9/youtubei/v1/player").unwrap(),
        crate::config::YouTubeMusicAuthType::Browser,
    );
    let error = resolver
        .resolve_with_capture(
            "fixture-pre-http-media",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
            session.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Authentication);
    session.finish(SafeTerminalCategory::Failed);
    wait_for_capture_state(&handle, SafeCaptureState::Incomplete).await;
    shutdown.cancel();
    worker_thread.join().unwrap().unwrap();

    let (store, _) = CaptureStore::open(&root, limits).unwrap();
    let capture = store
        .read_private(
            capture_ref,
            &CapturePassphrase::new("pre-http failure passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    assert!(!capture.records().iter().any(|record| matches!(
        record.kind(),
        CaptureRecordKind::HttpRequest | CaptureRecordKind::HttpResponse
    )));
    assert!(capture
        .records()
        .iter()
        .any(|record| record.kind() == CaptureRecordKind::AuthSelection));
    assert!(capture
        .records()
        .iter()
        .any(|record| record.kind() == CaptureRecordKind::NetworkFailure));
    assert_eq!(
        capture
            .records()
            .iter()
            .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
            .count(),
        1
    );
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn oversized_player_body_is_truncated_only_in_evidence_not_application_parsing() {
    use crate::developer_capture::{
        private_field, CaptureLimits, CapturePassphrase, CapturePurpose, CaptureRecordKind,
        CaptureStore, SafeCaptureState, SafeOperationRef, SafeTerminalCategory,
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("vault");
    let limits = CaptureLimits {
        player_response_bytes: 64,
        ..CaptureLimits::default()
    };
    let (handle, worker, _) = crate::developer_capture::prepare_runtime(&root, limits).unwrap();
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
    handle.request_arm().unwrap();
    handle
        .accept_consent(CapturePassphrase::new("oversized response passphrase".to_owned()).unwrap())
        .unwrap();
    let session = handle
        .claim(
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([8; 4]),
        )
        .unwrap()
        .unwrap();
    let capture_ref = session.capture_ref();
    let oversized_body: &'static [u8] = Box::leak(
        format!(
            "{{\"playabilityStatus\":{{\"status\":\"UNPLAYABLE\",\"reason\":\"{}\"}}}}",
            "private-provider-reason-".repeat(32)
        )
        .into_bytes()
        .into_boxed_slice(),
    );
    let (endpoint, _, server_thread) = serve_one_player_response(200, oversized_body);
    let resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let error = resolver
        .resolve_with_capture(
            "fixture-oversized-media",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
            session.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Unavailable);
    session.finish(SafeTerminalCategory::Failed);
    server_thread.join().unwrap();
    wait_for_capture_state(&handle, SafeCaptureState::Incomplete).await;
    shutdown.cancel();
    worker_thread.join().unwrap().unwrap();

    let (store, _) = CaptureStore::open(&root, limits).unwrap();
    let capture = store
        .read_private(
            capture_ref,
            &CapturePassphrase::new("oversized response passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let response_record = capture
        .records()
        .iter()
        .find(|record| record.kind() == CaptureRecordKind::HttpResponse)
        .unwrap();
    let response =
        crate::developer_capture::decode_private_fields(response_record.payload()).unwrap();
    assert_eq!(response.field_bytes(private_field::BODY).unwrap().len(), 64);
    assert_eq!(
        response.field_u64(private_field::BODY_LENGTH),
        Some(u64::try_from(oversized_body.len()).unwrap())
    );
    assert_eq!(
        response.field_u64(private_field::RETAINED_BODY_LENGTH),
        Some(64)
    );
    assert_eq!(
        response.field_bool(private_field::BODY_COMPLETE),
        Some(false)
    );
    assert!(capture.credential_values_present());
}

#[cfg(feature = "private-capture")]
#[tokio::test]
async fn native_player_403_exchange_retains_request_response_and_replay_facts() {
    use crate::developer_capture::{
        private_field, CaptureLimits, CapturePassphrase, CapturePurpose, CaptureRecordKind,
        CaptureStore, PrivatePayloadKind, SafeCaptureState, SafeOperationRef, SafeTerminalCategory,
    };

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("vault");
    let limits = CaptureLimits::default();
    let (handle, worker, _) = crate::developer_capture::prepare_runtime(&root, limits).unwrap();
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
    handle.request_arm().unwrap();
    handle
        .accept_consent(
            CapturePassphrase::new("player exchange fixture passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let session = handle
        .claim(
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([9; 4]),
        )
        .unwrap()
        .unwrap();
    let capture_ref = session.capture_ref();
    let response_body = include_bytes!("../../fixtures/player_unavailable.json");
    let (endpoint, sent_body, server_thread) = serve_one_player_response(403, response_body);
    let mut resolver = fixture_resolver(
        endpoint,
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let error = resolver
        .resolve_with_capture(
            "fixture-private-media-id",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
            session.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Unavailable);
    session.finish(SafeTerminalCategory::Failed);
    server_thread.join().unwrap();
    let sent_body = sent_body.recv_timeout(Duration::from_secs(1)).unwrap();
    let (plain_endpoint, plain_sent_body, plain_server_thread) =
        serve_one_player_response(403, response_body);
    resolver.set_player_endpoint_for_test(plain_endpoint);
    let plain_error = resolver
        .resolve(
            "fixture-private-media-id",
            crate::config::YouTubePlaybackQuality::High,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(plain_error.kind, error.kind);
    plain_server_thread.join().unwrap();
    assert_eq!(
        plain_sent_body
            .recv_timeout(Duration::from_secs(1))
            .unwrap(),
        sent_body
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while handle.snapshot().state != SafeCaptureState::Ready {
        assert!(
            std::time::Instant::now() < deadline,
            "captured exchange did not finalize"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    worker_thread.join().unwrap().unwrap();
    let (store, _) = CaptureStore::open(&root, limits).unwrap();
    let capture = store
        .read_private(
            capture_ref,
            &CapturePassphrase::new("player exchange fixture passphrase".to_owned()).unwrap(),
        )
        .unwrap();
    let mut request_body = None;
    let mut request_method = None;
    let mut request_url = None;
    let mut request_headers = None;
    let mut captured_response_body = None;
    let mut correlated_exchange = None;
    let mut captured_auth_kind = None;
    let mut captured_proof_token_present = None;
    let mut captured_quality = None;
    let mut terminal_count = 0;
    for record in capture.records() {
        match record.kind() {
            CaptureRecordKind::HttpRequest => {
                let decoded =
                    crate::developer_capture::decode_private_fields(record.payload()).unwrap();
                assert_eq!(decoded.kind(), PrivatePayloadKind::HttpRequest);
                request_body = decoded
                    .field_bytes(private_field::BODY)
                    .map(ToOwned::to_owned);
                request_method = decoded
                    .field_bytes(private_field::METHOD)
                    .map(ToOwned::to_owned);
                request_url = decoded
                    .field_bytes(private_field::URL)
                    .map(ToOwned::to_owned);
                request_headers = decoded
                    .field_bytes(private_field::HEADERS)
                    .map(ToOwned::to_owned);
                assert_eq!(decoded.field_bool(private_field::BODY_COMPLETE), Some(true));
                assert_eq!(
                    decoded.field_u64(private_field::BODY_LENGTH),
                    Some(u64::try_from(sent_body.len()).unwrap())
                );
                if let Some(expected) = correlated_exchange {
                    assert_eq!(record.exchange_ref(), Some(expected));
                } else {
                    correlated_exchange = record.exchange_ref();
                }
            }
            CaptureRecordKind::HttpResponse => {
                let decoded =
                    crate::developer_capture::decode_private_fields(record.payload()).unwrap();
                assert_eq!(decoded.kind(), PrivatePayloadKind::HttpResponse);
                assert_eq!(decoded.field_u64(private_field::STATUS), Some(403));
                captured_response_body = decoded
                    .field_bytes(private_field::BODY)
                    .map(ToOwned::to_owned);
                assert_eq!(decoded.field_bool(private_field::BODY_COMPLETE), Some(true));
                if let Some(expected) = correlated_exchange {
                    assert_eq!(record.exchange_ref(), Some(expected));
                } else {
                    correlated_exchange = record.exchange_ref();
                }
            }
            CaptureRecordKind::AuthSelection => {
                let decoded =
                    crate::developer_capture::decode_private_fields(record.payload()).unwrap();
                captured_auth_kind = decoded
                    .field_bytes(private_field::AUTH_KIND)
                    .map(ToOwned::to_owned);
                captured_proof_token_present =
                    decoded.field_bool(private_field::PROOF_TOKEN_PRESENT);
                captured_quality = decoded
                    .field_bytes(private_field::QUALITY)
                    .map(ToOwned::to_owned);
                if let Some(expected) = correlated_exchange {
                    assert_eq!(record.exchange_ref(), Some(expected));
                } else {
                    correlated_exchange = record.exchange_ref();
                }
            }
            CaptureRecordKind::PlayerParse
            | CaptureRecordKind::FormatInventory
            | CaptureRecordKind::SelectionDecision => {
                if let Some(expected) = correlated_exchange {
                    assert_eq!(record.exchange_ref(), Some(expected));
                } else {
                    correlated_exchange = record.exchange_ref();
                }
            }
            CaptureRecordKind::TerminalOutcome => {
                terminal_count += 1;
            }
            _ => {}
        }
    }
    assert_eq!(request_body.as_deref(), Some(sent_body.as_slice()));
    assert_eq!(request_method.as_deref(), Some(&b"POST"[..]));
    assert!(request_url
        .as_deref()
        .is_some_and(|url| url.ends_with(b"/youtubei/v1/player?key=fixture")));
    assert!(request_headers.as_deref().is_some_and(|headers| headers
        .windows(b"application/json".len())
        .any(|window| window == b"application/json")));
    assert_eq!(
        captured_response_body.as_deref(),
        Some(response_body.as_slice())
    );
    assert_eq!(captured_auth_kind.as_deref(), Some(&b"none"[..]));
    assert_eq!(captured_proof_token_present, Some(false));
    assert_eq!(captured_quality.as_deref(), Some(&b"high"[..]));
    assert_eq!(terminal_count, 1);
    assert_eq!(
        capture.records().last().map(|record| record.kind()),
        Some(CaptureRecordKind::TerminalOutcome)
    );
    assert!(capture.credential_values_present());
}
