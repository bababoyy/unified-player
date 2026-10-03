use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use anyhow::{Context as _, Result};
use rspotify::prelude::*;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{self, ActiveProvider},
    state::{PlayerState, Provider, ProviderPlaybackSession, SharedState},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeactivationReason {
    Pause,
    Switch,
    ReplacePlayback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResumeReason {
    Switch,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivePlaybackControlOutcome {
    Applied(ActiveProvider),
    AlreadySatisfied(ActiveProvider),
    Rejected(super::request::PlaybackControlRejection),
    Superseded,
}

#[derive(Clone, Debug)]
pub(super) struct ActivePlaybackSeekPermit {
    provider: ActiveProvider,
    ticket: WorkTicket,
}

impl ActivePlaybackSeekPermit {
    pub(super) const fn provider(&self) -> ActiveProvider {
        self.provider
    }

    pub(super) fn cancellation(&self) -> CancellationToken {
        self.ticket.cancellation.clone()
    }
}

fn preserve_switch_diagnostic(
    error: anyhow::Error,
    code: crate::observability::DiagnosticCode,
) -> anyhow::Error {
    if crate::observability::preserved_error_diagnostic(&error).is_some() {
        error
    } else {
        crate::observability::preserve_error_diagnostic(
            error,
            code,
            crate::observability::ErrorCategory::Unavailable,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackState {
    Idle,
    Active(ActiveProvider),
    Transitioning {
        active: Option<ActiveProvider>,
        target: ActiveProvider,
        generation: u64,
    },
    ShuttingDown,
    Stopped,
}

impl PlaybackState {
    fn active_provider(self) -> Option<ActiveProvider> {
        match self {
            Self::Active(provider) => Some(provider),
            Self::Transitioning { active, .. } => active,
            Self::Idle | Self::ShuttingDown | Self::Stopped => None,
        }
    }
}

#[derive(Debug)]
struct StateMachine {
    state: PlaybackState,
}

impl StateMachine {
    fn new(provider: ActiveProvider) -> Self {
        Self {
            state: PlaybackState::Active(provider),
        }
    }

    fn begin_transition(&mut self, target: ActiveProvider, generation: u64) {
        let previous = self.state.active_provider();
        self.state = PlaybackState::Transitioning {
            active: previous.filter(|active| *active == target),
            target,
            generation,
        };
    }

    fn finish_transition(&mut self, target: ActiveProvider, generation: u64) -> bool {
        if matches!(
            self.state,
            PlaybackState::Transitioning {
                target: current_target,
                generation: current_generation,
                ..
            } if current_target == target && current_generation == generation
        ) {
            self.state = PlaybackState::Active(target);
            true
        } else {
            false
        }
    }

    // Outer `None`: the ticket is stale; inner value: the owner that was restored.
    #[allow(clippy::option_option)]
    fn fail_transition(
        &mut self,
        target: ActiveProvider,
        generation: u64,
    ) -> Option<Option<ActiveProvider>> {
        if let PlaybackState::Transitioning {
            active,
            target: current_target,
            generation: current_generation,
        } = self.state
        {
            if current_target == target && current_generation == generation {
                self.restore(active);
                return Some(active);
            }
        }
        None
    }

    fn restore(&mut self, provider: Option<ActiveProvider>) {
        self.state = provider.map_or(PlaybackState::Idle, PlaybackState::Active);
    }
}

#[derive(Debug)]
struct WorkSlot {
    generation: AtomicU64,
    cancellation: Mutex<CancellationToken>,
}

impl Default for WorkSlot {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            cancellation: Mutex::new(CancellationToken::new()),
        }
    }
}

impl WorkSlot {
    fn begin(&self) -> WorkTicket {
        let mut current = self.cancellation.lock().expect("work slot mutex poisoned");
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        current.cancel();
        let cancellation = CancellationToken::new();
        *current = cancellation.clone();
        WorkTicket {
            generation,
            cancellation,
        }
    }

    fn cancel(&self) {
        let current = self.cancellation.lock().expect("work slot mutex poisoned");
        self.generation.fetch_add(1, Ordering::AcqRel);
        current.cancel();
    }

    fn is_current(&self, ticket: &WorkTicket) -> bool {
        !ticket.cancellation.is_cancelled()
            && self.generation.load(Ordering::Acquire) == ticket.generation
    }

    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn with_current<R>(&self, ticket: &WorkTicket, action: impl FnOnce() -> R) -> Option<R> {
        let _current = self.cancellation.lock().expect("work slot mutex poisoned");
        self.is_current(ticket).then(action)
    }
}

#[derive(Clone, Debug)]
struct WorkTicket {
    generation: u64,
    cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub(crate) struct ActivationPermit(WorkTicket);

impl ActivationPermit {
    pub(super) fn cancellation(&self) -> CancellationToken {
        self.0.cancellation.clone()
    }
}

#[async_trait::async_trait]
pub(super) trait PlaybackEngine: Sync {
    fn provider(&self) -> ActiveProvider;

    fn has_session(&self) -> bool;

    fn is_playing(&self) -> bool;

    async fn deactivate(&self, reason: DeactivationReason) -> Result<()>;

    async fn resume(&self, reason: ResumeReason, cancellation: CancellationToken) -> Result<()>;

    async fn shutdown(&self) -> Result<()>;
}

pub(super) trait PlaybackSessionStore: Sync {
    fn remember(&self, provider: ActiveProvider, resume_on_activate: bool);

    fn refresh(&self, provider: ActiveProvider);

    fn persist(&self);

    fn publish_owner(&self, _provider: Option<ActiveProvider>) {}
}

#[derive(Debug)]
struct CoordinatorInner {
    operations: tokio::sync::Mutex<()>,
    machine: Mutex<StateMachine>,
    session_persistence: Mutex<()>,
    activation: WorkSlot,
    active_seek: WorkSlot,
    youtube_prefetch: WorkSlot,
    spotify_updates: WorkSlot,
}

#[derive(Clone, Debug)]
pub(super) struct PlaybackCoordinator {
    inner: Arc<CoordinatorInner>,
}

impl PlaybackCoordinator {
    pub(super) fn new(provider: ActiveProvider) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                operations: tokio::sync::Mutex::new(()),
                machine: Mutex::new(StateMachine::new(provider)),
                session_persistence: Mutex::new(()),
                activation: WorkSlot::default(),
                active_seek: WorkSlot::default(),
                youtube_prefetch: WorkSlot::default(),
                spotify_updates: WorkSlot::default(),
            }),
        }
    }

    fn engine<'a>(
        provider: ActiveProvider,
        spotify: &'a dyn PlaybackEngine,
        youtube: &'a dyn PlaybackEngine,
    ) -> &'a dyn PlaybackEngine {
        debug_assert_eq!(spotify.provider(), ActiveProvider::Spotify);
        debug_assert_eq!(youtube.provider(), ActiveProvider::YouTubeMusic);
        match provider {
            ActiveProvider::Spotify => spotify,
            ActiveProvider::YouTubeMusic => youtube,
        }
    }

    fn begin_activation(&self) -> WorkTicket {
        self.inner.active_seek.cancel();
        self.inner.activation.begin()
    }

    pub(super) fn reserve_activation(&self) -> ActivationPermit {
        ActivationPermit(self.begin_activation())
    }

    pub(super) fn activation_permit_is_current(&self, permit: &ActivationPermit) -> bool {
        self.activation_is_current(&permit.0)
    }

    pub(super) fn activation_generation(&self) -> u64 {
        self.inner.activation.current_generation()
    }

    pub(super) fn with_current_activation<R>(
        &self,
        permit: &ActivationPermit,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        self.inner.activation.with_current(&permit.0, action)
    }

    fn activation_ticket(&self, permit: Option<&ActivationPermit>) -> WorkTicket {
        permit.map_or_else(|| self.begin_activation(), |permit| permit.0.clone())
    }

    fn is_stably_active(&self, provider: ActiveProvider) -> bool {
        self.inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state
            == PlaybackState::Active(provider)
    }

    pub(super) fn accepts_control(&self, provider: ActiveProvider) -> bool {
        self.is_stably_active(provider)
    }

    pub(super) fn stable_active_provider(&self) -> Option<ActiveProvider> {
        match self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state
        {
            PlaybackState::Active(provider) => Some(provider),
            PlaybackState::Idle
            | PlaybackState::Transitioning { .. }
            | PlaybackState::ShuttingDown
            | PlaybackState::Stopped => None,
        }
    }

    pub(super) async fn acquire_active_seek(
        &self,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
    ) -> std::result::Result<ActivePlaybackSeekPermit, super::request::PlaybackControlRejection>
    {
        use super::request::PlaybackControlRejection;

        let _operation = self.inner.operations.lock().await;
        let state = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state;
        let provider = match state {
            PlaybackState::Active(provider) => provider,
            PlaybackState::Idle => return Err(PlaybackControlRejection::NoActivePlayback),
            PlaybackState::Transitioning { .. } => {
                return Err(PlaybackControlRejection::TransitionInProgress);
            }
            PlaybackState::ShuttingDown | PlaybackState::Stopped => {
                return Err(PlaybackControlRejection::ShuttingDown);
            }
        };
        if !Self::engine(provider, spotify, youtube).has_session() {
            return Err(PlaybackControlRejection::NoActivePlayback);
        }
        tracing::debug!(
            provider = provider.title(),
            "Resolved the active playback seek owner"
        );
        Ok(ActivePlaybackSeekPermit {
            provider,
            ticket: self.inner.active_seek.begin(),
        })
    }

    pub(super) fn active_seek_is_current(&self, permit: &ActivePlaybackSeekPermit) -> bool {
        self.inner.active_seek.is_current(&permit.ticket) && self.is_stably_active(permit.provider)
    }

    pub(super) fn accepts_replacement(&self, provider: ActiveProvider) -> bool {
        match self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state
        {
            PlaybackState::Active(active) => active == provider,
            PlaybackState::Transitioning { target, .. } => target == provider,
            PlaybackState::Idle | PlaybackState::ShuttingDown | PlaybackState::Stopped => false,
        }
    }

    fn activation_is_current(&self, ticket: &WorkTicket) -> bool {
        self.inner.activation.is_current(ticket)
    }

    async fn prepare_activation(
        &self,
        ticket: &WorkTicket,
        target: ActiveProvider,
        reason: DeactivationReason,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        let _operation = self.inner.operations.lock().await;
        if !self.activation_is_current(ticket) {
            tracing::debug!(
                generation = ticket.generation,
                state = "stale_activation_discarded",
                "Discarding a superseded playback activation"
            );
            return Ok(false);
        }

        let state = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state;
        anyhow::ensure!(
            !matches!(state, PlaybackState::ShuttingDown | PlaybackState::Stopped),
            "playback coordinator is shutting down"
        );
        let source = state.active_provider();
        if reason == DeactivationReason::Switch && source == Some(target) {
            return Ok(false);
        }

        if let Some(source) = source.filter(|source| *source != target) {
            self.remember_and_persist(sessions, source, reason == DeactivationReason::Switch);
            if source == ActiveProvider::YouTubeMusic {
                self.cancel_youtube_prefetch();
            }
            if let Err(err) = Self::engine(source, spotify, youtube)
                .deactivate(reason)
                .await
            {
                self.inner
                    .machine
                    .lock()
                    .expect("playback state mutex poisoned")
                    .restore(Some(source));
                return Err(err).context("deactivate outgoing playback engine");
            }
            self.refresh_and_persist(sessions, source);
        }

        self.inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .begin_transition(target, ticket.generation);
        sessions.publish_owner(Some(target));
        Ok(true)
    }

    async fn finish_activation(
        &self,
        ticket: &WorkTicket,
        target: ActiveProvider,
        target_engine: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        let _operation = self.inner.operations.lock().await;
        if !self.activation_is_current(ticket) {
            let current = self
                .inner
                .machine
                .lock()
                .expect("playback state mutex poisoned")
                .state;
            let current_claims_target = match current {
                PlaybackState::Active(provider) => provider == target,
                PlaybackState::Transitioning {
                    target: current_target,
                    generation: current_generation,
                    ..
                } => current_target == target && current_generation != ticket.generation,
                PlaybackState::Idle | PlaybackState::ShuttingDown | PlaybackState::Stopped => false,
            };
            if !current_claims_target {
                target_engine
                    .deactivate(DeactivationReason::ReplacePlayback)
                    .await
                    .context("stop a stale playback activation")?;
            }
            return Ok(false);
        }
        let finished = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .finish_transition(target, ticket.generation);
        if finished {
            sessions.publish_owner(Some(target));
            self.refresh_and_persist(sessions, target);
        }
        Ok(finished)
    }

    fn fail_activation(
        &self,
        ticket: &WorkTicket,
        target: ActiveProvider,
        sessions: &dyn PlaybackSessionStore,
    ) {
        let restored = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .fail_transition(target, ticket.generation);
        if let Some(restored) = restored {
            sessions.publish_owner(restored);
        }
    }

    #[cfg(test)]
    pub(super) async fn activate_with<F, Fut>(
        &self,
        target: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
        start: F,
    ) -> Result<bool>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.activate_with_permit(None, target, spotify, youtube, sessions, start)
            .await
    }

    pub(super) async fn activate_with_permit<F, Fut>(
        &self,
        permit: Option<&ActivationPermit>,
        target: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
        start: F,
    ) -> Result<bool>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let ticket = self.activation_ticket(permit);
        if !self
            .prepare_activation(
                &ticket,
                target,
                DeactivationReason::ReplacePlayback,
                spotify,
                youtube,
                sessions,
            )
            .await?
        {
            return Ok(false);
        }

        let cancellation = ticket.cancellation.clone();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.fail_activation(&ticket, target, sessions);
                return Ok(false);
            },
            result = start(cancellation.clone()) => result,
        };
        if let Err(err) = result {
            self.fail_activation(&ticket, target, sessions);
            return Err(err);
        }

        self.finish_activation(
            &ticket,
            target,
            Self::engine(target, spotify, youtube),
            sessions,
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn switch_to(
        &self,
        target: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        self.switch_to_with_permit(None, target, spotify, youtube, sessions)
            .await
    }

    pub(super) async fn switch_to_with_permit(
        &self,
        permit: Option<&ActivationPermit>,
        target: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        if self.is_stably_active(target) {
            return Ok(false);
        }
        let ticket = self.activation_ticket(permit);
        if !self
            .prepare_activation(
                &ticket,
                target,
                DeactivationReason::Switch,
                spotify,
                youtube,
                sessions,
            )
            .await
            .map_err(|error| {
                preserve_switch_diagnostic(
                    error,
                    crate::observability::DiagnosticCode::PROVIDER_SWITCH_PREPARE_FAILED,
                )
                .context("prepare provider switch")
            })?
        {
            return Ok(false);
        }

        let target_engine = Self::engine(target, spotify, youtube);
        let cancellation = ticket.cancellation.clone();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.fail_activation(&ticket, target, sessions);
                return Ok(false);
            },
            result = target_engine.resume(ResumeReason::Switch, cancellation.clone()) => result,
        };
        if let Err(err) = result {
            self.fail_activation(&ticket, target, sessions);
            return Err(preserve_switch_diagnostic(
                err,
                crate::observability::DiagnosticCode::PROVIDER_SWITCH_RESUME_FAILED,
            )
            .context("resume target provider during switch"));
        }

        self.finish_activation(&ticket, target, target_engine, sessions)
            .await
    }

    pub(super) async fn pause(
        &self,
        provider: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        let _operation = self.inner.operations.lock().await;
        let active = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state
            .active_provider();
        if active != Some(provider) {
            return Ok(false);
        }
        let engine = Self::engine(provider, spotify, youtube);
        if !engine.is_playing() {
            return Ok(false);
        }
        engine.deactivate(DeactivationReason::Pause).await?;
        sessions.publish_owner(Some(provider));
        self.remember_and_persist(sessions, provider, false);
        Ok(true)
    }

    /// Stop local playback before replacing provider credentials. Account
    /// changes deliberately keep the media snapshot but disable automatic
    /// resume so one account cannot continue playing under another account.
    pub(super) async fn stop_for_account_change(
        &self,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<()> {
        self.cancel_pending_work();
        let _operation = self.inner.operations.lock().await;
        let state = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state;
        anyhow::ensure!(
            !matches!(state, PlaybackState::ShuttingDown | PlaybackState::Stopped),
            "playback coordinator is shutting down"
        );
        let active = state.active_provider();
        if let Some(active) = active {
            self.remember_and_persist(sessions, active, false);
            if active == ActiveProvider::YouTubeMusic {
                self.cancel_youtube_prefetch();
            }
            let engine = Self::engine(active, spotify, youtube);
            if active == ActiveProvider::YouTubeMusic || engine.is_playing() {
                engine
                    .deactivate(DeactivationReason::ReplacePlayback)
                    .await
                    .context("stop playback before changing account")?;
            }
            self.refresh_and_persist(sessions, active);
        }
        self.inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .restore(None);
        sessions.publish_owner(None);
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn resume(
        &self,
        provider: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        self.resume_with_permit(None, provider, spotify, youtube, sessions)
            .await
    }

    pub(super) async fn resume_with_permit(
        &self,
        permit: Option<&ActivationPermit>,
        provider: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        if !self.is_stably_active(provider) {
            return Ok(false);
        }
        if Self::engine(provider, spotify, youtube).is_playing() {
            return Ok(false);
        }
        let ticket = self.activation_ticket(permit);
        if !self
            .prepare_activation(
                &ticket,
                provider,
                DeactivationReason::Pause,
                spotify,
                youtube,
                sessions,
            )
            .await?
        {
            return Ok(false);
        }
        let engine = Self::engine(provider, spotify, youtube);
        let cancellation = ticket.cancellation.clone();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.fail_activation(&ticket, provider, sessions);
                return Ok(false);
            },
            result = engine.resume(ResumeReason::User, cancellation.clone()) => result,
        };
        if let Err(err) = result {
            self.fail_activation(&ticket, provider, sessions);
            return Err(err);
        }
        self.finish_activation(&ticket, provider, engine, sessions)
            .await
    }

    pub(super) async fn toggle_pause_with_permit(
        &self,
        permit: Option<&ActivationPermit>,
        provider: ActiveProvider,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<bool> {
        if Self::engine(provider, spotify, youtube).is_playing() {
            self.pause(provider, spotify, youtube, sessions).await
        } else {
            self.resume_with_permit(permit, provider, spotify, youtube, sessions)
                .await
        }
    }

    pub(super) async fn control_active_playback(
        &self,
        control: super::request::ActivePlaybackControl,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<ActivePlaybackControlOutcome> {
        use super::request::{ActivePlaybackControl, PlaybackControlRejection};

        let _operation = self.inner.operations.lock().await;
        let state = self
            .inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state;
        let provider = match state {
            PlaybackState::Active(provider) => provider,
            PlaybackState::Idle => {
                return Ok(ActivePlaybackControlOutcome::Rejected(
                    PlaybackControlRejection::NoActivePlayback,
                ));
            }
            PlaybackState::Transitioning { .. } => {
                return Ok(ActivePlaybackControlOutcome::Rejected(
                    PlaybackControlRejection::TransitionInProgress,
                ));
            }
            PlaybackState::ShuttingDown | PlaybackState::Stopped => {
                return Ok(ActivePlaybackControlOutcome::Rejected(
                    PlaybackControlRejection::ShuttingDown,
                ));
            }
        };
        let engine = Self::engine(provider, spotify, youtube);
        tracing::debug!(
            provider = provider.title(),
            control = ?control,
            "Resolved an active playback control"
        );
        if !engine.has_session() {
            return Ok(ActivePlaybackControlOutcome::Rejected(
                PlaybackControlRejection::NoActivePlayback,
            ));
        }
        sessions.publish_owner(Some(provider));

        let is_playing = engine.is_playing();
        let should_play = match (control, is_playing) {
            (ActivePlaybackControl::Play, true) | (ActivePlaybackControl::Pause, false) => {
                return Ok(ActivePlaybackControlOutcome::AlreadySatisfied(provider));
            }
            (ActivePlaybackControl::Play, false) => true,
            (ActivePlaybackControl::Pause, true) => false,
            (ActivePlaybackControl::Toggle, is_playing) => !is_playing,
        };

        if !should_play {
            engine.deactivate(DeactivationReason::Pause).await?;
            self.remember_and_persist(sessions, provider, false);
            return Ok(ActivePlaybackControlOutcome::Applied(provider));
        }

        let ticket = self.begin_activation();
        let cancellation = ticket.cancellation.clone();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Ok(ActivePlaybackControlOutcome::Superseded);
            }
            result = engine.resume(ResumeReason::User, cancellation.clone()) => result,
        };
        result?;
        if !self.activation_is_current(&ticket) {
            return Ok(ActivePlaybackControlOutcome::Superseded);
        }
        self.refresh_and_persist(sessions, provider);
        Ok(ActivePlaybackControlOutcome::Applied(provider))
    }

    pub(super) async fn shutdown(
        &self,
        spotify: &dyn PlaybackEngine,
        youtube: &dyn PlaybackEngine,
        sessions: &dyn PlaybackSessionStore,
    ) -> Result<()> {
        self.cancel_pending_work();

        let _operation = self.inner.operations.lock().await;
        let active = {
            let mut machine = self
                .inner
                .machine
                .lock()
                .expect("playback state mutex poisoned");
            let active = machine.state.active_provider();
            machine.state = PlaybackState::ShuttingDown;
            active
        };
        if let Some(active) = active {
            self.remember_and_persist(sessions, active, false);
        }

        let spotify_result = spotify.shutdown().await;
        let youtube_result = youtube.shutdown().await;
        {
            let _persistence = self
                .inner
                .session_persistence
                .lock()
                .expect("session persistence mutex poisoned");
            sessions.refresh(ActiveProvider::Spotify);
            sessions.refresh(ActiveProvider::YouTubeMusic);
            sessions.persist();
        }
        self.inner
            .machine
            .lock()
            .expect("playback state mutex poisoned")
            .state = PlaybackState::Stopped;
        sessions.publish_owner(None);

        spotify_result.context("shut down Spotify playback engine")?;
        youtube_result.context("shut down YouTube playback engine")?;
        Ok(())
    }

    pub(super) fn cancel_pending_work(&self) {
        self.inner.activation.cancel();
        self.inner.active_seek.cancel();
        self.inner.youtube_prefetch.cancel();
        self.inner.spotify_updates.cancel();
    }

    pub(super) fn begin_youtube_prefetch(&self) -> CancellationToken {
        self.inner.youtube_prefetch.begin().cancellation
    }

    pub(super) fn cancel_youtube_prefetch(&self) {
        self.inner.youtube_prefetch.cancel();
    }

    pub(super) fn begin_spotify_update(&self) -> u64 {
        self.inner.spotify_updates.begin().generation
    }

    #[cfg(feature = "streaming")]
    pub(super) fn cancel_spotify_update(&self) {
        self.inner.spotify_updates.cancel();
    }

    pub(super) fn spotify_update_is_current(&self, generation: u64) -> bool {
        let is_current = self
            .inner
            .spotify_updates
            .generation
            .load(Ordering::Acquire)
            == generation;
        if !is_current {
            tracing::debug!(
                generation,
                state = "stale_spotify_refresh_discarded",
                "Discarding a superseded Spotify refresh"
            );
        }
        is_current
    }

    pub(super) fn refresh_and_persist(
        &self,
        sessions: &dyn PlaybackSessionStore,
        provider: ActiveProvider,
    ) {
        let _persistence = self
            .inner
            .session_persistence
            .lock()
            .expect("session persistence mutex poisoned");
        sessions.refresh(provider);
        sessions.persist();
    }

    fn remember_and_persist(
        &self,
        sessions: &dyn PlaybackSessionStore,
        provider: ActiveProvider,
        resume_on_activate: bool,
    ) {
        let _persistence = self
            .inner
            .session_persistence
            .lock()
            .expect("session persistence mutex poisoned");
        sessions.remember(provider, resume_on_activate);
        sessions.persist();
    }

    pub(super) fn refresh_session(
        &self,
        sessions: &dyn PlaybackSessionStore,
        provider: ActiveProvider,
    ) {
        let _persistence = self
            .inner
            .session_persistence
            .lock()
            .expect("session persistence mutex poisoned");
        sessions.refresh(provider);
    }

    pub(super) fn persist_sessions(&self, sessions: &dyn PlaybackSessionStore) {
        let _persistence = self
            .inner
            .session_persistence
            .lock()
            .expect("session persistence mutex poisoned");
        sessions.persist();
    }
}

