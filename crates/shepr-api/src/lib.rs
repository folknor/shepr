pub mod client;
pub mod error;
pub mod guidance;
pub mod launch_env;
mod limits;
pub(crate) mod logging;
pub mod schema;
mod server;
pub mod session;
mod status;
mod stop;

pub use server::ServerHandle;
pub use server::{api_method_name, start_server_with_stop_control};
pub use status::{RuntimeStatus, read_runtime_status_at};
pub use stop::ServerStopSignal;

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::schema::Request;

pub fn serialize_response_or_error<T: serde::Serialize>(request_id: &str, response: &T) -> String {
    serialize_response_or_error_with_outcome(request_id, response).body
}

pub(crate) fn serialize_response_or_error_with_outcome<T: serde::Serialize>(
    request_id: &str,
    response: &T,
) -> error::EncodedApiResponse {
    match serde_json::to_string(response) {
        Ok(body) => error::EncodedApiResponse {
            body,
            outcome: error::ApiLogOutcome::Ok,
        },
        Err(error) => {
            tracing::error!(request_id, %error, "failed to serialize API response");
            error::EncodedApiResponse {
                body: serde_json::json!({
                    "id": request_id,
                    "error": {
                        "code": error::ApiErrorCode::SerializationError.as_str(),
                        "message": "failed to serialize API response",
                    },
                })
                .to_string(),
                outcome: error::ApiLogOutcome::Error,
            }
        }
    }
}

pub fn send_api_response(
    respond_to: &std::sync::mpsc::Sender<error::ApiResult>,
    request_id: &str,
    method: &'static str,
    response: error::ApiResult,
) {
    if respond_to.send(response).is_err() {
        tracing::debug!(request_id, method, "API response receiver was dropped");
    }
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<error::ApiResult>,
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path(paths: &shepr_config::AppPaths) -> PathBuf {
    crate::session::active_api_socket_path(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Method;

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

        assert!(
            request(Method::PaneRename(crate::schema::PaneRenameParams {
                pane_id: "w1:p1".into(),
                label: Some("name".into()),
            },))
            .method
            .traits()
            .mutates_ui
        );
        assert!(
            !request(Method::DetectCapture(crate::schema::PaneTarget {
                pane_id: "w1:p1".into(),
            },))
            .method
            .traits()
            .mutates_ui
        );
    }

    #[test]
    fn method_traits_carry_routing_and_log_facts() {
        let ping = Method::Ping(crate::schema::PingParams::default()).traits();
        assert_eq!(ping.name, "ping");
        assert!(ping.runs_on_socket_thread);
        assert!(!ping.mutates_ui);
        assert!(!ping.routine);

        let report = Method::PaneReportAgentSession(crate::schema::PaneReportAgentSessionParams {
            pane_id: "w1:p1".into(),
            source: "test".into(),
            agent: "pi".into(),
            seq: None,
            agent_session_id: None,
            agent_session_path: None,
            session_start_source: None,
        })
        .traits();
        assert_eq!(report.name, "pane.report_agent_session");
        assert!(!report.runs_on_socket_thread);
        assert!(report.mutates_ui);
        assert!(report.routine);

        let pane_clear = Method::PaneClear(crate::schema::PaneTarget {
            pane_id: "w1:p1".into(),
        })
        .traits();
        assert_eq!(pane_clear.name, "pane.clear");
        assert!(!pane_clear.runs_on_socket_thread);
        assert!(pane_clear.mutates_ui);
        assert!(!pane_clear.routine);
    }

    #[test]
    fn api_response_is_sent_to_its_receiver() {
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        send_api_response(
            &respond_to,
            "request-1",
            "detect.capture",
            Ok(schema::ResponseResult::Ok {}),
        );
        assert_eq!(
            response_rx.recv().expect("response was sent"),
            Ok(schema::ResponseResult::Ok {})
        );
    }
}
