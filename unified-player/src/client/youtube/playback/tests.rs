use reqwest::Url;
#[cfg(feature = "private-capture")]
use std::io::{Read as _, Write as _};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::player::load_request_auth;
use super::{
    append_player_response_chunk, automatic_player_client_enabled, automatic_player_clients,
    bounded_media_range_end, bounded_media_range_end_for_chunk, browser_request_auth,
    browser_source_metadata, cookie_value, decoder_media_probe_outcome, extract_player_script_url,
    media_probe_response_class, media_probe_target, media_range_header, media_redirect_candidates,
    media_response_length, media_url_failover_candidates, open_decoded_source,
    parse_signature_timestamp, parse_tv_client_version, parse_visitor_data, playable_formats,
    replayable_browser_headers, sapisid_authorization, select_audio_format, set_query_parameter,
    should_retry_browser_session_transport, should_use_browser_session_transport,
    validate_media_url, AdaptiveFormat, AudioSourceError, AudioSourceErrorKind,
    AudioSourceResolver, BrowserAudioTarget, InnertubeAudioResolver, MediaTransportDiagnostic,
    PlayerResponse, PoTokenMaterial, RequestAuth, YouTubeProbeClient, MEDIA_RANGE_CHUNK_BYTES,
    PLAYER_RESPONSE_COPY_LIMIT,
};

#[cfg(feature = "private-capture")]
use super::{
    build_player_request, parse_innertube_client_version, PlayerClient, PlayerVersionSource,
    YOUTUBE_MUSIC_ORIGIN,
};

include!("tests/fixtures.rs");
include!("tests/source.rs");
include!("tests/resolver.rs");
include!("tests/player.rs");
include!("tests/format.rs");
include!("tests/transport.rs");
include!("tests/forensics.rs");
include!("tests/replay.rs");