pub(super) struct AppPlaybackSessions<'a> {
    state: &'a SharedState,
}

impl<'a> AppPlaybackSessions<'a> {
    pub(super) fn new(state: &'a SharedState) -> Self {
        Self { state }
    }
}

impl PlaybackSessionStore for AppPlaybackSessions<'_> {
    fn remember(&self, provider: ActiveProvider, resume_on_activate: bool) {
        let mut player = self.state.player.write();
        match provider {
            ActiveProvider::Spotify => player.remember_spotify_activation_state(),
            ActiveProvider::YouTubeMusic => {
                player.remember_youtube_activation_state();
            }
        }
        if !resume_on_activate {
            let provider = match provider {
                ActiveProvider::Spotify => Provider::Spotify,
                ActiveProvider::YouTubeMusic => Provider::YouTubeMusic,
            };
            if let Some(session) = player.provider_sessions.get_mut(&provider) {
                session.resume_on_activate = false;
            }
        }
    }

    fn refresh(&self, provider: ActiveProvider) {
        let mut player = self.state.player.write();
        match provider {
            ActiveProvider::Spotify => player.refresh_spotify_session(),
            ActiveProvider::YouTubeMusic => player.refresh_youtube_session(),
        }
    }

    fn persist(&self) {
        self.state.persist_provider_sessions();
    }

    fn publish_owner(&self, provider: Option<ActiveProvider>) {
        let account_label = provider.and_then(|provider| {
            let configs = config::get_config();
            config::AccountRegistry::load(&configs.config_folder)
                .ok()
                .and_then(|registry| registry.active_label(provider).map(str::to_owned))
        });
        let mut player = self.state.player.write();
        player.active_playback_provider = provider;
        player.active_playback_account_provider = provider;
        player.active_playback_account_label = account_label;
    }
}

