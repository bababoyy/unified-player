use std::time::Duration;

use reqwest::{header, Url};
use serde::Deserialize;
use url::form_urlencoded;

use crate::config::YouTubePlaybackQuality;

use super::source::{AudioSourceError, AudioSourceErrorKind, ResolvedAudioSource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlayerAttemptAuthScope {
    Account,
    Public,
}

pub(super) fn playability_error_kind(status: &str) -> Option<AudioSourceErrorKind> {
    match status {
        "OK" => None,
        "LOGIN_REQUIRED" => Some(AudioSourceErrorKind::Authentication),
        "AGE_CHECK_REQUIRED" | "CONTENT_CHECK_REQUIRED" => {
            Some(AudioSourceErrorKind::ConsentAgeOrRegion)
        }
        _ => Some(AudioSourceErrorKind::Unavailable),
    }
}

pub(super) fn playable_formats(
    response: PlayerResponse,
) -> Result<Vec<AdaptiveFormat>, AudioSourceError> {
    if response.playability_status.status != "OK" {
        let kind = playability_error_kind(&response.playability_status.status)
            .unwrap_or(AudioSourceErrorKind::Unavailable);
        let message = match kind {
            AudioSourceErrorKind::Authentication => {
                "YouTube requires renewed playback authentication"
            }
            AudioSourceErrorKind::ConsentAgeOrRegion => {
                "YouTube requires an age, consent, or region check"
            }
            _ => "YouTube reported that this item is unavailable",
        };
        return Err(AudioSourceError::new(kind, message));
    }
    response
        .streaming_data
        .map(|streaming_data| streaming_data.adaptive_formats)
        .ok_or_else(|| {
            AudioSourceError::new(
                AudioSourceErrorKind::Contract,
                "YouTube returned no streamingData",
            )
        })
}

pub(super) fn playable_formats_for_attempt(
    response: PlayerResponse,
    auth_scope: PlayerAttemptAuthScope,
) -> Result<Vec<AdaptiveFormat>, AudioSourceError> {
    if auth_scope == PlayerAttemptAuthScope::Public
        && response.playability_status.status == "LOGIN_REQUIRED"
    {
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::Unavailable,
            "YouTube public playback requires an account-scoped client",
        ));
    }
    playable_formats(response)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlayerResponse {
    pub(super) playability_status: PlayabilityStatus,
    pub(super) streaming_data: Option<StreamingData>,
    pub(super) assets: Option<PlayerAssets>,
    #[cfg(feature = "private-capture")]
    #[serde(skip)]
    pub(super) http_status: u16,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlayerAssets {
    pub(super) js: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlayabilityStatus {
    pub(super) status: String,
    #[cfg(feature = "private-capture")]
    pub(super) reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StreamingData {
    pub(super) adaptive_formats: Vec<AdaptiveFormat>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdaptiveFormat {
    pub(super) itag: u64,
    pub(super) url: Option<String>,
    pub(super) signature_cipher: Option<String>,
    #[serde(alias = "cipher")]
    pub(super) cipher: Option<String>,
    pub(super) mime_type: String,
    pub(super) bitrate: u64,
    pub(super) content_length: Option<String>,
    pub(super) approx_duration_ms: Option<String>,
}

pub(super) fn cipher_for_format(format: &AdaptiveFormat) -> Option<&str> {
    format
        .signature_cipher
        .as_deref()
        .or(format.cipher.as_deref())
}

pub(super) fn cipher_contains_media_url(format: &AdaptiveFormat) -> bool {
    cipher_for_format(format).is_some_and(|cipher| {
        form_urlencoded::parse(cipher.as_bytes()).any(|(key, _)| key == "url")
    })
}

pub(super) fn set_query_parameter(url: &mut Url, key: &str, value: &str) {
    let pairs = url.query_pairs().into_owned().collect::<Vec<_>>();
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    let mut replaced = false;
    for (existing_key, existing_value) in pairs {
        if existing_key == key {
            if !replaced {
                serializer.append_pair(key, value);
                replaced = true;
            }
        } else {
            serializer.append_pair(&existing_key, &existing_value);
        }
    }
    if !replaced {
        serializer.append_pair(key, value);
    }
    url.set_query(Some(&serializer.finish()));
}

pub(super) fn apply_gvs_po_token(url: &mut Url, token: Option<&str>) {
    if let Some(token) = token {
        set_query_parameter(url, "pot", token);
    }
}

pub(super) fn source_from_format(
    format: AdaptiveFormat,
    url: Url,
    source_client: &'static str,
) -> ResolvedAudioSource {
    let expires_at_unix = url
        .query_pairs()
        .find(|(key, _)| key == "expire")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .map(|expiry| expiry.saturating_sub(60));
    ResolvedAudioSource {
        media_id: String::new(),
        #[cfg(feature = "private-capture")]
        itag: format.itag,
        url,
        required_headers: header::HeaderMap::new(),
        mime_type: format.mime_type,
        bitrate: format.bitrate,
        content_length: format
            .content_length
            .and_then(|length| length.parse::<u64>().ok()),
        duration: format
            .approx_duration_ms
            .and_then(|duration| duration.parse::<u64>().ok())
            .map(Duration::from_millis),
        expires_at_unix,
        source_client,
    }
}

#[derive(Debug)]
pub(super) struct BrowserAudioTarget {
    pub(super) itag: u64,
    pub(super) mime_type: String,
    pub(super) bitrate: u64,
    pub(super) content_length: Option<u64>,
    pub(super) duration: Option<Duration>,
}

pub(super) fn select_browser_audio_target(
    formats: &[AdaptiveFormat],
    quality: YouTubePlaybackQuality,
) -> Result<BrowserAudioTarget, AudioSourceError> {
    let supported = formats
        .iter()
        .filter(|format| format.mime_type.starts_with("audio/mp4"))
        .collect::<Vec<_>>();
    let format = supported
        .iter()
        .copied()
        .find(|format| format.itag == 140)
        .or_else(|| match quality {
            YouTubePlaybackQuality::High => {
                supported.into_iter().max_by_key(|format| format.bitrate)
            }
            YouTubePlaybackQuality::DataSaver => {
                supported.into_iter().min_by_key(|format| format.bitrate)
            }
        })
        .ok_or_else(|| {
            AudioSourceError::new(
                AudioSourceErrorKind::UnsupportedFormat,
                "YouTube returned no supported MP4/AAC audio format",
            )
        })?;
    Ok(BrowserAudioTarget {
        itag: format.itag,
        mime_type: format.mime_type.clone(),
        bitrate: format.bitrate,
        content_length: format
            .content_length
            .as_deref()
            .and_then(|length| length.parse::<u64>().ok()),
        duration: format
            .approx_duration_ms
            .as_deref()
            .and_then(|duration| duration.parse::<u64>().ok())
            .map(Duration::from_millis),
    })
}

pub(super) fn browser_source_metadata(
    url: &Url,
    target: &BrowserAudioTarget,
) -> (u64, String, Option<u64>) {
    let query = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let itag = query
        .get("itag")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(target.itag);
    let mime_type = query
        .get("mime")
        .filter(|value| value.starts_with("audio/"))
        .cloned()
        .unwrap_or_else(|| target.mime_type.clone());
    let content_length = query
        .get("clen")
        .and_then(|value| value.parse::<u64>().ok())
        .or(target.content_length);
    (itag, mime_type, content_length)
}

pub(super) fn select_audio_format(
    formats: Vec<AdaptiveFormat>,
    quality: YouTubePlaybackQuality,
    source_client: &'static str,
) -> Result<ResolvedAudioSource, AudioSourceError> {
    let mut supported = formats
        .into_iter()
        .filter(|format| format.mime_type.starts_with("audio/mp4"))
        .collect::<Vec<_>>();
    if supported.is_empty() {
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::UnsupportedFormat,
            "YouTube returned no supported MP4/AAC audio format",
        ));
    }
    let has_ciphered_format = supported
        .iter()
        .any(|format| cipher_for_format(format).is_some());
    let direct = supported.drain(..).filter(|format| format.url.is_some());
    let format = match quality {
        YouTubePlaybackQuality::High => direct.max_by_key(|format| format.bitrate),
        YouTubePlaybackQuality::DataSaver => direct.min_by_key(|format| format.bitrate),
    }
    .ok_or_else(|| {
        AudioSourceError::new(
            if has_ciphered_format {
                AudioSourceErrorKind::Decipher
            } else {
                AudioSourceErrorKind::Unavailable
            },
            if has_ciphered_format {
                "YouTube authenticated the request but returned only ciphered audio; JavaScript challenge solving is required"
            } else {
                "YouTube returned no direct audio URL"
            },
        )
    })?;
    let url = Url::parse(format.url.as_deref().expect("filtered direct URL")).map_err(|_| {
        AudioSourceError::new(AudioSourceErrorKind::Contract, "parse YouTube media URL")
    })?;
    validate_media_url(&url)?;
    Ok(source_from_format(format, url, source_client))
}

pub(super) fn validate_media_url(url: &Url) -> Result<(), AudioSourceError> {
    let host = url.host_str().unwrap_or_default();
    #[cfg(test)]
    if url.scheme() == "http" && matches!(host, "127.0.0.1" | "localhost") {
        return Ok(());
    }
    if url.scheme() != "https" || !(host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
    {
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "YouTube returned a media URL outside the HTTPS Google Video allowlist",
        ));
    }
    Ok(())
}
