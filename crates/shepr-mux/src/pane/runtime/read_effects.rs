use super::*;

/// The render a pane needs once a synchronized update (mode 2026) that never
/// ended is force-flushed by its timeout. Every PTY read inside the update
/// asks for it; one sleeping task per pane serves all of those requests
/// instead of one task per read.
#[derive(Debug, Default)]
pub(super) struct SyncTimeoutRender {
    /// The latest wake-up asked for, while a task is armed; `None` when no
    /// task is sleeping.
    pub(super) latest: Mutex<Option<std::time::Instant>>,
}

impl SyncTimeoutRender {
    /// Ask for a render at `at`. Returns the instant a new task must first
    /// wake at, or `None` when the armed task will cover it.
    pub(super) fn arm(&self, at: std::time::Instant) -> Option<std::time::Instant> {
        let mut latest = shepr_vt::lock_auxiliary(&self.latest);
        match *latest {
            Some(armed) => {
                if at > armed {
                    *latest = Some(at);
                }
                None
            }
            None => {
                *latest = Some(at);
                Some(at)
            }
        }
    }

    /// Called by the armed task after waking for `woke_for`. Returns a later
    /// instant to sleep until when a later update asked for one meanwhile;
    /// otherwise disarms and returns `None`, and the task renders. Skipping
    /// the earlier wake is safe: a newer update only begins after the earlier
    /// one ended, and ending an update requests its own render.
    pub(super) fn next_wake(&self, woke_for: std::time::Instant) -> Option<std::time::Instant> {
        let mut latest = shepr_vt::lock_auxiliary(&self.latest);
        match *latest {
            Some(later) if later > woke_for => Some(later),
            _ => {
                *latest = None;
                None
            }
        }
    }
}

/// What a pane's PTY read callback and its synchronized-output timer share,
/// behind one `Arc`: a read that defers work clones one pointer, not a dozen.
// Effects have their own lifetime: the sleeping timer upgrades a Weak to this
// bundle, while child reaping and detection remain independently owned tasks.
pub(super) struct PaneReadEffects {
    pub(super) pane_id: PaneId,
    pub(super) terminal: Arc<PaneTerminal>,
    pub(super) render_notify: Arc<Notify>,
    pub(super) render_dirty: Arc<RenderSignal>,
    pub(super) cwd: Arc<PaneCwdState>,
    pub(super) events: crate::events::EventSender,
    pub(super) child_liveness: Arc<ChildLiveness>,
    pub(super) sync_timeout_render: SyncTimeoutRender,
    pub(super) deferred_effect_order: Arc<DeferredEffectOrder>,
    /// The PTY actor's handle, set once the actor exists; the timer queues
    /// the replies of a flushed frame through it.
    pub(super) timer_writer: std::sync::OnceLock<PtyIoActorHandle>,
    pub(super) timer_reply_drop_reported: AtomicBool,
    pub(super) rt: tokio::runtime::Handle,
}

/// The effects of a terminal write that may block: the `/proc` scan for the
/// default-colour owner and the readlink behind an OSC 7 report. They run
/// with no terminal or reply-order lock held.
pub(super) struct DeferredEffects {
    pub(super) ticket: DeferredEffectTicket,
    pub(super) default_color_generation: Option<DefaultColorGeneration>,
    pub(super) reported_cwd: Option<std::path::PathBuf>,
}

/// Serializes the blocking effects produced by ordered terminal writes. The
/// reply-order lock assigns tickets; this gate waits for earlier effects to
/// finish after that lock has been released. Only a write with deferred
/// effects takes a ticket, so the common read never touches it.
#[derive(Default)]
pub(super) struct DeferredEffectOrder {
    pub(super) state: Mutex<DeferredEffectOrderState>,
    pub(super) ready: Condvar,
}

#[derive(Default)]
pub(super) struct DeferredEffectOrderState {
    pub(super) next_reserved: u64,
    pub(super) next_to_apply: u64,
    /// Tickets finished out of turn: dropped without being applied (a panic
    /// or early return between reservation and application). The sequence
    /// skips them once every earlier ticket has finished.
    pub(super) finished_early: std::collections::BTreeSet<u64>,
    /// Tickets parked in `apply` for an earlier one. Counted under the lock
    /// before waiting, so a finishing ticket only wakes the condvar when
    /// someone is parked on it.
    pub(super) waiting: usize,
}

impl DeferredEffectOrderState {
    pub(super) fn finish(&mut self, seq: u64) {
        if seq != self.next_to_apply {
            self.finished_early.insert(seq);
            return;
        }
        self.next_to_apply = self.next_to_apply.wrapping_add(1);
        while self.finished_early.remove(&self.next_to_apply) {
            self.next_to_apply = self.next_to_apply.wrapping_add(1);
        }
    }
}

