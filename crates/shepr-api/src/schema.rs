use serde::{Deserialize, Serialize};

pub mod common;
pub mod detection;
pub mod panes;
pub mod response;
pub mod server;

pub use common::*;
pub use detection::*;
pub use panes::*;
pub use response::*;
pub use server::*;

/// One JSON request line: `{"id": .., "method": .., "params": ..}`.
///
/// Deserialization refuses any other top-level key. serde's
/// `deny_unknown_fields` cannot be combined with the flattened method, and a
/// derived decode would silently drop a stray key such as an
/// `expected_boot_id` placed beside `server.stop` instead of inside
/// `server.stop_if_boot`'s params, turning a request meant to be conditional
/// into an unconditional stop. Serialization is the derived flattened form.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRequest {
            id: String,
            method: serde_json::Value,
            #[serde(default)]
            params: Option<UniqueKeysValue>,
        }

        let RawRequest { id, method, params } = RawRequest::deserialize(deserializer)?;
        let mut tagged = serde_json::Map::new();
        tagged.insert("method".into(), method);
        if let Some(UniqueKeysValue(params)) = params {
            tagged.insert("params".into(), params);
        }
        let method = Method::deserialize(serde_json::Value::Object(tagged))
            .map_err(serde::de::Error::custom)?;
        Ok(Self { id, method })
    }
}

/// A JSON value that refuses a repeated object key at any depth. A plain
/// `serde_json::Value` keeps the last of two equal keys, so a params object
/// with two `expected_boot_id`s would quietly guard on the second; the
/// derived params structs refuse duplicates only when they read the request
/// text themselves, which the method re-tagging in `Request`'s decode
/// prevents.
struct UniqueKeysValue(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueKeysValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueKeysValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value without repeated object keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(value.into()))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(
                    serde_json::Number::from_f64(value).map_or(serde_json::Value::Null, Into::into),
                ))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(value.into()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(value.into()))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(serde_json::Value::Null))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueKeysValue(serde_json::Value::Null))
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                UniqueKeysValue::deserialize(deserializer)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut items = Vec::new();
                while let Some(UniqueKeysValue(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(UniqueKeysValue(serde_json::Value::Array(items)))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut object = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    let UniqueKeysValue(value) = map.next_value()?;
                    if object.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
                    }
                    object.insert(key, value);
                }
                Ok(UniqueKeysValue(serde_json::Value::Object(object)))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
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

/// A request the app loop answers: the socket thread answers `ping`,
/// `server.stop` and `server.stop_if_boot` itself and hands every other method
/// to the app as this.
/// Not a wire type; the socket thread builds it from a decoded [`Request`].
#[derive(Debug, Clone, PartialEq)]
pub struct AppRequest {
    pub id: String,
    pub method: AppMethod,
}

macro_rules! define_methods {
    (
        socket {
            $(
                $socket_variant:ident($socket_params:ty) => $socket_name:literal {
                    mutates_ui: $socket_mutates_ui:literal,
                    routine: $socket_routine:literal,
                };
            )+
        }
        app {
            $(
                $app_variant:ident($app_params:ty) => $app_name:literal {
                    mutates_ui: $app_mutates_ui:literal,
                    routine: $app_routine:literal,
                };
            )+
        }
    ) => {
        /// Methods the client can request, preserving their flat wire names.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum Method {
            $(
                #[serde(rename = $socket_name)]
                $socket_variant($socket_params),
            )+
            $(
                #[serde(rename = $app_name)]
                $app_variant($app_params),
            )+
        }

        /// Methods the app loop answers, generated from the `app` route group.
        #[derive(Debug, Clone, PartialEq)]
        pub enum AppMethod {
            $($app_variant($app_params),)+
        }

        /// Methods the socket thread answers before they can reach the app loop.
        #[derive(Debug, Clone, PartialEq)]
        pub(crate) enum SocketMethod {
            $($socket_variant($socket_params),)+
        }

        /// The schema route after a method and its parameters are decoded.
        pub(crate) enum MethodRoute {
            Socket(SocketMethod),
            App(AppMethod),
        }

        #[derive(Clone, Copy)]
        enum MethodKind {
            $($socket_variant,)+
            $($app_variant,)+
        }

        impl MethodKind {
            fn traits(self) -> MethodTraits {
                // Keep each method's logging facts beside its route declaration.
                match self {
                    $(
                        Self::$socket_variant => MethodTraits {
                            name: $socket_name,
                            mutates_ui: $socket_mutates_ui,
                            routine: $socket_routine,
                        },
                    )+
                    $(
                        Self::$app_variant => MethodTraits {
                            name: $app_name,
                            mutates_ui: $app_mutates_ui,
                            routine: $app_routine,
                        },
                    )+
                }
            }
        }

        impl AppMethod {
            pub fn traits(&self) -> MethodTraits {
                match self {
                    $(
                        Self::$app_variant(_) => MethodTraits {
                            name: $app_name,
                            mutates_ui: $app_mutates_ui,
                            routine: $app_routine,
                        },
                    )+
                }
            }
        }

        impl Method {
            /// All wire names declared by the API schema.
            pub const ALL_NAMES: &'static [&'static str] = &[
                $($socket_name,)+
                $($app_name,)+
            ];

            /// Splits socket-thread controls from app-loop requests using the schema route.
            pub(crate) fn into_route(self) -> MethodRoute {
                match self {
                    $(
                        Self::$socket_variant(params) => {
                            MethodRoute::Socket(SocketMethod::$socket_variant(params))
                        }
                    )+
                    $(
                        Self::$app_variant(params) => {
                            MethodRoute::App(AppMethod::$app_variant(params))
                        }
                    )+
                }
            }

            pub fn traits(&self) -> MethodTraits {
                match self {
                    $(Self::$socket_variant(_) => MethodKind::$socket_variant,)+
                    $(Self::$app_variant(_) => MethodKind::$app_variant,)+
                }
                .traits()
            }
        }
    };
}

define_methods! {
    socket {
        Ping(PingParams) => "ping" {
            mutates_ui: false,
            routine: false,
        };
        ServerStop(ServerStopParams) => "server.stop" {
            mutates_ui: false,
            routine: false,
        };
        ServerStopIfBoot(ServerStopIfBootParams) => "server.stop_if_boot" {
            mutates_ui: false,
            routine: false,
        };
    }
    app {
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
}

#[cfg(test)]
mod tests;
