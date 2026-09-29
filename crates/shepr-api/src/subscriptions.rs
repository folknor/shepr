use crate::event_hub::EventHistoryError;
use crate::limits::APP_RESPONSE_TIMEOUT;
use crate::schema::{
    ErrorBody, ErrorResponse, EventKind, Method, PaneAgentStatusChangedEvent,
    PaneScrollChangedEvent, PaneScrollInfo, Request, Subscription, SubscriptionEventData,
    SubscriptionEventEnvelope, SubscriptionEventKind,
};
use crate::server::dispatch_to_app_with_timeout_result;
use crate::{ApiRequestSender, EventHub};

pub(super) struct ActiveAgentStatusChangedSubscription {
    pane_id: String,
    status_filter: Option<crate::schema::AgentStatus>,
    last_status: Option<crate::schema::AgentStatus>,
    last_presentation: Option<PanePresentationSnapshot>,
    last_sequence: u64,
    initial_event: Option<PaneAgentStatusChangedEvent>,
    request_prefix: String,
}

pub(super) struct ActiveScrollChangedSubscription {
    pane_id: String,
    last_scroll: Option<PaneScrollInfo>,
    request_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanePresentationSnapshot {
    title: Option<String>,
    display_agent: Option<String>,
}

impl PanePresentationSnapshot {
    fn from(pane: &crate::schema::PaneInfo) -> Self {
        Self {
            title: pane.title.clone(),
            display_agent: pane.display_agent.clone(),
        }
    }

