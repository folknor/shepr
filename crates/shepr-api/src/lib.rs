pub mod client;
pub mod error;
mod event_hub;
pub mod guidance;
pub mod launch_env;
pub mod schema;
mod server;
pub mod session;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::{EventHistoryError, EventHub};
pub use server::ServerHandle;
pub use server::{api_method_name, start_server_with_stop_control};
pub use status::{RuntimeStatus, read_runtime_status_at};

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
            request(Method::ClientWindowTitleSet(
                crate::schema::ClientWindowTitleSetParams {
                    title: "title".into(),
                },
            ))
            .method
            .traits()
            .mutates_ui
        );
        assert!(
            request(Method::PaneRename(crate::schema::PaneRenameParams {
                pane_id: "pane_1".into(),
                label: Some("name".into()),
            },))
            .method
            .traits()
            .mutates_ui
        );
        assert!(
            !request(Method::PaneRead(crate::schema::PaneReadParams {
                pane_id: "pane_1".into(),
                source: crate::schema::ReadSource::Recent,
                format: crate::schema::ReadFormat::Text,
                lines: None,
                strip_ansi: false,
                intent: crate::schema::ReadIntent::Passive,
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

        let pane_get = Method::PaneGet(crate::schema::PaneTarget {
            pane_id: "pane_1".into(),
        })
        .traits();
        assert_eq!(pane_get.name, "pane.get");
        assert!(!pane_get.runs_on_socket_thread);
        assert!(!pane_get.mutates_ui);
        assert!(pane_get.routine);

        let title_clear =
            Method::ClientWindowTitleClear(crate::schema::EmptyParams::default()).traits();
        assert_eq!(title_clear.name, "client.window_title.clear");
        assert!(!title_clear.runs_on_socket_thread);
        assert!(title_clear.mutates_ui);
        assert!(!title_clear.routine);
    }

    #[test]
    fn api_response_is_sent_to_its_receiver() {
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        send_api_response(
            &respond_to,
            "request-1",
            "pane.read",
            Ok(schema::ResponseResult::Ok {}),
        );
        assert_eq!(
            response_rx.recv().expect("response was sent"),
            Ok(schema::ResponseResult::Ok {})
        );
    }
}
