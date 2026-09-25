//! Build identity helpers.

pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_cargo_version() {
        assert_eq!(super::version(), env!("CARGO_PKG_VERSION"));
    }
}
