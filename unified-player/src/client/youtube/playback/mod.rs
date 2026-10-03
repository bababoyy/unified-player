mod cipher;
mod decoder;
#[cfg(feature = "private-capture")]
mod forensics;
mod format;
mod innertube;
mod player;
#[cfg(feature = "private-capture")]
mod replay;
mod source;
mod transport;

pub use decoder::open_decoded_source;
pub(crate) use decoder::open_decoded_source_for_probe;
#[cfg(feature = "private-capture")]
pub(crate) use decoder::open_decoded_source_with_capture;
pub use decoder::YouTubeProbeDecoderChunkSize;
#[cfg(feature = "private-capture")]
#[allow(unused_imports)]
pub(crate) use forensics::{
    MediaCaptureHandle, YouTubePlaybackAttemptInspection, YouTubePlaybackInspection,
    YouTubePlayerClientInspection,
};
pub use innertube::{InnertubeAudioResolver, YouTubeProbeAttempt, YouTubeProbeClient};
#[cfg(feature = "private-capture")]
#[allow(unused_imports)]
pub(crate) use replay::replay_player_decision;
#[cfg(feature = "private-capture")]
#[allow(unused_imports)]
pub(crate) use replay::{
    YouTubeFreshReplayAdapter, YouTubeFreshReplayMaterial, YouTubeOfflineReplayAdapter,
};
pub(crate) use source::diagnostic_category_inventory;
#[allow(unused_imports)]
pub use source::{
    AudioSourceError, AudioSourceErrorKind, AudioSourceResolver, ResolvedAudioSource,
};
#[allow(unused_imports)]
pub(crate) use transport::MediaTransportDiagnostic;

pub(crate) type DecodedAudioSource = Box<dyn rodio::Source<Item = f32> + Send>;

pub(crate) struct PreparedAudioSource {
    source: ResolvedAudioSource,
    decoded: Option<DecodedAudioSource>,
}

impl PreparedAudioSource {
    pub(crate) fn descriptor(source: ResolvedAudioSource) -> Self {
        Self {
            source,
            decoded: None,
        }
    }

    pub(crate) fn decoded(source: ResolvedAudioSource, decoded: DecodedAudioSource) -> Self {
        Self {
            source,
            decoded: Some(decoded),
        }
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.source.is_expired()
    }

    pub(crate) fn into_parts(self) -> (ResolvedAudioSource, Option<DecodedAudioSource>) {
        (self.source, self.decoded)
    }
}

#[cfg(test)]
use innertube::{
    automatic_player_client_enabled, automatic_player_clients,
    should_retry_browser_session_transport, should_use_browser_session_transport, PlayerClientPlan,
};

#[cfg(all(test, feature = "private-capture"))]
use forensics::PRIVATE_CAPTURE_HEADER_BYTES_LIMIT;
#[cfg(all(test, feature = "private-capture"))]
use forensics::{
    bounded_private_header_map, capture_player_http_response, inspect_player_attempt,
    record_media_failure, CapturedMediaSource, MediaCaptureLifecycle, MediaPrivateEvidence,
};

#[cfg(test)]
use cipher::extract_player_script_url;
#[cfg(test)]
use format::AdaptiveFormat;
#[cfg(all(test, feature = "private-capture"))]
use format::PlayabilityStatus;
#[cfg(test)]
use format::{
    browser_source_metadata, select_audio_format, select_browser_audio_target, BrowserAudioTarget,
    PlayerResponse,
};
#[cfg(test)]
use format::{playable_formats, set_query_parameter, validate_media_url};
#[cfg(all(test, feature = "private-capture"))]
use format::{playable_formats_for_attempt, PlayerAttemptAuthScope};
#[cfg(all(test, feature = "private-capture"))]
use player::parse_innertube_client_version;
#[cfg(all(test, feature = "private-capture"))]
use player::YOUTUBE_MUSIC_ORIGIN;
#[cfg(test)]
use player::{append_player_response_chunk, build_player_request};
#[cfg(test)]
use player::{browser_request_auth, browser_request_auth_at, cookie_value, sapisid_authorization};
#[cfg(test)]
use player::{
    parse_signature_timestamp, parse_tv_client_version, parse_visitor_data, PlayerClient,
    PlayerVersionSource, PoTokenMaterial, RequestAuth,
};
#[cfg(test)]
use player::{PLAYER_ENDPOINT, PLAYER_RESPONSE_COPY_LIMIT};
#[cfg(test)]
use transport::{
    bounded_media_range_end, bounded_media_range_end_for_chunk, decoder_media_probe_outcome,
    media_probe_response_class, media_probe_target, media_range_header, media_redirect_candidates,
    media_response_length, media_url_failover_candidates, replayable_browser_headers,
    MEDIA_RANGE_CHUNK_BYTES,
};
#[cfg(all(test, feature = "private-capture"))]
use transport::{MediaHttpClient, RedactedMediaUrl};

#[cfg(test)]
mod tests;
