use std::{fmt, time::Duration};

use reqwest::{header, Url};
use tokio_util::sync::CancellationToken;

use crate::config::YouTubePlaybackQuality;

#[derive(Clone)]
pub struct ResolvedAudioSource {
    pub media_id: String,
    #[cfg(feature = "private-capture")]
    pub itag: u64,
    pub url: Url,
    pub required_headers: header::HeaderMap,
    pub mime_type: String,
    pub bitrate: u64,
    pub content_length: Option<u64>,
    pub duration: Option<Duration>,
    pub expires_at_unix: Option<u64>,
    pub source_client: &'static str,
}

impl fmt::Debug for ResolvedAudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedAudioSource")
            .field("media_id", &"<redacted media identifier>")
            .field("itag", &"<redacted provider format>")
            .field("url", &"<redacted signed media URL>")
            .field("required_headers", &"<redacted media headers>")
            .field("mime_type", &self.mime_type)
            .field("bitrate", &self.bitrate)
            .field("content_length", &self.content_length)
            .field("duration", &self.duration)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("source_client", &self.source_client)
            .finish()
    }
}

impl ResolvedAudioSource {
    pub fn is_expired(&self) -> bool {
        let Some(expiry) = self.expires_at_unix else {
            return false;
        };
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .is_ok_and(|now| now.as_secs() >= expiry)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSourceErrorKind {
    Authentication,
    ConsentAgeOrRegion,
    Unavailable,
    ProofToken,
    Decipher,
    RateLimited,
    Network,
    Cancelled,
    Contract,
    UnsupportedFormat,
    MediaForbidden,
    MediaRangeContract,
}

impl AudioSourceErrorKind {
    const ALL: [Self; 12] = [
        Self::Authentication,
        Self::ConsentAgeOrRegion,
        Self::Unavailable,
        Self::ProofToken,
        Self::Decipher,
        Self::RateLimited,
        Self::Network,
        Self::Cancelled,
        Self::Contract,
        Self::UnsupportedFormat,
        Self::MediaForbidden,
        Self::MediaRangeContract,
    ];

    pub(crate) const fn diagnostic_category(self) -> crate::observability::ErrorCategory {
        use crate::observability::ErrorCategory;

        match self {
            Self::Authentication => ErrorCategory::Authentication,
            Self::ConsentAgeOrRegion => ErrorCategory::ConsentAgeRegion,
            Self::Unavailable => ErrorCategory::ProviderUnavailable,
            Self::ProofToken => ErrorCategory::ProofToken,
            Self::Decipher => ErrorCategory::Decipher,
            Self::RateLimited => ErrorCategory::RateLimited,
            Self::Network => ErrorCategory::Network,
            Self::Cancelled => ErrorCategory::Cancelled,
            Self::Contract => ErrorCategory::Contract,
            Self::UnsupportedFormat => ErrorCategory::UnsupportedFormat,
            Self::MediaForbidden => ErrorCategory::MediaForbidden,
            Self::MediaRangeContract => ErrorCategory::MediaRangeContract,
        }
    }
}

pub(crate) fn diagnostic_category_inventory() -> String {
    AudioSourceErrorKind::ALL
        .iter()
        .map(|kind| kind.diagnostic_category().as_str())
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug)]
pub struct AudioSourceError {
    pub kind: AudioSourceErrorKind,
    message: &'static str,
}

impl AudioSourceError {
    pub(super) const fn new(kind: AudioSourceErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }
}

impl fmt::Display for AudioSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for AudioSourceError {}

#[async_trait::async_trait]
pub trait AudioSourceResolver: Send + Sync {
    /// Warm resolver-side client material and JavaScript runtime state without
    /// resolving a specific media item. Implementations must keep this best
    /// effort so playback never depends on the warm-up completing.
    async fn warm_up(&self, cancellation: &CancellationToken) -> Result<(), AudioSourceError> {
        let _ = cancellation;
        Ok(())
    }

    async fn resolve(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
    ) -> Result<ResolvedAudioSource, AudioSourceError>;

    #[cfg(feature = "private-capture")]
    async fn resolve_with_capture(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
        _capture: crate::developer_capture::CaptureSession,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        self.resolve(video_id, quality, cancellation).await
    }

    async fn resolve_for_prefetch(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        self.resolve(video_id, quality, cancellation).await
    }

    /// Describe the current playback ladder without exposing credentials or
    /// signed media URLs. Implementations that do not have a route ladder can
    /// use the empty default.
    fn playback_route(&self) -> crate::state::YouTubePlaybackRoute {
        crate::state::YouTubePlaybackRoute::default()
    }

    fn backend_name(&self) -> &'static str;
}
