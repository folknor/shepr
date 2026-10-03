use serde::{Deserialize, Serialize};

/// The kind of bounded resource whose limit was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitKind {
    FrameBytes,
    MessageBytes,
    CollectionItems,
    InputPayloadBytes,
    SurfaceMessageBytes,
    EndpointResponseBytes,
    ConnectionCount,
}

impl LimitKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::FrameBytes => "frame payload",
            Self::MessageBytes => "message",
            Self::CollectionItems => "collection",
            Self::InputPayloadBytes => "input payload",
            Self::SurfaceMessageBytes => "surface message",
            Self::EndpointResponseBytes => "endpoint response",
            Self::ConnectionCount => "connection count",
        }
    }

    pub const fn unit(self) -> &'static str {
        match self {
            Self::CollectionItems => "items",
            Self::FrameBytes
            | Self::MessageBytes
            | Self::InputPayloadBytes
            | Self::SurfaceMessageBytes
            | Self::EndpointResponseBytes => "bytes",
            Self::ConnectionCount => "connections",
        }
    }
}

/// A named resource limit and its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    kind: LimitKind,
    max: usize,
}

impl Limit {
    pub const fn new(kind: LimitKind, max: usize) -> Self {
        Self { kind, max }
    }

    pub const fn kind(self) -> LimitKind {
        self.kind
    }

    pub const fn max(self) -> usize {
        self.max
    }
}

/// A measured value that exceeded a named resource cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitExceeded {
    pub limit: Limit,
    pub actual: usize,
}

impl LimitExceeded {
    pub const fn new(limit: Limit, actual: usize) -> Self {
        Self { limit, actual }
    }
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.limit.kind();
        write!(
            f,
            "{} of {} {} exceeds its limit of {} {}",
            kind.name(),
            self.actual,
            kind.unit(),
            self.limit.max(),
            kind.unit()
        )
    }
}

impl std::error::Error for LimitExceeded {}
