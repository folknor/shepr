use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// A path on the endpoint host. Clients display it or return it to that host;
/// they never resolve or open it on their own filesystem.
/// Bytes preserve Linux paths that are not UTF-8 through the positional codec.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RemotePath(
    #[serde(
        serialize_with = "crate::codec::serialize_byte_vec",
        deserialize_with = "crate::codec::deserialize_byte_vec"
    )]
    Vec<u8>,
);

impl RemotePath {
    pub fn as_path(&self) -> &Path {
        Path::new(std::ffi::OsStr::from_bytes(&self.0))
    }

    pub fn into_path_buf(self) -> PathBuf {
        PathBuf::from(std::ffi::OsString::from_vec(self.0))
    }

    pub fn display_text(&self) -> std::borrow::Cow<'_, str> {
        self.as_path().to_string_lossy()
    }
}

impl From<PathBuf> for RemotePath {
    fn from(path: PathBuf) -> Self {
        Self(path.into_os_string().into_vec())
    }
}

impl From<&Path> for RemotePath {
    fn from(path: &Path) -> Self {
        Self(path.as_os_str().as_bytes().to_vec())
    }
}

impl From<String> for RemotePath {
    fn from(path: String) -> Self {
        Self(path.into_bytes())
    }
}

impl From<&str> for RemotePath {
    fn from(path: &str) -> Self {
        Self(path.as_bytes().to_vec())
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_path().display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_path_roundtrip_preserves_non_utf8_bytes() {
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\xff".to_vec()));
        let remote = RemotePath::from(path.clone());
        let mut bytes = Vec::new();
        crate::codec::encode_into(&mut bytes, &remote).expect("encode remote path");
        let decoded: RemotePath =
            crate::codec::from_slice_exact(&bytes).expect("decode remote path");
        assert_eq!(decoded.into_path_buf(), path);
    }
}