pub(super) struct SpotifyEngineAdapter<'a> {
    client: &'a super::AppClient,
    state: &'a SharedState,
}

fn publish_spotify_playing_state(player: &mut PlayerState, is_playing: bool) {
    if let Some(playback) = &mut player.playback {
        playback.is_playing = is_playing;
    }
    if let Some(playback) = &mut player.buffered_playback {
        playback.is_playing = is_playing;
    }
    if let Some(session) = player.provider_sessions.get_mut(&Provider::Spotify) {
        session.is_playing = is_playing;
    }
}

fn spotify_control_targets_integrated_device(
    active_device_id: Option<&str>,
    integrated_device_id: Option<&str>,
) -> bool {
    active_device_id.is_some() && active_device_id == integrated_device_id
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpotifySeekRoute {
    IntegratedSpirc(u32),
    WebApi,
}

fn spotify_seek_route(
    active_device_id: Option<&str>,
    integrated_device_id: Option<&str>,
    position: chrono::Duration,
) -> Result<SpotifySeekRoute> {
    if !spotify_control_targets_integrated_device(active_device_id, integrated_device_id) {
        return Ok(SpotifySeekRoute::WebApi);
    }

    let position_ms = u32::try_from(position.num_milliseconds())
        .context("convert integrated Spotify seek position")?;
    Ok(SpotifySeekRoute::IntegratedSpirc(position_ms))
}

impl<'a> SpotifyEngineAdapter<'a> {
    pub(super) fn new(client: &'a super::AppClient, state: &'a SharedState) -> Self {
        Self { client, state }
    }

    async fn playback_disappeared_after_command_failure(&self) -> bool {
        if let Err(error) = self
            .client
            .retrieve_current_playback(self.state, true)
            .await
        {
            // `/me/player` returns 404 when the remote device has already
            // disappeared. Treat that as an idempotent stopped state during
            // a switch; authentication and transport failures must still
            // keep failing the transition.
            if !super::provider_metadata::is_spotify_no_active_device(&error) {
                return false;
            }
            let mut player = self.state.player.write();
            player.playback = None;
            player.buffered_playback = None;
            player.active_playback_account_provider = None;
            player.active_playback_account_label = None;
        }
        self.state.player.read().buffered_playback.is_none()
    }

    fn active_device_id(&self) -> Option<String> {
        let player = self.state.player.read();
        player
            .buffered_playback
            .as_ref()
            .and_then(|playback| playback.device_id.clone())
            .or_else(|| {
                player
                    .playback
                    .as_ref()
                    .and_then(|playback| playback.device.id.clone())
            })
    }

    pub(super) async fn targets_integrated_device(&self) -> bool {
        let active_device_id = self.active_device_id();
        let integrated_device_id = self.client.connected_integrated_spotify_device_id().await;
        spotify_control_targets_integrated_device(
            active_device_id.as_deref(),
            integrated_device_id.as_deref(),
        )
    }

    #[cfg(feature = "streaming")]
    async fn control_integrated_device(&self, should_play: bool) -> Option<Result<()>> {
        if !self.targets_integrated_device().await {
            return None;
        }

        tracing::debug!(
            route = "integrated_spirc",
            should_play,
            "Routing Spotify playback control to the integrated device"
        );
        let connection = self.client.stream_conn.lock();
        let connection = connection.as_ref()?;
        let result = if should_play {
            connection.play()
        } else {
            connection.pause()
        };
        Some(result.map_err(anyhow::Error::from))
    }

    // Stub keeps the async signature of the streaming implementation for shared callers.
    #[cfg(not(feature = "streaming"))]
    #[allow(clippy::unused_async)]
    async fn control_integrated_device(&self, _should_play: bool) -> Option<Result<()>> {
        None
    }

    #[cfg(feature = "streaming")]
    fn seek_integrated_device(&self, position_ms: u32) -> Option<Result<()>> {
        let connection = self.client.stream_conn.lock();
        let connection = connection.as_ref()?;
        Some(
            connection
                .set_position_ms(position_ms)
                .map_err(anyhow::Error::from),
        )
    }

    #[cfg(not(feature = "streaming"))]
    fn seek_integrated_device(&self, _position_ms: u32) -> Option<Result<()>> {
        None
    }

    pub(super) async fn seek(&self, position: chrono::Duration) -> Result<()> {
        let active_device_id = self.active_device_id();
        let integrated_device_id = self.client.connected_integrated_spotify_device_id().await;
        match spotify_seek_route(
            active_device_id.as_deref(),
            integrated_device_id.as_deref(),
            position,
        )? {
            SpotifySeekRoute::IntegratedSpirc(position_ms) => {
                if let Some(result) = self.seek_integrated_device(position_ms) {
                    crate::observability::operation_stage_detail(
                        crate::observability::Component::Spotify,
                        "spotify_seek_route",
                        None,
                        Some(crate::observability::OperationOutcome::Success),
                        None,
                        Some("integrated_spirc"),
                        None,
                        None,
                    );
                    tracing::debug!(
                        route = "integrated_spirc",
                        "Routing Spotify seek to the integrated device"
                    );
                    result.context("seek integrated Spotify playback")?;
                    return Ok(());
                }
            }
            SpotifySeekRoute::WebApi => {}
        }

        crate::observability::operation_stage_detail(
            crate::observability::Component::Spotify,
            "spotify_seek_route",
            None,
            Some(crate::observability::OperationOutcome::Success),
            None,
            Some("web_api"),
            None,
            None,
        );
        tracing::debug!(route = "web_api", "Routing Spotify seek to the Web API");
        self.client
            .spotify_api()
            .seek_track(position, active_device_id.as_deref())
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PlaybackEngine for SpotifyEngineAdapter<'_> {
    fn provider(&self) -> ActiveProvider {
        ActiveProvider::Spotify
    }

    fn has_session(&self) -> bool {
        let player = self.state.player.read();
        player.buffered_playback.is_some()
            || player
                .provider_sessions
                .get(&Provider::Spotify)
                .is_some_and(|session| session.media_id.is_some())
    }

    fn is_playing(&self) -> bool {
        self.state
            .player
            .read()
            .buffered_playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing)
    }

    async fn deactivate(&self, _reason: DeactivationReason) -> Result<()> {
        if let Some(result) = self.control_integrated_device(false).await {
            result.context("pause integrated Spotify playback")?;
            publish_spotify_playing_state(&mut self.state.player.write(), false);
            return Ok(());
        }
        if let Err(error) = self.client.spotify_api().pause_playback(None).await {
            // Spotify can report a stale remote device as unavailable after
            // playback was stopped elsewhere. Reconcile before failing the
            // transition so a harmless already-stopped state is idempotent.
            if !self.playback_disappeared_after_command_failure().await {
                return Err(error).context("pause Spotify playback");
            }
        }
        publish_spotify_playing_state(&mut self.state.player.write(), false);
        Ok(())
    }

    async fn resume(&self, reason: ResumeReason, cancellation: CancellationToken) -> Result<()> {
        let saved = self
            .state
            .player
            .read()
            .provider_sessions
            .get(&Provider::Spotify)
            .cloned();
        let should_resume = match reason {
            ResumeReason::Switch => {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Ok(()),
                    result = self.client.retrieve_current_playback(self.state, true) => result?,
                }
                let current = self
                    .state
                    .player
                    .read()
                    .provider_sessions
                    .get(&Provider::Spotify)
                    .cloned();
                spotify_should_resume(saved.as_ref(), current.as_ref())
            }
            // The coordinator already established that the locally published
            // state is paused. Do not let Spotify's eventually consistent read
            // endpoint veto an explicit user command.
            ResumeReason::User => true,
        };
        if should_resume {
            if let Some(result) = self.control_integrated_device(true).await {
                result.context("resume integrated Spotify playback")?;
                publish_spotify_playing_state(&mut self.state.player.write(), true);
                return Ok(());
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                result = async { self.client.spotify_api().resume_playback(None, None).await } => {
                    if let Err(error) = result {
                        if !self.playback_disappeared_after_command_failure().await {
                            return Err(error).context("resume Spotify playback");
                        }
                        publish_spotify_playing_state(&mut self.state.player.write(), false);
                        return Ok(());
                    }
                },
            }
            // Spotify's read API is eventually consistent. Publish the successful
            // command immediately, then let the existing delayed refresh converge.
            publish_spotify_playing_state(&mut self.state.player.write(), true);
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        // Preserve Spotify Connect behavior: quitting the TUI does not stop a
        // remote device. Only this process's integrated connection is owned by
        // the runtime and stopped here.
        #[cfg(feature = "streaming")]
        self.client.shutdown_streaming_connection()?;
        Ok(())
    }
}

pub(super) struct YouTubeEngineAdapter<'a> {
    client: &'a super::AppClient,
    state: &'a SharedState,
}

