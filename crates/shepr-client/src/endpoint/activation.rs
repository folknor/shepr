use std::time::Instant;

use shepr_protocol::command::{EndpointError, EndpointReply};

use super::{ClientEndpointId, ClientEndpointStatus, EndpointRegistry, EndpointSendOutcome};
use crate::limits::ACTIVATION_TIMEOUT;

mod model;
mod protocol;
use self::protocol::*;
pub use model::{
    ActivationBeginError, ActivationCompletion, PendingEndpointActivation,
    SurfaceActivationProgress,
};
use model::{ActivationEvidence, ActivationPhase, EndpointLease};
pub(crate) use model::{ActivationRollback, EndpointActivationIntent};

fn release_surface_best_effort(
    lease: &EndpointLease,
    endpoints: &mut EndpointRegistry,
    request_id: &shepr_protocol::RequestId,
) {
    if !endpoints.accepts(&lease.endpoint_id, lease.generation) {
        return;
    }
    endpoints.set_surface_active(&lease.endpoint_id, false);
    let _ = endpoints.send_to(
        &lease.endpoint_id,
        &shepr_protocol::ClientMessage::ClientShellFocus { focused: false },
    );
    // A lease without a boot id never had a server to release a surface on.
    let Some(boot_id) = lease.boot_id.as_ref() else {
        return;
    };
    let _ = endpoints.send_to(
        &lease.endpoint_id,
        &surface_interest_request(boot_id, request_id, false),
    );
}

impl PendingEndpointActivation {
    pub fn prepare(
        shell: &crate::shell::ClientShellState,
        endpoints: &EndpointRegistry,
        target: &ClientEndpointId,
        focus: Option<crate::shell::ClientEndpointFocusTarget>,
        resize: shepr_protocol::ClientMessage,
        serial: u64,
        now: Instant,
    ) -> Result<Self, ActivationBeginError> {
        let geometry = resize_geometry(&resize).ok_or_else(|| {
            ActivationBeginError::Preflight(
                "endpoint activation did not include a surface resize".to_owned(),
            )
        })?;
        let source_id = endpoints.active_id().clone();
        let source_has_live_surface = endpoints
            .connection(&source_id)
            .is_some_and(|connection| connection.surface_active);
        let (source, source_available) = if source_has_live_surface {
            (
                endpoint_lease(shell, endpoints, &source_id)
                    .map_err(ActivationBeginError::Preflight)?,
                true,
            )
        } else {
            (disconnected_endpoint_lease(shell, &source_id), false)
        };
        let target_lease =
            endpoint_lease(shell, endpoints, target).map_err(ActivationBeginError::Preflight)?;
        let source_is_target = source.endpoint_id == target_lease.endpoint_id;
        // Check that every lease the lifecycle requests will name has a boot before the first
        // transport write. Any error above this line is guaranteed not to have changed either
        // endpoint.
        if source_available && !source_is_target {
            source
                .request_boot_id()
                .map_err(ActivationBeginError::Preflight)?;
        }
        target_lease
            .request_boot_id()
            .map_err(ActivationBeginError::Preflight)?;

        Ok(Self {
            source,
            source_available,
            target: target_lease,
            focus,
            host_focused: shell.host_focus_baseline(),
            resize,
            geometry,
            phase: ActivationPhase::ReleasingSource {
                request_id: format!("client-shell-surface:{serial}:off").into(),
            },
            deadline: crate::limits::Deadline::after(now, ACTIVATION_TIMEOUT).instant(),
            epoch: serial,
            next_focus_serial: 0,
            rollback_error: None,
            successor: None,
        })
    }

