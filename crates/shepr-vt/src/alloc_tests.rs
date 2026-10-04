//! Allocation guards for the screen-row read hot path. Detection and the
//! server's row readers visit every cell of every watched row, times panes,
//! so one owned grapheme per cell multiplies into whole cores of allocator
//! work. `Terminal::visit_screen_row_text` writes each cell into the caller's
//! scratch `String` instead; these tests fail if a change makes a row read
//! allocate per cell.
//!
//! The counting allocator below is the test binary's global allocator. It is
//! a pass-through to the system allocator that counts only on a thread whose
//! test has armed it with [`count_allocations`], so every other test in the
//! binary runs exactly as it would without it.

use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as CountCell;
use std::hint::black_box;

struct CountingAllocator;

thread_local! {
    // `None` while disarmed. Const-initialized with no destructor, so reading
    // it from inside the allocator never allocates or registers a TLS dtor.
    static ALLOCATIONS: CountCell<Option<usize>> = const { CountCell::new(None) };
}

fn note_allocation() {
    // `try_with` because the allocator also serves threads tearing down their
    // thread-locals; an inaccessible slot just means nothing is armed there.
    ALLOCATIONS
        .try_with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        })
        .unwrap_or(());
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees are this allocator's guarantees; the
// counting touches only a const thread-local and never allocates.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        // SAFETY: `ptr` was allocated by this allocator, which is `System`,
        // with `layout`; the caller upholds the rest of `realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` (through this allocator)
        // with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Disarms the current thread's counter when dropped, so a panicking body
/// does not leave it counting.
struct Armed;

impl Drop for Armed {
    fn drop(&mut self) {
        ALLOCATIONS.with(|count| count.set(None));
    }
}

/// Runs `body` with this thread's counter armed and returns its result with
/// the number of allocations and reallocations it made on this thread.
fn count_allocations<R>(body: impl FnOnce() -> R) -> (R, usize) {
    ALLOCATIONS.with(|count| count.set(Some(0)));
    let armed = Armed;
    let result = body();
    let allocations = ALLOCATIONS.with(CountCell::get).unwrap_or(0);
    drop(armed);
    (result, allocations)
}

/// A terminal `cols` wide whose top screen row is filled with `cell`.
fn terminal_with_row(cols: u16, cell: &str) -> Terminal {
    let mut terminal = Terminal::new(
        shepr_core::geometry::PaneGeometry::cells_only(cols, 3),
        shepr_core::scrollback::ScrollbackBudget::new(0),
    );
    // Autowrap off, so the last cell stays on row 0 without wrapping.
    terminal.write(b"\x1b[?7l");
    // A wide cell's text takes two columns, so `cols` repeats always fill the
    // row; the surplus overwrites the last cell.
    terminal.write(cell.repeat(usize::from(cols)).as_bytes());
    terminal
}

/// Reads screen row 0 through the hot-path visitor, folding every cell into
/// a sum that does not allocate. Returns cells visited and text bytes seen.
fn read_row(terminal: &Terminal, scratch: &mut String) -> (usize, usize) {
    let mut cells = 0;
    let mut bytes = 0;
    terminal
        .visit_screen_row_text(ScreenRow(0), scratch, |_, wide, text| {
            cells += 1;
            bytes += text.len() + usize::from(wide.columns());
        })
        .expect("screen row 0 is retained");
    black_box((cells, bytes))
}

#[test]
fn the_allocation_counter_sees_allocations_on_its_own_thread() {
    let (boxed, allocations) = count_allocations(|| black_box(Box::new(7_u64)));
    assert_eq!(*boxed, 7);
    assert_eq!(allocations, 1);
}

fn assert_row_read_does_not_allocate_per_cell(cols: u16, cell: &str) {
    let terminal = terminal_with_row(cols, cell);

    // A cold scratch grows once for the widest cell, never once per cell.
    let mut scratch = String::new();
    let ((cells, _), cold) = count_allocations(|| read_row(&terminal, &mut scratch));
    assert_eq!(cells, usize::from(cols), "{cols} columns of {cell:?}");
    assert!(
        cold <= 1,
        "a cold read of {cols} columns of {cell:?} allocated {cold} times"
    );

    // Once warm, a whole row read allocates nothing.
    let ((cells, _), warm) = count_allocations(|| read_row(&terminal, &mut scratch));
    assert_eq!(cells, usize::from(cols));
    assert_eq!(
        warm, 0,
        "a warm read of {cols} columns of {cell:?} allocated {warm} times"
    );
}

#[test]
fn an_ascii_row_read_does_not_allocate_once_the_scratch_is_warm() {
    for cols in [80, 330] {
        assert_row_read_does_not_allocate_per_cell(cols, "x");
    }
}

#[test]
fn a_row_of_combining_marks_does_not_allocate_per_cell() {
    for cols in [80, 330] {
        assert_row_read_does_not_allocate_per_cell(cols, "e\u{301}\u{323}");
    }
}

#[test]
fn a_row_of_wide_characters_does_not_allocate_per_cell() {
    for cols in [80, 330] {
        assert_row_read_does_not_allocate_per_cell(cols, "\u{754c}");
    }
}
