use std::collections::HashMap;

use futures::StreamExt as _;
use reqwest::{header, Url};
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

use crate::config::YouTubePlaybackQuality;

use super::{
    format::{
        cipher_for_format, set_query_parameter, source_from_format, validate_media_url,
        AdaptiveFormat,
    },
    player::{PlayerClient, WEB_MUSIC_PAGE, WEB_PAGE},
    source::{AudioSourceError, AudioSourceErrorKind, ResolvedAudioSource},
};

const PLAYER_SCRIPT_COPY_LIMIT: usize = 8 * 1024 * 1024;
const MAX_EJS_SOLUTION_CACHE_ENTRIES: usize = 512;

pub(super) fn extract_player_script_url(page: &str) -> Option<Url> {
    for marker in ["\"PLAYER_JS_URL\":\"", "\"jsUrl\":\""] {
        let Some(index) = page.find(marker) else {
            continue;
        };
        let start = index.saturating_add(marker.len());
        let value = page.get(start..)?.split('\"').next()?;
        let value = value.replace("\\u0026", "&").replace("\\/", "/");
        if let Some(url) = parse_player_script_url(&value) {
            return Some(url);
        }
    }
    None
}

pub(super) fn parse_player_script_url(value: &str) -> Option<Url> {
    let url = match Url::parse(value) {
        Ok(url) => url,
        Err(_) => Url::parse(WEB_PAGE).ok()?.join(value).ok()?,
    };
    let host = url.host_str()?;
    (url.scheme() == "https" && matches!(host, "www.youtube.com" | "music.youtube.com"))
        .then_some(url)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum EjsChallengeKind {
    Signature,
    N,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct EjsSolutionCacheKey {
    player_script_url: String,
    kind: EjsChallengeKind,
    challenge: String,
}

pub(super) struct CipherEngine<'a> {
    client: &'a reqwest::Client,
    player_script_cache: &'a tokio::sync::Mutex<HashMap<String, String>>,
    ejs_solution_cache: &'a tokio::sync::Mutex<HashMap<EjsSolutionCacheKey, String>>,
    javascript_solver: &'a super::super::javascript::Solver,
}

impl<'a> CipherEngine<'a> {
    pub(super) fn new(
        client: &'a reqwest::Client,
        player_script_cache: &'a tokio::sync::Mutex<HashMap<String, String>>,
        ejs_solution_cache: &'a tokio::sync::Mutex<HashMap<EjsSolutionCacheKey, String>>,
        javascript_solver: &'a super::super::javascript::Solver,
    ) -> Self {
        Self {
            client,
            player_script_cache,
            ejs_solution_cache,
            javascript_solver,
        }
    }

    pub(super) async fn discover_player_script_url(
        &self,
        player_client: &PlayerClient,
        cancellation: &CancellationToken,
    ) -> Option<Url> {
        let page_url = if player_client.endpoint_host == "music.youtube.com" {
            WEB_MUSIC_PAGE
        } else {
            WEB_PAGE
        };
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => return None,
            response = self
                .client
                .get(page_url)
                .header(header::USER_AGENT, player_client.user_agent)
                .send() => response.ok()?,
        };
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return None,
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk.ok()?;
            if body.len().saturating_add(chunk.len()) > PLAYER_SCRIPT_COPY_LIMIT {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let page = String::from_utf8(body).ok()?;
        extract_player_script_url(&page)
    }

    pub(super) async fn fetch_player_script(
        &self,
        player_script_url: &Url,
        player_client: &PlayerClient,
        cancellation: &CancellationToken,
    ) -> Result<String, AudioSourceError> {
        let cache_key = player_script_url.as_str().to_owned();
        if let Some(script) = self
            .player_script_cache
            .lock()
            .await
            .get(&cache_key)
            .cloned()
        {
            return Ok(script);
        }
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Cancelled,
                    "YouTube player JavaScript request was cancelled",
                ));
            }
            response = self
                .client
                .get(player_script_url.clone())
                .header(header::USER_AGENT, player_client.user_agent)
                .send() => response.map_err(|_| {
                    AudioSourceError::new(
                        AudioSourceErrorKind::Network,
                        "request the YouTube player JavaScript",
                    )
                })?,
        };
        if !response.status().is_success() {
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::Network,
                "YouTube player JavaScript request failed",
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Cancelled,
                    "YouTube player JavaScript request was cancelled",
                ));
            }
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk.map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "read the YouTube player JavaScript",
                )
            })?;
            if body.len().saturating_add(chunk.len()) > PLAYER_SCRIPT_COPY_LIMIT {
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Contract,
                    "YouTube player JavaScript exceeded the local size limit",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let script = String::from_utf8(body).map_err(|_| {
            AudioSourceError::new(
                AudioSourceErrorKind::Contract,
                "YouTube player JavaScript was not valid UTF-8",
            )
        })?;
        self.player_script_cache
            .lock()
            .await
            .insert(cache_key, script.clone());
        Ok(script)
    }

    pub(super) async fn warm_up_javascript(
        &self,
        player_script: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), AudioSourceError> {
        self.javascript_solver
            .warm_up(player_script, cancellation)
            .await
            .map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Decipher,
                    "YouTube JavaScript warm-up did not complete",
                )
            })
    }

    pub(super) async fn solve_javascript_challenges(
        &self,
        player_script_url: &Url,
        player_script: &str,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> anyhow::Result<super::super::javascript::Solutions> {
        let player_script_url = player_script_url.as_str().to_owned();
        let mut solutions = super::super::javascript::Solutions::default();
        let mut missing_signatures = Vec::new();
        let mut missing_n_values = Vec::new();
        {
            let cache = self.ejs_solution_cache.lock().await;
            for challenge in signature_challenges {
                let key = EjsSolutionCacheKey {
                    player_script_url: player_script_url.clone(),
                    kind: EjsChallengeKind::Signature,
                    challenge: challenge.clone(),
                };
                if let Some(value) = cache.get(&key) {
                    solutions
                        .signatures
                        .insert(challenge.clone(), value.clone());
                } else if !missing_signatures.iter().any(|item| item == challenge) {
                    missing_signatures.push(challenge.clone());
                }
            }
            for challenge in n_challenges {
                let key = EjsSolutionCacheKey {
                    player_script_url: player_script_url.clone(),
                    kind: EjsChallengeKind::N,
                    challenge: challenge.clone(),
                };
                if let Some(value) = cache.get(&key) {
                    solutions.n_values.insert(challenge.clone(), value.clone());
                } else if !missing_n_values.iter().any(|item| item == challenge) {
                    missing_n_values.push(challenge.clone());
                }
            }
        }

        if !missing_signatures.is_empty() || !missing_n_values.is_empty() {
            let fresh = self
                .javascript_solver
                .solve(
                    player_script,
                    &missing_signatures,
                    &missing_n_values,
                    cancellation,
                )
                .await?;
            let mut cache = self.ejs_solution_cache.lock().await;
            for (challenge, value) in fresh.signatures {
                solutions
                    .signatures
                    .insert(challenge.clone(), value.clone());
                cache.insert(
                    EjsSolutionCacheKey {
                        player_script_url: player_script_url.clone(),
                        kind: EjsChallengeKind::Signature,
                        challenge,
                    },
                    value,
                );
            }
            for (challenge, value) in fresh.n_values {
                solutions.n_values.insert(challenge.clone(), value.clone());
                cache.insert(
                    EjsSolutionCacheKey {
                        player_script_url: player_script_url.clone(),
                        kind: EjsChallengeKind::N,
                        challenge,
                    },
                    value,
                );
            }
            while cache.len() > MAX_EJS_SOLUTION_CACHE_ENTRIES {
                let Some(key) = cache.keys().next().cloned() else {
                    break;
                };
                cache.remove(&key);
            }
        }
        Ok(solutions)
    }

    pub(super) async fn resolve_ciphered_audio_format(
        &self,
        formats: &[AdaptiveFormat],
        quality: YouTubePlaybackQuality,
        player_client: &PlayerClient,
        player_script_url: &Url,
        cancellation: &CancellationToken,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        let mut candidates = Vec::new();
        let mut signature_challenges = Vec::new();
        let mut n_challenges = Vec::new();
        for format in formats.iter().filter(|format| {
            format.mime_type.starts_with("audio/mp4") && cipher_for_format(format).is_some()
        }) {
            let Some(cipher) = cipher_for_format(format) else {
                continue;
            };
            let params = form_urlencoded::parse(cipher.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>();
            let Some(base_url) = params.get("url") else {
                continue;
            };
            let Ok(url) = Url::parse(base_url) else {
                continue;
            };
            validate_media_url(&url)?;
            let signature = params.get("s").cloned();
            let n_value = url
                .query_pairs()
                .find(|(key, _)| key == "n")
                .map(|(_, value)| value.into_owned());
            if let Some(challenge) = &signature {
                if !signature_challenges.iter().any(|item| item == challenge) {
                    signature_challenges.push(challenge.clone());
                }
            }
            if let Some(challenge) = &n_value {
                if !n_challenges.iter().any(|item| item == challenge) {
                    n_challenges.push(challenge.clone());
                }
            }
            if signature.is_none() && n_value.is_none() {
                continue;
            }
            candidates.push((format.clone(), params, url, signature, n_value));
        }
        if candidates.is_empty() {
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::Decipher,
                "YouTube returned no ciphered MP4/AAC format for JavaScript solving",
            ));
        }

        let player_script = self
            .fetch_player_script(player_script_url, player_client, cancellation)
            .await?;
        let solutions = self
            .solve_javascript_challenges(
                player_script_url,
                &player_script,
                &signature_challenges,
                &n_challenges,
                cancellation,
            )
            .await
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    AudioSourceError::new(
                        AudioSourceErrorKind::Cancelled,
                        "YouTube JavaScript challenge solving was cancelled",
                    )
                } else {
                    let _ = error;
                    AudioSourceError::new(
                        AudioSourceErrorKind::Decipher,
                        "YouTube JavaScript challenge solving failed",
                    )
                }
            })?;

        let mut resolved = Vec::with_capacity(candidates.len());
        for (format, params, mut url, signature, n_value) in candidates {
            if let Some(challenge) = signature {
                let signature = solutions.signatures.get(&challenge).ok_or_else(|| {
                    AudioSourceError::new(
                        AudioSourceErrorKind::Decipher,
                        "YouTube JavaScript solver returned no signature",
                    )
                })?;
                let signature_parameter = params.get("sp").map_or("sig", String::as_str);
                set_query_parameter(&mut url, signature_parameter, signature);
            }
            if let Some(challenge) = n_value {
                let n_value = solutions.n_values.get(&challenge).ok_or_else(|| {
                    AudioSourceError::new(
                        AudioSourceErrorKind::Decipher,
                        "YouTube JavaScript solver returned no n challenge value",
                    )
                })?;
                set_query_parameter(&mut url, "n", n_value);
            }
            validate_media_url(&url)?;
            resolved.push(source_from_format(format, url, player_client.source_name));
        }
        match quality {
            YouTubePlaybackQuality::High => {
                resolved.into_iter().max_by_key(|source| source.bitrate)
            }
            YouTubePlaybackQuality::DataSaver => {
                resolved.into_iter().min_by_key(|source| source.bitrate)
            }
        }
        .ok_or_else(|| {
            AudioSourceError::new(
                AudioSourceErrorKind::Decipher,
                "YouTube JavaScript solver returned no playable MP4/AAC format",
            )
        })
    }
}
