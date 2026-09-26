use std::path::Path;

pub(crate) fn create_private_state_file(path: &Path) -> std::io::Result<std::fs::File> {
    super::create_remote_ssh_config_file(path)
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

/// Fsyncs `directory` itself, making a rename or unlink inside it durable.
/// Callers pass the directory that holds the entry they just changed, not the
/// entry.
pub(crate) fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}
