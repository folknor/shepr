//! Remembered sources: directories an agent was seen using, kept in a small
//! file in the server's data directory so an idle account stays tracked
//! after its pane exits. The file holds providers and paths only, never a
//! credential.
//!
//! Writes run on their own thread, never the scheduler's. They are
//! serialized and coalesced: the thread always writes the newest set it has
//! been given, so an older write cannot land after a newer one.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use serde_json::Value;

use crate::limits::{MAX_REGISTRY_FILE_BYTES, MAX_REMEMBERED_SOURCES};
use crate::source::{Provider, SourceLocator};

// limits-exempt: the version of this file's own format.
const FORMAT_VERSION: u64 = 1;

/// Reads the remembered sources. A missing file is an empty set.
pub(crate) fn load(path: &Path) -> std::io::Result<Vec<SourceLocator>> {
    let file = match shepr_platform::open_regular_file(path) {
        Ok(Ok(file)) => file,
        Ok(Err(_)) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "remembered usage sources path is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_REGISTRY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_REGISTRY_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "remembered usage sources file is too large",
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "remembered usage sources file is not JSON",
        )
    })?;
    if value.get("version").and_then(Value::as_u64) != Some(FORMAT_VERSION) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "remembered usage sources file has an unknown version",
        ));
    }
    let sources = value
        .get("sources")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let provider = Provider::from_tag(entry.get("provider")?.as_str()?)?;
                    let directory = PathBuf::from(entry.get("directory")?.as_str()?);
                    directory.is_absolute().then_some(SourceLocator {
                        provider,
                        directory,
                    })
                })
                .take(MAX_REMEMBERED_SOURCES)
                .collect()
        });
    Ok(sources)
}

fn encode(sources: &[SourceLocator]) -> Vec<u8> {
    let entries: Vec<Value> = sources
        .iter()
        // A path that is not UTF-8 cannot be written as JSON text; it is
        // remembered for this run only.
        .filter_map(|source| {
            Some(serde_json::json!({
                "provider": source.provider.tag(),
                "directory": source.directory.to_str()?,
            }))
        })
        .take(MAX_REMEMBERED_SOURCES)
        .collect();
    let document = serde_json::json!({"version": FORMAT_VERSION, "sources": entries});
    let mut bytes = serde_json::to_vec_pretty(&document).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// The newest set waiting to be written, and whether the handle is gone.
#[derive(Default)]
struct Pending {
    newest: Option<Vec<SourceLocator>>,
    closed: bool,
}

/// The handle to the registry writer thread. It holds at most one waiting
/// set, the newest: saving replaces it, so nothing queues behind a slow or
/// stuck write.
pub(crate) struct RegistryWriter {
    pending: Arc<(Mutex<Pending>, Condvar)>,
}

fn lock_pending(pending: &Mutex<Pending>) -> MutexGuard<'_, Pending> {
    // The value is replaced or flagged whole.
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RegistryWriter {
    /// Starts the writer; `report` gets each write's success.
    pub(crate) fn start(
        path: PathBuf,
        report: impl Fn(bool) + Send + 'static,
    ) -> std::io::Result<Self> {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let shared = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("usage-registry".into())
            .spawn(move || {
                loop {
                    let newest = {
                        let (lock, ready) = &*shared;
                        let mut pending = lock_pending(lock);
                        loop {
                            if let Some(newest) = pending.newest.take() {
                                break newest;
                            }
                            if pending.closed {
                                return;
                            }
                            pending = ready.wait(pending).unwrap_or_else(PoisonError::into_inner);
                        }
                    };
                    let written = shepr_platform::publish_file::publish_private(
                        &path,
                        &mut encode(&newest).as_slice(),
                        0o600,
                        shepr_platform::publish_file::PublishTarget::ReplaceExisting,
                    );
                    if let Err(error) = &written {
                        shepr_platform::structured_log!(
                            WARN,
                            event = usage.registry_write,
                            outcome = Error,
                            error = %error,
                            path = %path.display(),
                            "could not save remembered usage sources"
                        );
                    }
                    report(written.is_ok());
                }
            })?;
        Ok(Self { pending })
    }

    pub(crate) fn save(&self, sources: Vec<SourceLocator>) {
        let (lock, ready) = &*self.pending;
        lock_pending(lock).newest = Some(sources);
        ready.notify_one();
    }
}

impl Drop for RegistryWriter {
    fn drop(&mut self) {
        let (lock, ready) = &*self.pending;
        lock_pending(lock).closed = true;
        ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_round_trip_and_a_missing_file_is_empty() {
        let scratch = shepr_test_support::ScratchDir::new("usage-registry");
        let path = scratch.join("usage-sources.json");
        assert_eq!(load(&path).expect("missing is empty"), Vec::new());
        let sources = vec![
            SourceLocator {
                provider: Provider::Claude,
                directory: PathBuf::from("/srv/claude-b"),
            },
            SourceLocator {
                provider: Provider::Codex,
                directory: PathBuf::from("/srv/codex-b"),
            },
        ];
        std::fs::write(&path, encode(&sources)).expect("write");
        assert_eq!(load(&path).expect("load"), sources);
    }

    #[test]
    fn a_foreign_version_or_shape_is_refused_and_relative_paths_dropped() {
        let scratch = shepr_test_support::ScratchDir::new("usage-registry-bad");
        let path = scratch.join("usage-sources.json");
        std::fs::write(&path, br#"{"version": 9, "sources": []}"#).expect("write");
        assert!(load(&path).is_err());
        std::fs::write(
            &path,
            br#"{"version": 1, "sources": [{"provider": "codex", "directory": "rel"},
                {"provider": "other", "directory": "/x"}]}"#,
        )
        .expect("write");
        assert_eq!(load(&path).expect("load"), Vec::new());
    }

    #[test]
    fn the_writer_lands_the_newest_set() {
        let scratch = shepr_test_support::ScratchDir::new("usage-registry-writer");
        let path = scratch.join("usage-sources.json");
        let (done, written) = std::sync::mpsc::channel();
        let writer = RegistryWriter::start(path.clone(), move |ok| {
            done.send(ok).ok();
        })
        .expect("writer");
        let newest = vec![SourceLocator {
            provider: Provider::Codex,
            directory: PathBuf::from("/srv/newest"),
        }];
        writer.save(Vec::new());
        writer.save(newest.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            assert!(
                written
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("a write")
            );
            if load(&path).expect("load") == newest {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "newest set never landed"
            );
        }
    }
}