    pub fn start_at(
        mut self,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<Self, ActivationBeginError> {
        endpoints.freeze_input();
        match self.start_prepared(endpoints, now) {
            Ok(()) => Ok(self),
            Err(error) => Err(ActivationBeginError::Partial {
                activation: Box::new(self),
                error,
            }),
        }
    }

    fn start_prepared(
        &mut self,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<(), String> {
        let source_is_target = self.source.endpoint_id == self.target.endpoint_id;
        // Local must not depend on a remote acknowledgement to become usable.
        if source_is_target || !self.source_available || self.target.endpoint_id.is_local() {
            if self.source_available && !source_is_target {
                release_surface_best_effort(
                    &self.source,
                    endpoints,
                    &format!("client-shell-surface:{}:off", self.epoch).into(),
                );
            }
            let resize = self.resize.clone();
            return self.start_target(endpoints, &resize, now);
        }
        // Old servers emit PTY focus loss only while the viewer is still active.
        if endpoints.send_to(
            &self.source.endpoint_id,
            &shepr_protocol::ClientMessage::ClientShellFocus { focused: false },
        ) != EndpointSendOutcome::Sent
        {
            return Err("source endpoint focus revoke could not be sent".into());
        }
        let request = surface_interest_request(
            self.source.request_boot_id()?,
            &format!("client-shell-surface:{}:off", self.epoch).into(),
            false,
        );
        if endpoints.send_to(&self.source.endpoint_id, &request) != EndpointSendOutcome::Sent {
            return Err("source endpoint release could not be sent".into());
        }
        endpoints.set_surface_active(&self.source.endpoint_id, false);
        Ok(())
    }

    pub(crate) fn abandon(&self, endpoints: &mut EndpointRegistry) {
        endpoints.freeze_input();
        for lease in [&self.source, &self.target] {
            release_surface_best_effort(
                lease,
                endpoints,
                &format!("client-shell-surface:{}:abandon", self.epoch).into(),
            );
            if self.source.endpoint_id == self.target.endpoint_id {
                break;
            }
        }
    }

    pub(crate) fn target(&self) -> &ClientEndpointId {
        &self.target.endpoint_id
    }

    fn geometry(&self) -> shepr_protocol::ClientSurfaceSize {
        self.geometry
    }

    /// The surface size this handoff asked its endpoint to render. It was computed from the
    /// shell layout of the projection current when the handoff started (or last resized), which
    /// can differ from the committed projection's layout: the tab bar hides for a single tab.
    pub(crate) fn requested_surface_size(&self) -> shepr_protocol::ClientSurfaceSize {
        self.geometry
    }

    pub(crate) fn presentation_sync_endpoint(&self) -> Option<&ClientEndpointId> {
        match self.phase {
            ActivationPhase::ActivatingTarget { .. } => Some(&self.target.endpoint_id),
            ActivationPhase::RestoringSource { .. } => Some(&self.source.endpoint_id),
            _ => None,
        }
    }

    /// Whether the frame on screen stays frozen. It does until this handoff has installed a
    /// coherent snapshot and surface pair for the endpoint it commits (the target, or the source
    /// it restores). From presentation synchronization on that pair is on screen, and only pane
    /// input stays closed until the effects fence. A rollback out of synchronization re-enters
    /// a frozen phase.
    pub(crate) fn freezes_frame(&self) -> bool {
        !matches!(
            self.phase,
            ActivationPhase::SynchronizingPresentation { .. }
                | ActivationPhase::AwaitingPresentationEffects { .. }
        )
    }

    /// The complete source command lane cannot safely cross source-off into a later presentation
    /// epoch. Other endpoint lanes are not part of this retirement.
    pub(crate) fn source_command_lane(&self) -> Option<&ClientEndpointId> {
        (self.source_available && self.source.endpoint_id != self.target.endpoint_id)
            .then_some(&self.source.endpoint_id)
    }

    pub(crate) fn can_retarget(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.target.endpoint_id == *endpoint_id
            && self.successor.is_none()
            && matches!(
                self.phase,
                ActivationPhase::ReleasingSource { .. } | ActivationPhase::ActivatingTarget { .. }
            )
    }

    pub(crate) fn supersede_at(
        &mut self,
        endpoint_id: ClientEndpointId,
        target: Option<crate::shell::ClientEndpointFocusTarget>,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> ActivationRollback {
        self.successor = Some(EndpointActivationIntent {
            endpoint_id,
            target,
        });
        // Source-on is already ordered and must finish before any replacement is allowed to
        // begin. Later rapid selections only replace the retained intent; they never turn a
        // safe restoration into an unavailable state.
        let source_restoration_in_flight = matches!(
            self.phase,
            ActivationPhase::RestoringSource { .. }
        ) || matches!(
            &self.phase,
            ActivationPhase::SynchronizingPresentation { completion, .. }
                if matches!(completion.as_ref(), ActivationCompletion::RestoredSource { .. })
        );
        if source_restoration_in_flight {
            return ActivationRollback::Pending;
        }
        self.rollback_at(
            endpoints,
            "endpoint handoff superseded by a newer selection",
            false,
            now,
        )
    }

    pub(crate) fn accepts_endpoint(&self, endpoint_id: &ClientEndpointId, generation: u64) -> bool {
        (self.source_available
            && self.source.endpoint_id == *endpoint_id
            && self.source.generation == generation)
            || (self.target.endpoint_id == *endpoint_id && self.target.generation == generation)
    }

    pub(crate) fn involves_endpoint(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.source.endpoint_id == *endpoint_id || self.target.endpoint_id == *endpoint_id
    }

    pub(crate) fn accepts_response(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        request_id: &str,
    ) -> bool {
        match &self.phase {
            ActivationPhase::ReleasingSource {
                request_id: expected,
            }
            | ActivationPhase::RestoringSource {
                request_id: expected,
                ..
            } => {
                endpoint_matches(&self.source, endpoint_id, generation, boot_id)
                    && expected == request_id
            }
            ActivationPhase::ActivatingTarget {
                request_id: expected,
                focus_request_id,
                ..
            } => {
                endpoint_matches(&self.target, endpoint_id, generation, boot_id)
                    && (expected == request_id || focus_request_id.as_deref() == Some(request_id))
            }
            ActivationPhase::ReleasingTargetForRollback {
                request_id: expected,
            } => {
                endpoint_matches(&self.target, endpoint_id, generation, boot_id)
                    && expected == request_id
            }
            ActivationPhase::SynchronizingPresentation {
                lease,
                request_id: expected,
                ..
            } => {
                endpoint_matches(lease, endpoint_id, generation, boot_id) && expected == request_id
            }
            ActivationPhase::AwaitingPresentationEffects { .. } => false,
        }
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        crate::limits::Deadline::at(self.deadline).is_expired(now)
    }

    pub fn receive_response_for_boot_at(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, EndpointError>,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> SurfaceActivationProgress {
        if !self.accepts_response(endpoint_id, generation, boot_id, request_id) {
            return SurfaceActivationProgress::Stale;
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                return SurfaceActivationProgress::Rejected {
                    source_release_rejected: matches!(
                        self.phase,
                        ActivationPhase::ReleasingSource { .. }
                    ),
                    message: error.message,
                };
            }
        };
        match &mut self.phase {
            ActivationPhase::ReleasingSource { .. } => {
                if let Err(message) = surface_set_revision(&result, false) {
                    return SurfaceActivationProgress::Rejected {
                        message,
                        source_release_rejected: false,
                    };
                }
                let resize = self.resize.clone();
                if let Err(message) = self.start_target(endpoints, &resize, now) {
                    return SurfaceActivationProgress::Rejected {
                        message,
                        source_release_rejected: false,
                    };
                }
                SurfaceActivationProgress::Pending
            }
            ActivationPhase::ActivatingTarget {
                request_id: surface_request_id,
                acknowledged_revision,
                ..
            } if surface_request_id == request_id => {
                let revision = match surface_set_revision(&result, true) {
                    Ok(revision) => revision,
                    Err(message) => {
                        return SurfaceActivationProgress::Rejected {
                            message,
                            source_release_rejected: false,
                        };
                    }
                };
                *acknowledged_revision = Some(revision);
                self.progress()
            }
            ActivationPhase::ActivatingTarget {
                focus_request_id,
                focus_request_target,
                focus_acknowledged,
                ..
            } if focus_request_id.as_deref() == Some(request_id) => {
                let requested = focus_request_target.clone();
                let Some(requested) = requested else {
                    return SurfaceActivationProgress::Stale;
                };
                if !focus_result_matches(Some(&requested), &result) {
                    return SurfaceActivationProgress::Rejected {
                        message: "endpoint focus returned an unexpected result".into(),
                        source_release_rejected: false,
                    };
                }
                *focus_request_id = None;
                *focus_request_target = None;
                if self.focus != Some(requested) {
                    *focus_acknowledged = false;
                    if let Err(message) = self.send_latest_focus(endpoints) {
                        return SurfaceActivationProgress::Rejected {
                            message,
                            source_release_rejected: false,
                        };
                    }
                    return self.progress();
                }
                *focus_acknowledged = true;
                self.progress()
            }
            ActivationPhase::ReleasingTargetForRollback { .. } => {
                if let Err(message) = surface_set_revision(&result, false) {
                    return SurfaceActivationProgress::Rejected {
                        message,
                        source_release_rejected: false,
                    };
                }
                endpoints.set_surface_active(&self.target.endpoint_id, false);
                if !self.source_available {
                    return SurfaceActivationProgress::Rejected {
                        message: self.rollback_error.clone().unwrap_or_else(|| {
                            "the previous endpoint is no longer connected".into()
                        }),
                        source_release_rejected: false,
                    };
                }
                let resize = self.resize.clone();
                if let Err(message) = self.start_source_restore(endpoints, &resize, now) {
                    return SurfaceActivationProgress::Rejected {
                        message,
                        source_release_rejected: false,
                    };
                }
                SurfaceActivationProgress::Pending
            }
            ActivationPhase::RestoringSource {
                acknowledged_revision,
                ..
            }
            | ActivationPhase::SynchronizingPresentation {
                acknowledged_revision,
                ..
            } => {
                let revision = match surface_set_revision(&result, true) {
                    Ok(revision) => revision,
                    Err(message) => {
                        return SurfaceActivationProgress::Rejected {
                            message,
                            source_release_rejected: false,
                        };
                    }
                };
                *acknowledged_revision = Some(revision);
                self.progress()
            }
            _ => SurfaceActivationProgress::Stale,
        }
    }

    pub fn receive_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: &shepr_protocol::ClientShellSnapshot,
    ) -> SurfaceActivationProgress {
        let lease = match &self.phase {
            ActivationPhase::ActivatingTarget { .. } => &self.target,
            ActivationPhase::RestoringSource { .. } => &self.source,
            ActivationPhase::SynchronizingPresentation { lease, .. } => lease,
            _ => return SurfaceActivationProgress::Stale,
        };
        if !endpoint_matches(lease, endpoint_id, generation, &snapshot.boot_id)
            || snapshot.revision < lease.minimum_revision
        {
            return SurfaceActivationProgress::Stale;
        }
        let evidence = match &mut self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. }
                if endpoint_matches(&self.target, endpoint_id, generation, &snapshot.boot_id) =>
            {
                evidence
            }
            ActivationPhase::RestoringSource { evidence, .. }
                if endpoint_matches(&self.source, endpoint_id, generation, &snapshot.boot_id) =>
            {
                evidence
            }
            ActivationPhase::SynchronizingPresentation {
                lease, evidence, ..
            } if endpoint_matches(lease, endpoint_id, generation, &snapshot.boot_id) => evidence,
            _ => return SurfaceActivationProgress::Stale,
        };
        evidence.record_snapshot(snapshot);
        self.progress()
    }