    fn from_event(title: &Option<String>, display_agent: &Option<String>) -> Self {
        Self {
            title: title.clone(),
            display_agent: display_agent.clone(),
        }
    }
}

pub(super) struct ActiveEventSubscription {
    event_kind: crate::schema::EventKind,
    last_sequence: u64,
}

pub(super) enum ActiveSubscription {
    Event(ActiveEventSubscription),
    AgentStatusChanged(Box<ActiveAgentStatusChangedSubscription>),
    ScrollChanged(ActiveScrollChangedSubscription),
}

impl ActiveSubscription {
    pub(super) fn new(
        subscription: Subscription,
        request_id: &str,
        index: usize,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        event_start_sequence: u64,
    ) -> Result<Self, ErrorResponse> {
        let event_subscription = |event_kind| {
            Self::Event(ActiveEventSubscription {
                event_kind,
                last_sequence: event_start_sequence,
            })
        };

        match subscription {
            Subscription::WorkspaceCreated {} => {
                Ok(event_subscription(EventKind::WorkspaceCreated))
            }
            Subscription::WorkspaceMetadataUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceMetadataUpdated))
            }
            Subscription::WorkspaceRenamed {} => {
                Ok(event_subscription(EventKind::WorkspaceRenamed))
            }
            Subscription::WorkspaceMoved {} => Ok(event_subscription(EventKind::WorkspaceMoved)),
            Subscription::WorkspaceReordered {} => {
                Ok(event_subscription(EventKind::WorkspaceReordered))
            }
            Subscription::WorkspaceClosed {} => Ok(event_subscription(EventKind::WorkspaceClosed)),
            Subscription::WorkspaceFocused {} => {
                Ok(event_subscription(EventKind::WorkspaceFocused))
            }
            Subscription::TabCreated {} => Ok(event_subscription(EventKind::TabCreated)),
            Subscription::TabClosed {} => Ok(event_subscription(EventKind::TabClosed)),
            Subscription::TabFocused {} => Ok(event_subscription(EventKind::TabFocused)),
            Subscription::TabRenamed {} => Ok(event_subscription(EventKind::TabRenamed)),
            Subscription::TabMoved {} => Ok(event_subscription(EventKind::TabMoved)),
            Subscription::PaneCreated {} => Ok(event_subscription(EventKind::PaneCreated)),
            Subscription::PaneClosed {} => Ok(event_subscription(EventKind::PaneClosed)),
            Subscription::PaneUpdated {} => Ok(event_subscription(EventKind::PaneUpdated)),
            Subscription::PaneFocused {} => Ok(event_subscription(EventKind::PaneFocused)),
            Subscription::PaneMoved {} => Ok(event_subscription(EventKind::PaneMoved)),
            Subscription::PaneExited {} => Ok(event_subscription(EventKind::PaneExited)),
            Subscription::PaneAgentDetected {} => {
                Ok(event_subscription(EventKind::PaneAgentDetected))
            }
            Subscription::LayoutUpdated {} => Ok(event_subscription(EventKind::LayoutUpdated)),
            Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status,
            } => {
                let last_sequence = event_hub.current_sequence();
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;
                let last_status = probe.agent_status;
                let last_presentation = PanePresentationSnapshot::from(&probe);
                let initial_event = agent_status
                    .is_some_and(|wanted| wanted == probe.agent_status)
                    .then_some(PaneAgentStatusChangedEvent {
                        pane_id: probe.pane_id.clone(),
                        workspace_id: probe.workspace_id,
                        agent_status: probe.agent_status,
                        agent: probe.agent,
                        title: probe.title,
                        display_agent: probe.display_agent,
                    });

                Ok(Self::AgentStatusChanged(Box::new(
                    ActiveAgentStatusChangedSubscription {
                        pane_id: probe.pane_id,
                        status_filter: agent_status,
                        last_status: Some(last_status),
                        last_presentation: Some(last_presentation),
                        last_sequence,
                        initial_event,
                        request_prefix: format!("{request_id}:sub:{index}"),
                    },
                )))
            }
            Subscription::PaneScrollChanged { pane_id } => {
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;

                Ok(Self::ScrollChanged(ActiveScrollChangedSubscription {
                    pane_id: probe.pane_id,
                    last_scroll: probe.scroll,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
        }
    }

    /// The next matching event, if any. Errors are final: a subscription whose
    /// pane closed or moved (its public id changes with the workspace), or
    /// whose event history was lost, can never deliver again, so it reports
    /// that instead of going silent while still polling the app.
    pub(super) fn poll_for_wait(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<serde_json::Value>, ErrorBody> {
        let event = match self {
            Self::Event(subscription) => return subscription.poll(event_hub),
            Self::AgentStatusChanged(subscription) => {
                subscription.poll_result(api_tx, event_hub)?
            }
            Self::ScrollChanged(subscription) => subscription.poll(api_tx)?,
        };
        event
            .map(|event| serde_json::to_value(event).map_err(|err| event_encoding_error(&err)))
            .transpose()
    }

    /// The hub sequence this subscription has consumed history through, or
    /// `None` for subscriptions that only sample current state.
    fn history_cursor(&self) -> Option<u64> {
        match self {
            Self::Event(subscription) => Some(subscription.last_sequence),
            Self::AgentStatusChanged(subscription) => Some(subscription.last_sequence),
            Self::ScrollChanged(_) => None,
        }
    }

    /// Offers one hub event. Events at or before this subscription's own
    /// cursor were already consumed (or predate it) and are skipped; later
    /// ones advance the cursor whether or not they match.
    fn offer_history(
        &mut self,
        sequence: u64,
        event: &crate::schema::EventEnvelope,
    ) -> Result<Option<serde_json::Value>, ErrorBody> {
        match self {
            Self::Event(subscription) => {
                if sequence <= subscription.last_sequence {
                    return Ok(None);
                }
                subscription.last_sequence = sequence;
                if event.data.kind() != subscription.event_kind {
                    return Ok(None);
                }
                serde_json::to_value(event)
                    .map(Some)
                    .map_err(|err| event_encoding_error(&err))
            }
            Self::AgentStatusChanged(subscription) => {
                if sequence <= subscription.last_sequence {
                    return Ok(None);
                }
                subscription.last_sequence = sequence;
                if event.data.kind() != EventKind::PaneAgentStatusChanged {
                    return Ok(None);
                }
                subscription
                    .event_from_history(event.clone())
                    .map(|event| {
                        serde_json::to_value(event).map_err(|err| event_encoding_error(&err))
                    })
                    .transpose()
            }
            Self::ScrollChanged(_) => Ok(None),
        }
    }

    /// Samples current state for subscriptions that are not (only) history
    /// backed. Called after history has been offered for this poll.
    fn poll_sampled(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<serde_json::Value>, ErrorBody> {
        let event = match self {
            Self::Event(_) => return Ok(None),
            Self::AgentStatusChanged(subscription) => {
                subscription.poll_snapshot(api_tx, event_hub)?
            }
            Self::ScrollChanged(subscription) => subscription.poll(api_tx)?,
        };
        event
            .map(|event| serde_json::to_value(event).map_err(|err| event_encoding_error(&err)))
            .transpose()
    }
}

/// Field carrying the event hub's sequence number on every streamed event.
const STREAM_SEQUENCE_FIELD: &str = "seq";

/// The subscriptions of one `events.subscribe` stream, polled together so
/// that events reach the client in the hub's global order.
///
/// Draining each subscription's history in turn reorders events that land in
/// one poll window: with `[pane.closed, pane.created]`, a create followed by a
/// close would arrive as closed, then created. Instead the history is fetched
/// once, from the oldest subscription cursor, and every event is offered to
/// each subscription (in request order) before the next event.
///
/// Every line carries the hub sequence as `seq`. History events carry their
/// own sequence, which is strictly increasing across distinct hub events.
/// Sampled events (`pane.scroll_changed` and the snapshot-derived
/// `pane.agent_status_changed`) are not hub events: they follow all history
/// delivered in the same poll and carry the sequence of the last hub event
/// delivered before them, so `seq` never decreases along a stream.
pub(super) struct SubscriptionStream {
    subscriptions: Vec<ActiveSubscription>,
    delivered_through: u64,
}

/// One poll's output. `events` are in delivery order; an `error` is final and
/// goes out after them.
pub(super) struct SubscriptionPoll {
    pub(super) events: Vec<serde_json::Value>,
    pub(super) error: Option<ErrorBody>,
}

impl SubscriptionStream {
    pub(super) fn new(subscriptions: Vec<ActiveSubscription>, start_sequence: u64) -> Self {
        Self {
            subscriptions,
            delivered_through: start_sequence,
        }
    }

    pub(super) fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> SubscriptionPoll {
        let mut events = Vec::new();
        let error = self.poll_into(api_tx, event_hub, &mut events).err();
        SubscriptionPoll { events, error }
    }

    fn poll_into(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        events: &mut Vec<serde_json::Value>,
    ) -> Result<(), ErrorBody> {
        let mut history_matched = vec![false; self.subscriptions.len()];
        let cursor = self
            .subscriptions
            .iter()
            .filter_map(ActiveSubscription::history_cursor)
            .min();
        if let Some(cursor) = cursor {
            for (sequence, event) in subscription_events_after(event_hub, cursor)? {
                for (subscription, matched) in self
                    .subscriptions
                    .iter_mut()
                    .zip(history_matched.iter_mut())
                {
                    if let Some(value) = subscription.offer_history(sequence, &event)? {
                        *matched = true;
                        events.push(with_stream_sequence(value, sequence));
                    }
                }
                self.delivered_through = self.delivered_through.max(sequence);
            }
        }
        // A status subscription that delivered history this poll skips its
        // snapshot, as the history already reflects the newest state.
        for (subscription, matched) in self.subscriptions.iter_mut().zip(history_matched) {
            if matched {
                continue;
            }
            if let Some(value) = subscription.poll_sampled(api_tx, event_hub)? {
                events.push(with_stream_sequence(value, self.delivered_through));
            }
        }
        Ok(())
    }
}

fn with_stream_sequence(mut value: serde_json::Value, sequence: u64) -> serde_json::Value {
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(STREAM_SEQUENCE_FIELD.into(), sequence.into());
    }
    value
}

pub(super) fn subscription_events_after(
    event_hub: &EventHub,
    sequence: u64,
) -> Result<Vec<(u64, crate::schema::EventEnvelope)>, ErrorBody> {
    event_hub.events_after_checked(sequence).map_err(|error| match error {
        EventHistoryError::Lost => crate::error::ApiError::new(
            crate::error::ApiErrorCode::EventsLost,
            "event subscription fell behind retained history; resubscribe and resync with session.snapshot",
        ).into_body(),
        EventHistoryError::Unavailable => crate::error::ApiError::new(
            crate::error::ApiErrorCode::ServerUnavailable,
            "event history is unavailable",
        ).into_body(),
    })
}

fn event_encoding_error(error: &serde_json::Error) -> ErrorBody {
    crate::error::ApiError::new(
        crate::error::ApiErrorCode::InternalError,
        format!("failed to encode subscription event: {error}"),
    )
    .into_body()
}

impl ActiveEventSubscription {
    fn poll(&mut self, event_hub: &EventHub) -> Result<Option<serde_json::Value>, ErrorBody> {
        for (sequence, event) in subscription_events_after(event_hub, self.last_sequence)? {
            self.last_sequence = sequence;
            if event.data.kind() == self.event_kind {
                return serde_json::to_value(event)
                    .map(Some)
                    .map_err(|err| event_encoding_error(&err));
            }
        }
        Ok(None)
    }
}

impl ActiveAgentStatusChangedSubscription {
    fn poll_result(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorBody> {
        for (sequence, event) in subscription_events_after(event_hub, self.last_sequence)? {
            self.last_sequence = sequence;
            if let Some(event) = self.event_from_history(event) {
                return Ok(Some(event));
            }
        }

        self.poll_snapshot(api_tx, event_hub)
    }

    fn event_from_history(
        &mut self,
        event: crate::schema::EventEnvelope,
    ) -> Option<SubscriptionEventEnvelope> {
        if event.data.kind() != EventKind::PaneAgentStatusChanged {
            return None;
        }
        let crate::schema::EventData::PaneAgentStatusChanged {
            pane_id,
            workspace_id,
            agent_status,
            agent,
            title,
            display_agent,
        } = event.data
        else {
            return None;
        };
        if pane_id != self.pane_id {
            return None;
        }
        self.last_status = Some(agent_status);
        self.last_presentation = Some(PanePresentationSnapshot::from_event(&title, &display_agent));
        self.initial_event = None;
        if self
            .status_filter
            .is_some_and(|wanted| wanted != agent_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
            }),
        })
    }

    fn poll_snapshot(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorBody> {
        if event_hub.current_sequence() != self.last_sequence {
            return Ok(None);
        } else if let Some(event) = self.initial_event.take() {
            return Ok(Some(SubscriptionEventEnvelope {
                event: SubscriptionEventKind::PaneAgentStatusChanged,
                data: SubscriptionEventData::PaneAgentStatusChanged(event),
            }));
        }

        let before_snapshot_sequence = self.last_sequence;
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        );
        let after_snapshot_sequence = event_hub.current_sequence();
        if after_snapshot_sequence != before_snapshot_sequence {
            return Ok(None);
        }
        let pane = pane.map_err(|response| response.error)?;

        let event = self.event_from_snapshot(pane);
        if event.is_some() {
            self.last_sequence = after_snapshot_sequence;
        }
        Ok(event)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let current_status = pane.agent_status;
        let current_presentation = PanePresentationSnapshot::from(&pane);
        let previous_status = self.last_status.replace(current_status);
        let previous_presentation = self.last_presentation.replace(current_presentation.clone());
        let presentation_changed = previous_presentation
            .as_ref()
            .is_some_and(|previous| previous != &current_presentation);
        let status_changed = previous_status.is_some_and(|previous| previous != current_status);
        if !(status_changed || presentation_changed) {
            return None;
        }
        if self
            .status_filter
            .is_some_and(|wanted| wanted != current_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                agent_status: current_status,
                agent: pane.agent,
                title: pane.title,
                display_agent: pane.display_agent,
            }),
        })
    }
}