/// One reserved place in the deferred-effect order. Dropping it finishes
/// that place, whether its effect ran, panicked or was never started, so a
/// lost ticket can never block later effects.
pub(super) struct DeferredEffectTicket {
    pub(super) order: Arc<DeferredEffectOrder>,
    pub(super) seq: u64,
}

impl Drop for DeferredEffectTicket {
    fn drop(&mut self) {
        let mut state = shepr_vt::lock_auxiliary(&self.order.state);
        state.finish(self.seq);
        let parked = state.waiting > 0;
        drop(state);
        if parked {
            self.order.ready.notify_all();
        }
    }
}

impl DeferredEffectOrder {
    /// Called while the terminal reply-order lock is held.
    pub(super) fn reserve(self: &Arc<Self>) -> DeferredEffectTicket {
        let mut state = shepr_vt::lock_auxiliary(&self.state);
        let seq = state.next_reserved;
        state.next_reserved = state.next_reserved.wrapping_add(1);
        DeferredEffectTicket {
            order: Arc::clone(self),
            seq,
        }
    }
}

impl DeferredEffectTicket {
    /// Runs the effect after every earlier ticket has finished, then
    /// finishes this one (also when the effect panics).
    pub(super) fn apply(self, effect: impl FnOnce()) {
        let mut state = shepr_vt::lock_auxiliary(&self.order.state);
        if state.next_to_apply != self.seq {
            state.waiting += 1;
            while state.next_to_apply != self.seq {
                state = match self.order.ready.wait(state) {
                    Ok(state) => state,
                    Err(poisoned) => shepr_vt::recover_auxiliary_poison(poisoned),
                };
            }
            state.waiting -= 1;
        }
        drop(state);
        effect();
    }
}

pub(super) fn has_deferred_effects(result: &ProcessBytesEffects) -> bool {
    result.default_color_generation.is_some() || result.reported_cwd.is_some()
}

/// The initial screen for a child-I/O fixture is state, not output from a
/// live child. Clear every queued parser effect before later writes can collect
/// it as though the child had just produced it.
pub(super) fn discard_initial_terminal_effects(terminal: &mut shepr_vt::Terminal) {
    drop(terminal.take_effects());
}

impl PaneReadEffects {
    pub(super) fn read(self: &Arc<Self>, output: &PaneOutputWriter, bytes: &[u8]) -> PtyReadResult {
        let write = output.begin();
        // Ticks an expired synchronized update first, then parses; the
        // core lock is released when this returns.
        let mut result = match write.process(bytes, std::time::Instant::now()) {
            Ok(result) => result,
            Err(_) => return PtyReadResult::CoreBroken,
        };
        let deferred_ticket = self.reserve_deferred(&result);
        let terminal_responses = std::mem::take(&mut result.terminal_responses);
        if let RenderRequest::After(delay) = result.render_request {
            self.arm_sync_timeout(delay);
        }
        let after_response_order: Option<Box<dyn FnOnce() + Send>> = self
            .apply_immediate(result, deferred_ticket)
            .map(|deferred| {
                let effects = Arc::clone(self);
                let run: Box<dyn FnOnce() + Send> =
                    Box::new(move || effects.apply_deferred(deferred));
                run
            });
        PtyReadResult::Effects(PtyReadEffects {
            terminal_responses,
            after_response_order,
        })
    }

    /// Applies the effects that never block (render and title requests,
    /// clipboard writes) and returns the ones that may, if any. A read with
    /// nothing to defer, the common case, allocates nothing for them.
    /// `ticket` is the write's place in the deferred-effect order, reserved
    /// under the reply-order lock exactly when `has_deferred_effects` held.
    pub(super) fn apply_immediate(
        &self,
        result: ProcessBytesEffects,
        ticket: Option<DeferredEffectTicket>,
    ) -> Option<DeferredEffects> {
        let pane_id = self.pane_id;
        let title_requested =
            result.terminal_title_changed && self.render_dirty.request_terminal_title(pane_id);
        let render_requested = matches!(result.render_request, RenderRequest::Now)
            && self
                .render_dirty
                .request_pty_coalesced(pane_id, &self.terminal.render_queued);
        if title_requested || render_requested {
            self.render_notify.notify_one();
        }
        for content in result.clipboard_writes {
            if let Err(err) = self
                .events
                .try_send(AppEvent::ClipboardWrite { pane_id, content })
            {
                warn!(
                    pane = pane_id.raw(),
                    error = %err,
                    "failed to send OSC 52 clipboard write"
                );
            }
        }
        ticket.map(|ticket| DeferredEffects {
            ticket,
            default_color_generation: result.default_color_generation,
            reported_cwd: result.reported_cwd,
        })
    }