    pub fn receive_surface(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        surface: shepr_protocol::PaneSurfaceFrame,
    ) -> SurfaceActivationProgress {
        let lease = match &self.phase {
            ActivationPhase::ActivatingTarget { .. } => &self.target,
            ActivationPhase::RestoringSource { .. } => &self.source,
            ActivationPhase::SynchronizingPresentation { lease, .. } => lease,
            _ => return SurfaceActivationProgress::Stale,
        };
        if !endpoint_matches(lease, endpoint_id, generation, &surface.boot_id) {
            return SurfaceActivationProgress::Stale;
        }
        if !surface_matches_geometry(&surface, self.geometry()) {
            return SurfaceActivationProgress::Pending;
        }
        match &mut self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. }
            | ActivationPhase::RestoringSource { evidence, .. }
            | ActivationPhase::SynchronizingPresentation { evidence, .. } => {
                evidence.record_surface(surface);
            }
            // The lease match above only succeeds in the three phases handled here.
            _ => return SurfaceActivationProgress::Stale,
        }
        self.progress()
    }

    pub fn receive_presentation_effects_ready(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        token: &str,
    ) -> SurfaceActivationProgress {
        let ActivationPhase::AwaitingPresentationEffects {
            lease,
            token: expected,
            ready,
            ..
        } = &mut self.phase
        else {
            return SurfaceActivationProgress::Stale;
        };
        if lease.endpoint_id != *endpoint_id || lease.generation != generation || expected != token
        {
            return SurfaceActivationProgress::Stale;
        }
        *ready = true;
        SurfaceActivationProgress::Ready
    }

    /// A same-endpoint navigation request replaces the desired target but never joins the
    /// in-flight focus RPC. Once that request resolves, `send_latest_focus` sends only the most
    /// recent desired target.
    pub(crate) fn retarget(
        &mut self,
        focus: Option<crate::shell::ClientEndpointFocusTarget>,
        endpoints: &mut EndpointRegistry,
    ) -> Result<(), String> {
        self.focus = focus;
        if let ActivationPhase::ActivatingTarget {
            focus_request_id,
            focus_acknowledged,
            ..
        } = &mut self.phase
        {
            *focus_acknowledged = self.focus.is_none() && focus_request_id.is_none();
        } else {
            // The latest target is retained and will be sent immediately after source release.
            return Ok(());
        }
        self.send_latest_focus(endpoints)
    }

    pub(crate) fn update_resize_at(
        &mut self,
        resize: &shepr_protocol::ClientMessage,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<(), String> {
        self.geometry = resize_geometry(resize)
            .ok_or_else(|| "endpoint activation did not include a surface resize".to_owned())?;
        self.resize = resize.clone();
        let restart_effects_fence = match &self.phase {
            ActivationPhase::AwaitingPresentationEffects {
                lease, completion, ..
            } => Some((lease.clone(), (**completion).clone())),
            _ => None,
        };
        if let Some((lease, completion)) = restart_effects_fence {
            if endpoints.send_to(&lease.endpoint_id, resize) != EndpointSendOutcome::Sent {
                return Err("pending endpoint resize could not be sent".into());
            }
            return self.start_presentation_sync(endpoints, &lease, completion, now);
        }
        match &mut self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. }
            | ActivationPhase::RestoringSource { evidence, .. }
            | ActivationPhase::SynchronizingPresentation { evidence, .. } => {
                evidence.invalidate_surface();
            }
            _ => {}
        }
        let destination = match &self.phase {
            ActivationPhase::ActivatingTarget { .. } => Some(&self.target.endpoint_id),
            ActivationPhase::RestoringSource { .. } => Some(&self.source.endpoint_id),
            ActivationPhase::SynchronizingPresentation { lease, .. } => Some(&lease.endpoint_id),
            _ => None,
        };
        if let Some(destination) = destination
            && endpoints.send_to(destination, resize) != EndpointSendOutcome::Sent
        {
            return Err("pending endpoint resize could not be sent".into());
        }
        Ok(())
    }

    pub(crate) fn update_host_focus_at(
        &mut self,
        focused: bool,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<(), String> {
        self.host_focused = focused;
        let restart = self.presentation_restart();
        let destination = match &self.phase {
            ActivationPhase::ActivatingTarget { .. } => Some(&self.target.endpoint_id),
            ActivationPhase::RestoringSource { .. } => Some(&self.source.endpoint_id),
            ActivationPhase::SynchronizingPresentation { lease, .. }
            | ActivationPhase::AwaitingPresentationEffects { lease, .. } => {
                Some(&lease.endpoint_id)
            }
            _ => None,
        };
        if let Some(destination) = destination
            && endpoints.send_to(
                destination,
                &shepr_protocol::ClientMessage::ClientShellFocus { focused },
            ) != EndpointSendOutcome::Sent
        {
            return Err("pending endpoint focus baseline could not be sent".into());
        }
        if let Some((lease, completion)) = restart {
            self.start_presentation_sync(endpoints, &lease, completion, now)?;
        } else {
            self.invalidate_current_evidence();
        }
        Ok(())
    }

    pub(crate) fn update_host_theme_at(
        &mut self,
        update: shepr_protocol::ClientHostThemeUpdate,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<(), String> {
        let restart = self.presentation_restart();
        let destination = match &self.phase {
            ActivationPhase::ActivatingTarget { .. } => Some(&self.target.endpoint_id),
            ActivationPhase::RestoringSource { .. } => Some(&self.source.endpoint_id),
            ActivationPhase::SynchronizingPresentation { lease, .. }
            | ActivationPhase::AwaitingPresentationEffects { lease, .. } => {
                Some(&lease.endpoint_id)
            }
            _ => None,
        };
        if let Some(destination) = destination {
            let message = shepr_protocol::ClientMessage::ClientShellHostTheme { update };
            if endpoints.send_to(destination, &message) != EndpointSendOutcome::Sent {
                return Err("pending endpoint host theme could not be sent".into());
            }
        }
        if let Some((lease, completion)) = restart {
            self.start_presentation_sync(endpoints, &lease, completion, now)?;
        } else {
            self.invalidate_current_evidence();
        }
        Ok(())
    }

    fn presentation_restart(&self) -> Option<(EndpointLease, ActivationCompletion)> {
        match &self.phase {
            ActivationPhase::SynchronizingPresentation {
                lease, completion, ..
            }
            | ActivationPhase::AwaitingPresentationEffects {
                lease, completion, ..
            } => Some((lease.clone(), (**completion).clone())),
            _ => None,
        }
    }

    fn invalidate_current_evidence(&mut self) {
        match &mut self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. }
            | ActivationPhase::RestoringSource { evidence, .. } => evidence.invalidate_surface(),
            _ => {}
        }
    }

    pub(crate) fn endpoint_disconnected_at(
        &mut self,
        endpoints: &mut EndpointRegistry,
        endpoint_id: &ClientEndpointId,
        error: String,
        now: Instant,
    ) -> ActivationRollback {
        self.rollback_error = Some(error.clone());
        if self.target.endpoint_id == *endpoint_id && self.source.endpoint_id != *endpoint_id {
            let resize = self.resize.clone();
            let restoring_source = match &self.phase {
                ActivationPhase::RestoringSource { .. } => true,
                ActivationPhase::SynchronizingPresentation { lease, .. }
                | ActivationPhase::AwaitingPresentationEffects { lease, .. } => {
                    lease.endpoint_id == self.source.endpoint_id
                }
                _ => false,
            };
            if restoring_source {
                return ActivationRollback::Pending;
            }
            // A source prepared as disconnected carries a placeholder lease (generation 0):
            // restoring through it would turn on the surface of whatever connection now holds
            // that id, whose acknowledgement can never match, leaving a live surface and focus
            // the client never releases. Same check as the target-release rollback path.
            if !self.source_available {
                return ActivationRollback::Unavailable(format!(
                    "{error}; the previous endpoint is no longer connected"
                ));
            }
            return match self.start_source_restore(endpoints, &resize, now) {
                Ok(()) => ActivationRollback::Pending,
                Err(restore_error) => ActivationRollback::Unavailable(format!(
                    "{error}; source endpoint could not be restored safely: {restore_error}"
                )),
            };
        }
        if self.source.endpoint_id != *endpoint_id {
            return ActivationRollback::Unavailable(error);
        }
        self.source_available = false;
        if self.source.endpoint_id != self.target.endpoint_id {
            match &self.phase {
                ActivationPhase::ReleasingSource { .. } => {
                    let resize = self.resize.clone();
                    return match self.start_target(endpoints, &resize, now) {
                        Ok(()) => ActivationRollback::Pending,
                        Err(message) => ActivationRollback::Unavailable(message),
                    };
                }
                ActivationPhase::ActivatingTarget { .. } => return ActivationRollback::Pending,
                ActivationPhase::SynchronizingPresentation { lease, .. }
                | ActivationPhase::AwaitingPresentationEffects { lease, .. }
                    if lease.endpoint_id == self.target.endpoint_id =>
                {
                    return ActivationRollback::Pending;
                }
                _ => {}
            }
        }
        match self.phase {
            ActivationPhase::ReleasingSource { .. } | ActivationPhase::RestoringSource { .. } => {
                ActivationRollback::Unavailable(error)
            }
            ActivationPhase::ActivatingTarget { .. } => {
                match self.start_target_release(endpoints, now) {
                    Ok(()) => ActivationRollback::Pending,
                    Err(release_error) => ActivationRollback::Unavailable(format!(
                        "{error}; target endpoint could not be released safely: {release_error}"
                    )),
                }
            }
            ActivationPhase::ReleasingTargetForRollback { .. } => ActivationRollback::Pending,
            ActivationPhase::SynchronizingPresentation { ref lease, .. }
            | ActivationPhase::AwaitingPresentationEffects { ref lease, .. } => {
                if lease.endpoint_id == self.target.endpoint_id {
                    match self.start_target_release(endpoints, now) {
                        Ok(()) => ActivationRollback::Pending,
                        Err(release_error) => ActivationRollback::Unavailable(format!(
                            "{error}; target endpoint could not be released safely: {release_error}"
                        )),
                    }
                } else {
                    ActivationRollback::Unavailable(error)
                }
            }
        }
    }

    pub(crate) fn rollback_at(
        &mut self,
        endpoints: &mut EndpointRegistry,
        error: &str,
        source_release_rejected: bool,
        now: Instant,
    ) -> ActivationRollback {
        self.rollback_error = Some(error.to_owned());
        if matches!(self.phase, ActivationPhase::ReleasingSource { .. }) && source_release_rejected
        {
            // Rejection proves source-off did not commit, but cached source metadata may have
            // advanced while the frame was frozen. Restore through the same coherent on/sync
            // path rather than immediately exposing a stale source projection.
            let resize = self.resize.clone();
            return match self.start_source_restore(endpoints, &resize, now) {
                Ok(()) => ActivationRollback::Pending,
                Err(restore_error) => ActivationRollback::Unavailable(format!(
                    "{error}; source endpoint could not resume: {restore_error}"
                )),
            };
        }
        let result = match self.phase {
            ActivationPhase::ReleasingSource { .. } => {
                if self.source_available {
                    let resize = self.resize.clone();
                    self.start_source_restore(endpoints, &resize, now)
                } else {
                    Err("the previous endpoint is no longer connected".into())
                }
            }
            ActivationPhase::ActivatingTarget { .. } => self.start_target_release(endpoints, now),
            ActivationPhase::ReleasingTargetForRollback { .. } => {
                // The target may have observed target-on or target-off. Closing this transport
                // is the only safe local revocation when target-off is not acknowledged.
                endpoints.fail(
                    &self.target.endpoint_id,
                    &std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "endpoint did not acknowledge surface revocation",
                    ),
                );
                if !self.source_available {
                    return ActivationRollback::Unavailable(format!(
                        "{error}; the target connection was closed because no presentation owner could be proven"
                    ));
                }
                let resize = self.resize.clone();
                self.start_source_restore(endpoints, &resize, now)
            }
            ActivationPhase::RestoringSource { .. } => {
                return ActivationRollback::Unavailable(format!(
                    "{error}; source endpoint could not be restored"
                ));
            }
            ActivationPhase::SynchronizingPresentation { ref lease, .. }
            | ActivationPhase::AwaitingPresentationEffects { ref lease, .. } => {
                if lease.endpoint_id == self.target.endpoint_id {
                    self.start_target_release(endpoints, now)
                } else {
                    return ActivationRollback::Unavailable(format!(
                        "{error}; source endpoint presentation could not be synchronized"
                    ));
                }
            }
        };
        match result {
            Ok(()) => ActivationRollback::Pending,
            Err(rollback_error) => ActivationRollback::Unavailable(format!(
                "{error}; source endpoint could not be restored safely: {rollback_error}"
            )),
        }
    }

    pub fn complete_at(
        &mut self,
        shell: &mut crate::shell::ClientShellState,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<ActivationCompletion, String> {
        if let ActivationPhase::SynchronizingPresentation {
            lease,
            acknowledged_revision,
            evidence,
            completion,
            ..
        } = &self.phase
        {
            let lease = lease.clone();
            let completion = (**completion).clone();
            let surface = coherent_completion_surface(
                shell,
                &lease,
                evidence,
                *acknowledged_revision,
                self.geometry(),
            )?;
            if endpoints.active_id() != &lease.endpoint_id
                || !shell.endpoint_projection_available(&lease.endpoint_id)
                || !shell.activate_endpoint_projection(&lease.endpoint_id)
            {
                return Err(
                    "endpoint became unavailable during presentation synchronization".into(),
                );
            }
            shell.set_pane_surface(surface);
            self.start_presentation_effects_fence(endpoints, &lease, completion, now)?;
            return Ok(ActivationCompletion::AwaitingPresentationEffects);
        }
        if let ActivationPhase::AwaitingPresentationEffects {
            ready: true,
            completion,
            ..
        } = &self.phase
        {
            return Ok((**completion).clone());
        }

        let (lease, evidence, acknowledgement_revision, completion) = match &self.phase {
            ActivationPhase::ActivatingTarget {
                evidence,
                acknowledged_revision,
                ..
            } => {
                let completion = ActivationCompletion::Activated;
                (
                    self.target.clone(),
                    evidence,
                    *acknowledged_revision,
                    completion,
                )
            }
            ActivationPhase::RestoringSource {
                evidence,
                acknowledged_revision,
                ..
            } => {
                let completion = ActivationCompletion::RestoredSource {
                    error: self
                        .rollback_error
                        .clone()
                        .unwrap_or_else(|| "endpoint handoff was rolled back".into()),
                    successor: self.successor.clone(),
                };
                (
                    self.source.clone(),
                    evidence,
                    *acknowledged_revision,
                    completion,
                )
            }
            _ => return Err("endpoint activation completed in an invalid phase".into()),
        };
        let surface = coherent_completion_surface(
            shell,
            &lease,
            evidence,
            acknowledgement_revision,
            self.geometry(),
        )?;
        endpoints.set_surface_active(&lease.endpoint_id, true);
        shell.set_endpoint_status(&lease.endpoint_id, ClientEndpointStatus::Online);
        if !shell.endpoint_projection_available(&lease.endpoint_id)
            || !endpoints.set_active(&lease.endpoint_id)
        {
            return Err("endpoint became unavailable during activation".into());
        }
        // The preflight above makes both of these hold; if either does not, the handoff is
        // rolled back like any other activation failure rather than presenting a surface
        // under the wrong projection.
        if !shell.activate_endpoint_projection(&lease.endpoint_id) {
            return Err("preflighted endpoint projection did not activate".into());
        }
        if !shell.endpoint_is_active(endpoints.active_id()) {
            return Err("activated endpoint projection is not the active endpoint".into());
        }
        shell.set_pane_surface(surface);
        self.start_presentation_sync(endpoints, &lease, completion, now)?;
        Ok(ActivationCompletion::AwaitingPresentationSync {
            previous: self.source.endpoint_id.clone(),
            endpoint: lease.endpoint_id,
        })
    }

    fn start_target(
        &mut self,
        endpoints: &mut EndpointRegistry,
        resize: &shepr_protocol::ClientMessage,
        now: Instant,
    ) -> Result<(), String> {
        let request_id =
            shepr_protocol::RequestId::from(format!("client-shell-surface:{}:on", self.epoch));
        self.deadline = crate::limits::Deadline::after(now, ACTIVATION_TIMEOUT).instant();
        // A transport may fail after writing any baseline or surface message. Enter the target
        // phase first so every uncertain target write is reversed through target-off before
        // source restoration is considered.
        self.phase = ActivationPhase::ActivatingTarget {
            request_id: request_id.clone(),
            acknowledged_revision: None,
            focus_request_id: None,
            focus_request_target: None,
            focus_acknowledged: self.focus.is_none(),
            evidence: ActivationEvidence::default(),
        };
        send_surface_activation(
            endpoints,
            &self.target,
            &request_id,
            resize,
            self.host_focused,
        )?;

        // From this point the target may have processed surface.set(true). Optional navigation
        // is serialized through one coalescing focus lane.
        self.send_latest_focus(endpoints)
    }

    fn start_presentation_sync(
        &mut self,
        endpoints: &mut EndpointRegistry,
        lease: &EndpointLease,
        completion: ActivationCompletion,
        now: Instant,
    ) -> Result<(), String> {
        let request_id = shepr_protocol::RequestId::from(format!(
            "client-shell-surface:{}:presentation-sync",
            self.epoch
        ));
        let request = surface_interest_request(lease.request_boot_id()?, &request_id, true);
        self.phase = ActivationPhase::SynchronizingPresentation {
            lease: lease.clone(),
            request_id,
            acknowledged_revision: None,
            evidence: ActivationEvidence::default(),
            completion: Box::new(completion),
        };
        self.deadline = crate::limits::Deadline::after(now, ACTIVATION_TIMEOUT).instant();
        if endpoints.send_to(&lease.endpoint_id, &request) != EndpointSendOutcome::Sent {
            return Err("endpoint presentation synchronization could not be sent".into());
        }
        Ok(())
    }

    fn start_presentation_effects_fence(
        &mut self,
        endpoints: &mut EndpointRegistry,
        lease: &EndpointLease,
        completion: ActivationCompletion,
        now: Instant,
    ) -> Result<(), String> {
        let token = format!(
            "{}:{}:{}",
            self.epoch,
            lease.generation,
            lease.request_boot_id()?
        );
        self.phase = ActivationPhase::AwaitingPresentationEffects {
            lease: lease.clone(),
            token: token.clone(),
            ready: false,
            completion: Box::new(completion),
        };
        self.deadline = crate::limits::Deadline::after(now, ACTIVATION_TIMEOUT).instant();
        let message = shepr_protocol::ClientMessage::PresentationSync(token);
        if endpoints.send_to(&lease.endpoint_id, &message) != EndpointSendOutcome::Sent {
            return Err("endpoint presentation effects fence could not be sent".into());
        }
        Ok(())
    }

    fn start_target_release(
        &mut self,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> Result<(), String> {
        let request_id = shepr_protocol::RequestId::from(format!(
            "client-shell-surface:{}:rollback-target-off",
            self.epoch
        ));
        let request = surface_interest_request(self.target.request_boot_id()?, &request_id, false);
        // Set the rollback phase before the potentially observed target-off write.
        self.phase = ActivationPhase::ReleasingTargetForRollback { request_id };
        self.deadline = crate::limits::Deadline::after(now, ACTIVATION_TIMEOUT).instant();
        if endpoints.send_to(&self.target.endpoint_id, &request) != EndpointSendOutcome::Sent {
            return Err("target endpoint release could not be sent".into());
        }
        Ok(())
    }

    fn start_source_restore(
        &mut self,
        endpoints: &mut EndpointRegistry,
        resize: &shepr_protocol::ClientMessage,
        now: Instant,
    ) -> Result<(), String> {
        let request_id = shepr_protocol::RequestId::from(format!(
            "client-shell-surface:{}:rollback-source-on",
            self.epoch
        ));
        // Source baseline writes can also be observed before their send reports an error.
        self.phase = ActivationPhase::RestoringSource {
            request_id: request_id.clone(),
            acknowledged_revision: None,
            evidence: ActivationEvidence::default(),
        };
        self.deadline = crate::limits::Deadline::after(now, ACTIVATION_TIMEOUT).instant();
        send_surface_activation(
            endpoints,
            &self.source,
            &request_id,
            resize,
            self.host_focused,
        )
    }

    fn send_latest_focus(&mut self, endpoints: &mut EndpointRegistry) -> Result<(), String> {
        let desired = self.focus.clone();
        let Some(desired) = desired else {
            if let ActivationPhase::ActivatingTarget {
                focus_request_id,
                focus_acknowledged,
                ..
            } = &mut self.phase
                && focus_request_id.is_none()
            {
                *focus_acknowledged = true;
            }
            return Ok(());
        };
        if !matches!(
            &self.phase,
            ActivationPhase::ActivatingTarget {
                focus_request_id: None,
                ..
            }
        ) {
            return Ok(());
        }
        // `desired` is `self.focus`, so this always yields an id.
        let Some(request_id) = self.next_focus_request_id() else {
            return Ok(());
        };
        if let ActivationPhase::ActivatingTarget {
            focus_request_id,
            focus_request_target,
            focus_acknowledged,
            ..
        } = &mut self.phase
        {
            *focus_acknowledged = false;
            *focus_request_id = Some(request_id.clone());
            *focus_request_target = Some(desired.clone());
        }
        let request = focus_request(self.target.request_boot_id()?, &request_id, &desired);
        if endpoints.send_to(&self.target.endpoint_id, &request) != EndpointSendOutcome::Sent {
            return Err("endpoint focus could not be sent".into());
        }
        Ok(())
    }

    fn next_focus_request_id(&mut self) -> Option<shepr_protocol::RequestId> {
        self.focus.as_ref()?;
        self.next_focus_serial = self.next_focus_serial.saturating_add(1);
        Some(
            format!(
                "client-shell-focus:{}:{}",
                self.epoch, self.next_focus_serial
            )
            .into(),
        )
    }

    fn progress(&self) -> SurfaceActivationProgress {
        match &self.phase {
            ActivationPhase::ActivatingTarget {
                acknowledged_revision,
                focus_acknowledged,
                evidence,
                ..
            } if acknowledged_revision.is_some_and(|revision| {
                *focus_acknowledged
                    && evidence
                        .coherent_surface(revision, self.geometry())
                        .is_some_and(|surface| self.target_matches(surface))
            }) =>
            {
                SurfaceActivationProgress::Ready
            }
            ActivationPhase::RestoringSource {
                acknowledged_revision,
                evidence,
                ..
            }
            | ActivationPhase::SynchronizingPresentation {
                acknowledged_revision,
                evidence,
                ..
            } if acknowledged_revision.is_some_and(|revision| {
                evidence
                    .coherent_surface(revision, self.geometry())
                    .is_some()
            }) =>
            {
                SurfaceActivationProgress::Ready
            }
            _ => SurfaceActivationProgress::Pending,
        }
    }

    fn target_matches(&self, surface: &shepr_protocol::PaneSurfaceFrame) -> bool {
        let evidence = match &self.phase {
            ActivationPhase::ActivatingTarget { evidence, .. } => evidence,
            _ => return false,
        };
        match &self.focus {
            Some(crate::shell::ClientEndpointFocusTarget::Pane(pane_id)) => {
                evidence.focused_pane_id.as_deref() == Some(pane_id)
                    && surface
                        .panes
                        .iter()
                        .any(|pane| pane.focused && &pane.pane_id == pane_id)
            }
            Some(crate::shell::ClientEndpointFocusTarget::Workspace(workspace_id)) => {
                evidence.focused_workspace_id.as_deref() == Some(workspace_id)
            }
            None => true,
        }
    }
}

#[cfg(test)]
#[path = "activation_tests.rs"]
mod tests;