impl ActiveScrollChangedSubscription {
    fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorBody> {
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .map_err(|response| response.error)?;
        Ok(self.event_from_snapshot(pane))
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let scroll = pane.scroll;
        if self.last_scroll == scroll {
            return None;
        }
        self.last_scroll = scroll;
        let scroll = scroll?;

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                scroll,
            }),
        })
    }
}

fn pane_get(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<crate::schema::PaneInfo, ErrorResponse> {
    let response = dispatch_to_app_with_timeout_result(
        Request {
            id: request_id.clone(),
            method: Method::PaneGet(crate::schema::PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    match response {
        Ok(crate::schema::ResponseResult::PaneInfo { pane }) => Ok(pane),
        Err(error) => Err(ErrorResponse {
            id: request_id,
            error: error.into_body(),
        }),
        Ok(_) => Err(ErrorResponse {
            id: request_id,
            error: crate::error::ApiError::new(
                crate::error::ApiErrorCode::InternalError,
                "app returned an unexpected pane get result",
            )
            .into_body(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::schema::{AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo};

    fn presentation_event(title: Option<&str>) -> EventEnvelope {
        EventEnvelope {
            data: EventData::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: title.map(str::to_string),
                display_agent: None,
            },
        }
    }

    fn workspace_focused_event(workspace_id: &str) -> EventEnvelope {
        EventEnvelope {
            data: EventData::WorkspaceFocused {
                workspace_id: workspace_id.into(),
            },
        }
    }

    fn pane_info_with_scroll(scroll: Option<PaneScrollInfo>) -> PaneInfo {
        PaneInfo {
            pane_id: "pane_1".into(),
            terminal_id: "terminal_1".into(),
            workspace_id: "workspace_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Idle,
            tokens: HashMap::new(),
            agent_session: None,
            scroll,
            revision: 0,
        }
    }

    /// An app stand-in that answers every request with `respond`.
    fn answering_app(
        respond: impl Fn(&Request) -> crate::error::ApiResult + Send + 'static,
    ) -> ApiRequestSender {
        let (api_tx, mut api_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::ApiRequestMessage>();
        std::thread::spawn(move || {
            while let Some(message) = api_rx.blocking_recv() {
                // Like the real app (`send_api_response`), a requester that stopped
                // waiting is not the stand-in's failure; the test asserts on what
                // the requester saw.
                drop(message.respond_to.send(respond(&message.request)));
            }
        });
        api_tx
    }

    fn pane_not_found_app() -> ApiRequestSender {
        answering_app(|_request| {
            Err(crate::error::ApiError::new(
                crate::error::ApiErrorCode::PaneNotFound,
                "pane gone",
            ))
        })
    }

    fn overflow_history(event_hub: &EventHub) {
        for index in 0..600 {
            event_hub.push(workspace_focused_event(&format!("overflow_{index}")));
        }
    }

    #[test]
    fn sampling_subscriptions_report_a_vanished_pane_instead_of_going_silent() {
        let event_hub = EventHub::default();
        let api_tx = pane_not_found_app();
        let mut subscriptions = [
            ActiveSubscription::ScrollChanged(ActiveScrollChangedSubscription {
                pane_id: "pane_1".into(),
                last_scroll: None,
                request_prefix: "scroll".into(),
            }),
            ActiveSubscription::AgentStatusChanged(Box::new(
                ActiveAgentStatusChangedSubscription {
                    pane_id: "pane_1".into(),
                    status_filter: None,
                    last_status: Some(AgentStatus::Working),
                    last_presentation: None,
                    last_sequence: event_hub.current_sequence(),
                    initial_event: None,
                    request_prefix: "status".into(),
                },
            )),
        ];
        for subscription in &mut subscriptions {
            let error = subscription
                .poll_for_wait(&api_tx, &event_hub)
                .expect_err("a vanished pane must end the wait");
            assert_eq!(error.code, "pane_not_found");
        }
        for subscription in subscriptions {
            let mut stream = stream_of(subscription, &event_hub);
            let poll = stream.poll(&api_tx, &event_hub);
            assert!(poll.events.is_empty());
            let error = poll
                .error
                .expect("a vanished pane must end the subscription");
            assert_eq!(error.code, "pane_not_found");
        }
    }

    fn stream_of(subscription: ActiveSubscription, event_hub: &EventHub) -> SubscriptionStream {
        let start = subscription
            .history_cursor()
            .unwrap_or_else(|| event_hub.current_sequence());
        SubscriptionStream::new(vec![subscription], start)
    }

    fn poll_stream(
        stream: &mut SubscriptionStream,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Vec<serde_json::Value> {
        let poll = stream.poll(api_tx, event_hub);
        assert!(poll.error.is_none(), "{:?}", poll.error);
        poll.events
    }

    fn pane_lifecycle_event(kind: EventKind) -> EventEnvelope {
        let data = match kind {
            EventKind::PaneCreated => EventData::PaneCreated {
                pane: pane_info_with_scroll(None),
            },
            EventKind::PaneClosed => EventData::PaneClosed {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
            },
            other => panic!("not a pane lifecycle kind: {other:?}"),
        };
        EventEnvelope { data }
    }

    #[test]
    fn stream_delivers_events_across_subscriptions_in_hub_order() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let start = event_hub.current_sequence();
        // Subscription order is the reverse of the order events happen in.
        let subscriptions = [Subscription::PaneClosed {}, Subscription::PaneCreated {}]
            .into_iter()
            .enumerate()
            .map(|(index, subscription)| {
                ActiveSubscription::new(subscription, "order", index, &api_tx, &event_hub, start)
                    .expect("test precondition")
            })
            .collect();
        let mut stream = SubscriptionStream::new(subscriptions, start);

        event_hub.push(pane_lifecycle_event(EventKind::PaneCreated));
        event_hub.push(workspace_focused_event("unsubscribed"));
        event_hub.push(pane_lifecycle_event(EventKind::PaneClosed));

        let events = poll_stream(&mut stream, &api_tx, &event_hub);
        let kinds = events
            .iter()
            .map(|event| event["data"]["type"].as_str().expect("test precondition"))
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["pane_created", "pane_closed"]);
        let sequences = events
            .iter()
            .map(|event| {
                event[STREAM_SEQUENCE_FIELD]
                    .as_u64()
                    .expect("sequence on the wire")
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, [start + 1, start + 3]);
        assert!(poll_stream(&mut stream, &api_tx, &event_hub).is_empty());
    }

    #[test]
    fn stream_sequences_never_decrease_across_history_and_samples() {
        let event_hub = EventHub::default();
        let api_tx = answering_app(|_request| {
            let pane = pane_info_with_scroll(Some(PaneScrollInfo {
                offset_from_bottom: 3,
                max_offset_from_bottom: 10,
                viewport_rows: 5,
            }));
            Ok(crate::schema::ResponseResult::PaneInfo { pane })
        });
        let start = event_hub.current_sequence();
        let focused = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "mixed",
            1,
            &api_tx,
            &event_hub,
            start,
        )
        .expect("test precondition");
        let scroll = ActiveSubscription::ScrollChanged(ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: None,
            request_prefix: "mixed:sub:0".into(),
        });
        // The sampled subscription comes first in request order, yet its
        // event follows the history delivered in the same poll.
        let mut stream = SubscriptionStream::new(vec![scroll, focused], start);
        event_hub.push(workspace_focused_event("first"));
        event_hub.push(workspace_focused_event("second"));

        let events = poll_stream(&mut stream, &api_tx, &event_hub);
        let summary = events
            .iter()
            .map(|event| {
                (
                    event["event"]
                        .as_str()
                        .or_else(|| event["data"]["type"].as_str())
                        .expect("test precondition")
                        .to_string(),
                    event[STREAM_SEQUENCE_FIELD]
                        .as_u64()
                        .expect("sequence on the wire"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("workspace_focused".to_string(), start + 1),
                ("workspace_focused".to_string(), start + 2),
                ("pane.scroll_changed".to_string(), start + 2),
            ]
        );
    }

    #[test]
    fn stream_emits_history_before_a_final_sampling_error() {
        let event_hub = EventHub::default();
        let api_tx = pane_not_found_app();
        let start = event_hub.current_sequence();
        let focused = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "fail",
            0,
            &api_tx,
            &event_hub,
            start,
        )
        .expect("test precondition");
        let scroll = ActiveSubscription::ScrollChanged(ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: None,
            request_prefix: "fail:sub:1".into(),
        });
        let mut stream = SubscriptionStream::new(vec![focused, scroll], start);
        event_hub.push(workspace_focused_event("before_close"));

        let poll = stream.poll(&api_tx, &event_hub);
        assert_eq!(poll.events.len(), 1);
        assert_eq!(poll.events[0]["data"]["workspace_id"], "before_close");
        assert_eq!(
            poll.error.expect("sampling failure ends the stream").code,
            "pane_not_found"
        );
    }

    #[test]
    fn history_backed_polls_report_lost_events() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "lost",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("test precondition");
        let mut status = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: None,
            last_sequence: event_hub.current_sequence(),
            initial_event: None,
            request_prefix: "lost".into(),
        };
        overflow_history(&event_hub);

        let error = lifecycle
            .poll_for_wait(&api_tx, &event_hub)
            .expect_err("lost history must be reported");
        assert_eq!(error.code, "events_lost");
        let error = status
            .poll_result(&api_tx, &event_hub)
            .expect_err("lost history must be reported");
        assert_eq!(error.code, "events_lost");
    }

    #[test]
    fn lifecycle_subscription_skips_history_but_keeps_setup_window_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("before_subscription"));
        let event_start_sequence = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("during_setup"));

        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_start_sequence,
        )
        .expect("workspace focus subscription");

        let setup_event = subscription
            .poll_for_wait(&api_tx, &event_hub)
            .expect("history poll succeeds")
            .expect("setup-window event");
        assert_eq!(setup_event["data"]["workspace_id"], "during_setup");
        assert!(
            subscription
                .poll_for_wait(&api_tx, &event_hub)
                .expect("history poll succeeds")
                .is_none()
        );

        event_hub.push(workspace_focused_event("after_setup"));
        let live_event = subscription
            .poll_for_wait(&api_tx, &event_hub)
            .expect("history poll succeeds")
            .expect("live event");
        assert_eq!(live_event["data"]["workspace_id"], "after_setup");
    }

    #[test]
    fn workspace_metadata_subscription_uses_dedicated_event_kind() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::WorkspaceMetadataUpdated {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace metadata subscription");

        assert!(matches!(
            subscription,
            ActiveSubscription::Event(ActiveEventSubscription {
                event_kind: EventKind::WorkspaceMetadataUpdated,
                ..
            })
        ));
    }

    #[test]
    fn lifecycle_batch_drains_in_order_and_advances_past_unmatched_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("old"));
        let start = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("setup"));
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "batch",
            0,
            &api_tx,
            &event_hub,
            start,
        )
        .expect("test precondition");
        event_hub.push(presentation_event(None));
        event_hub.push(workspace_focused_event("live"));
        let mut stream = SubscriptionStream::new(vec![subscription], start);
        let events = poll_stream(&mut stream, &api_tx, &event_hub);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["data"]["workspace_id"], "setup");
        assert_eq!(events[1]["data"]["workspace_id"], "live");
        assert!(poll_stream(&mut stream, &api_tx, &event_hub).is_empty());
        let Some(ActiveSubscription::Event(subscription)) = stream.subscriptions.pop() else {
            panic!("expected lifecycle subscription");
        };
        assert_eq!(subscription.last_sequence, event_hub.current_sequence());
    }

    #[test]
    fn agent_status_batch_preserves_transitions_filters_and_initial_state_ordering() {
        for filtered in [false, true] {
            let event_hub = EventHub::default();
            let subscription = ActiveSubscription::AgentStatusChanged(Box::new(
                ActiveAgentStatusChangedSubscription {
                    pane_id: "pane_1".into(),
                    status_filter: filtered.then_some(AgentStatus::Working),
                    last_status: Some(AgentStatus::Working),
                    last_presentation: None,
                    last_sequence: event_hub.current_sequence(),
                    initial_event: Some(PaneAgentStatusChangedEvent {
                        pane_id: "pane_1".into(),
                        workspace_id: "workspace_1".into(),
                        agent_status: AgentStatus::Working,
                        agent: Some("pi".into()),
                        title: Some("stale initial snapshot".into()),
                        display_agent: None,
                    }),
                    request_prefix: "batch".into(),
                },
            ));
            for (status, title) in [
                (AgentStatus::Working, "started"),
                (AgentStatus::Blocked, "approval"),
                (AgentStatus::Idle, "finished"),
                (AgentStatus::Working, "restarted"),
            ] {
                let mut event = presentation_event(Some(title));
                let EventData::PaneAgentStatusChanged { agent_status, .. } = &mut event.data else {
                    panic!("expected status data");
                };
                *agent_status = status;
                event_hub.push(event);
            }
            let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut stream = stream_of(subscription, &event_hub);
            let events = poll_stream(&mut stream, &api_tx, &event_hub);
            let titles = events
                .iter()
                .map(|event| event["data"]["title"].as_str().expect("test precondition"))
                .collect::<Vec<_>>();
            assert_eq!(
                titles,
                if filtered {
                    vec!["started", "restarted"]
                } else {
                    vec!["started", "approval", "finished", "restarted"]
                }
            );
            let Some(ActiveSubscription::AgentStatusChanged(subscription)) =
                stream.subscriptions.pop()
            else {
                panic!("expected agent subscription");
            };
            assert_eq!(subscription.last_sequence, event_hub.current_sequence());
            assert!(subscription.initial_event.is_none());
        }
    }

    #[test]
    fn scroll_subscription_emits_when_scroll_snapshot_changes() {
        let at_bottom = PaneScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let scrolled_back = PaneScrollInfo {
            offset_from_bottom: 8,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let mut subscription = ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: Some(at_bottom),
            request_prefix: "test".into(),
        };

        assert!(
            subscription
                .event_from_snapshot(pane_info_with_scroll(Some(at_bottom)))
                .is_none()
        );

        let event = subscription
            .event_from_snapshot(pane_info_with_scroll(Some(scrolled_back)))
            .expect("scroll event");
        assert_eq!(event.event, SubscriptionEventKind::ScrollChanged);
        let SubscriptionEventData::ScrollChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.pane_id, "pane_1");
        assert_eq!(data.workspace_id, "workspace_1");
        assert_eq!(data.scroll, scrolled_back);
    }

    #[test]
    fn agent_status_subscription_replays_queued_metadata_set_and_expiry_events() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: None,
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("history poll succeeds")
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("history poll succeeds")
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_prefers_setup_window_events_over_initial_snapshot() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: None,
                display_agent: None,
            }),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("history poll succeeds")
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("history poll succeeds")
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_emits_setup_window_event_already_reflected_by_probe() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: Some("short lived".into()),
                display_agent: None,
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
            }),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));

        let event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("history poll succeeds")
            .expect("setup-window event");
        let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.title.as_deref(), Some("short lived"));
        assert!(subscription.initial_event.is_none());
    }
}
