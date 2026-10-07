//! A credential value confined to the usage subsystem.

/// Bytes of a token or key. It has no `Display`, its `Debug` prints only its
/// length, and its bytes are overwritten when it is dropped, so it cannot leak
/// through a log line, an error message or a snapshot.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(Vec<u8>);

impl Secret {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Secret({} bytes)", self.0.len())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Volatile writes the optimizer cannot drop as dead stores.
        for byte in &mut self.0 {
            // SAFETY: `byte` is a valid, exclusive reference into the buffer.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

/// A byte buffer holding secrets in transit (a raw credential file, a curl
/// config), wiped on drop like [`Secret`].
pub(crate) struct SecretBuffer(pub(crate) Vec<u8>);

impl Drop for SecretBuffer {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: `byte` is a valid, exclusive reference into the buffer.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_only_the_length() {
        let secret = Secret::new(b"sk-very-secret".to_vec());
        assert_eq!(format!("{secret:?}"), "Secret(14 bytes)");
    }
}
