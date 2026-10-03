#[cfg(feature = "streaming")]
use std::sync::{Arc, Mutex as StdMutex};
use std::{
    future::Future,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use rspotify::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::{
    config,
    state::{now_unix_secs, ContextId, Playback, PlaybackMetadata, SessionEntry, SharedState},
};

use super::{
    playback_coordinator, request, request_router::RequestDisposition, youtube, AppClient,
    ClientRequest, PlayerRequest,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum YouTubeResolutionPurpose {
    InteractivePlayback,
    Resume,
}

#[cfg(feature = "private-capture")]
fn claim_capture_for_resolution(
    handle: &crate::developer_capture::CaptureHandle,
    purpose: YouTubeResolutionPurpose,
    operation_ref: crate::developer_capture::SafeOperationRef,
) -> Option<crate::developer_capture::CaptureSession> {
    let purpose = match purpose {
        YouTubeResolutionPurpose::InteractivePlayback => {
            crate::developer_capture::CapturePurpose::InteractivePlayback
        }
        YouTubeResolutionPurpose::Resume => crate::developer_capture::CapturePurpose::Resume,
    };
    handle.claim(purpose, operation_ref).ok()?
}

fn youtube_prefetch_operation_context() -> crate::observability::OperationContext {
    let mut context = crate::observability::child_or_new(
        "youtube_prefetch",
        crate::observability::OperationSource::System,
    );
    "youtube_prefetch".clone_into(&mut context.operation);
    context
}

async fn in_youtube_prefetch<F: Future>(
    context: crate::observability::OperationContext,
    future: F,
) -> F::Output {
    crate::observability::in_operation(context, future).await
}

/// Whether a failed start proves the queue item itself cannot play. Account,
/// network, rate-limit, output, cancellation, and unclassified failures would
/// hit the next item too, so they must not consume the remaining queue.
fn is_unplayable_item(err: &anyhow::Error) -> bool {
    use crate::observability::ErrorCategory;
    matches!(
        crate::observability::preserved_error_diagnostic(err).map(|(_, category)| category),
        Some(
            ErrorCategory::ConsentAgeRegion
                | ErrorCategory::ProviderUnavailable
                | ErrorCategory::UnsupportedFormat
        )
    )
}

/// Play `item`, moving to `next` only while items prove unplayable.
async fn play_skipping_unplayable_items<Fut>(
    mut item: crate::state::PlayableMedia,
    mut play: impl FnMut(crate::state::PlayableMedia) -> Fut,
    mut next: impl FnMut() -> Option<crate::state::PlayableMedia>,
) -> Result<()>
where
    Fut: std::future::Future<Output = Result<()>>,
{
    for _ in 0..32 {
        let Err(err) = play(item.clone()).await else {
            return Ok(());
        };
        if !is_unplayable_item(&err) {
            return Err(err);
        }
        crate::observability::log_safe_error!(
            warn,
            crate::observability::DiagnosticCode::UNIFIED_QUEUE_ITEM_UNAVAILABLE,
            crate::observability::preserved_error_diagnostic(&err).map_or(
                crate::observability::ErrorCategory::Unavailable,
                |(_, category)| { category }
            ),
            &err,
            "Skipping an unavailable unified-queue item"
        );
        // A stale activation yields no next item, so newer playback keeps
        // the queue.
        let Some(following) = next() else {
            return Err(err);
        };
        if following == item {
            return Err(err);
        }
        item = following;
    }
    anyhow::bail!("unified queue exceeded its unavailable-item retry limit")
}

fn refresh_completion_for_same_queue_occurrence(
    current: &mut Option<request::UnifiedQueueCompletion>,
    refreshed: request::UnifiedQueueCompletion,
) -> bool {
    if current.is_none() {
        *current = Some(refreshed);
        return true;
    }
    if current.as_ref().is_some_and(|completion| {
        completion.source == refreshed.source && completion.queue == refreshed.queue
    }) {
        *current = Some(refreshed);
        true
    } else {
        false
    }
}

fn resolve_seek_target(
    seek: request::ActivePlaybackSeek,
    position: Option<Duration>,
    duration: Option<Duration>,
) -> Option<Duration> {
    let target = match seek {
        request::ActivePlaybackSeek::Start => Duration::ZERO,
        request::ActivePlaybackSeek::Absolute(position) => position,
        request::ActivePlaybackSeek::Relative(offset) => {
            let position = position?;
            let offset_ms = offset.num_milliseconds();
            if offset_ms >= 0 {
                position.saturating_add(Duration::from_millis(offset_ms as u64))
            } else {
                position.saturating_sub(Duration::from_millis(offset_ms.unsigned_abs()))
            }
        }
        request::ActivePlaybackSeek::Fraction {
            numerator,
            denominator,
        } => {
            if denominator == 0 {
                return None;
            }
            let duration = duration?;
            let target_ms = duration.as_millis().saturating_mul(u128::from(numerator))
                / u128::from(denominator);
            Duration::from_millis(u64::try_from(target_ms).ok()?)
        }
    };
    Some(duration.map_or(target, |duration| target.min(duration)))
}

fn parse_youtube_track_duration(value: &str) -> Option<Duration> {
    let mut seconds = 0_u64;
    for part in value.split(':') {
        seconds = seconds.checked_mul(60)?;
        seconds = seconds.checked_add(part.parse::<u64>().ok()?)?;
    }
    Some(Duration::from_secs(seconds))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum YouTubeSeekOutcome {
    AppliedInPlace,
    Reactivated,
    Unavailable,
    Superseded,
}

impl AppClient {
    pub(crate) fn reserve_playback_activation(&self) -> playback_coordinator::ActivationPermit {
        self.playback.reserve_activation()
    }

    async fn seek_youtube_at(
        &self,
        state: &SharedState,
        position: Duration,
        seek_permit: &playback_coordinator::ActivePlaybackSeekPermit,
    ) -> Result<YouTubeSeekOutcome> {
        let cancellation = seek_permit.cancellation();
        let (snapshot, restart) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Ok(YouTubeSeekOutcome::Superseded);
            }
            player = self.youtube_player.lock() => {
            let mut player = player;
            let restart = player.restart_info();
            (player.seek(position)?, restart)
            }
        };
        let restored_playback = if snapshot.is_none() && restart.is_none() {
            state.player.read().youtube_playback.clone()
        } else {
            None
        };
        if let Some(snapshot) = snapshot {
            if !self.playback.active_seek_is_current(seek_permit) {
                return Ok(YouTubeSeekOutcome::Superseded);
            }
            self.state_application
                .apply_youtube_control_snapshot(&mut state.player.write(), &snapshot);
            self.refresh_and_persist_session(state, config::ActiveProvider::YouTubeMusic);
            return Ok(YouTubeSeekOutcome::AppliedInPlace);
        }
        if let Some(restart) = restart {
            if !self.playback.active_seek_is_current(seek_permit) {
                return Ok(YouTubeSeekOutcome::Superseded);
            }
            let source = (!restart.source.is_expired()).then_some(restart.source);
            let activated = self
                .activate_youtube_track_at_with_source(
                    state,
                    None,
                    restart.track,
                    position,
                    restart.was_playing,
                    YouTubeResolutionPurpose::Resume,
                    source,
                )
                .await?;
            return Ok(if activated {
                YouTubeSeekOutcome::Reactivated
            } else {
                YouTubeSeekOutcome::Superseded
            });
        }
        if let Some(playback) = restored_playback {
            if !self.playback.active_seek_is_current(seek_permit) {
                return Ok(YouTubeSeekOutcome::Superseded);
            }
            let activated = self
                .activate_youtube_track_at_with_source(
                    state,
                    None,
                    playback.track,
                    position,
                    playback.is_playing,
                    YouTubeResolutionPurpose::Resume,
                    None,
                )
                .await?;
            return Ok(if activated {
                YouTubeSeekOutcome::Reactivated
            } else {
                YouTubeSeekOutcome::Superseded
            });
        }
        Ok(YouTubeSeekOutcome::Unavailable)
    }

    pub(crate) fn capture_unified_queue_completion(
        &self,
        state: &SharedState,
        source: config::ActiveProvider,
        media: &crate::state::PlayableMedia,
    ) -> Option<request::UnifiedQueueCompletion> {
        let activation_generation = self.playback.activation_generation();
        let queue = state
            .player
            .read()
            .unified_queue
            .as_ref()?
            .completion_token_for(media)?;
        Some(request::UnifiedQueueCompletion {
            source,
            queue,
            activation_generation,
        })
    }

    pub(crate) fn emit_unified_queue_completion(
        &self,
        completion: request::UnifiedQueueCompletion,
    ) {
        if self.playback_completion_tx.send(completion).is_err() {
            tracing::debug!("Unified queue completion ingress is unavailable");
        }
    }

    pub(super) fn refresh_youtube_queue_completion(&self, state: &SharedState) {
        let track = state
            .player
            .read()
            .youtube_playback
            .as_ref()
            .map(|playback| playback.track.clone());
        let Some(track) = track else {
            return;
        };
        let Some(refreshed) = self.capture_unified_queue_completion(
            state,
            config::ActiveProvider::YouTubeMusic,
            &crate::state::PlayableMedia::YouTube(track.clone()),
        ) else {
            return;
        };
        let registry = self
            .youtube_queue_completion
            .lock()
            .expect("YouTube queue completion mutex poisoned");
        let Some((track_id, slot)) = registry.as_ref().filter(|(id, _)| id == &track.id) else {
            return;
        };
        debug_assert_eq!(track_id, &track.id);
        let mut current = slot
            .lock()
            .expect("YouTube queue completion slot mutex poisoned");
        refresh_completion_for_same_queue_occurrence(&mut current, refreshed);
    }

    pub(super) fn refresh_spotify_queue_completion(
        &self,
        state: &SharedState,
        playable_id: &rspotify::model::PlayableId<'static>,
    ) {
        let media = crate::state::PlayableMedia::Spotify(playable_id.clone());
        let Some(refreshed) =
            self.capture_unified_queue_completion(state, config::ActiveProvider::Spotify, &media)
        else {
            return;
        };
        let registry = self
            .spotify_queue_completion
            .lock()
            .expect("Spotify queue completion registry mutex poisoned");
        let Some(slot) = registry.as_ref() else {
            return;
        };
        *slot
            .lock()
            .expect("Spotify queue completion slot mutex poisoned") =
            Some((playable_id.clone(), refreshed));
    }

    #[cfg(feature = "streaming")]
    pub(crate) fn register_spotify_queue_completion_slot(
        &self,
    ) -> super::SpotifyQueueCompletionSlot {
        let slot = Arc::new(StdMutex::new(None));
        *self
            .spotify_queue_completion
            .lock()
            .expect("Spotify queue completion registry mutex poisoned") = Some(slot.clone());
        slot
    }

    #[cfg(feature = "streaming")]
    pub(crate) fn set_spotify_queue_completion(
        slot: &super::SpotifyQueueCompletionSlot,
        completion: Option<(
            rspotify::model::PlayableId<'static>,
            request::UnifiedQueueCompletion,
        )>,
    ) {
        *slot
            .lock()
            .expect("Spotify queue completion slot mutex poisoned") = completion;
    }

    #[cfg(feature = "streaming")]
    pub(crate) fn take_spotify_queue_completion(
        slot: &super::SpotifyQueueCompletionSlot,
        playable_id: &rspotify::model::PlayableId<'static>,
    ) -> Option<request::UnifiedQueueCompletion> {
        slot.lock()
            .expect("Spotify queue completion slot mutex poisoned")
            .take()
            .filter(|(current, _)| current == playable_id)
            .map(|(_, completion)| completion)
    }

    #[cfg(feature = "streaming")]
    pub(crate) fn release_spotify_queue_completion_slot(
        &self,
        slot: &super::SpotifyQueueCompletionSlot,
    ) {
        let mut registry = self
            .spotify_queue_completion
            .lock()
            .expect("Spotify queue completion registry mutex poisoned");
        if registry
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            *registry = None;
        }
    }

    pub(super) fn with_current_activation<R>(
        &self,
        activation: Option<&playback_coordinator::ActivationPermit>,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        match activation {
            Some(permit) => self.playback.with_current_activation(permit, action),
            None => Some(action()),
        }
    }

    pub(super) fn refresh_and_persist_session(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) {
        let sessions = playback_coordinator::AppPlaybackSessions::new(state);
        self.playback.refresh_and_persist(&sessions, provider);
    }

    pub(super) async fn play_youtube_track(
        &self,
        state: &SharedState,
        track: crate::state::YouTubeTrack,
        activation: Option<&playback_coordinator::ActivationPermit>,
    ) -> Result<()> {
        *self
            .youtube_expiry_recovery
            .lock()
            .expect("YouTube expiry recovery mutex poisoned") = None;
        self.play_youtube_track_at(
            state,
            track,
            Duration::ZERO,
            true,
            YouTubeResolutionPurpose::InteractivePlayback,
            activation,
        )
        .await
    }

    async fn play_youtube_track_at(
        &self,
        state: &SharedState,
        track: crate::state::YouTubeTrack,
        position: Duration,
        is_playing: bool,
        purpose: YouTubeResolutionPurpose,
        activation: Option<&playback_coordinator::ActivationPermit>,
    ) -> Result<()> {
        self.play_youtube_track_at_with_source(
            state, activation, track, position, is_playing, purpose, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn play_youtube_track_at_with_source(
        &self,
        state: &SharedState,
        activation: Option<&playback_coordinator::ActivationPermit>,
        track: crate::state::YouTubeTrack,
        position: Duration,
        is_playing: bool,
        purpose: YouTubeResolutionPurpose,
        source: Option<youtube::playback::ResolvedAudioSource>,
    ) -> Result<()> {
        self.activate_youtube_track_at_with_source(
            state, activation, track, position, is_playing, purpose, source,
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn activate_youtube_track_at_with_source(
        &self,
        state: &SharedState,
        activation: Option<&playback_coordinator::ActivationPermit>,
        track: crate::state::YouTubeTrack,
        position: Duration,
        is_playing: bool,
        purpose: YouTubeResolutionPurpose,
        source: Option<youtube::playback::ResolvedAudioSource>,
    ) -> Result<bool> {
        let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
        let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
        let sessions = playback_coordinator::AppPlaybackSessions::new(state);
        self.playback
            .activate_with_permit(
                activation,
                config::ActiveProvider::YouTubeMusic,
                &spotify,
                &youtube,
                &sessions,
                |cancellation| {
                    self.start_youtube_track_at_with_source(
                        state,
                        track,
                        position,
                        is_playing,
                        purpose,
                        cancellation,
                        source,
                    )
                },
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start_youtube_track_at_with_source(
        &self,
        state: &SharedState,
        track: crate::state::YouTubeTrack,
        position: Duration,
        is_playing: bool,
        purpose: YouTubeResolutionPurpose,
        cancellation: CancellationToken,
        source: Option<youtube::playback::ResolvedAudioSource>,
    ) -> Result<()> {
        #[cfg(feature = "private-capture")]
        let capture = self.claim_youtube_capture(purpose);
        #[cfg(not(feature = "private-capture"))]
        let _ = purpose;
        #[cfg(feature = "private-capture")]
        let outcome_cancellation = cancellation.clone();
        let result = self
            .start_youtube_track_at_inner(
                state,
                track,
                position,
                is_playing,
                cancellation,
                source,
                #[cfg(feature = "private-capture")]
                capture.clone(),
            )
            .await;
        #[cfg(feature = "private-capture")]
        if let Some(capture) = capture {
            if outcome_cancellation.is_cancelled() {
                capture.finish(crate::developer_capture::SafeTerminalCategory::Cancelled);
            } else if result.is_err() {
                capture.finish(crate::developer_capture::SafeTerminalCategory::Failed);
            }
        }
        result
    }

    #[cfg(feature = "private-capture")]
    fn claim_youtube_capture(
        &self,
        purpose: YouTubeResolutionPurpose,
    ) -> Option<crate::developer_capture::CaptureSession> {
        let context = crate::observability::current_operation()?;
        let operation_ref =
            crate::developer_capture::SafeOperationRef::from_hex(context.short_reference()).ok()?;
        let session =
            claim_capture_for_resolution(&self.developer_capture()?, purpose, operation_ref)?;
        if let Ok(payload) = crate::developer_capture::encode_private_fields(
            crate::developer_capture::PrivatePayloadKind::Operation,
            &[crate::developer_capture::PrivateField::text(
                crate::developer_capture::private_field::STAGE,
                "claimed",
            )],
        ) {
            let _ = session.record_with_context(
                None,
                crate::developer_capture::EndpointRole::Operation,
                crate::developer_capture::ProviderClientKind::Unknown,
                crate::developer_capture::TransportKind::Unknown,
                0,
                crate::developer_capture::CaptureRecordKind::OperationBoundary,
                payload,
            );
        } else {
            session.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
        Some(session)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_youtube_track_at_inner(
        &self,
        state: &SharedState,
        mut track: crate::state::YouTubeTrack,
        position: Duration,
        is_playing: bool,
        cancellation: CancellationToken,
        source: Option<youtube::playback::ResolvedAudioSource>,
        #[cfg(feature = "private-capture")] capture: Option<
            crate::developer_capture::CaptureSession,
        >,
    ) -> Result<()> {
        let startup_started = Instant::now();
        let restored_controls = state
            .player
            .read()
            .youtube_playback
            .as_ref()
            .filter(|playback| playback.track.id == track.id)
            .map(|playback| (playback.volume, playback.mute_state));
        {
            let mut player = self.youtube_player.lock().await;
            if let Some((volume, mute_state)) = restored_controls {
                player.restore_controls(volume, mute_state);
            }
            player.stop();
        }
        {
            let mut player = state.player.write();
            player.youtube_playback = None;
            self.state_application
                .set_youtube_phase(&mut player, crate::state::YouTubePlaybackPhase::Resolving);
        }

        tracing::debug!(
            "Resolving YouTube audio with {}",
            self.youtube_audio_resolver.backend_name()
        );
        let resolution_started = Instant::now();
        self.playback.cancel_youtube_prefetch();
        let supplied_source = source
            .filter(|source| source.media_id == track.id && !source.is_expired())
            .map(youtube::playback::PreparedAudioSource::descriptor);
        let prefetched = self
            .youtube_prefetch
            .lock()
            .await
            .take()
            .filter(|(id, source)| id == &track.id && !source.is_expired())
            .map(|(_, source)| source);
        let reused_active_source = supplied_source.is_some();
        let prepared_source = supplied_source.or(prefetched);
        let (resolved_source, mut prepared_decoded) = if let Some(source) = prepared_source {
            let source_stage = if reused_active_source {
                "reused_active_descriptor"
            } else {
                "prefetched_descriptor"
            };
            tracing::debug!(
                stage = source_stage,
                "Using prepared native YouTube audio source"
            );
            #[cfg(feature = "private-capture")]
            if let Some(capture) = &capture {
                if let Ok(payload) = crate::developer_capture::encode_private_fields(
                    crate::developer_capture::PrivatePayloadKind::Operation,
                    &[crate::developer_capture::PrivateField::text(
                        crate::developer_capture::private_field::STAGE,
                        source_stage,
                    )],
                ) {
                    let _ = capture.record_with_context(
                        None,
                        crate::developer_capture::EndpointRole::Operation,
                        crate::developer_capture::ProviderClientKind::Unknown,
                        crate::developer_capture::TransportKind::Unknown,
                        0,
                        crate::developer_capture::CaptureRecordKind::OperationBoundary,
                        payload,
                    );
                } else {
                    capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
                }
            }
            source.into_parts()
        } else {
            #[cfg(feature = "private-capture")]
            let resolution = match capture.clone() {
                Some(capture) => {
                    self.youtube_audio_resolver
                        .resolve_with_capture(
                            &track.id,
                            self.youtube_audio_quality,
                            cancellation.clone(),
                            capture,
                        )
                        .await
                }
                None => {
                    self.youtube_audio_resolver
                        .resolve(&track.id, self.youtube_audio_quality, cancellation.clone())
                        .await
                }
            };
            #[cfg(not(feature = "private-capture"))]
            let resolution = self
                .youtube_audio_resolver
                .resolve(&track.id, self.youtube_audio_quality, cancellation.clone())
                .await;
            let source = match resolution {
                Ok(source) => source,
                Err(err) => {
                    let category = err.kind.diagnostic_category();
                    if !cancellation.is_cancelled() {
                        self.state_application.set_youtube_phase(
                            &mut state.player.write(),
                            crate::state::YouTubePlaybackPhase::Failed(err.to_string()),
                        );
                    }
                    return Err(crate::observability::preserve_error_diagnostic(
                        anyhow::Error::new(err),
                        crate::observability::DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
                        category,
                    )
                    .context("resolve native YouTube audio source"));
                }
            };
            (source, None)
        };
        crate::observability::operation_stage(
            crate::observability::Component::YoutubeMusic,
            "youtube_source_resolve",
            Some(resolution_started.elapsed()),
            Some(crate::observability::OperationOutcome::Success),
        );
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if let Some(duration) = resolved_source
            .duration
            .and_then(|duration| self.state_application.youtube_duration_label(duration))
        {
            track.duration = duration;
        }
        self.state_application.set_youtube_phase(
            &mut state.player.write(),
            crate::state::YouTubePlaybackPhase::Buffering,
        );
        let decode_started = Instant::now();
        #[cfg(feature = "private-capture")]
        let media_exchange_ref = crate::developer_capture::ExchangeRef::from_bytes(rand::random());
        #[cfg(feature = "private-capture")]
        let decoded_result = match (prepared_decoded.take(), capture.clone(), position.is_zero()) {
            (Some(source), None, true) => Ok((source, None)),
            (_, Some(capture), _) => youtube::playback::open_decoded_source_with_capture(
                &resolved_source,
                position,
                self.youtube_audio_cache_size_bytes,
                cancellation.clone(),
                capture,
                media_exchange_ref,
            )
            .await
            .map(|(source, handle)| (source, Some(handle))),
            (_, None, _) => youtube::playback::open_decoded_source(
                &resolved_source,
                position,
                self.youtube_audio_cache_size_bytes,
                cancellation.clone(),
            )
            .await
            .map(|source| (source, None)),
        };
        #[cfg(not(feature = "private-capture"))]
        let decoded_result = match prepared_decoded.take() {
            Some(source) if position.is_zero() => Ok(source),
            _ => {
                youtube::playback::open_decoded_source(
                    &resolved_source,
                    position,
                    self.youtube_audio_cache_size_bytes,
                    cancellation.clone(),
                )
                .await
            }
        };
        #[cfg(feature = "private-capture")]
        let (decoded_source, media_capture) = match decoded_result {
            Ok(source) => source,
            Err(err) => {
                if !cancellation.is_cancelled() {
                    self.state_application.set_youtube_phase(
                        &mut state.player.write(),
                        crate::state::YouTubePlaybackPhase::Failed(format!("{err:#}")),
                    );
                }
                return Err(err);
            }
        };
        #[cfg(not(feature = "private-capture"))]
        let decoded_source = match decoded_result {
            Ok(source) => source,
            Err(err) => {
                if !cancellation.is_cancelled() {
                    self.state_application.set_youtube_phase(
                        &mut state.player.write(),
                        crate::state::YouTubePlaybackPhase::Failed(format!("{err:#}")),
                    );
                }
                return Err(err);
            }
        };
        crate::observability::operation_stage(
            crate::observability::Component::Audio,
            "youtube_media_decode",
            Some(decode_started.elapsed()),
            Some(crate::observability::OperationOutcome::Success),
        );
        if cancellation.is_cancelled() {
            return Ok(());
        }
        #[cfg(feature = "private-capture")]
        let output_started = std::time::Instant::now();
        #[cfg(feature = "private-capture")]
        if let Some(media_capture) = &media_capture {
            media_capture.record_audio_output("started", Duration::ZERO);
        }
        let source_client = resolved_source.source_client;
        let snapshot_result = {
            let mut player = self.youtube_player.lock().await;
            if cancellation.is_cancelled() {
                #[cfg(feature = "private-capture")]
                if let Some(media_capture) = &media_capture {
                    media_capture.record_audio_output("cancelled", output_started.elapsed());
                }
                return Ok(());
            }
            player.play_native(track, resolved_source, decoded_source, position, is_playing)
        };
        #[cfg(feature = "private-capture")]
        if let Some(media_capture) = &media_capture {
            media_capture.record_audio_output(
                if snapshot_result.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
                output_started.elapsed(),
            );
        }
        #[cfg(feature = "private-capture")]
        if snapshot_result.is_ok() {
            if let Some(media_capture) = &media_capture {
                media_capture.commit();
            }
        }
        let mut snapshot = snapshot_result?;
        let mut route = self.youtube_audio_resolver.playback_route();
        route.selected = Some(source_client.to_owned());
        snapshot.route = route;
        crate::observability::operation_stage(
            crate::observability::Component::Audio,
            "youtube_audio_ready",
            Some(startup_started.elapsed()),
            Some(crate::observability::OperationOutcome::Success),
        );
        self.schedule_youtube_thumbnail(state, snapshot.track.clone());
        self.publish_youtube_snapshot(state, snapshot);
        self.schedule_youtube_prefetch(state);
        Ok(())
    }

    fn schedule_youtube_thumbnail(&self, state: &SharedState, track: crate::state::YouTubeTrack) {
        let client = self.clone();
        let state = state.clone();
        tokio::task::spawn(async move {
            if let Err(err) = client.load_youtube_thumbnail(&state, &track).await {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::YOUTUBE_THUMBNAIL_FAILED,
                    crate::observability::ErrorCategory::Unavailable,
                    &err,
                    "Unable to load a YouTube Music thumbnail"
                );
            }
        });
    }

    pub(super) fn schedule_youtube_prefetch(&self, state: &SharedState) {
        let next = state
            .player
            .read()
            .unified_queue
            .as_ref()
            .and_then(crate::state::UnifiedQueue::next_candidate)
            .and_then(|media| match media {
                crate::state::PlayableMedia::YouTube(track) => Some(track.clone()),
                crate::state::PlayableMedia::Spotify(_) => None,
            });
        if let Some(track) = next.as_ref() {
            self.schedule_youtube_thumbnail(state, track.clone());
        }
        let cancellation = self.playback.begin_youtube_prefetch();
        let resolver = self.youtube_audio_resolver.clone();
        let quality = self.youtube_audio_quality;
        let cache_size_bytes = self.youtube_audio_cache_size_bytes;
        let prefetched = self.youtube_prefetch.clone();
        let context = youtube_prefetch_operation_context();
        let task = async move {
            *prefetched.lock().await = None;
            let Some(track) = next else {
                return;
            };
            let resolution = resolver
                .resolve_for_prefetch(&track.id, quality, cancellation.clone())
                .await;
            let source = match resolution {
                Ok(source) if !cancellation.is_cancelled() => source,
                Err(err) if !cancellation.is_cancelled() => {
                    crate::observability::log_safe_error!(
                        debug,
                        crate::observability::DiagnosticCode::YOUTUBE_DESCRIPTOR_PREFETCH_FAILED,
                        crate::observability::ErrorCategory::Unavailable,
                        &err,
                        "Unable to prefetch the next YouTube audio descriptor"
                    );
                    return;
                }
                Ok(_) | Err(_) => return,
            };

            let stream_cancellation = CancellationToken::new();
            let decoded = tokio::select! {
                () = cancellation.cancelled() => {
                    stream_cancellation.cancel();
                    return;
                }
                decoded = youtube::playback::open_decoded_source(
                    &source,
                    Duration::ZERO,
                    cache_size_bytes,
                    stream_cancellation.clone(),
                ) => decoded,
            };
            if cancellation.is_cancelled() {
                stream_cancellation.cancel();
                return;
            }
            let prepared = match decoded {
                Ok(decoded) => {
                    tracing::debug!("Prepared the next native YouTube audio decoder");
                    youtube::playback::PreparedAudioSource::decoded(source, decoded)
                }
                Err(err) => {
                    crate::observability::log_safe_error!(
                            debug,
                            crate::observability::DiagnosticCode::YOUTUBE_AUDIO_PREFETCH_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &err,
                            "Unable to prebuffer the next YouTube audio source; retaining its descriptor"
                        );
                    youtube::playback::PreparedAudioSource::descriptor(source)
                }
            };
            let mut slot = prefetched.lock().await;
            if cancellation.is_cancelled() {
                stream_cancellation.cancel();
                return;
            }
            *slot = Some((track.id, prepared));
        };
        tokio::task::spawn(in_youtube_prefetch(context, task));
    }

    fn publish_youtube_snapshot(
        &self,
        state: &SharedState,
        snapshot: youtube::YouTubePlaybackSnapshot,
    ) {
        let session_entry = SessionEntry::from_youtube_track(&snapshot.track, now_unix_secs());
        let completion = self.capture_unified_queue_completion(
            state,
            config::ActiveProvider::YouTubeMusic,
            &crate::state::PlayableMedia::YouTube(snapshot.track.clone()),
        );
        let track_id = self
            .state_application
            .apply_youtube_playback_snapshot(&mut state.player.write(), snapshot);
        let completion = std::sync::Arc::new(std::sync::Mutex::new(completion));
        *self
            .youtube_queue_completion
            .lock()
            .expect("YouTube queue completion mutex poisoned") =
            Some((track_id.clone(), completion.clone()));
        let configs = config::get_config();
        if let Err(error) = state
            .data
            .write()
            .record_session_entry(session_entry, &configs.app_config.session_history)
        {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::SESSION_HISTORY_SAVE_FAILED,
                crate::observability::ErrorCategory::Storage,
                &error,
                "Local YouTube Music session history could not be saved"
            );
        }
        self.spawn_youtube_playback_monitor(state.clone(), track_id, completion);
    }

    pub(crate) async fn play_unified_item(
        &self,
        state: &SharedState,
        item: crate::state::PlayableMedia,
        activation: Option<&playback_coordinator::ActivationPermit>,
    ) -> Result<()> {
        match item {
            crate::state::PlayableMedia::YouTube(track) => {
                self.play_youtube_track(state, track, activation).await
            }
            crate::state::PlayableMedia::Spotify(playable_id) => {
                let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                self.playback
                    .activate_with_permit(
                        activation,
                        config::ActiveProvider::Spotify,
                        &spotify,
                        &youtube,
                        &sessions,
                        |_cancellation| async {
                            let track_uri = playable_id.uri();
                            match self.start_integrated_spotify_tracks(vec![track_uri]) {
                                Some(Ok(())) => {
                                    tracing::debug!(
                                        route = "integrated_spirc",
                                        "Started a Unified Spotify item"
                                    );
                                    return Ok(());
                                }
                                Some(Err(_)) => tracing::warn!(
                                    "Integrated Spotify start failed; falling back to the Web API"
                                ),
                                None => {}
                            }

                            let device_id = self.connected_integrated_spotify_device_id().await;
                            tracing::debug!(
                                route = "web_api",
                                has_explicit_device = device_id.is_some(),
                                "Starting a Unified Spotify item"
                            );
                            self.start_playback(
                                Playback::URIs(vec![playable_id], None),
                                device_id.as_deref(),
                            )
                            .await?;
                            self.retrieve_current_playback(state, true).await
                        },
                    )
                    .await?;
                Ok(())
            }
        }
    }

    /// Try a unified-queue item and skip items that cannot be played. This
    /// keeps one unavailable provider item from permanently stopping a mixed
    /// queue, while failures that are not about the item leave the remaining
    /// queue where it is.
    pub(crate) async fn play_unified_item_resilient(
        &self,
        state: &SharedState,
        item: crate::state::PlayableMedia,
        activation: Option<&playback_coordinator::ActivationPermit>,
    ) -> Result<()> {
        play_skipping_unplayable_items(
            item,
            move |item| self.play_unified_item(state, item, activation),
            || {
                self.with_current_activation(activation, || {
                    state
                        .player
                        .write()
                        .unified_queue
                        .as_mut()
                        .and_then(crate::state::UnifiedQueue::next)
                })
                .flatten()
            },
        )
        .await
    }

    /// Handle a player request, return a new playback metadata on success
    pub async fn handle_player_request(
        &self,
        request: PlayerRequest,
        mut playback: Option<PlaybackMetadata>,
    ) -> Result<Option<PlaybackMetadata>> {
        // handle requests that don't require an active playback
        match request {
            PlayerRequest::TransferPlayback(device_id, force_play) => {
                // `TransferPlayback` needs to be handled separately from other player requests
                // because `TransferPlayback` doesn't require an active playback
                self.spotify_api()
                    .transfer_playback(&device_id, Some(force_play))
                    .await?;
                tracing::info!("Transferred Spotify playback to the selected device");
                return Ok(None);
            }
            PlayerRequest::StartPlayback(p, shuffle) => {
                // Set the playback's shuffle state if specified in the request
                if let (Some(shuffle), Some(playback)) = (shuffle, playback.as_mut()) {
                    playback.shuffle_state = shuffle;
                }
                let device_id = playback.as_ref().and_then(|p| p.device_id.as_deref());
                self.start_playback(p, device_id).await?;
                // For some reasons, when starting a new playback, the integrated `unified-player`
                // client doesn't respect the initial shuffle state, so we need to manually update the state
                if let Some(ref playback) = playback {
                    self.spotify_api()
                        .shuffle(playback.shuffle_state, device_id)
                        .await?;
                }
                return Ok(None);
            }
            _ => {}
        }

        let mut playback = playback.context("no playback found")?;
        let device_id = playback.device_id.as_deref();

        match request {
            PlayerRequest::NextTrack => self.spotify_api().next_track(device_id).await?,
            PlayerRequest::PreviousTrack => self.spotify_api().previous_track(device_id).await?,
            PlayerRequest::Resume => {
                if !playback.is_playing {
                    self.spotify_api().resume_playback(device_id, None).await?;
                    playback.is_playing = true;
                }
            }

            PlayerRequest::Pause => {
                if playback.is_playing {
                    self.spotify_api().pause_playback(device_id).await?;
                    playback.is_playing = false;
                }
            }
            PlayerRequest::ResumePause => {
                if playback.is_playing {
                    self.spotify_api().pause_playback(device_id).await?;
                } else {
                    self.spotify_api().resume_playback(device_id, None).await?;
                }
                playback.is_playing = !playback.is_playing;
            }
            PlayerRequest::SeekTrack(position_ms) => {
                self.spotify_api()
                    .seek_track(position_ms, device_id)
                    .await?;
            }
            PlayerRequest::Repeat => {
                let next_repeat_state = match playback.repeat_state {
                    rspotify::model::RepeatState::Off => rspotify::model::RepeatState::Track,
                    rspotify::model::RepeatState::Track => rspotify::model::RepeatState::Context,
                    rspotify::model::RepeatState::Context => rspotify::model::RepeatState::Off,
                };

                self.spotify_api()
                    .repeat(next_repeat_state, device_id)
                    .await?;

                playback.repeat_state = next_repeat_state;
            }
            PlayerRequest::Shuffle => {
                self.spotify_api()
                    .shuffle(!playback.shuffle_state, device_id)
                    .await?;

                playback.shuffle_state = !playback.shuffle_state;
            }
            PlayerRequest::Volume(volume) => {
                self.volume_with_diagnostics(volume, device_id).await?;

                playback.volume = Some(u32::from(volume));
                playback.mute_state = None;
            }
            PlayerRequest::ToggleMute => {
                let new_mute_state = match playback.mute_state {
                    None => {
                        self.volume_with_diagnostics(0, device_id).await?;
                        Some(playback.volume.unwrap_or_default())
                    }
                    Some(volume) => {
                        self.volume_with_diagnostics(volume as u8, device_id)
                            .await?;
                        None
                    }
                };

                playback.mute_state = new_mute_state;
            }
            PlayerRequest::StartPlayback(..) => {
                anyhow::bail!("`StartPlayback` should be handled earlier")
            }
            PlayerRequest::TransferPlayback(..) => {
                anyhow::bail!("`TransferPlayback` should be handled earlier")
            }
        }

        Ok(Some(playback))
    }
}

#[cfg(all(test, feature = "private-capture"))]
mod private_capture_tests {
    use super::{claim_capture_for_resolution, YouTubeResolutionPurpose};
    use crate::developer_capture::{
        CaptureLimits, CapturePassphrase, SafeCaptureState, SafeOperationRef, SafeTerminalCategory,
    };

    fn armed_handle() -> (
        tempfile::TempDir,
        crate::developer_capture::CaptureHandle,
        crate::developer_capture::CaptureWorker,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let (handle, worker, _) = crate::developer_capture::prepare_runtime(
            directory.path().join("vault"),
            CaptureLimits::default(),
        )
        .unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("purpose route passphrase".to_owned()).unwrap())
            .unwrap();
        (directory, handle, worker)
    }

    #[test]
    fn resume_route_leaves_the_arm_for_one_manual_foreground_claim() {
        let (_directory, handle, _worker) = armed_handle();
        assert!(claim_capture_for_resolution(
            &handle,
            YouTubeResolutionPurpose::Resume,
            SafeOperationRef::from_bytes([1; 4])
        )
        .is_none());
        assert_eq!(handle.snapshot().state, SafeCaptureState::Armed);

        let session = claim_capture_for_resolution(
            &handle,
            YouTubeResolutionPurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([2; 4]),
        )
        .unwrap();
        assert_eq!(handle.snapshot().state, SafeCaptureState::Claimed);
        assert!(claim_capture_for_resolution(
            &handle,
            YouTubeResolutionPurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([3; 4])
        )
        .is_none());
        session.finish(SafeTerminalCategory::Failed);
    }
}

impl AppClient {
    /// Start a playback
    async fn start_playback(&self, playback: Playback, device_id: Option<&str>) -> Result<()> {
        match playback {
            Playback::Context(id, offset) => match id {
                ContextId::Album(id) => {
                    self.spotify_api()
                        .start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Artist(id) => {
                    self.spotify_api()
                        .start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Playlist(id) => {
                    self.spotify_api()
                        .start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Show(id) => {
                    self.spotify_api()
                        .start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Tracks(_) => {
                    anyhow::bail!("`StartPlayback` request for `tracks` context is not supported")
                }
            },
            Playback::URIs(ids, offset) => {
                self.spotify_api()
                    .start_uris_playback(ids, device_id, offset, None)
                    .await?;
            }
        }

        Ok(())
    }
    fn spawn_youtube_playback_monitor(
        &self,
        state: SharedState,
        track_id: String,
        completion_slot: std::sync::Arc<std::sync::Mutex<Option<request::UnifiedQueueCompletion>>>,
    ) {
        let youtube_player = self.youtube_player.clone();
        let client = self.clone();
        tokio::task::spawn(async move {
            let Some(completion_sink) = youtube_player.lock().await.completion_sink(&track_id)
            else {
                return;
            };
            let wait_sink = completion_sink.clone();
            let mut completion_wait = Some(tokio::task::spawn_blocking(move || {
                wait_sink.sleep_until_end();
            }));
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            let mut last_persisted = Instant::now();
            loop {
                let completion_observed = if let Some(wait) = completion_wait.as_mut() {
                    tokio::select! {
                        _ = interval.tick() => false,
                        _ = wait => true,
                    }
                } else {
                    interval.tick().await;
                    false
                };
                if completion_observed {
                    completion_wait = None;
                }
                let Some((playing_track_id, snapshot, ended, source_expired)) = youtube_player
                    .lock()
                    .await
                    .snapshot_for_sink(&completion_sink)
                else {
                    break;
                };

                if playing_track_id != track_id {
                    break;
                }

                let playback_matches = {
                    let mut player = state.player.write();
                    let matches = match &mut player.youtube_playback {
                        Some(playback) if playback.track.id == track_id => {
                            playback.is_playing = snapshot.is_playing;
                            playback.progress = snapshot.progress;
                            playback.volume = snapshot.volume;
                            playback.mute_state = snapshot.mute_state;
                            true
                        }
                        Some(_) => false,
                        None => true,
                    };
                    if matches {
                        client.state_application.set_youtube_phase(
                            &mut player,
                            if ended {
                                crate::state::YouTubePlaybackPhase::Idle
                            } else if snapshot.is_playing {
                                crate::state::YouTubePlaybackPhase::Playing
                            } else {
                                crate::state::YouTubePlaybackPhase::Paused
                            },
                        );
                    }
                    matches
                };
                if !playback_matches {
                    break;
                }

                let sessions = playback_coordinator::AppPlaybackSessions::new(&state);
                client
                    .playback
                    .refresh_session(&sessions, config::ActiveProvider::YouTubeMusic);

                if last_persisted.elapsed() >= Duration::from_secs(10) {
                    client.playback.persist_sessions(&sessions);
                    last_persisted = Instant::now();
                }

                if ended {
                    tracing::info!("YouTube Music playback ended");
                    client.playback.persist_sessions(&sessions);
                    if source_expired {
                        let should_retry = {
                            let mut recovered = client
                                .youtube_expiry_recovery
                                .lock()
                                .expect("YouTube expiry recovery mutex poisoned");
                            if recovered.as_deref() == Some(track_id.as_str()) {
                                false
                            } else {
                                *recovered = Some(track_id.clone());
                                true
                            }
                        };
                        if should_retry {
                            let track = state
                                .player
                                .read()
                                .youtube_playback
                                .as_ref()
                                .map(|playback| playback.track.clone());
                            if let Some(track) = track {
                                tracing::info!(
                                    "Refreshing expired YouTube audio descriptor at {:?}",
                                    snapshot.progress
                                );
                                if let Err(err) = client
                                    .play_youtube_track_at(
                                        &state,
                                        track,
                                        snapshot.progress,
                                        true,
                                        YouTubeResolutionPurpose::Resume,
                                        None,
                                    )
                                    .await
                                {
                                    crate::observability::log_safe_error!(
                                        warn,
                                        crate::observability::DiagnosticCode::YOUTUBE_SOURCE_REFRESH_FAILED,
                                        crate::observability::ErrorCategory::Unavailable,
                                        &err,
                                        "Unable to recover an expired YouTube audio source"
                                    );
                                }
                                break;
                            }
                        }
                    }
                    let completion = completion_slot
                        .lock()
                        .expect("YouTube queue completion slot mutex poisoned")
                        .clone();
                    {
                        let mut registry = client
                            .youtube_queue_completion
                            .lock()
                            .expect("YouTube queue completion mutex poisoned");
                        if registry.as_ref().is_some_and(|(_, current)| {
                            std::sync::Arc::ptr_eq(current, &completion_slot)
                        }) {
                            *registry = None;
                        }
                    }
                    if let Some(completion) = completion {
                        client.emit_unified_queue_completion(completion);
                    }
                    break;
                }
            }
        });
    }
    #[cfg(feature = "notify")]
    /// Create a notification for a new playback
    pub(super) fn notify_new_playback(
        playable: &rspotify::model::PlayableItem,
        cover_img_path: &std::path::Path,
    ) -> Result<()> {
        let mut n = notify_rust::Notification::new();

        let re = regex::Regex::new(r"\{.*?\}").unwrap();
        // Generate a text described a track from a format string.
        // For example, a format string "{track} - {artists}" will generate
        // a text consisting of the track's name followed by a dash then artists' names.
        let get_text_from_format_str = |format_str: &str| {
            let mut text = String::new();

            let mut ptr = 0;
            for m in re.find_iter(format_str) {
                let s = m.start();
                let e = m.end();

                if ptr < s {
                    text += &format_str[ptr..s];
                }
                ptr = e;
                match m.as_str() {
                    "{track}" => {
                        let name = match playable {
                            rspotify::model::PlayableItem::Track(ref track) => &track.name,
                            rspotify::model::PlayableItem::Episode(ref episode) => &episode.name,
                            rspotify::model::PlayableItem::Unknown(_) => continue,
                        };
                        text += name;
                    }
                    "{artists}" => {
                        if let rspotify::model::PlayableItem::Track(ref track) = playable {
                            text += &crate::utils::map_join(&track.artists, |a| &a.name, ", ");
                        }
                    }
                    "{album}" => match playable {
                        rspotify::model::PlayableItem::Track(ref track) => {
                            text += &track.album.name;
                        }
                        rspotify::model::PlayableItem::Episode(ref episode) => {
                            text += &episode.show.name;
                        }
                        rspotify::model::PlayableItem::Unknown(_) => {}
                    },
                    &_ => {}
                }
            }
            if ptr < format_str.len() {
                text += &format_str[ptr..];
            }

            text
        };

        let configs = config::get_config();

        n.appname("unified-player")
            .summary(&get_text_from_format_str(
                &configs.app_config.notify_format.summary,
            ))
            .body(&get_text_from_format_str(
                &configs.app_config.notify_format.body,
            ));
        if cover_img_path.exists() {
            n.icon(cover_img_path.to_str().context("valid cover_img_path")?);
        }
        if configs.app_config.notify_timeout_in_secs > 0 {
            n.timeout(std::time::Duration::from_secs(
                configs.app_config.notify_timeout_in_secs,
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        if configs.app_config.notify_transient {
            use notify_rust::Hint;
            n.hint(Hint::Transient(true));
        }
        n.show()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        in_youtube_prefetch, parse_youtube_track_duration,
        refresh_completion_for_same_queue_occurrence, resolve_seek_target,
        youtube_prefetch_operation_context,
    };

    mod unified_queue_failures {
        use std::{cell::RefCell, collections::VecDeque};

        use super::super::play_skipping_unplayable_items;
        use crate::observability::{preserve_error_diagnostic, DiagnosticCode, ErrorCategory};
        use crate::state::{PlayableMedia, YouTubeTrack};

        fn item(id: &str) -> PlayableMedia {
            PlayableMedia::YouTube(YouTubeTrack {
                id: id.to_owned(),
                name: id.to_owned(),
                artists: String::new(),
                album: None,
                duration: String::new(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            })
        }

        fn failure(category: Option<ErrorCategory>) -> anyhow::Error {
            let error = anyhow::anyhow!("start failed");
            match category {
                Some(category) => preserve_error_diagnostic(
                    error,
                    DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
                    category,
                ),
                None => error,
            }
        }

        /// Run the skip policy over `a, b, c` where each start returns the
        /// next scripted outcome; returns the result, the started items and
        /// how many items the queue handed out.
        async fn run(
            outcomes: Vec<Option<Option<ErrorCategory>>>,
            next_available: bool,
        ) -> (anyhow::Result<()>, Vec<String>, usize) {
            let outcomes = RefCell::new(VecDeque::from(outcomes));
            let started = RefCell::new(Vec::new());
            let queue = RefCell::new(VecDeque::from([item("b"), item("c")]));
            let advanced = RefCell::new(0);
            let result = play_skipping_unplayable_items(
                item("a"),
                |media| {
                    started.borrow_mut().push(media.media_id().raw_id);
                    let outcome = outcomes.borrow_mut().pop_front().flatten();
                    async move { outcome.map_or(Ok(()), |category| Err(failure(category))) }
                },
                || {
                    *advanced.borrow_mut() += 1;
                    next_available
                        .then(|| queue.borrow_mut().pop_front())
                        .flatten()
                },
            )
            .await;
            (result, started.into_inner(), advanced.into_inner())
        }

        #[tokio::test]
        async fn unplayable_items_are_skipped_until_one_starts() {
            for category in [
                ErrorCategory::ConsentAgeRegion,
                ErrorCategory::ProviderUnavailable,
                ErrorCategory::UnsupportedFormat,
            ] {
                let (result, started, advanced) =
                    run(vec![Some(Some(category)), Some(Some(category)), None], true).await;
                assert!(result.is_ok(), "{category:?}");
                assert_eq!(started, ["a", "b", "c"], "{category:?}");
                assert_eq!(advanced, 2, "{category:?}");
            }
        }

        #[tokio::test]
        async fn failures_that_are_not_about_the_item_keep_the_queue() {
            for category in [
                Some(ErrorCategory::Authentication),
                Some(ErrorCategory::RateLimited),
                Some(ErrorCategory::Network),
                Some(ErrorCategory::NetworkUnavailable),
                Some(ErrorCategory::ProofToken),
                Some(ErrorCategory::Decipher),
                Some(ErrorCategory::MediaForbidden),
                Some(ErrorCategory::Resource),
                Some(ErrorCategory::Unavailable),
                Some(ErrorCategory::Cancelled),
                None,
            ] {
                let (result, started, advanced) = run(vec![Some(category)], true).await;
                let error = result.expect_err("the failure must reach the caller");
                assert_eq!(
                    crate::observability::preserved_error_diagnostic(&error)
                        .map(|(_, category)| category),
                    category,
                    "the original category must stay reportable"
                );
                assert_eq!(started, ["a"], "{category:?}");
                assert_eq!(advanced, 0, "{category:?} must not consume the queue");
            }
        }

        #[tokio::test]
        async fn a_stale_activation_stops_after_an_unplayable_item() {
            let (result, started, advanced) =
                run(vec![Some(Some(ErrorCategory::ProviderUnavailable))], false).await;
            assert!(result.is_err());
            assert_eq!(started, ["a"]);
            assert_eq!(advanced, 1);
        }

        #[tokio::test]
        async fn a_successful_or_superseded_start_does_not_advance() {
            let (result, started, advanced) = run(vec![None], true).await;
            assert!(result.is_ok());
            assert_eq!(started, ["a"]);
            assert_eq!(advanced, 0);
        }
    }

    #[test]
    fn active_seek_targets_apply_relative_bounds_and_playback_bar_fraction() {
        let duration = std::time::Duration::from_secs(60);
        assert_eq!(
            resolve_seek_target(
                super::request::ActivePlaybackSeek::Start,
                None,
                Some(duration),
            ),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(
            resolve_seek_target(
                super::request::ActivePlaybackSeek::Absolute(std::time::Duration::from_secs(75),),
                None,
                Some(duration),
            ),
            Some(duration)
        );
        assert_eq!(
            resolve_seek_target(
                super::request::ActivePlaybackSeek::Relative(chrono::Duration::seconds(5)),
                Some(std::time::Duration::from_secs(58)),
                Some(duration),
            ),
            Some(duration)
        );
        assert_eq!(
            resolve_seek_target(
                super::request::ActivePlaybackSeek::Relative(chrono::Duration::seconds(-10)),
                Some(std::time::Duration::from_secs(3)),
                Some(duration),
            ),
            Some(std::time::Duration::ZERO)
        );
        assert_eq!(
            resolve_seek_target(
                super::request::ActivePlaybackSeek::Fraction {
                    numerator: 3,
                    denominator: 4,
                },
                None,
                Some(duration),
            ),
            Some(std::time::Duration::from_secs(45))
        );
        assert_eq!(
            resolve_seek_target(
                super::request::ActivePlaybackSeek::Fraction {
                    numerator: 1,
                    denominator: 0,
                },
                None,
                Some(duration),
            ),
            None
        );
    }

    #[test]
    fn youtube_seek_duration_parser_rejects_invalid_parts() {
        assert_eq!(
            parse_youtube_track_duration("1:02:03"),
            Some(std::time::Duration::from_secs(3_723))
        );
        assert_eq!(parse_youtube_track_duration("not-a-duration"), None);
        assert_eq!(parse_youtube_track_duration(""), None);
    }

    fn youtube_media(id: &str) -> crate::state::PlayableMedia {
        crate::state::PlayableMedia::YouTube(crate::state::YouTubeTrack {
            id: id.to_owned(),
            name: id.to_owned(),
            artists: "artist".to_owned(),
            album: None,
            duration: "1:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        })
    }

    #[test]
    fn youtube_resume_refreshes_generation_only_for_the_same_queue_occurrence() {
        let media = youtube_media("same");
        let queue = crate::state::UnifiedQueue::new(vec![media.clone()], 0);
        let queue_token = queue.completion_token_for(&media).unwrap();
        let mut current = Some(super::request::UnifiedQueueCompletion {
            source: crate::config::ActiveProvider::YouTubeMusic,
            queue: queue_token.clone(),
            activation_generation: 3,
        });

        assert!(refresh_completion_for_same_queue_occurrence(
            &mut current,
            super::request::UnifiedQueueCompletion {
                source: crate::config::ActiveProvider::YouTubeMusic,
                queue: queue_token,
                activation_generation: 5,
            }
        ));
        assert_eq!(current.as_ref().unwrap().activation_generation, 5);

        let replacement = crate::state::UnifiedQueue::new(vec![media.clone()], 0);
        assert!(!refresh_completion_for_same_queue_occurrence(
            &mut current,
            super::request::UnifiedQueueCompletion {
                source: crate::config::ActiveProvider::YouTubeMusic,
                queue: replacement.completion_token_for(&media).unwrap(),
                activation_generation: 7,
            }
        ));
        assert_eq!(current.as_ref().unwrap().activation_generation, 5);
    }

    #[test]
    fn queue_created_mid_track_installs_missing_completion_evidence() {
        let media = youtube_media("current");
        let queue = crate::state::UnifiedQueue::new(vec![media.clone()], 0);
        let mut current = None;

        assert!(refresh_completion_for_same_queue_occurrence(
            &mut current,
            super::request::UnifiedQueueCompletion {
                source: crate::config::ActiveProvider::YouTubeMusic,
                queue: queue.completion_token_for(&media).unwrap(),
                activation_generation: 4,
            }
        ));
        assert_eq!(current.unwrap().activation_generation, 4);
    }

    #[tokio::test]
    async fn spawned_prefetch_inherits_trace_and_owns_a_named_child_span() {
        let mut parent = crate::observability::OperationContext::new(
            "play_youtube_track",
            crate::observability::OperationSource::Terminal,
        );
        parent.provider = Some(crate::observability::ProviderKind::YoutubeMusic);

        let context = crate::observability::in_operation(parent.clone(), async {
            youtube_prefetch_operation_context()
        })
        .await;
        let observed = tokio::task::spawn(in_youtube_prefetch(context.clone(), async {
            crate::observability::child_or_new(
                "unexpected_new_operation",
                crate::observability::OperationSource::System,
            )
        }))
        .await
        .expect("prefetch task should complete");

        assert_eq!(context.trace_id, parent.trace_id);
        assert_ne!(context.span_id, parent.span_id);
        assert_eq!(context.operation, "youtube_prefetch");
        assert_eq!(context.source, parent.source);
        assert_eq!(context.provider, parent.provider);
        assert_eq!(observed.trace_id, context.trace_id);
        assert_ne!(observed.span_id, context.span_id);
        assert_eq!(observed.operation, context.operation);
        assert_eq!(observed.source, context.source);
        assert_eq!(observed.provider, context.provider);
    }
}

impl AppClient {
    pub(crate) fn cancel_pending_playback_work(&self) {
        self.playback.cancel_pending_work();
    }

    pub(super) async fn handle_playback_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
        activation: Option<&playback_coordinator::ActivationPermit>,
    ) -> Result<RequestDisposition> {
        match request {
            ClientRequest::ActivePlaybackControl(control) => {
                let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                let outcome = self
                    .playback
                    .control_active_playback(control, &spotify, &youtube, &sessions)
                    .await?;
                match outcome {
                    playback_coordinator::ActivePlaybackControlOutcome::Applied(provider) => {
                        if provider == config::ActiveProvider::Spotify {
                            self.update_playback(state);
                        }
                        Ok(RequestDisposition::Applied)
                    }
                    playback_coordinator::ActivePlaybackControlOutcome::AlreadySatisfied(
                        provider,
                    ) => {
                        tracing::debug!(
                            provider = provider.title(),
                            "Active playback control was already satisfied"
                        );
                        Ok(RequestDisposition::Applied)
                    }
                    playback_coordinator::ActivePlaybackControlOutcome::Rejected(reason) => {
                        Ok(RequestDisposition::Rejected(reason))
                    }
                    playback_coordinator::ActivePlaybackControlOutcome::Superseded => {
                        Ok(RequestDisposition::Superseded)
                    }
                }
            }
            ClientRequest::ActivePlaybackSeek(seek) => {
                let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                let permit = match self.playback.acquire_active_seek(&spotify, &youtube).await {
                    Ok(permit) => permit,
                    Err(reason) => return Ok(RequestDisposition::Rejected(reason)),
                };
                let cancellation = permit.cancellation();
                match permit.provider() {
                    config::ActiveProvider::Spotify => {
                        let (position, duration) = {
                            let player = state.player.read();
                            let position = player
                                .playback_progress()
                                .and_then(|progress| progress.to_std().ok());
                            let duration = match player.currently_playing() {
                                Some(rspotify::model::PlayableItem::Track(track)) => {
                                    track.duration.to_std().ok()
                                }
                                Some(rspotify::model::PlayableItem::Episode(episode)) => {
                                    episode.duration.to_std().ok()
                                }
                                Some(rspotify::model::PlayableItem::Unknown(_)) | None => None,
                            };
                            (position, duration)
                        };
                        let Some(position) = resolve_seek_target(seek, position, duration) else {
                            return Ok(RequestDisposition::Rejected(
                                request::PlaybackControlRejection::SeekUnavailable,
                            ));
                        };
                        let position = chrono::Duration::from_std(position)?;
                        tokio::select! {
                            biased;
                            () = cancellation.cancelled() => {
                                return Ok(RequestDisposition::Superseded);
                            }
                            result = spotify.seek(position) => result?,
                        }
                        if !self.playback.active_seek_is_current(&permit) {
                            return Ok(RequestDisposition::Superseded);
                        }
                        state.player.write().apply_spotify_seek(position);
                        self.refresh_and_persist_session(state, config::ActiveProvider::Spotify);
                        self.update_playback(state);
                    }
                    config::ActiveProvider::YouTubeMusic => {
                        let local = tokio::select! {
                            biased;
                            () = cancellation.cancelled() => {
                                return Ok(RequestDisposition::Superseded);
                            }
                            player = self.youtube_player.lock() => {
                                player.restart_info().map(|restart| {
                                    (
                                        Some(restart.progress),
                                        parse_youtube_track_duration(&restart.track.duration),
                                    )
                                })
                            }
                        };
                        let (position, duration) = local.unwrap_or_else(|| {
                            state.player.read().youtube_playback.as_ref().map_or(
                                (None, None),
                                |playback| {
                                    (
                                        Some(playback.progress),
                                        parse_youtube_track_duration(&playback.track.duration),
                                    )
                                },
                            )
                        });
                        let Some(position) = resolve_seek_target(seek, position, duration) else {
                            return Ok(RequestDisposition::Rejected(
                                request::PlaybackControlRejection::SeekUnavailable,
                            ));
                        };
                        if !self.playback.active_seek_is_current(&permit) {
                            return Ok(RequestDisposition::Superseded);
                        }
                        match self.seek_youtube_at(state, position, &permit).await? {
                            YouTubeSeekOutcome::AppliedInPlace => {
                                if !self.playback.active_seek_is_current(&permit) {
                                    return Ok(RequestDisposition::Superseded);
                                }
                            }
                            YouTubeSeekOutcome::Reactivated => {}
                            YouTubeSeekOutcome::Unavailable => {
                                return Ok(RequestDisposition::Rejected(
                                    request::PlaybackControlRejection::SeekUnavailable,
                                ));
                            }
                            YouTubeSeekOutcome::Superseded => {
                                return Ok(RequestDisposition::Superseded);
                            }
                        }
                    }
                }
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::Player(request) => {
                let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                let coordinated_control = match &request {
                    PlayerRequest::Pause => {
                        let changed = self
                            .playback
                            .pause(
                                config::ActiveProvider::Spotify,
                                &spotify,
                                &youtube,
                                &sessions,
                            )
                            .await?;
                        Some(changed)
                    }
                    PlayerRequest::Resume => {
                        let changed = self
                            .playback
                            .resume_with_permit(
                                activation,
                                config::ActiveProvider::Spotify,
                                &spotify,
                                &youtube,
                                &sessions,
                            )
                            .await?;
                        Some(changed)
                    }
                    PlayerRequest::ResumePause => {
                        let changed = self
                            .playback
                            .toggle_pause_with_permit(
                                activation,
                                config::ActiveProvider::Spotify,
                                &spotify,
                                &youtube,
                                &sessions,
                            )
                            .await?;
                        Some(changed)
                    }
                    _ => None,
                };
                if let Some(changed) = coordinated_control {
                    if !changed {
                        return Ok(RequestDisposition::NoOp);
                    }
                    self.update_playback(state);
                    return Ok(RequestDisposition::Applied);
                }
                let starts_playback = matches!(&request, PlayerRequest::StartPlayback(..));
                if !starts_playback
                    && !self
                        .playback
                        .accepts_control(config::ActiveProvider::Spotify)
                {
                    return Ok(RequestDisposition::NoOp);
                }
                if let PlayerRequest::StartPlayback(Playback::URIs(ids, offset), _) = &request {
                    if self
                        .with_current_activation(activation, || {
                            let mut player = state.player.write();
                            if ids.len() > 1 {
                                let start_position = offset
                                    .as_ref()
                                    .and_then(|offset| match offset {
                                        rspotify::model::Offset::Uri(uri) => {
                                            ids.iter().position(|id| id.uri() == *uri)
                                        }
                                        rspotify::model::Offset::Position(_) => None,
                                    })
                                    .unwrap_or_default();
                                player.unified_queue = Some(crate::state::UnifiedQueue::new(
                                    ids.iter()
                                        .cloned()
                                        .map(crate::state::PlayableMedia::Spotify)
                                        .collect(),
                                    start_position,
                                ));
                            } else {
                                player.clear_local_queue_for_native_playback();
                            }
                        })
                        .is_none()
                    {
                        return Ok(RequestDisposition::NoOp);
                    }
                }
                if starts_playback {
                    let starts_native_context = matches!(
                        &request,
                        PlayerRequest::StartPlayback(Playback::Context(..), _)
                    );
                    let changed = self
                        .playback
                        .activate_with_permit(
                            activation,
                            config::ActiveProvider::Spotify,
                            &spotify,
                            &youtube,
                            &sessions,
                            |_cancellation| async {
                                let playback = state.player.read().buffered_playback.clone();
                                let playback =
                                    self.handle_player_request(request, playback).await?;
                                let mut player = state.player.write();
                                player.buffered_playback = playback;
                                if starts_native_context {
                                    player.clear_local_queue_for_native_playback();
                                }
                                Ok(())
                            },
                        )
                        .await?;
                    if !changed {
                        return Ok(RequestDisposition::NoOp);
                    }
                } else if let PlayerRequest::SeekTrack(position) = &request {
                    spotify.seek(*position).await?;
                    state.player.write().apply_spotify_seek(*position);
                    let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                    self.playback
                        .refresh_and_persist(&sessions, config::ActiveProvider::Spotify);
                } else {
                    let playback = state.player.read().buffered_playback.clone();
                    let playback = self.handle_player_request(request, playback).await?;
                    state.player.write().buffered_playback = playback;
                    let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                    self.playback
                        .refresh_and_persist(&sessions, config::ActiveProvider::Spotify);
                }
                self.update_playback(state);
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::GetCurrentPlayback => {
                self.retrieve_current_playback(state, true).await?;
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::ShutdownPlayback => {
                let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                let result = self.playback.shutdown(&spotify, &youtube, &sessions).await;
                state.mark_shutdown_phase(crate::runtime::ShutdownPhase::PlaybackStopped)?;
                state.mark_shutdown_phase(crate::runtime::ShutdownPhase::SessionsPersisted)?;
                youtube::browser_auth::shutdown_playback_browser().await;
                state.mark_shutdown_phase(crate::runtime::ShutdownPhase::BrowserClosed)?;
                state.mark_playback_shutdown_complete();
                result?;
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::SwitchProvider(provider) => {
                let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                let changed = self
                    .playback
                    .switch_to_with_permit(activation, provider, &spotify, &youtube, &sessions)
                    .await?;
                if !changed {
                    return Ok(RequestDisposition::NoOp);
                }
                // The coordinator claims the target before this best-effort
                // refresh so controls cannot be routed to the old provider
                // while the library request is in flight.
                if provider == config::ActiveProvider::YouTubeMusic {
                    if let Err(error) = self.refresh_youtube_library(state).await {
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::YOUTUBE_LIBRARY_FETCH_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &error,
                            "YouTube Music library refresh during provider switch failed"
                        );
                    }
                }
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::PlayYouTubeContext {
                tracks,
                start_index,
            } => {
                let Some(track) = tracks.get(start_index).cloned() else {
                    tracing::warn!("Ignoring empty YouTube playback context");
                    return Ok(RequestDisposition::NoOp);
                };
                if self
                    .with_current_activation(activation, || {
                        state.player.write().unified_queue = Some(crate::state::UnifiedQueue::new(
                            tracks
                                .into_iter()
                                .map(crate::state::PlayableMedia::YouTube)
                                .collect(),
                            start_index,
                        ));
                    })
                    .is_none()
                {
                    return Ok(RequestDisposition::NoOp);
                }
                self.play_youtube_track(state, track, activation).await?;
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::PlayUnifiedItems { items, start_index } => {
                let Some(item) = items.get(start_index).cloned() else {
                    tracing::warn!("Ignoring empty unified playlist");
                    return Ok(RequestDisposition::NoOp);
                };
                if self
                    .with_current_activation(activation, || {
                        state.player.write().unified_queue =
                            Some(crate::state::UnifiedQueue::new(items, start_index));
                    })
                    .is_none()
                {
                    return Ok(RequestDisposition::NoOp);
                }
                self.play_unified_item_resilient(state, item, activation)
                    .await?;
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::ContinueUnifiedQueue(completion) => {
                let item = self
                    .with_current_activation(activation, || {
                        state
                            .player
                            .write()
                            .unified_queue
                            .as_mut()
                            .and_then(|queue| queue.advance_after_completion(&completion.queue))
                    })
                    .flatten();
                let Some(item) = item else {
                    return Ok(RequestDisposition::NoOp);
                };
                let handoff = match (completion.source, item.provider()) {
                    (config::ActiveProvider::Spotify, crate::state::Provider::Spotify) => {
                        "spotify_to_spotify"
                    }
                    (config::ActiveProvider::Spotify, crate::state::Provider::YouTubeMusic) => {
                        "spotify_to_youtube"
                    }
                    (config::ActiveProvider::YouTubeMusic, crate::state::Provider::Spotify) => {
                        "youtube_to_spotify"
                    }
                    (
                        config::ActiveProvider::YouTubeMusic,
                        crate::state::Provider::YouTubeMusic,
                    ) => "youtube_to_youtube",
                };
                tracing::info!(
                    state = handoff,
                    "Continuing the unified queue after provider completion"
                );
                self.play_unified_item_resilient(state, item, activation)
                    .await?;
                Ok(RequestDisposition::Applied)
            }
            ClientRequest::UnifiedNext => {
                let item = self
                    .with_current_activation(activation, || {
                        state
                            .player
                            .write()
                            .unified_queue
                            .as_mut()
                            .and_then(crate::state::UnifiedQueue::next)
                    })
                    .flatten();
                if let Some(item) = item {
                    self.play_unified_item_resilient(state, item, activation)
                        .await?;
                    return Ok(RequestDisposition::Applied);
                }
                Ok(RequestDisposition::NoOp)
            }
            ClientRequest::UnifiedPrevious => {
                let item = self
                    .with_current_activation(activation, || {
                        state
                            .player
                            .write()
                            .unified_queue
                            .as_mut()
                            .and_then(crate::state::UnifiedQueue::previous)
                    })
                    .flatten();
                if let Some(item) = item {
                    self.play_unified_item_resilient(state, item, activation)
                        .await?;
                    return Ok(RequestDisposition::Applied);
                }
                Ok(RequestDisposition::NoOp)
            }
            ClientRequest::YouTubePlayer(request) => {
                let accepts_request = match &request {
                    request::YouTubePlayerRequest::Next
                    | request::YouTubePlayerRequest::Previous => self
                        .playback
                        .accepts_replacement(config::ActiveProvider::YouTubeMusic),
                    _ => self
                        .playback
                        .accepts_control(config::ActiveProvider::YouTubeMusic),
                };
                if !accepts_request {
                    return Ok(RequestDisposition::NoOp);
                }
                match request {
                    request::YouTubePlayerRequest::Refresh => {
                        let Some((_, snapshot, _, _)) = self.youtube_player.lock().await.snapshot()
                        else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        self.state_application
                            .apply_youtube_control_snapshot(&mut state.player.write(), &snapshot);
                        self.refresh_and_persist_session(
                            state,
                            config::ActiveProvider::YouTubeMusic,
                        );
                    }
                    request::YouTubePlayerRequest::Next => {
                        let item = self
                            .with_current_activation(activation, || {
                                state
                                    .player
                                    .write()
                                    .unified_queue
                                    .as_mut()
                                    .and_then(crate::state::UnifiedQueue::next)
                            })
                            .flatten();
                        let Some(item) = item else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        self.play_unified_item_resilient(state, item, activation)
                            .await?;
                    }
                    request::YouTubePlayerRequest::Previous => {
                        let item = self
                            .with_current_activation(activation, || {
                                state
                                    .player
                                    .write()
                                    .unified_queue
                                    .as_mut()
                                    .and_then(crate::state::UnifiedQueue::previous)
                            })
                            .flatten();
                        let Some(item) = item else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        self.play_unified_item_resilient(state, item, activation)
                            .await?;
                    }
                    request::YouTubePlayerRequest::Repeat => {
                        let mut player = state.player.write();
                        let Some(queue) = player.unified_queue.as_mut() else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        let next = match queue.repeat() {
                            rspotify::model::RepeatState::Off => {
                                rspotify::model::RepeatState::Track
                            }
                            rspotify::model::RepeatState::Track => {
                                rspotify::model::RepeatState::Context
                            }
                            rspotify::model::RepeatState::Context => {
                                rspotify::model::RepeatState::Off
                            }
                        };
                        queue.set_repeat(next);
                        drop(player);
                        self.refresh_and_persist_session(
                            state,
                            config::ActiveProvider::YouTubeMusic,
                        );
                        self.schedule_youtube_prefetch(state);
                    }
                    request::YouTubePlayerRequest::Shuffle => {
                        let mut player = state.player.write();
                        let Some(queue) = player.unified_queue.as_mut() else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        queue.toggle_shuffle();
                        drop(player);
                        self.refresh_and_persist_session(
                            state,
                            config::ActiveProvider::YouTubeMusic,
                        );
                        self.schedule_youtube_prefetch(state);
                    }
                    request::YouTubePlayerRequest::Resume => {
                        let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                        let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                        let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                        let changed = self
                            .playback
                            .resume_with_permit(
                                activation,
                                config::ActiveProvider::YouTubeMusic,
                                &spotify,
                                &youtube,
                                &sessions,
                            )
                            .await?;
                        if !changed {
                            return Ok(RequestDisposition::NoOp);
                        }
                    }
                    request::YouTubePlayerRequest::Pause => {
                        let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
                        let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
                        let sessions = playback_coordinator::AppPlaybackSessions::new(state);
                        let changed = self
                            .playback
                            .pause(
                                config::ActiveProvider::YouTubeMusic,
                                &spotify,
                                &youtube,
                                &sessions,
                            )
                            .await?;
                        if !changed {
                            return Ok(RequestDisposition::NoOp);
                        }
                    }
                    request::YouTubePlayerRequest::Volume(volume) => {
                        let snapshot = self.youtube_player.lock().await.set_volume(volume);
                        let Some(snapshot) = snapshot else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        self.state_application
                            .apply_youtube_control_snapshot(&mut state.player.write(), &snapshot);
                        self.refresh_and_persist_session(
                            state,
                            config::ActiveProvider::YouTubeMusic,
                        );
                    }
                    request::YouTubePlayerRequest::ToggleMute => {
                        let snapshot = self.youtube_player.lock().await.toggle_mute();
                        let Some(snapshot) = snapshot else {
                            return Ok(RequestDisposition::NoOp);
                        };
                        self.state_application
                            .apply_youtube_control_snapshot(&mut state.player.write(), &snapshot);
                        self.refresh_and_persist_session(
                            state,
                            config::ActiveProvider::YouTubeMusic,
                        );
                    }
                }
                Ok(RequestDisposition::Applied)
            }
            _ => unreachable!("request routed to the wrong playback handler"),
        }
    }
}
