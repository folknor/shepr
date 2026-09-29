use serde::{Deserialize, Serialize};

pub mod agents;
pub mod common;
pub mod panes;
pub mod response;
pub mod server;
pub mod session;
pub mod tabs;
pub mod workspaces;

pub use agents::*;
pub use common::*;
pub use panes::*;
pub use response::*;
pub use server::*;
pub use session::*;
pub use tabs::*;
pub use workspaces::*;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

/// Facts about one API method, kept together so request handling, rendering
/// and logging share one exhaustive classification. The JSON API is the
/// socket's vocabulary only; a client shell asks through
/// `shepr_protocol::command::EndpointCommand` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodTraits {
    pub name: &'static str,
    pub mutates_ui: bool,
    pub routine: bool,
}

/// A request the app loop answers: the socket thread answers `ping` and
/// `server.stop` itself and hands every other method to the app as this.
/// Not a wire type; the socket thread builds it from a decoded [`Request`].
#[derive(Debug, Clone, PartialEq)]
pub struct AppRequest {
    pub id: String,
    pub method: AppMethod,
}

/// The methods the app loop answers, a subset of [`Method`] with no arm for
/// the ones the socket thread keeps.
#[derive(Debug, Clone, PartialEq)]
pub enum AppMethod {
    DetectCapture(PaneTarget),
    DetectExplain(PaneTarget),
    PaneReportAgent(PaneReportAgentParams),
    PaneReportAgentSession(PaneReportAgentSessionParams),
}

impl AppMethod {
    pub fn traits(&self) -> MethodTraits {
        match self {
            Self::DetectCapture(_) => MethodKind::DetectCapture,
            Self::DetectExplain(_) => MethodKind::DetectExplain,
            Self::PaneReportAgent(_) => MethodKind::PaneReportAgent,
            Self::PaneReportAgentSession(_) => MethodKind::PaneReportAgentSession,
        }
        .traits()
    }
}

macro_rules! define_methods {
    (
        $(
            $variant:ident($params:ty) => $name:literal {
                mutates_ui: $mutates_ui:literal,
                routine: $routine:literal,
            };
        )+
    ) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum Method {
            $(
                #[serde(rename = $name)]
                $variant($params),
            )+
        }

        #[derive(Clone, Copy)]
        enum MethodKind {
            $($variant,)+
        }

        impl MethodKind {
            fn traits(self) -> MethodTraits {
                // Keep each method's routing facts in one generated match arm.
                match self {
                    $(
                        Self::$variant => MethodTraits {
                            name: $name,
                            mutates_ui: $mutates_ui,
                            routine: $routine,
                        },
                    )+
                }
            }
        }

        impl Method {
            /// All wire names declared by the API schema.
            pub const ALL_NAMES: &'static [&'static str] = &[$($name,)+];

            pub fn traits(&self) -> MethodTraits {
                match self {
                    $(Self::$variant(_) => MethodKind::$variant,)+
                }
                .traits()
            }
        }
    };
}

define_methods! {
    Ping(PingParams) => "ping" {
        mutates_ui: false,
        routine: false,
    };
    ServerStop(ServerStopParams) => "server.stop" {
        mutates_ui: false,
        routine: false,
    };
    DetectCapture(PaneTarget) => "detect.capture" {
        mutates_ui: false,
        routine: false,
    };
    DetectExplain(PaneTarget) => "detect.explain" {
        mutates_ui: false,
        routine: false,
    };
    PaneReportAgent(PaneReportAgentParams) => "pane.report_agent" {
        mutates_ui: true,
        routine: true,
    };
    PaneReportAgentSession(PaneReportAgentSessionParams) => "pane.report_agent_session" {
        mutates_ui: true,
        routine: true,
    };
}

#[cfg(test)]
mod tests;
