//! Damage tracking: alacritty's per-read damage folded into monotonic
//! generation counters, so every [`crate::RenderState`] can ask "what changed
//! since the generation I last saw" independently.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Term, TermDamage};

pub(super) struct Damage {
    /// Monotonic damage counter; a render snapshot remembers the last value it saw.
    generation: u64,
    /// Generation of the most recent whole-viewport damage.
    full_generation: u64,
    /// Per viewport row: generation of the most recent damage to that row.
    row_generations: Vec<u64>,
}

impl Damage {
    /// Starts fully damaged, which matches a fresh terminal whose renderers
    /// have seen nothing.
    pub(super) fn new(screen_lines: usize) -> Self {
        Self {
            generation: 1,
            full_generation: 1,
            row_generations: vec![0; screen_lines],
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the whole viewport was damaged after `seen`.
    pub(super) fn full_since(&self, seen: u64) -> bool {
        self.full_generation > seen
    }

    /// Whether viewport row `y` was damaged after `seen`.
    pub(super) fn row_since(&self, y: usize, seen: u64) -> bool {
        self.row_generations
            .get(y)
            .is_some_and(|generation| *generation > seen)
    }

    pub(super) fn bump_full(&mut self) {
        self.generation += 1;
        self.full_generation = self.generation;
    }

    /// Folds alacritty's damage since the last call into the generation
    /// counters, then resets alacritty's tracking.
    pub(super) fn collect<T>(&mut self, term: &mut Term<T>) {
        let screen_lines = term.screen_lines();
        if self.row_generations.len() != screen_lines {
            self.row_generations = vec![0; screen_lines];
            self.bump_full();
        }
        let next = self.generation + 1;
        let mut damaged = false;
        match term.damage() {
            TermDamage::Full => {
                self.full_generation = next;
                damaged = true;
            }
            TermDamage::Partial(lines) => {
                for bounds in lines {
                    if let Some(slot) = self.row_generations.get_mut(bounds.line) {
                        *slot = next;
                        damaged = true;
                    }
                }
            }
        }
        term.reset_damage();
        if damaged {
            self.generation = next;
        }
    }
}