impl<'a> YouTubeEngineAdapter<'a> {
    pub(super) fn new(client: &'a super::AppClient, state: &'a SharedState) -> Self {
        Self { client, state }
    }

    fn publish(&self, snapshot: &super::youtube::YouTubePlaybackControlSnapshot) {
        self.client
            .state_application
            .apply_youtube_control_snapshot(&mut self.state.player.write(), snapshot);
    }
}

#[async_trait::async_trait]
impl PlaybackEngine for YouTubeEngineAdapter<'_> {
    fn provider(&self) -> ActiveProvider {
        ActiveProvider::YouTubeMusic
    }

    fn has_session(&self) -> bool {
        self.state.player.read().youtube_playback.is_some()
    }

    fn is_playing(&self) -> bool {
        self.state
            .player
            .read()
            .youtube_playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing)
    }

    async fn deactivate(&self, reason: DeactivationReason) -> Result<()> {
        if reason != DeactivationReason::Pause {
            super::youtube::browser_auth::shutdown_playback_browser().await;
        }
        let mut player = self.client.youtube_player.lock().await;
        match reason {
            DeactivationReason::Pause => {
                if let Some(snapshot) = player.pause() {
                    self.publish(&snapshot);
                }
                if let Some(sink) = player.active_sink() {
                    spawn_youtube_output_release(
                        self.client.youtube_player.clone(),
                        YOUTUBE_PAUSED_OUTPUT_RELEASE,
                        move |player| player.suspend_if_idle(&sink),
                    );
                }
            }
            DeactivationReason::Switch => {
                // Another provider is taking over the speakers; keeping the
                // YouTube output open would stream silence indefinitely.
                if let Some(snapshot) = player.suspend()? {
                    self.publish(&snapshot);
                }
            }
            DeactivationReason::ReplacePlayback => {
                player.stop();
                // A replacement YouTube track reuses the output within the
                // delay; anything else leaves it unused.
                spawn_youtube_output_release(
                    self.client.youtube_player.clone(),
                    YOUTUBE_UNUSED_OUTPUT_RELEASE,
                    super::youtube::YouTubeLocalPlayer::release_output_if_unused,
                );
                let mut state = self.state.player.write();
                if let Some(playback) = &mut state.youtube_playback {
                    playback.is_playing = false;
                }
            }
        }
        Ok(())
    }

    async fn resume(&self, reason: ResumeReason, cancellation: CancellationToken) -> Result<()> {
        if reason == ResumeReason::Switch
            && !self.state.player.read().youtube_should_resume_on_activate()
        {
            return Ok(());
        }
        let restart = {
            let mut player = self.client.youtube_player.lock().await;
            if let Some(snapshot) = player.resume() {
                self.publish(&snapshot);
                drop(player);
                self.client.refresh_youtube_queue_completion(self.state);
                return Ok(());
            }
            player.restart_info()
        };
        let restart = restart
            .map(|restart| {
                let source = (!restart.source.is_expired()).then_some(restart.source);
                (
                    restart.track,
                    if restart.ended {
                        std::time::Duration::ZERO
                    } else {
                        restart.progress
                    },
                    source,
                )
            })
            .or_else(|| {
                let player = self.state.player.read();
                player.youtube_playback.as_ref().map(|playback| {
                    let position = if player.youtube_playback_phase
                        == crate::state::YouTubePlaybackPhase::Idle
                    {
                        std::time::Duration::ZERO
                    } else {
                        playback.progress
                    };
                    (playback.track.clone(), position, None)
                })
            });
        if let Some((track, progress, source)) = restart {
            self.client
                .start_youtube_track_at_with_source(
                    self.state,
                    track,
                    progress,
                    true,
                    super::playback_actions::YouTubeResolutionPurpose::Resume,
                    cancellation,
                    source,
                )
                .await?;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        self.client.youtube_player.lock().await.shutdown()?;
        if let Some(playback) = &mut self.state.player.write().youtube_playback {
            playback.is_playing = false;
        }
        Ok(())
    }
}

