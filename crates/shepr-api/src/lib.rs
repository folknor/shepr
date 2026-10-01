pub mod client;
pub mod daemon_exit;
pub mod error;
pub mod guidance;
mod limits;
pub(crate) mod logging;
pub mod schema;
mod server;
pub mod server_stop;
mod status;
mod stop;

pub use limits::MAX_ACTIVE_CONNECTIONS;
pub use server::start_server;
pub use server::{ClientGate, ClientProtocolHandler, ConnectionSlot, ServerHandle};
pub use status::{RuntimeStatus, ServerPresence, read_server_presence_at};
pub use stop::ServerStopSignal;

use tokio::sync::mpsc;

use crate::schema::AppRequest;

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

/// One request handed from a socket connection thread to the app loop, with
/// the channel its answer goes back on.
pub struct ApiRequestMessage {
    pub request: AppRequest,
    pub respond_to: std::sync::mpsc::Sender<error::ApiResult>,
}

/// The bounded queue from socket connection threads to the app loop. A full
/// queue refuses the request with `server_unavailable` instead of waiting.
pub type ApiRequestSender = mpsc::Sender<ApiRequestMessage>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Method, Request};

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
            request(Method::PaneReportAgent(
                crate::schema::PaneReportAgentParams {
                    pane_id: "w1:p1".into(),
                    source: "shepr:pi".into(),
                    agent: "pi".into(),
                    state: crate::schema::PaneAgentState::Working,
                    seq: None,
                    agent_session_id: None,
                    agent_session_path: None,
                }
            ))
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
        assert!(report.mutates_ui);
        assert!(report.routine);

        let capture = Method::DetectCapture(crate::schema::PaneTarget {
            pane_id: "w1:p1".into(),
        })
        .traits();
        assert_eq!(capture.name, "detect.capture");
        assert!(!capture.mutates_ui);
        assert!(!capture.routine);

        let app_capture = crate::schema::AppMethod::DetectCapture(crate::schema::PaneTarget {
            pane_id: "w1:p1".into(),
        })
        .traits();
        assert_eq!(app_capture, capture);
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
