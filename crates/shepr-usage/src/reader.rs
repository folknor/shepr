//! One bounded read of a source's credentials file. Blocking: it runs on a
//! reader thread the worker caps, never on the scheduler thread.

use std::io::Read;
use std::os::unix::fs::MetadataExt;

use crate::credentials::{ParseFailure, ParsedCredentials};
use crate::limits::MAX_CREDENTIAL_FILE_BYTES;
use crate::model::ReadFailure;
use crate::secret::SecretBuffer;
use crate::source::SourceLocator;

/// What reading a source found.
#[derive(Debug, Clone)]
pub(crate) enum ReadResult {
    /// No credentials file (or no directory).
    Missing,
    Parsed(ParsedCredentials),
    Failed(ReadFailure),
}

/// Reads and parses `locator`'s credentials file. A symlink is followed; the
/// object opened must be a regular file owned by this user, checked on the
/// opened object itself, and a FIFO or device there is refused without
/// blocking on it.
pub(crate) fn read_source(locator: &SourceLocator) -> ReadResult {
    let path = locator.credentials_path();
    let file = match shepr_platform::open_regular_file(&path) {
        Ok(Ok(file)) => file,
        Ok(Err(_)) => return ReadResult::Failed(ReadFailure::NotAcceptable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ReadResult::Missing,
        Err(error) => return ReadResult::Failed(ReadFailure::Io(error.kind())),
    };
    match file.metadata() {
        Ok(metadata) if metadata.uid() == shepr_platform::effective_uid() => {}
        Ok(_) => return ReadResult::Failed(ReadFailure::NotAcceptable),
        Err(error) => return ReadResult::Failed(ReadFailure::Io(error.kind())),
    }
    let mut bytes = SecretBuffer(Vec::new());
    if let Err(error) = file
        .take(MAX_CREDENTIAL_FILE_BYTES + 1)
        .read_to_end(&mut bytes.0)
    {
        return ReadResult::Failed(ReadFailure::Io(error.kind()));
    }
    if u64::try_from(bytes.0.len()).unwrap_or(u64::MAX) > MAX_CREDENTIAL_FILE_BYTES {
        return ReadResult::Failed(ReadFailure::TooLarge);
    }
    match crate::credentials::parse(locator.provider, &bytes.0) {
        Ok(parsed) => ReadResult::Parsed(parsed),
        Err(ParseFailure::NotJson) => ReadResult::Failed(ReadFailure::NotJson),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Provider;

    #[test]
    fn a_missing_file_a_fifo_and_a_torn_write_are_told_apart() {
        let scratch = shepr_test_support::ScratchDir::new("usage-reader");
        let locator = SourceLocator {
            provider: Provider::Codex,
            directory: scratch.join("codex"),
        };
        assert!(matches!(read_source(&locator), ReadResult::Missing));

        std::fs::create_dir(&locator.directory).expect("directory");
        let path = locator.credentials_path();
        std::fs::write(&path, b"{\"tokens\": {\"access_tok").expect("torn file");
        assert!(matches!(
            read_source(&locator),
            ReadResult::Failed(ReadFailure::NotJson)
        ));

        std::fs::remove_file(&path).expect("remove");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: a NUL-terminated path; mkfifo touches no other memory.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            read_source(&locator),
            ReadResult::Failed(ReadFailure::NotAcceptable)
        ));
    }

    #[test]
    fn a_well_formed_file_is_parsed() {
        let scratch = shepr_test_support::ScratchDir::new("usage-reader-ok");
        let locator = SourceLocator {
            provider: Provider::Claude,
            directory: scratch.path().to_path_buf(),
        };
        std::fs::write(
            locator.credentials_path(),
            br#"{"claudeAiOauth":{"accessToken":"t"}}"#,
        )
        .expect("write");
        assert!(matches!(
            read_source(&locator),
            ReadResult::Parsed(ParsedCredentials::Usable(_))
        ));
    }
}
