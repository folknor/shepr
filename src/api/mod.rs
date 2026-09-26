pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub use status::{RuntimeStatus, read_runtime_status_at};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::{Method, Request};

pub const SOCKET_PATH_ENV_VAR: &str = "SHEPR_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    // Keep this exhaustive: adding an API method must make its render impact
    // an explicit decision instead of silently defaulting to no UI change.
    match &request.method {
        Method::ServerReloadAgentManifests(_)
        | Method::ClientWindowTitleSet(_)
        | Method::ClientWindowTitleClear(_)
        | Method::ClientShellSurfaceSet(_)
        | Method::WorkspaceCreate(_)
        | Method::WorkspaceFocus(_)
        | Method::WorkspaceRename(_)
        | Method::WorkspaceMove(_)
        | Method::WorkspaceMoveBlock(_)
        | Method::WorkspaceReportMetadata(_)
        | Method::WorkspaceClose(_)
        | Method::TabCreate(_)
        | Method::TabFocus(_)
        | Method::TabRename(_)
        | Method::TabMove(_)
        | Method::TabClose(_)
        | Method::LayoutApply(_)
        | Method::LayoutSetSplitRatio(_)
        | Method::AgentRename(_)
        | Method::AgentFocus(_)
        | Method::AgentStart(_)
        | Method::AgentPrompt(_)
        | Method::AgentSendKeys(_)
        | Method::PaneSplit(_)
        | Method::PaneSwap(_)
        | Method::PaneMove(_)
        | Method::PaneZoom(_)
        | Method::PaneFocusDirection(_)
        | Method::PaneResize(_)
        | Method::PaneScroll(_)
        | Method::PaneClear(_)
        | Method::PaneFocus(_)
        | Method::PaneInputSet(_)
        | Method::PaneRename(_)
        | Method::PaneReportAgent(_)
        | Method::PaneReportAgentSession(_)
        | Method::PaneReportMetadata(_)
        | Method::PaneClearAgentAuthority(_)
        | Method::PaneReleaseAgent(_)
        | Method::PaneClose(_) => true,
        Method::Ping(_)
        | Method::ServerStop(_)
        | Method::ServerSshAgentRegister(_)
        | Method::ServerAgentManifests(_)
        | Method::SessionSnapshot(_)
        | Method::WorkspaceList(_)
        | Method::WorkspaceGet(_)
        | Method::TabList(_)
        | Method::TabGet(_)
        | Method::AgentList(_)
        | Method::AgentGet(_)
        | Method::AgentRead(_)
        | Method::AgentExplain(_)
        | Method::AgentWait(_)
        | Method::PaneLayout(_)
        | Method::PaneProcessInfo(_)
        | Method::LayoutExport(_)
        | Method::PaneNeighbor(_)
        | Method::PaneEdges(_)
        | Method::PaneSelectionRead(_)
        | Method::PaneCopyMotion(_)
        | Method::PaneCopySearch(_)
        | Method::PaneList(_)
        | Method::PaneCurrent(_)
        | Method::PaneGet(_)
        | Method::PaneSendText(_)
        | Method::PaneSendKeys(_)
        | Method::PaneSendInput(_)
        | Method::PaneRead(_)
        | Method::EventsSubscribe(_)
        | Method::EventsWait(_)
        | Method::PaneWaitForOutput(_) => false,
    }
}

pub(crate) fn serialize_response_or_error<T: serde::Serialize>(
    request_id: &str,
    response: &T,
) -> String {
    match serde_json::to_string(response) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(request_id, %error, "failed to serialize API response");
            // A string ID is itself infallibly serializable by serde_json;
            // retaining it keeps this fallback correlated and valid JSON.
            let encoded_id = serde_json::to_string(request_id).unwrap_or_else(|_| "\"\"".into());
            format!(
                r#"{{"id":{encoded_id},"error":{{"code":"serialization_error","message":"failed to serialize API response"}}}}"#
            )
        }
    }
}

pub(crate) fn send_api_response(
    respond_to: &std::sync::mpsc::Sender<String>,
    request_id: &str,
    method: &'static str,
    response: String,
) -> bool {
    if respond_to.send(response).is_err() {
        tracing::debug!(request_id, method, "API response receiver was dropped");
        false
    } else {
        true
    }
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingResponse;

    impl serde::Serialize for FailingResponse {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("forced test failure"))
        }
    }

    #[test]
    fn failed_response_encoding_preserves_request_id_and_returns_an_error() {
        let request_id = "request\nwith\"escapes";
        let encoded = serialize_response_or_error(request_id, &FailingResponse);
        let response: serde_json::Value =
            serde_json::from_str(&encoded).expect("fallback must be valid JSON");

        assert_eq!(response["id"], request_id);
        assert_eq!(response["error"]["code"], "serialization_error");
    }

    #[test]
    fn request_ui_changes_are_classified_explicitly() {
        let request = |method| Request {
            id: "test".into(),
            method,
        };

        assert!(request_changes_ui(&request(Method::ClientWindowTitleSet(
            crate::api::schema::ClientWindowTitleSetParams {
                title: "title".into(),
            },
        ))));
        assert!(request_changes_ui(&request(Method::PaneRename(
            crate::api::schema::PaneRenameParams {
                pane_id: "pane_1".into(),
                label: Some("name".into()),
            },
        ))));
        assert!(!request_changes_ui(&request(Method::PaneRead(
            crate::api::schema::PaneReadParams {
                pane_id: "pane_1".into(),
                source: crate::api::schema::ReadSource::Recent,
                format: crate::api::schema::ReadFormat::Text,
                lines: None,
                strip_ansi: false,
                intent: crate::api::schema::ReadIntent::Passive,
            },
        ))));
    }

    #[test]
    fn disconnected_api_response_receiver_is_detected() {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        drop(response_rx);

        assert!(!send_api_response(
            &respond_to,
            "request-1",
            "pane.read",
            "response".into(),
        ));
    }
}