    /// Reserves the write's place in the deferred-effect order when it has
    /// deferred effects. Called under the reply-order lock.
    pub(super) fn reserve_deferred(
        &self,
        result: &ProcessBytesEffects,
    ) -> Option<DeferredEffectTicket> {
        has_deferred_effects(result).then(|| self.deferred_effect_order.reserve())
    }

    pub(super) fn apply_deferred(&self, deferred: DeferredEffects) {
        // This still blocks the PTY actor after the reply-order lock is
        // released. Moving it off-thread needs a bounded queue ordered by
        // ticket reservation, including timer flushes. OSC 10/11 scans /proc,
        // and OSC 7 validates its path with stat, which can block on a remote
        // mount. An unbounded queue behind one blocked operation could grow
        // without limit; a bounded nonblocking queue needs a policy for
        // coalescing or dropping cwd reports while retaining their order.
        deferred.ticket.apply(|| {
            if let Some(generation) = deferred.default_color_generation {
                self.terminal.resolve_default_color_owner(
                    self.pane_id,
                    &self.child_liveness,
                    generation,
                );
            }
            if let Some(cwd) = deferred.reported_cwd {
                publish_reported_cwd(
                    self.pane_id,
                    &self.child_liveness,
                    cwd,
                    &self.cwd.reported,
                    &self.events,
                );
            }
        });
    }

    /// Makes sure a task will flush the synchronized update this read began
    /// or continued once `delay` passes, so a child that goes quiet inside an
    /// update it never ends still gets its frame shown and its queries
    /// answered. One task per pane serves every read's request.
    pub(super) fn arm_sync_timeout(self: &Arc<Self>, delay: std::time::Duration) {
        let Some(first_wake) = self
            .sync_timeout_render
            .arm(std::time::Instant::now() + delay)
        else {
            return;
        };
        let effects = Arc::downgrade(self);
        self.rt.spawn(async move {
            let mut wake_at = first_wake;
            loop {
                tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)).await;
                let Some(current_effects) = effects.upgrade() else {
                    return;
                };
                match current_effects.sync_timeout_render.next_wake(wake_at) {
                    Some(later) => {
                        wake_at = later;
                        drop(current_effects);
                    }
                    None => {
                        // The weak reference keeps the pane alive only while
                        // the timer is actively flushing, not while it sleeps.
                        // Once queued, this blocking flush cannot be aborted.
                        // It may tick a detached terminal; late events are
                        // rejected by generation after runtime removal.
                        tokio::task::spawn_blocking(move || {
                            current_effects.flush_expired_synchronized_output();
                        });
                        return;
                    }
                }
            }
        });
    }

    /// The timer's half of the runtime tick: flush an expired update, queue
    /// its replies at one point in the reply order (taken before the core
    /// lock, as the reader does), then apply its effects with no lock held.
    pub(super) fn flush_expired_synchronized_output(&self) {
        let mut tick_result: Option<ProcessBytesResult> = None;
        let mut deferred_ticket = None;
        let mut tick = || match self.terminal.tick(std::time::Instant::now()) {
            Ok(mut result) => {
                deferred_ticket = self.reserve_deferred(&result);
                let replies = std::mem::take(&mut result.terminal_responses);
                tick_result = Some(Ok(result));
                replies
            }
            Err(poisoned) => {
                tick_result = Some(Err(poisoned));
                Vec::new()
            }
        };
        match self.timer_writer.get() {
            Some(writer) => writer.write_terminal_responses(tick),
            // The actor is set right after it spawns, so this is only a timer
            // that beat that store. Flush anyway: the frame must not stay
            // hidden until the child's next output. Its replies have no route.
            None => {
                let replies = tick();
                if !replies.is_empty()
                    && !self.timer_reply_drop_reported.swap(true, Ordering::Relaxed)
                {
                    warn!(
                        pane = self.pane_id.raw(),
                        dropped_replies = replies.len(),
                        "synchronized update replies had no PTY actor route"
                    );
                }
                drop(replies);
            }
        }
        let Some(Ok(result)) = tick_result else {
            // The PTY actor checks the poisoned core on every loop, including
            // idle polls, and reports that exit through its broken-core path.
            return;
        };
        if let Some(deferred) = self.apply_immediate(result, deferred_ticket) {
            self.apply_deferred(deferred);
        }
    }
}