/// How long `YouTube` playback may stay paused before its audio output is
/// closed. Resuming after the release reopens the output from the saved
/// position, which costs a short rebuffer, so brief pauses keep it open.
const YOUTUBE_PAUSED_OUTPUT_RELEASE: std::time::Duration = std::time::Duration::from_mins(1);
/// How long a stopped `YouTube` output waits for a replacement track.
const YOUTUBE_UNUSED_OUTPUT_RELEASE: std::time::Duration = std::time::Duration::from_secs(5);

/// An open output streams silence and keeps the audio device awake, so idle
/// outputs are closed after `delay` when `release` still finds them idle.
fn spawn_youtube_output_release(
    player: Arc<tokio::sync::Mutex<super::youtube::YouTubeLocalPlayer>>,
    delay: std::time::Duration,
    release: impl FnOnce(&mut super::youtube::YouTubeLocalPlayer) -> Result<bool> + Send + 'static,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if release(&mut *player.lock().await).is_err() {
            tracing::warn!("YouTube audio output worker terminated unexpectedly");
        }
    });
}

fn spotify_should_resume(
    saved: Option<&ProviderPlaybackSession>,
    current: Option<&ProviderPlaybackSession>,
) -> bool {
    saved.is_some_and(|saved| {
        saved.resume_on_activate
            && saved.media_id.is_some()
            && current
                .is_some_and(|current| current.media_id == saved.media_id && !current.is_playing)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, Mutex};

    use super::*;

    #[test]
    fn integrated_spotify_control_requires_the_active_connected_device() {
        assert!(spotify_control_targets_integrated_device(
            Some("integrated"),
            Some("integrated")
        ));
        assert!(!spotify_control_targets_integrated_device(
            Some("phone"),
            Some("integrated")
        ));
        assert!(!spotify_control_targets_integrated_device(
            Some("integrated"),
            None
        ));
        assert!(!spotify_control_targets_integrated_device(
            None,
            Some("integrated")
        ));
    }

    #[test]
    fn spotify_seek_uses_spirc_only_for_the_active_integrated_device() {
        assert_eq!(
            spotify_seek_route(
                Some("integrated"),
                Some("integrated"),
                chrono::Duration::milliseconds(12_345),
            )
            .unwrap(),
            SpotifySeekRoute::IntegratedSpirc(12_345)
        );
        assert_eq!(
            spotify_seek_route(
                Some("phone"),
                Some("integrated"),
                chrono::Duration::milliseconds(12_345),
            )
            .unwrap(),
            SpotifySeekRoute::WebApi
        );
        assert_eq!(
            spotify_seek_route(
                None,
                Some("integrated"),
                chrono::Duration::milliseconds(12_345),
            )
            .unwrap(),
            SpotifySeekRoute::WebApi
        );
    }

    #[test]
    fn integrated_spotify_seek_rejects_an_invalid_position() {
        assert!(spotify_seek_route(
            Some("integrated"),
            Some("integrated"),
            chrono::Duration::milliseconds(-1),
        )
        .is_err());
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Effect {
        Remember(ActiveProvider, bool),
        Persist,
        Refresh(ActiveProvider),
        Deactivate(ActiveProvider, DeactivationReason),
        Resume(ActiveProvider),
        Shutdown(ActiveProvider),
    }

    struct FakeSessions {
        effects: Mutex<Vec<Effect>>,
        trace: Option<Arc<Mutex<Vec<Effect>>>>,
        owner: Mutex<Option<ActiveProvider>>,
    }

    impl FakeSessions {
        fn new() -> Self {
            Self {
                effects: Mutex::new(Vec::new()),
                trace: None,
                owner: Mutex::new(None),
            }
        }

        fn with_trace(trace: Arc<Mutex<Vec<Effect>>>) -> Self {
            Self {
                effects: Mutex::new(Vec::new()),
                trace: Some(trace),
                owner: Mutex::new(None),
            }
        }

        fn record(&self, effect: Effect) {
            self.effects.lock().unwrap().push(effect);
            if let Some(trace) = &self.trace {
                trace.lock().unwrap().push(effect);
            }
        }

        fn effects(&self) -> Vec<Effect> {
            self.effects.lock().unwrap().clone()
        }

        fn owner(&self) -> Option<ActiveProvider> {
            *self.owner.lock().unwrap()
        }
    }

    impl PlaybackSessionStore for FakeSessions {
        fn remember(&self, provider: ActiveProvider, resume: bool) {
            self.record(Effect::Remember(provider, resume));
        }

        fn refresh(&self, provider: ActiveProvider) {
            self.record(Effect::Refresh(provider));
        }

        fn persist(&self) {
            self.record(Effect::Persist);
        }

        fn publish_owner(&self, provider: Option<ActiveProvider>) {
            *self.owner.lock().unwrap() = provider;
        }
    }

    struct FakeEngine {
        provider: ActiveProvider,
        effects: Arc<Mutex<Vec<Effect>>>,
        playing: AtomicBool,
        has_session: AtomicBool,
        fail_deactivate: bool,
        fail_resume: bool,
    }

    impl FakeEngine {
        fn new(provider: ActiveProvider, effects: Arc<Mutex<Vec<Effect>>>) -> Self {
            Self {
                provider,
                effects,
                playing: AtomicBool::new(false),
                has_session: AtomicBool::new(true),
                fail_deactivate: false,
                fail_resume: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl PlaybackEngine for FakeEngine {
        fn provider(&self) -> ActiveProvider {
            self.provider
        }

        fn is_playing(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }

        fn has_session(&self) -> bool {
            self.has_session.load(Ordering::Acquire)
        }

        async fn deactivate(&self, reason: DeactivationReason) -> Result<()> {
            self.effects
                .lock()
                .unwrap()
                .push(Effect::Deactivate(self.provider, reason));
            anyhow::ensure!(!self.fail_deactivate, "injected deactivate failure");
            self.playing.store(false, Ordering::Release);
            Ok(())
        }

        async fn resume(
            &self,
            _reason: ResumeReason,
            _cancellation: CancellationToken,
        ) -> Result<()> {
            self.effects
                .lock()
                .unwrap()
                .push(Effect::Resume(self.provider));
            anyhow::ensure!(!self.fail_resume, "injected resume failure");
            self.playing.store(true, Ordering::Release);
            Ok(())
        }

        async fn shutdown(&self) -> Result<()> {
            self.effects
                .lock()
                .unwrap()
                .push(Effect::Shutdown(self.provider));
            Ok(())
        }
    }

    fn harness(
        active: ActiveProvider,
    ) -> (
        PlaybackCoordinator,
        FakeEngine,
        FakeEngine,
        FakeSessions,
        Arc<Mutex<Vec<Effect>>>,
    ) {
        let effects = Arc::new(Mutex::new(Vec::new()));
        (
            PlaybackCoordinator::new(active),
            FakeEngine::new(ActiveProvider::Spotify, effects.clone()),
            FakeEngine::new(ActiveProvider::YouTubeMusic, effects.clone()),
            FakeSessions::new(),
            effects,
        )
    }

    #[tokio::test]
    async fn switch_pauses_outgoing_before_resuming_incoming() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let coordinator = PlaybackCoordinator::new(ActiveProvider::Spotify);
        let spotify = FakeEngine::new(ActiveProvider::Spotify, trace.clone());
        let youtube = FakeEngine::new(ActiveProvider::YouTubeMusic, trace.clone());
        let sessions = FakeSessions::with_trace(trace.clone());

        assert!(coordinator
            .switch_to(ActiveProvider::YouTubeMusic, &spotify, &youtube, &sessions,)
            .await
            .unwrap());

        assert_eq!(
            trace.lock().unwrap().as_slice(),
            [
                Effect::Remember(ActiveProvider::Spotify, true),
                Effect::Persist,
                Effect::Deactivate(ActiveProvider::Spotify, DeactivationReason::Switch),
                Effect::Refresh(ActiveProvider::Spotify),
                Effect::Persist,
                Effect::Resume(ActiveProvider::YouTubeMusic),
                Effect::Refresh(ActiveProvider::YouTubeMusic),
                Effect::Persist,
            ]
        );
        assert_eq!(
            sessions.effects(),
            [
                Effect::Remember(ActiveProvider::Spotify, true),
                Effect::Persist,
                Effect::Refresh(ActiveProvider::Spotify),
                Effect::Persist,
                Effect::Refresh(ActiveProvider::YouTubeMusic),
                Effect::Persist,
            ]
        );
        assert_eq!(sessions.owner(), Some(ActiveProvider::YouTubeMusic));
    }

    #[tokio::test]
    async fn outgoing_failure_never_resumes_incoming_engine() {
        let (coordinator, mut spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        spotify.fail_deactivate = true;

        assert!(coordinator
            .switch_to(ActiveProvider::YouTubeMusic, &spotify, &youtube, &sessions,)
            .await
            .is_err());
        assert_eq!(
            engine_effects.lock().unwrap().as_slice(),
            [Effect::Deactivate(
                ActiveProvider::Spotify,
                DeactivationReason::Switch
            )]
        );
        assert_eq!(
            coordinator
                .inner
                .machine
                .lock()
                .unwrap()
                .state
                .active_provider(),
            Some(ActiveProvider::Spotify)
        );
    }

    #[tokio::test]
    async fn reverse_switch_uses_the_same_engine_contract() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::YouTubeMusic);

        assert!(coordinator
            .switch_to(ActiveProvider::Spotify, &spotify, &youtube, &sessions)
            .await
            .unwrap());
        assert_eq!(
            engine_effects.lock().unwrap().as_slice(),
            [
                Effect::Deactivate(ActiveProvider::YouTubeMusic, DeactivationReason::Switch),
                Effect::Resume(ActiveProvider::Spotify),
            ]
        );
    }

    #[tokio::test]
    async fn incoming_failure_leaves_no_engine_logically_active() {
        let (coordinator, spotify, mut youtube, sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        youtube.fail_resume = true;

        assert!(coordinator
            .switch_to(ActiveProvider::YouTubeMusic, &spotify, &youtube, &sessions,)
            .await
            .is_err());
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Idle
        );
        assert_eq!(
            sessions.effects(),
            [
                Effect::Remember(ActiveProvider::Spotify, true),
                Effect::Persist,
                Effect::Refresh(ActiveProvider::Spotify),
                Effect::Persist,
            ]
        );
    }

    #[tokio::test]
    async fn duplicate_switch_is_a_noop_without_cancelling_current_work() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        let current = coordinator.begin_activation();

        assert!(!coordinator
            .switch_to(ActiveProvider::Spotify, &spotify, &youtube, &sessions)
            .await
            .unwrap());
        assert!(coordinator.activation_is_current(&current));
        assert!(engine_effects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pause_and_resume_are_rejected_for_the_inactive_engine() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::YouTubeMusic);

        assert!(!coordinator
            .pause(ActiveProvider::Spotify, &spotify, &youtube, &sessions)
            .await
            .unwrap());
        assert!(!coordinator
            .resume(ActiveProvider::Spotify, &spotify, &youtube, &sessions)
            .await
            .unwrap());
        assert!(engine_effects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_control_routes_through_the_coordinator_owner() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        spotify.playing.store(true, Ordering::Release);

        assert_eq!(
            coordinator
                .control_active_playback(
                    super::super::request::ActivePlaybackControl::Toggle,
                    &spotify,
                    &youtube,
                    &sessions,
                )
                .await
                .unwrap(),
            ActivePlaybackControlOutcome::Applied(ActiveProvider::Spotify)
        );
        assert_eq!(
            engine_effects.lock().unwrap().as_slice(),
            [Effect::Deactivate(
                ActiveProvider::Spotify,
                DeactivationReason::Pause
            )]
        );
        assert_eq!(sessions.owner(), Some(ActiveProvider::Spotify));
    }

    #[tokio::test]
    async fn active_control_distinguishes_idempotent_and_unavailable_states() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        spotify.playing.store(true, Ordering::Release);
        assert_eq!(
            coordinator
                .control_active_playback(
                    super::super::request::ActivePlaybackControl::Play,
                    &spotify,
                    &youtube,
                    &sessions,
                )
                .await
                .unwrap(),
            ActivePlaybackControlOutcome::AlreadySatisfied(ActiveProvider::Spotify)
        );
        assert!(engine_effects.lock().unwrap().is_empty());

        spotify.has_session.store(false, Ordering::Release);
        assert_eq!(
            coordinator
                .control_active_playback(
                    super::super::request::ActivePlaybackControl::Toggle,
                    &spotify,
                    &youtube,
                    &sessions,
                )
                .await
                .unwrap(),
            ActivePlaybackControlOutcome::Rejected(
                super::super::request::PlaybackControlRejection::NoActivePlayback
            )
        );
    }

    #[tokio::test]
    async fn active_control_rejects_transition_without_touching_an_engine() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        coordinator.inner.machine.lock().unwrap().state = PlaybackState::Transitioning {
            active: Some(ActiveProvider::Spotify),
            target: ActiveProvider::YouTubeMusic,
            generation: 7,
        };

        assert_eq!(
            coordinator
                .control_active_playback(
                    super::super::request::ActivePlaybackControl::Pause,
                    &spotify,
                    &youtube,
                    &sessions,
                )
                .await
                .unwrap(),
            ActivePlaybackControlOutcome::Rejected(
                super::super::request::PlaybackControlRejection::TransitionInProgress
            )
        );
        assert!(engine_effects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_seek_uses_the_coordinator_owner_and_activation_cancels_it() {
        let (coordinator, spotify, youtube, _sessions, _engine_effects) =
            harness(ActiveProvider::YouTubeMusic);

        let permit = coordinator
            .acquire_active_seek(&spotify, &youtube)
            .await
            .unwrap();
        assert_eq!(permit.provider(), ActiveProvider::YouTubeMusic);
        assert!(coordinator.active_seek_is_current(&permit));
        let cancellation = permit.cancellation();

        let _activation = coordinator.reserve_activation();
        assert!(cancellation.is_cancelled());
        assert!(!coordinator.active_seek_is_current(&permit));
    }

    #[tokio::test]
    async fn active_seek_rejects_transition_and_missing_active_session() {
        let (coordinator, spotify, youtube, _sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        spotify.has_session.store(false, Ordering::Release);
        assert!(matches!(
            coordinator.acquire_active_seek(&spotify, &youtube).await,
            Err(super::super::request::PlaybackControlRejection::NoActivePlayback)
        ));

        coordinator.inner.machine.lock().unwrap().state = PlaybackState::Transitioning {
            active: Some(ActiveProvider::Spotify),
            target: ActiveProvider::YouTubeMusic,
            generation: 7,
        };
        assert!(matches!(
            coordinator.acquire_active_seek(&spotify, &youtube).await,
            Err(super::super::request::PlaybackControlRejection::TransitionInProgress)
        ));
    }

    #[tokio::test]
    async fn explicit_pause_is_persisted_by_the_coordinator() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        spotify.playing.store(true, Ordering::Release);

        assert!(coordinator
            .pause(ActiveProvider::Spotify, &spotify, &youtube, &sessions)
            .await
            .unwrap());
        assert_eq!(
            engine_effects.lock().unwrap().as_slice(),
            [Effect::Deactivate(
                ActiveProvider::Spotify,
                DeactivationReason::Pause
            )]
        );
        assert_eq!(
            sessions.effects(),
            [
                Effect::Remember(ActiveProvider::Spotify, false),
                Effect::Persist
            ]
        );
    }

    #[tokio::test]
    async fn account_change_stops_active_engine_and_disables_resume() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        let stale = coordinator.begin_activation();
        spotify.playing.store(true, Ordering::Release);

        coordinator
            .stop_for_account_change(&spotify, &youtube, &sessions)
            .await
            .unwrap();

        assert!(!coordinator.activation_is_current(&stale));
        assert_eq!(
            engine_effects.lock().unwrap().as_slice(),
            [Effect::Deactivate(
                ActiveProvider::Spotify,
                DeactivationReason::ReplacePlayback
            )]
        );
        assert_eq!(
            sessions.effects(),
            [
                Effect::Remember(ActiveProvider::Spotify, false),
                Effect::Persist,
                Effect::Refresh(ActiveProvider::Spotify),
                Effect::Persist,
            ]
        );
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Idle
        );
    }

    #[tokio::test]
    async fn failed_same_engine_replacement_restores_previous_active_state() {
        let (coordinator, spotify, youtube, sessions, _engine_effects) =
            harness(ActiveProvider::YouTubeMusic);

        assert!(coordinator
            .activate_with(
                ActiveProvider::YouTubeMusic,
                &spotify,
                &youtube,
                &sessions,
                |_| async { anyhow::bail!("injected start failure") },
            )
            .await
            .is_err());
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Active(ActiveProvider::YouTubeMusic)
        );
    }

    #[tokio::test]
    async fn replacement_cancels_stale_activation_before_it_can_commit() {
        let (coordinator, spotify, youtube, sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        let release = Arc::new(tokio::sync::Notify::new());
        let active_cancellation = Arc::new(Mutex::new(None));
        let first = coordinator.activate_with(
            ActiveProvider::YouTubeMusic,
            &spotify,
            &youtube,
            &sessions,
            |cancellation| {
                let release = release.clone();
                *active_cancellation.lock().unwrap() = Some(cancellation);
                async move {
                    release.notified().await;
                    Ok(())
                }
            },
        );
        tokio::pin!(first);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut first)
                .await
                .is_err()
        );
        assert!(!coordinator.accepts_control(ActiveProvider::YouTubeMusic));
        assert!(coordinator.accepts_replacement(ActiveProvider::YouTubeMusic));
        assert!(!coordinator.accepts_replacement(ActiveProvider::Spotify));
        assert!(!active_cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled());

        let second = coordinator.activate_with(
            ActiveProvider::Spotify,
            &spotify,
            &youtube,
            &sessions,
            |_| async { Ok(()) },
        );
        assert!(second.await.unwrap());
        assert!(active_cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled());
        release.notify_waiters();
        assert!(!first.await.unwrap());
        assert_eq!(
            coordinator
                .inner
                .machine
                .lock()
                .unwrap()
                .state
                .active_provider(),
            Some(ActiveProvider::Spotify)
        );
    }

    #[tokio::test]
    async fn dispatch_reserved_permits_make_received_request_order_authoritative() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        let older = coordinator.reserve_activation();
        let newer = coordinator.reserve_activation();

        assert!(!coordinator.activation_permit_is_current(&older));
        assert!(coordinator.activation_permit_is_current(&newer));
        let published = AtomicBool::new(false);
        assert!(coordinator
            .with_current_activation(&older, || published.store(true, Ordering::Release))
            .is_none());
        assert!(!published.load(Ordering::Acquire));
        assert!(coordinator
            .with_current_activation(&newer, || published.store(true, Ordering::Release))
            .is_some());
        assert!(published.load(Ordering::Acquire));
        assert!(!coordinator
            .activate_with_permit(
                Some(&older),
                ActiveProvider::YouTubeMusic,
                &spotify,
                &youtube,
                &sessions,
                |_| async { Ok(()) },
            )
            .await
            .unwrap());
        assert!(engine_effects.lock().unwrap().is_empty());

        assert!(coordinator
            .activate_with_permit(
                Some(&newer),
                ActiveProvider::YouTubeMusic,
                &spotify,
                &youtube,
                &sessions,
                |_| async { Ok(()) },
            )
            .await
            .unwrap());
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Active(ActiveProvider::YouTubeMusic)
        );
    }

    #[tokio::test]
    async fn completed_stale_start_is_deactivated_before_new_target_commits() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::Spotify);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let first = coordinator.activate_with(
            ActiveProvider::YouTubeMusic,
            &spotify,
            &youtube,
            &sessions,
            |_| {
                let started = started.clone();
                let release = release.clone();
                async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(())
                }
            },
        );
        tokio::pin!(first);
        tokio::select! {
            _ = started.notified() => {}
            result = &mut first => panic!("first activation ended early: {result:?}"),
        }

        let operation = coordinator.inner.operations.lock().await;
        release.notify_one();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut first)
                .await
                .is_err()
        );

        let second = coordinator.activate_with(
            ActiveProvider::Spotify,
            &spotify,
            &youtube,
            &sessions,
            |_| async { Ok(()) },
        );
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut second)
                .await
                .is_err()
        );
        drop(operation);

        let (first_result, second_result) = tokio::join!(&mut first, &mut second);
        assert!(!first_result.unwrap());
        assert!(second_result.unwrap());
        let effects = engine_effects.lock().unwrap().clone();
        assert!(
            effects.contains(&Effect::Deactivate(
                ActiveProvider::YouTubeMusic,
                DeactivationReason::ReplacePlayback,
            )),
            "effects={effects:?}"
        );
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Active(ActiveProvider::Spotify)
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_work_stops_both_adapters_and_persists() {
        let (coordinator, spotify, youtube, sessions, engine_effects) =
            harness(ActiveProvider::YouTubeMusic);
        let stale = coordinator.begin_activation();

        coordinator
            .shutdown(&spotify, &youtube, &sessions)
            .await
            .unwrap();

        assert!(stale.cancellation.is_cancelled());
        assert!(!coordinator.activation_is_current(&stale));
        assert_eq!(
            engine_effects.lock().unwrap().as_slice(),
            [
                Effect::Shutdown(ActiveProvider::Spotify),
                Effect::Shutdown(ActiveProvider::YouTubeMusic),
            ]
        );
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Stopped
        );
        assert!(sessions
            .effects()
            .contains(&Effect::Remember(ActiveProvider::YouTubeMusic, false)));
        assert_eq!(sessions.effects().last(), Some(&Effect::Persist));
    }

    #[tokio::test]
    async fn shutdown_rejects_later_activation() {
        let (coordinator, spotify, youtube, sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        coordinator
            .shutdown(&spotify, &youtube, &sessions)
            .await
            .unwrap();

        let result = coordinator
            .activate_with(
                ActiveProvider::YouTubeMusic,
                &spotify,
                &youtube,
                &sessions,
                |_| async { Ok(()) },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            coordinator.inner.machine.lock().unwrap().state,
            PlaybackState::Stopped
        );
    }

    #[test]
    fn newer_prefetch_cancels_the_previous_token() {
        let (coordinator, _spotify, _youtube, _sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        let first = coordinator.begin_youtube_prefetch();
        let second = coordinator.begin_youtube_prefetch();

        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        coordinator.cancel_youtube_prefetch();
        assert!(second.is_cancelled());
    }

    #[test]
    fn spotify_update_generation_rejects_stale_refreshes() {
        let (coordinator, _spotify, _youtube, _sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        let first = coordinator.begin_spotify_update();
        let second = coordinator.begin_spotify_update();

        assert!(!coordinator.spotify_update_is_current(first));
        assert!(coordinator.spotify_update_is_current(second));
    }

    #[test]
    #[cfg(feature = "streaming")]
    fn matching_event_can_cancel_a_pending_spotify_refresh() {
        let (coordinator, _spotify, _youtube, _sessions, _engine_effects) =
            harness(ActiveProvider::Spotify);
        let pending = coordinator.begin_spotify_update();

        coordinator.cancel_spotify_update();

        assert!(!coordinator.spotify_update_is_current(pending));
    }

    #[test]
    fn typed_state_never_represents_two_active_engines() {
        let mut machine = StateMachine::new(ActiveProvider::Spotify);
        machine.begin_transition(ActiveProvider::YouTubeMusic, 1);
        assert_eq!(machine.state.active_provider(), None);
        assert!(machine.finish_transition(ActiveProvider::YouTubeMusic, 1));
        assert_eq!(
            machine.state.active_provider(),
            Some(ActiveProvider::YouTubeMusic)
        );
    }

    #[test]
    fn spotify_resume_requires_same_paused_remote_media() {
        use crate::state::{MediaId, MediaKind};

        let session = |media: &str, playing: bool, resume: bool| ProviderPlaybackSession {
            provider: Provider::Spotify,
            media_id: Some(MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: media.to_string(),
            }),
            queue_index: None,
            progress: std::time::Duration::from_secs(20),
            is_playing: playing,
            resume_on_activate: resume,
            repeat: rspotify::model::RepeatState::Off,
            shuffle: false,
            volume: 50,
        };
        let saved = session("track", true, true);
        let matching = session("track", false, false);
        let playing = session("track", true, false);
        let different = session("other", false, false);

        assert!(spotify_should_resume(Some(&saved), Some(&matching)));
        assert!(!spotify_should_resume(Some(&saved), Some(&playing)));
        assert!(!spotify_should_resume(Some(&saved), Some(&different)));
        assert!(!spotify_should_resume(None, Some(&matching)));
    }

    #[test]
    fn spotify_command_result_is_published_before_delayed_remote_refresh() {
        use crate::state::PlaybackMetadata;

        let mut player = PlayerState {
            buffered_playback: Some(PlaybackMetadata {
                device_name: "test".to_string(),
                device_id: Some("device".to_string()),
                volume: Some(50),
                is_playing: false,
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                mute_state: None,
            }),
            ..PlayerState::default()
        };
        player.provider_sessions.insert(
            Provider::Spotify,
            ProviderPlaybackSession {
                provider: Provider::Spotify,
                media_id: None,
                queue_index: None,
                progress: std::time::Duration::ZERO,
                is_playing: false,
                resume_on_activate: true,
                repeat: rspotify::model::RepeatState::Off,
                shuffle: false,
                volume: 50,
            },
        );

        publish_spotify_playing_state(&mut player, true);
        assert!(player.buffered_playback.as_ref().unwrap().is_playing);
        assert!(
            player
                .provider_sessions
                .get(&Provider::Spotify)
                .unwrap()
                .is_playing
        );

        publish_spotify_playing_state(&mut player, false);
        assert!(!player.buffered_playback.as_ref().unwrap().is_playing);
        let session = player.provider_sessions.get(&Provider::Spotify).unwrap();
        assert!(!session.is_playing);
        assert!(session.resume_on_activate);
    }
}
