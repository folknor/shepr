//! Pane history kept from one save to the next, and what a save resolves
//! against it.

use std::collections::HashMap;
use std::sync::Arc;

use shepr_core::layout::PaneId;
use shepr_protocol::PanePublicNumber;

use super::{HistoryDigest, HistoryText, SessionHistory};
use crate::persist::schema::{PaneHistorySnapshot, SNAPSHOT_VERSION, SessionHistorySnapshot};

/// The history file's text for a pane that has not run yet; see
/// `HistoryCarry`.
struct RestoredEntry {
    ansi: Arc<str>,
    /// Names `ansi` the way a `PaneHistoryCache` revision names its text.
    revision: HistoryRevision,
}

/// Restored and live text have distinct identity spaces.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryRevision {
    Live(std::num::NonZeroU64),
    Restored(u64),
}

/// Names for restored text, independent of live cache revisions.
fn next_restored_revision() -> HistoryRevision {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    HistoryRevision::Restored(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// What a save's history holds for one pane, by content identity rather than
/// content: two saves with equal stamps hold equal text.
type PaneStamp = Option<HistoryRevision>;

/// Every pane of a save by workspace, with its public number, pane id and
/// stamp, sorted by public number within a workspace.
type NamedPanes = Vec<Vec<(PanePublicNumber, PaneId, PaneStamp)>>;

/// What one save's history was made of: the content of every pane, under the
/// workspace position and pane number it is saved with, sorted by pane
/// number. Two equal stamps serialize to the same bytes: the history file
/// holds nothing but that mapping (and the format version), and one build's
/// serializer and cap are fixed.
#[derive(PartialEq, Eq)]
struct HistoryStamp {
    panes: Vec<Vec<(PanePublicNumber, PaneStamp)>>,
}

/// What resolving a save's history produced.
pub(in crate::persist) enum ResolvedHistory {
    /// The history file already holds exactly this history, whose digest is
    /// given: nothing was assembled and nothing needs writing.
    Unchanged(HistoryDigest),
    Changed(SessionHistory),
}

/// Pane history kept from one save to the next. Restore creates it and hands
/// it to the session persister, which owns it from then on: every history
/// capture is resolved against it, on the persister's thread, in save order,
/// so nothing else touches it and it needs no lock.
///
/// It keeps, keyed by the pane's live ID:
///
/// - The history a pane's reader formatted before (`readers`, a
///   `PaneHistoryCache`), so a save formats only the lines that are new since
///   the last one. This is also the one copy of a pane's history that outlives
///   a save: a pane on the alternate screen (vim, an agent TUI) cannot have
///   its primary screen read, so saves use what the cache held at its last
///   successful read, and a pane that lost its runtime keeps the text it had.
///   A successful read of an empty screen leaves the cache with no text, which
///   saves as no history.
/// - What a pane's live screen cannot supply (`restored`): a restored pane
///   without a runtime (deferred agent resume, failed restore) keeps its
///   history from the loaded file until it runs. Capture reads live runtimes
///   only, so without this a save made before the pane runs would lose its
///   saved screen for good. The first save that sees a runtime for the pane
///   drops that entry: from then on the pane's own screen supersedes it, even
///   if its first read happens on the alternate screen.
/// - What the last successful save's history was made of (`saved`), so a save
///   with the same content is recognised without assembling, serializing or
///   hashing any text.
///
/// Each save drops the entries of panes no longer in its layout. That pruning
/// is why this belongs to one app's persister instead of being process-wide:
/// a save only knows its own layout, and would drop every other owner's
/// entries.
#[derive(Default)]
pub struct HistoryCarry {
    restored: HashMap<PaneId, RestoredEntry>,
    readers: HashMap<PaneId, crate::pane::PaneHistoryCache>,
    /// The stamp of the history the last successful save wrote, with the
    /// digest its layout names it by.
    saved: Option<(HistoryStamp, HistoryDigest)>,
    /// The stamp of the history resolved for the save in progress.
    resolved: Option<HistoryStamp>,
}

impl HistoryCarry {
    /// Keeps a restored pane's saved history for later saves until the pane
    /// has a runtime of its own.
    pub fn carry_restored(&mut self, pane: PaneId, history: Option<&PaneHistorySnapshot>) {
        if let Some(history) = history {
            self.restored.insert(
                pane,
                RestoredEntry {
                    ansi: Arc::from(history.ansi.as_str()),
                    revision: next_restored_revision(),
                },
            );
        }
    }

    /// Forgets every pane: a cleared session has none.
    pub(in crate::persist) fn clear(&mut self) {
        self.restored.clear();
        self.readers.clear();
        self.forget_saved();
    }

    /// The save whose history was last resolved reached the disk, its layout
    /// naming that history by `digest`. A save that wrote no history
    /// (`None`) leaves nothing to skip against.
    pub(in crate::persist) fn note_saved(&mut self, digest: Option<HistoryDigest>) {
        let resolved = self.resolved.take();
        self.saved = resolved.zip(digest);
    }

    /// The history file may not hold what the last resolution said; the next
    /// save writes its history in full.
    pub(in crate::persist) fn forget_saved(&mut self) {
        self.saved = None;
        self.resolved = None;
    }

    /// Drops what belongs to panes outside `panes` (the ones a save saw).
    fn retain(&mut self, panes: &std::collections::HashSet<PaneId>) {
        self.restored.retain(|id, _| panes.contains(id));
        self.readers.retain(|id, _| panes.contains(id));
    }

    /// A pane without a runtime: what is carried for it (its restored
    /// history, else what its runtime's cache still holds), by content name.
    fn stamp_runtimeless(&self, pane: PaneId) -> PaneStamp {
        if let Some(entry) = self.restored.get(&pane) {
            return Some(entry.revision);
        }
        self.readers
            .get(&pane)
            .filter(|cache| cache.has_text())
            .and_then(|cache| cache.revision().map(HistoryRevision::Live))
    }

    /// A live pane: brings its cache up to date, or leaves it as it was while
    /// the alternate screen hides the primary screen, and names what it holds.
    fn stamp_live(&mut self, pane: PaneId, source: &crate::pane::PaneHistorySource) -> PaneStamp {
        // The pane has a runtime of its own now: its own screen supersedes
        // the history restored for it, permanently, even while that screen is
        // on the alternate buffer and cannot be read.
        self.restored.remove(&pane);
        let cache = self.readers.entry(pane).or_default();
        // A refresh that cannot complete leaves the cache as it was, and the
        // save carries that earlier read, whatever the reason.
        if let Err(reason) = source.refresh(cache) {
            tracing::debug!(?reason, "pane history refresh unavailable; keeping cache");
        }
        cache
            .has_text()
            .then(|| cache.revision().map(HistoryRevision::Live))
            .flatten()
    }

    /// The text a stamped pane saves, sharing the carried text.
    fn text(&self, pane: PaneId) -> Option<HistoryText> {
        match self.restored.get(&pane) {
            Some(entry) => Some(HistoryText::single(Arc::clone(&entry.ansi))),
            None => self.readers.get(&pane).map(|cache| HistoryText {
                pieces: cache.pieces(),
            }),
        }
    }
}

pub(in crate::persist) enum PendingPaneHistory {
    /// A pane without a runtime: whatever history is carried for it.
    Runtimeless(PaneId),
    /// A running pane, read where the history is resolved.
    Live(PaneId, crate::pane::PaneHistorySource),
}

/// Pane history captured on the event loop: which pane each history belongs
/// to and a handle to read it through, nothing formatted. `resolve` turns it
/// into a `SessionHistorySnapshot` off the loop. The shape mirrors the
/// workspaces it was captured from.
pub struct PendingHistory {
    workspaces: Vec<Vec<(PanePublicNumber, PendingPaneHistory)>>,
}

impl PendingHistory {
    pub(in crate::persist) fn new(
        workspaces: Vec<Vec<(PanePublicNumber, PendingPaneHistory)>>,
    ) -> Self {
        Self { workspaces }
    }

    /// Formats every live pane's history, keyed as the layout it was captured
    /// alongside keys its panes. Saves go through `resolve_for_save`; this
    /// stays public for tests in this and the server crate, which have no
    /// other way to read a capture back. Meant for the persister's thread: it can
    /// take as long as formatting what is new in every pane's scrollback
    /// does, in bounded chunks per hold of each pane's terminal lock.
    pub fn resolve(self, carry: &mut HistoryCarry) -> SessionHistorySnapshot {
        self.resolve_changed(carry).into_snapshot()
    }

    /// Always assembles the history, for a persister whose history file is
    /// not known to hold what the last save wrote. The caller reports how the
    /// save went through `HistoryCarry::note_saved` or `forget_saved`.
    pub(in crate::persist) fn resolve_changed(self, carry: &mut HistoryCarry) -> SessionHistory {
        let (named, stamp) = self.name_panes(carry);
        Self::assemble(named, stamp, carry)
    }

    /// Like [`resolve_changed`], for a persister whose history file is known
    /// to hold what the last save wrote: a history with the same content as
    /// that save's is reported as `Unchanged` without assembling any text.
    ///
    /// [`resolve_changed`]: Self::resolve_changed
    pub(in crate::persist) fn resolve_for_save(self, carry: &mut HistoryCarry) -> ResolvedHistory {
        let (named, stamp) = self.name_panes(carry);
        if let Some((saved, digest)) = &carry.saved
            && *saved == stamp
        {
            let digest = *digest;
            carry.resolved = Some(stamp);
            return ResolvedHistory::Unchanged(digest);
        }
        ResolvedHistory::Changed(Self::assemble(named, stamp, carry))
    }

    /// Brings every pane's history in `carry` up to date and names what each
    /// holds, as the stamp of the whole save.
    fn name_panes(self, carry: &mut HistoryCarry) -> (NamedPanes, HistoryStamp) {
        carry.retain(
            &self
                .workspaces
                .iter()
                .flatten()
                .map(|(_, pending)| match pending {
                    PendingPaneHistory::Runtimeless(pane) | PendingPaneHistory::Live(pane, _) => {
                        *pane
                    }
                })
                .collect(),
        );
        // Bring every pane up to date first, naming what each one holds.
        let mut named: NamedPanes = Vec::with_capacity(self.workspaces.len());
        for panes in self.workspaces {
            let mut named_panes: Vec<_> = panes
                .into_iter()
                .map(|(id, pending)| {
                    let (pane, stamp) = match pending {
                        PendingPaneHistory::Runtimeless(pane) => {
                            let stamp = carry.stamp_runtimeless(pane);
                            (pane, stamp)
                        }
                        PendingPaneHistory::Live(pane, source) => {
                            let stamp = carry.stamp_live(pane, &source);
                            (pane, stamp)
                        }
                    };
                    (id, pane, stamp)
                })
                .collect();
            named_panes.sort_unstable_by_key(|(id, _, _)| *id);
            named.push(named_panes);
        }

        // Keyed by the public numbers of the layout this history is saved
        // with. A pane's number is stable across saves and across a restore,
        // so the first save after a restore names each pane's history as the
        // file it came from did.
        let stamp = HistoryStamp {
            panes: named
                .iter()
                .map(|panes| panes.iter().map(|(id, _, stamp)| (*id, *stamp)).collect())
                .collect(),
        };
        (named, stamp)
    }

    /// The text of every pane named in `named`, recording `stamp` as what
    /// this save resolved to.
    fn assemble(
        named: NamedPanes,
        stamp: HistoryStamp,
        carry: &mut HistoryCarry,
    ) -> SessionHistory {
        carry.resolved = Some(stamp);
        SessionHistory {
            version: SNAPSHOT_VERSION,
            workspaces: named
                .into_iter()
                .map(|panes| {
                    panes
                        .into_iter()
                        .filter_map(|(id, pane, stamp)| {
                            let text = carry.text(pane).filter(|_| stamp.is_some())?;
                            Some((id, text))
                        })
                        .collect()
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneRuntimeRegistry;
    use crate::persist::capture::capture_pending_history;
    use crate::persist::history::history_digest;
    use crate::workspace::{Workspace, WorkspaceSet};

    #[test]
    fn live_and_restored_history_revisions_have_distinct_identities() {
        let one = std::num::NonZeroU64::MIN;
        assert!(HistoryRevision::Live(one) != HistoryRevision::Restored(1));
        assert!(
            HistoryRevision::Live(std::num::NonZeroU64::MAX) != HistoryRevision::Restored(u64::MAX)
        );
    }

    fn set_of(workspace: Workspace) -> WorkspaceSet {
        WorkspaceSet::restored(
            crate::workspace::WorkspaceIdAllocator::new(),
            vec![workspace],
            None,
        )
    }

    #[tokio::test]
    async fn history_with_the_saved_content_is_recognised_without_assembling() {
        let workspaces = set_of(Workspace::test_new("history-unchanged"));
        let pane_id = workspaces.as_slice()[0].tree().root();
        let mut runtimes = PaneRuntimeRegistry::new();
        runtimes.insert(
            pane_id,
            crate::pane::PaneRuntime::test_with_scrollback_bytes(20, 3, 4096, b"ONE\r\n"),
        );
        let mut carry = HistoryCarry::default();
        let first = history_digest(b"first");
        let second = history_digest(b"second");
        let third = history_digest(b"third");
        let resolve = |carry: &mut HistoryCarry, allow: bool| {
            let pending = capture_pending_history(&workspaces, &runtimes);
            if allow {
                pending.resolve_for_save(carry)
            } else {
                ResolvedHistory::Changed(pending.resolve_changed(carry))
            }
        };

        assert!(matches!(
            resolve(&mut carry, true),
            ResolvedHistory::Changed(_)
        ));
        carry.note_saved(Some(first));
        assert!(matches!(
            resolve(&mut carry, true),
            ResolvedHistory::Unchanged(digest) if digest == first
        ));
        // An unchanged save keeps what it can skip against.
        carry.note_saved(Some(first));
        assert!(
            matches!(resolve(&mut carry, false), ResolvedHistory::Changed(_)),
            "a caller that cannot skip always gets the history"
        );
        carry.note_saved(Some(second));
        assert!(matches!(
            resolve(&mut carry, true),
            ResolvedHistory::Unchanged(digest) if digest == second
        ));
        carry.note_saved(None);
        assert!(
            matches!(resolve(&mut carry, true), ResolvedHistory::Changed(_)),
            "a save that wrote no history leaves nothing to skip against"
        );
        carry.note_saved(Some(third));

        runtimes
            .get(&pane_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"TWO\r\n");
        assert!(matches!(
            resolve(&mut carry, true),
            ResolvedHistory::Changed(_)
        ));
        carry.forget_saved();
        assert!(
            matches!(resolve(&mut carry, true), ResolvedHistory::Changed(_)),
            "a save that failed leaves nothing to skip against"
        );
    }
}
