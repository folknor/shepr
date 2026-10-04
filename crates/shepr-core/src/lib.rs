pub mod absolute_path;
pub mod agent_session;
pub mod agent_state;
pub mod chrome;
pub mod env;
pub mod geometry;
pub mod layout;
pub mod limits;
pub mod locks;
pub mod pathutil;
pub mod scrollback;
pub mod shell;
pub mod shell_quote;
pub mod socket_path;

pub mod workspace_label;

/// The text of a panic payload, or `fallback` when the payload is not a
/// string. Only string payloads are read: formatting any other type could run
/// arbitrary code, which a panic hook must not do.
pub fn panic_message<'a>(payload: &'a (dyn std::any::Any + Send), fallback: &'a str) -> &'a str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        fallback
    }
}
