use std::io;

/// Return 64 bits from the kernel random source for identifiers and runtime names.
///
/// These tokens distinguish concurrently created paths. If the kernel random
/// source is unavailable, fail the operation rather than silently falling
/// back to a value whose unpredictability is harder to audit.
pub fn unpredictable_token() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: getrandom(2) writes at most the requested number of bytes
        // into the remaining live portion of this stack buffer.
        let result = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned no bytes for a private runtime name",
            ));
        }
        filled += result.cast_unsigned();
    }
    Ok(u64::from_ne_bytes(bytes))
}
