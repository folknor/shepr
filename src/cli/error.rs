use shepr_api::schema::ErrorResponse;

#[derive(Debug)]
pub(crate) enum CliError {
    Response(ErrorResponse),
    Session(SessionCliError),
    Usage(String),
    Io(std::io::Error),
}

#[derive(Debug)]
pub(crate) enum SessionCliError {
    InvalidName(shepr_api::session::SessionError),
    Stop(shepr_api::session::SessionError),
    Delete(shepr_api::session::SessionError),
}

impl SessionCliError {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidName(_) => "invalid_session_name",
            Self::Stop(_) => "session_stop_failed",
            Self::Delete(_) => "session_delete_failed",
        }
    }
}

impl std::fmt::Display for SessionCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(error) => error.fmt(f),
            Self::Stop(error) | Self::Delete(error) => error.fmt(f),
        }
    }
}

impl CliError {
    pub(crate) fn exit_code(&self) -> i32 {
        if matches!(self, Self::Usage(_)) { 2 } else { 1 }
    }

    pub(crate) fn print(&self) {
        match self {
            Self::Response(response) => match serde_json::to_string(response) {
                Ok(json) => eprintln!("{json}"),
                Err(error) => eprintln!("error: {error}"),
            },
            Self::Session(error) => eprintln!(
                "{}",
                serde_json::json!({ "error": { "code": error.code(), "message": error.to_string() } })
            ),
            Self::Usage(message) => {
                eprintln!("error: {message}");
                eprintln!("run 'shepr --help' for usage");
            }
            Self::Io(error) => eprintln!("error: {error}"),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(response) => f.write_str(&response.error.message),
            Self::Session(error) => error.fmt(f),
            Self::Usage(message) => f.write_str(message),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Session(SessionCliError::Stop(error) | SessionCliError::Delete(error)) => {
                Some(error)
            }
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
