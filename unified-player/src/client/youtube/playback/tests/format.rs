#[tokio::test]
async fn ejs_challenges_are_batched_and_cached_per_player_script() {
    if which::which("node").is_err() && which::which("nodejs").is_err() {
        return;
    }
    let player_script = r#"(function(){
function R(){this.v=new Map();}
R.prototype.set=function(k,v){this.v.set(k,v);};
R.prototype.get=function(k){return this.v.get(k);};
R.prototype.clone=function(){return this;};
R.prototype.transform=function(){var s=this.v.get("s");if(s)this.v.set("s",s.split("").reverse().join(""));var n=this.v.get("n");if(n)this.v.set("n",n.split("").reverse().join(""));};
var M={mark:function(a,b){return b;}};
var H=function(a,b,c){M.mark("alr","yes");var r=new R();if(c!==undefined)r.set(b,c);return r;};
}).call(this);"#;
    let resolver = fixture_resolver(
        reqwest::Url::parse("http://127.0.0.1:1/youtubei/v1/player?key=fixture").unwrap(),
        crate::config::YouTubeMusicAuthType::Unauthenticated,
    );
    let script_url =
        reqwest::Url::parse("https://www.youtube.com/s/player/fixture/fixture_player.js").unwrap();
    let signatures = ["alpha".to_owned(), "beta".to_owned()];
    let n_values = ["gamma".to_owned()];
    let first = resolver
        .solve_javascript_challenges_for_test(
            &script_url,
            player_script,
            &signatures,
            &n_values,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.signatures.len(), 2);
    assert_eq!(first.n_values.len(), 1);
    assert_eq!(resolver.ejs_solution_cache_len_for_test().await, 3);
    let second = resolver
        .solve_javascript_challenges_for_test(
            &script_url,
            player_script,
            &["alpha".to_owned()],
            &[],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.signatures.get("alpha"), Some(&"ahpla".to_owned()));
    assert_eq!(resolver.ejs_solution_cache_len_for_test().await, 3);
}

#[test]
fn selects_highest_bitrate_direct_mp4_audio() {
    let selected = select_audio_format(
        vec![
            format(
                Some("https://r1.googlevideo.com/videoplayback?expire=200"),
                "audio/mp4; codecs=\"mp4a.40.5\"",
                50_000,
            ),
            format(
                Some("https://r1.googlevideo.com/videoplayback?expire=300"),
                "audio/mp4; codecs=\"mp4a.40.2\"",
                130_000,
            ),
            format(
                Some("https://r1.googlevideo.com/videoplayback?expire=400"),
                "audio/webm; codecs=\"opus\"",
                160_000,
            ),
        ],
        crate::config::YouTubePlaybackQuality::High,
        "TEST",
    )
    .unwrap();
    assert_eq!(selected.bitrate, 130_000);
    assert_eq!(selected.expires_at_unix, Some(240));
}

#[test]
fn data_saver_selects_the_lowest_supported_bitrate() {
    let selected = select_audio_format(
        vec![
            format(
                Some("https://r1.googlevideo.com/videoplayback?expire=200"),
                "audio/mp4; codecs=\"mp4a.40.5\"",
                50_000,
            ),
            format(
                Some("https://r1.googlevideo.com/videoplayback?expire=300"),
                "audio/mp4; codecs=\"mp4a.40.2\"",
                130_000,
            ),
        ],
        crate::config::YouTubePlaybackQuality::DataSaver,
        "TEST",
    )
    .unwrap();
    assert_eq!(selected.bitrate, 50_000);
}

#[test]
fn rejects_non_google_or_non_https_media_urls() {
    for url in [
        "http://r1.googlevideo.com/videoplayback",
        "https://example.com/audio.m4a",
    ] {
        let err = validate_media_url(&url.parse().unwrap()).unwrap_err();
        assert_eq!(err.kind, AudioSourceErrorKind::Contract);
    }
}

#[test]
fn extracts_public_player_script_urls_without_accepting_other_hosts() {
    let page = r#"{"PLAYER_JS_URL":"/s/player/example/base.js"}"#;
    let url = extract_player_script_url(page).unwrap();
    assert_eq!(
        url.as_str(),
        "https://www.youtube.com/s/player/example/base.js"
    );
    assert!(
        extract_player_script_url(r#"{"PLAYER_JS_URL":"https://example.com/player.js"}"#).is_none()
    );
}

#[test]
fn replaces_existing_cipher_query_parameters() {
    let mut url = Url::parse("https://r1.googlevideo.com/videoplayback?n=old&x=1").unwrap();
    set_query_parameter(&mut url, "n", "new value");
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![("n".into(), "new value".into()), ("x".into(), "1".into())]
    );
}

#[test]
fn errors_when_only_ciphered_or_unsupported_formats_exist() {
    let err = select_audio_format(
        vec![format(None, "audio/mp4; codecs=\"mp4a.40.2\"", 130_000)],
        crate::config::YouTubePlaybackQuality::High,
        "TEST",
    )
    .unwrap_err();
    assert_eq!(err.kind, AudioSourceErrorKind::Decipher);
}

#[test]
fn browser_source_uses_the_audio_format_actually_selected_by_browser() {
    let target = BrowserAudioTarget {
        itag: 140,
        mime_type: "audio/mp4".to_owned(),
        bitrate: 128_000,
        content_length: Some(3_274_344),
        duration: Some(Duration::from_secs(10)),
    };
    let url = reqwest::Url::parse(
        "https://r1.googlevideo.com/videoplayback?itag=251&mime=audio%2Fwebm&clen=4123456&pot=proof&sig=signed",
    )
    .unwrap();

    assert_eq!(
        browser_source_metadata(&url, &target),
        (251, "audio/webm".to_owned(), Some(4_123_456))
    );
}

#[test]
fn direct_and_cipher_player_fixtures_exercise_both_selection_paths() {
    let direct: PlayerResponse =
        serde_json::from_str(include_str!("../../fixtures/player_direct.json")).unwrap();
    let direct = select_audio_format(
        playable_formats(direct).unwrap(),
        crate::config::YouTubePlaybackQuality::High,
        "FIXTURE",
    )
    .unwrap();
    assert_eq!(direct.source_client, "FIXTURE");

    let cipher: PlayerResponse =
        serde_json::from_str(include_str!("../../fixtures/player_cipher_only.json")).unwrap();
    let formats = playable_formats(cipher).unwrap();
    let error = select_audio_format(
        formats.clone(),
        crate::config::YouTubePlaybackQuality::High,
        "FIXTURE",
    )
    .unwrap_err();
    assert_eq!(error.kind, AudioSourceErrorKind::Decipher);
    assert!(super::select_browser_audio_target(
        &formats,
        crate::config::YouTubePlaybackQuality::High
    )
    .is_ok());
}
