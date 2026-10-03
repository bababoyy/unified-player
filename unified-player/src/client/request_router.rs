use anyhow::Result;

use crate::state::SharedState;

use super::{
    playback_coordinator,
    request::{PlaybackControlRejection, RequestDomain},
    AppClient, ClientRequest,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestDisposition {
    Applied,
    NoOp,
    Rejected(PlaybackControlRejection),
    Superseded,
}

impl AppClient {
    /// Route a client request to the one service that owns its effects.
    pub(crate) async fn handle_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
        activation: Option<playback_coordinator::ActivationPermit>,
    ) -> Result<RequestDisposition> {
        let timer = std::time::Instant::now();
        let activation = activation.as_ref();
        if self.with_current_activation(activation, || ()).is_none() {
            return Ok(RequestDisposition::Superseded);
        }

        let domain = request.domain();
        let (component, stage) = match domain {
            RequestDomain::AuthSession => {
                (crate::observability::Component::Application, "auth_session")
            }
            RequestDomain::ProviderRead => (
                crate::observability::Component::Application,
                "provider_read",
            ),
            RequestDomain::Playback => (
                crate::observability::Component::Coordinator,
                "playback_coordination",
            ),
            RequestDomain::PlaylistMutation => (
                crate::observability::Component::Application,
                "playlist_mutation",
            ),
        };
        let result = match domain {
            RequestDomain::AuthSession => self
                .handle_auth_session_request(state, request)
                .await
                .map(|()| RequestDisposition::Applied),
            RequestDomain::ProviderRead => {
                Box::pin(self.handle_provider_read_request(state, request, activation))
                    .await
                    .map(|()| RequestDisposition::Applied)
            }
            RequestDomain::Playback => {
                self.handle_playback_request(state, request, activation)
                    .await
            }
            RequestDomain::PlaylistMutation => self
                .handle_playlist_mutation_request(state, request)
                .await
                .map(|()| RequestDisposition::Applied),
        };
        let result = if activation
            .is_some_and(|permit| !self.playback.activation_permit_is_current(permit))
        {
            Ok(RequestDisposition::Superseded)
        } else {
            result
        };
        let outcome = match &result {
            Ok(RequestDisposition::Applied) => crate::observability::OperationOutcome::Success,
            Ok(RequestDisposition::NoOp | RequestDisposition::Rejected(_)) => {
                crate::observability::OperationOutcome::Rejected
            }
            Ok(RequestDisposition::Superseded) => {
                crate::observability::OperationOutcome::Superseded
            }
            Err(_) => crate::observability::OperationOutcome::Error,
        };
        crate::observability::operation_stage(
            component,
            stage,
            Some(timer.elapsed()),
            Some(outcome),
        );
        let disposition = result?;

        match disposition {
            RequestDisposition::Applied => {
                tracing::info!(
                    duration_ms = timer.elapsed().as_millis() as u64,
                    "Successfully handled the client request"
                );
            }
            RequestDisposition::NoOp => {
                tracing::debug!(
                    duration_ms = timer.elapsed().as_millis() as u64,
                    "Client request had no effect"
                );
            }
            RequestDisposition::Rejected(reason) => {
                tracing::debug!(
                    duration_ms = timer.elapsed().as_millis() as u64,
                    reason = reason.diagnostic_reason(),
                    "Client request was rejected"
                );
            }
            RequestDisposition::Superseded => {
                tracing::debug!(
                    duration_ms = timer.elapsed().as_millis() as u64,
                    "Discarding a superseded client request"
                );
            }
        }
        Ok(disposition)
    }
}
