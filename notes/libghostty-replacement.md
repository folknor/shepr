# Replacing libghostty-vt with a pure-Rust terminal core

## Outcome (pass 1: terminal core)

Done, written blind (no build was possible until libghostty was gone), so the
first compile is still ahead.

* `alacritty_terminal = "=0.26.0"` (default features off, so no serde) replaced
  the vendored libghostty-vt. `build.rs`, the vendored tree, its patches, the
  bindgen output and the vendoring scripts are deleted. `vte` is reached
  through `alacritty_terminal::vte`, not a direct dependency. `Cargo.lock` has
  not been regenerated.
* `src/ghostty/mod.rs` keeps the old public surface for the rest of the tree:
  `Terminal`, `RenderState`, `RowIter`, `RowCellIter` and the value types. It is
  backed by alacritty. `format.rs` is the plain/VT formatter. `scan.rs` is the
  side scanner for OSC 7 (plus OSC 9;9 and 1337 CurrentDir), modes
  9/1016/2031/2048, CSI ? 996 n, CSI 16 t, 7-bit and C1 XTGETTCAP, and RIS. It
  replaces `src/pane/xtgettcap.rs`.
* Key and mouse encoding are shepr's own now. `src/pane/input.rs` is gone.
  `input::encode_terminal_key_with_modes` covers DECCKM, kitty functional keys,
  modifyOtherKeys, Ctrl+Backspace and kitty Esc/Backtab. `input::encode_mouse_event`
  covers mode filtering, SGR, SGR-pixels, UTF-8 and X10. The kitty keypad-code bug
  in `try_encode_csi_u` is fixed. `input/parse.rs` was left alone: it maps host
  keypad codes to plain arrows on purpose, and a new encoder test asserts the
  non-keypad forms.
* Dropped: incremental page compression (the task in `pane.rs`), grapheme-cluster
  mode 2027 (ZWJ/flag sequences now occupy one cell group per codepoint, with
  zero-width characters attached to the previous cell), blink and overline, the
  libghostty colour-reply stripping, and the exact ghostty DA and XTVERSION strings.

Deviations from the plan above:

* OSC colour queries are not answered inside the adapter. alacritty's
  `ColorRequest` events come out as structured `PtyResponse::ColorQuery`
  values, in byte order with the other replies, and the pane resolves them:
  host theme first unless the child owns that colour, otherwise the core's
  value. The pane still splits writes at OSC 10/11/12/4 set/reset boundaries,
  but only for its own bookkeeping.
* `set_write_pty_callback` is replaced by `Terminal::take_pty_responses()`.
  `modify_other_keys_enabled()` is gone, and `KittyKeyboardTracker` provides
  that level.
* Damage is folded into generation counters after every mutation, so
  `RenderState::update` still takes `&Terminal`.
* Scrollback conversion is `bytes / (cols * size_of::<Cell>())`, clamped to
  1 000..=1 000 000 lines for any non-zero budget, and recomputed on column
  changes. Growth is applied before a reflow and shrinking after it. The
  1 000-line floor copies ghostty's page-granular minimum. Tests relied on that
  minimum.
* `clear_screen` still keeps the cursor's (soft-wrapped) line. It does this with
  direct grid operations, not Handler calls.

Knowingly incomplete or changed behaviour:

* OSC replies arrive when the OSC's terminating ESC arrives (vte dispatches
  there), not after the final `\`. Four split-query tests were updated.
* Multi-entry OSC 4/10 queries get one reply per entry instead of one
  aggregate reply.
* Inside a synchronized update (2026) alacritty buffers bytes until ESU or the
  timeout. The side scanner is not buffered, so its replies and mode changes
  land before the buffered content is applied. After a timeout, a render,
  dirty-patch collection or sync-state query flushes the buffer. The replies
  that flush produces wait for the next PTY read.
* A raw C1 DCS (0x90) is still answered, but vte prints the payload as text.
* Minimum terminal width is 2 columns, because alacritty panics on a wide
  character in a 1-column grid.

## Outcome (pass 2: PTY)

Also written blind. The vendored `portable-pty` (and `vendor/`) is gone, as are
its `[patch.crates-io]` entry and the brokkr gremlins exclude. `Cargo.lock` has
not been regenerated, so it still lists portable-pty and the crates it pulled in.

The plan was to use `alacritty_terminal::tty`. It turned out to be a poor fit,
so the PTY is implemented directly on libc (already a dependency; no rustix
dependency was added):

* `tty::Options::env` is an additive `HashMap<String, String>`. Panes must
  remove inherited variables (host terminal handles, outer agent session IDs),
  and there is no way to do that short of mutating the server's own environment.
  It also sets `ALACRITTY_WINDOW_ID`/`WINDOWID` and overwrites `USER`/`HOME`.
* It has no argv0 control on Linux, so login shells (`-zsh`) are impossible.
* `Pty` owns the `Child` and only lends `&Child`. Its `Drop` sends SIGHUP and
  then blocks in `wait()`. Shepr reaps each child on a `spawn_blocking` thread
  that needs to own it.
* `on_resize` calls `process::exit` if `TIOCSWINSZ` fails, and every `Pty`
  registers a signal-hook SIGCHLD pipe. Shepr already has its own IO actor
  and resize path, so neither is wanted.

What replaced it:

* `src/pty/command.rs`: `PtyCommand`. Its semantics are portable-pty's
  `CommandBuilder`: the env starts from the server's own environment (with
  `SHELL` filled in from passwd), `env`/`env_remove`/`get_env` work on it, the
  command runs with exactly that env, and an invalid cwd falls back to `HOME`.
  A login shell takes `SHELL` and argv0 `-<basename>`. Missing or
  non-executable programs are rejected before fork, with the same messages.
* `src/pty/backend.rs`: `open_pty` (libc `openpty`, both fds CLOEXEC, IUTF8
  set as alacritty does), `spawn_in_pty` (slave on stdio; pre-exec resets
  signal dispositions and the mask, runs `setsid`, then `TIOCSCTTY`), and
  `spawn_pty`. That last one returns the master fd straight to the actor,
  without the old dup-and-drop step, plus a plain `std::process::Child`.
* Changed: leaked inherited fds are marked close-on-exec with
  `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)`, falling back to a `/proc/self/fd`
  walk. portable-pty closed them outright, which also closed std's exec-error
  pipe. Exec failures now surface as `spawn()` errors. SIGPIPE is also reset to
  default in the child (std does this as well).
* `classify_child_exit` takes `std::process::ExitStatus`. Pane exit logs use
  its `Display` (`exit status: 0`, `signal: 15 (SIGTERM)`) in place of
  portable-pty's `Debug` form.

---

Research only. Nothing was compiled or run. Claims about `alacritty_terminal`
and `wezterm-term` come **from memory** because neither source is on this
machine (`~/.cargo/registry` has only `vte-0.14.1`, `termwiz-0.23.3`, and some
`wezterm-*` helper crates). Everything else comes from source I read under
`src/`, `research/`, and the registry.

---

## 0. TL;DR

* **Primary: `alacritty_terminal`** (Apache-2.0, vte-based, damage tracking,
  primary-screen reflow, OSC 8/52/4/10/11/12, kitty keyboard flag stack, and
  it answers DA/DSR/DECRQM/CSI ?u/CSI 14/18t through `Event::PtyWrite`). Its
  one real modelling gap for shepr is grapheme-cluster width (no mode 2027).
* **Fallback: `par-term-emu-core-rust`** with the `sim` profile (MIT, in
  `research/`). It has the most protocol coverage, but cells hold concrete colours (you cannot tell
  "default fg" apart from "palette 7"), dirty tracking covers only
  character writes, it has heavy mandatory deps (image+rayon, swash, …), and
  its MSRV is 1.98.
* **Rejected as the core:** vt100 (no reflow, no query replies, no hyperlinks);
  fux-vt (no reflow by design, no kitty, no OSC 7/8); ansi-rs (a tokenizer only);
  vterm-rs (a toy with no wide characters or scrollback); term-wm (an app on a vt100
  fork); winter-term (the grid lives inside a wgpu render crate).
* **Key finding:** shepr already owns most of the input side. Char keys already use
  `src/input/encode.rs`. Kitty flags and modifyOtherKeys are already tracked by
  `pane/kitty_keyboard.rs`. Titles and progress come from `AgentOscStateTracker`. So the
  core has to supply the grid, modes, and replies. Key and mouse encoding stay in shepr.
* **Blind-compile strategy:** keep the **public API of `src/ghostty/mod.rs`
  unchanged** and reimplement it on top of alacritty. That includes `Terminal`,
  `RenderState`, `RowIterator`, `RowCells`, `RowCellIter`, and the value types.
  Only `pane/input.rs` and the key/mouse paths in `pane/terminal.rs` then change
  shape. Rename `ghostty`→`vt` afterwards, once everything compiles.
* **Step 0:** add `alacritty_terminal = "=<ver>"` to Cargo.toml and let cargo
  *fetch* it (`cargo fetch` downloads without building), so the adapter is written
  against real source, not memory.

---

## 1. What shepr uses from libghostty

### 1.1 Glue size

| File | Lines | Notes |
|---|---|---|
| `src/ghostty/bindings.rs` | 5376 | bindgen output; delete |
| `src/ghostty/mod.rs` | 2981 | ~2200 safe wrapper + ~780 tests (lines 2209-2981) |
| `src/pane/terminal.rs` | 6701 | ~3280 prod (1-3281) + ~3420 tests; 513 ghostty refs |
| `src/pane/input.rs` | 350 | 100% ghostty key/mouse event glue (uses `ffi::GhosttyKey_*`) |
| `src/pane/osc.rs` | 1433 | 3 fns take `&mut ghostty::Terminal` (write OSC 10/11 into core) |
| `src/pane/xtgettcap.rs` | 335 | raw-C1 XTGETTCAP only; 7-bit DCS answered by ghostty today |
| `src/pane/kitty_keyboard.rs` | 227 | shepr-side kitty flag stack + modifyOtherKeys tracker (keep) |
| `src/pane.rs` | - | `Terminal::new` (l.1984), compression task (l.1122-1290), `encode_focus` (l.2885) |
| `build.rs` | 97 | zig build + link; SHEPR_BUILD_* rerun lines are redundant (`option_env!` is tracked by rustc) |
| `scripts/*libghostty*` | 149 | vendoring / bindgen / build scripts |
| `vendor/libghostty-vt` | 23 MB | + `vendor/patches/libghostty-vt/` (5 patches), `.patches.md`, `.vendor.json` |

Other references are small: `copy_mode.rs`, `app/actions.rs`
(`unicode_codepoint_width`), `app/api.rs`, `app/api/agents.rs`,
`server/headless.rs`, `server/headless/client_views.rs`, `terminal/runtime.rs`
(`FocusEvent`, `encode_focus`), `terminal/history_read.rs`,
`server/alt_screen_read.rs` (`ScreenTextRow`/`CellWide`/`ActiveScreen`
data types only), and `protocol/render_ansi.rs` (one test).

`PaneTerminal` (terminal.rs:213) is already a facade over
`GhosttyPaneTerminal`. No code outside `pane/` touches `ghostty::Terminal`
except tests, `pane.rs` construction, and `osc.rs`.

### 1.2 Capability inventory

Hot = per byte/chunk or per render × panes × clients (see AGENTS.md "multiplicative paths").

| # | Capability | ghostty API used | shepr users | Centrality | Hot? |
|---|---|---|---|---|---|
| 1 | VT parse into grid | `Terminal::write` | `process_pty_bytes`, `seed_history_ansi`, osc.rs (writes OSC 10/11), resize replay | core | **per chunk** |
| 2 | Primary/alt screen | `active_screen()` | input_state, wheel_routing, detection read range, alt_screen_read, history_read, theme restore | core | per query |
| 3 | Scrollback, **byte-limited** | `OPT_SCROLLBACK_MAX_BYTES`, `total_rows`, `scrollback_rows`, `scrollbar`, `max_scrollback` | config `scrollback_limit_bytes` (default 10 MB) | core | no |
| 4 | Viewport scrolling | `scroll_viewport_{bottom,delta,row}`, `scrollbar` | scroll_up/down/reset, `set_scroll_offset_from_bottom`, `scroll_metrics` | core | UI events |
| 5 | Reflow on resize | `resize(cols,rows,cw,ch)` | `GhosttyPaneTerminal::resize` (+ shepr's blank-bottom replay hack, l.1530) | high | resize |
| 6 | Cursor pos/visibility/style | `RenderState::cursor()`, `cursor_y` | `cursor_state`, render, detection read range; DECSCUSR shape via shepr `DecscusrTracker` | core | per render |
| 7 | Modes (DECSET) | `mode_get/mode_set` for 1, 1004, 1005, 1006, 1007, 1016, 2004, 2026, 2027, 2031, 9/1000/1002/1003; `mouse_tracking_enabled` | `input_state`, `bracketed_paste_enabled`, `focus_reporting_enabled`, `wheel_routing`, `plain_page_keys_use_host_scrollback`, render gating | core | per input/render (scalar) |
| 8 | Synchronized output 2026 | `mode_get(2026)` | render/dirty-patch suppression, `synchronized_output_epoch` | high | per chunk/render |
| 9 | Kitty keyboard flags | `kitty_keyboard_flags()` | `keyboard_protocol()`, Enter special case; shepr also tracks the stack itself (`KittyKeyboardTracker`, replay on reattach) | high | per key |
| 10 | modifyOtherKeys | `modify_other_keys_enabled()` (local patch 0002) | `input_state`, Enter special case; **shepr tracker already has the level** | medium | per key |
| 11 | Key encoding | `KeyEncoder` + `KeyEvent` (only for **non-Char** keys; Char keys already go to `input::encode_terminal_key`) | `encode_terminal_key_once` | high | per key |
| 12 | Mouse encoding | `MouseEncoder` (X10/normal/UTF-8/SGR/SGR-pixels, mode filtering) | `encode_mouse_{button,motion,wheel}` | high | per mouse event |
| 13 | Focus encoding | `encode_focus` (`ESC[I`/`ESC[O`) | pane.rs, runtime.rs, api, headless | low | no |
| 14 | Query replies (DA1/DA2, DSR 5/6, DECRQM, XTGETTCAP 7-bit, CSI ?u, CSI 14/16/18t size, OSC 4/10/11/12 queries, CSI ?996n colour scheme) | `write_pty` callback → `pending_pty_responses`; `size_trampoline`, `color_scheme_trampoline`, `OPT_TERMINFO_NAME` | `write_pty_bytes_with_ordered_responses` merges ghostty replies with shepr's own OSC colour and C1-XTGETTCAP replies in byte order | high | per chunk |
| 15 | OSC 7 cwd | `pwd_changed` callback | `reported_cwd` | medium | per chunk |
| 16 | OSC 52 clipboard write | clipboard callback (text/plain, ≤192 KiB, queries never answered) | `clipboard_writes` | medium | per chunk |
| 17 | BEL | bell callback | `terminal_bells` | low | per chunk |
| 18 | Titles / OSC 9 progress | **not from ghostty**: shepr `AgentOscStateTracker` scans raw bytes | - | - | - |
| 19 | OSC 8 hyperlinks | `has_hyperlink` cell flag, `viewport_hyperlink_uri` | render dirty-patch (fallback when present), `visible_hyperlinks` | medium | per render |
| 20 | Palette / default colours | `set_default_palette`, `default_palette`, `RenderState::colors()`, `effective_{foreground,cursor}_color`, OSC 10/11 writes | host theme application, `PaletteOverrides` (forward indexed unless redefined), OSC colour query replies | high | per render (256-entry compare) |
| 21 | Cell style for rendering | `RowCellIter::basic_data/style/fg_color/bg_color/content_bg_color/grapheme_text_into/wide` | `render`, `ghostty_collect_dirty_patch` | core | **per cell per render** |
| 22 | Damage tracking | `RenderState::dirty()`, `RowIter::next_dirty/clear_dirty/set_dirty`, `clean()` | `collect_dirty_patch` (per-row patches to clients), `render` | high (perf) | **per render** |
| 23 | Unicode width / graphemes | mode 2027 default on; `unicode_codepoint_width`, `unicode_grapheme_width` | copy_mode.rs, app/actions.rs, symbol width normalisation in terminal.rs | medium | per cell |
| 24 | Text extraction, cell rows | `screen_text_rows[_range]` (cells + `soft_wrapped` + `wrap_continuation`), `screen_cell` | search, word/paragraph motions, `detection_text`/recent reads, alt_screen_read, history_read | high | detection tick × panes |
| 25 | Text extraction, formatter | `read_text_{viewport,screen}`, `read_ansi_{viewport,screen}(unwrap)` (Plain/VT, trim, unwrap soft wraps) | `extract_selection`, `visible_ansi`, `recent_*_snapshot`, **history persistence** (`recent_unwrapped_ansi` → `seed_history_ansi` on restore), resize replay | high | on demand |
| 26 | Clear screen+history, keep cursor line | `clear_screen` (local patch 0006) | `PaneTerminal::clear_screen` | low | no |
| 27 | Incremental page compression | `compression_activity`, `compress_incremental` | `TerminalCompressionTask` in pane.rs | none (ghostty-specific) | background |
| 28 | Kitty unicode placeholder filtering | constant `0x10EEEE` | render/text | trivial | - |
| - | Core-side selection | `RowIter::selection()` always `None` (shepr draws its own) | dirty patch | none | - |
| - | Bounded word selection (patch 0005) | test only (`link_target_*`) | none in prod | none | - |
| - | Kitty graphics / PNG (patch 0007) | `GLYPH_PROTOCOL=false`; not rendered | none | none | - |

**Who generates query replies today:** ghostty, via `write_pty`, for DA/DSR/DECRQM/XTGETTCAP/CSI ?u/size/OSC colour/?996n. Shepr then patches the stream. It
replaces OSC 10/11/12/4 replies with host-theme answers (`remove_last_matching_libghostty_color_reply`)
and adds raw-C1 XTGETTCAP replies (`xtgettcap.rs`). After the swap, shepr has to own more of this.

---

## 2. Candidates

### 2.1 Summary

| Candidate | What it is | License | Size | Deps | Maint. | Verdict |
|---|---|---|---|---|---|---|
| **alacritty_terminal** *(memory)* | Alacritty's emulator core; used by Zed | Apache-2.0 | ~15k | vte(ansi), bitflags, log, parking_lot, regex-automata, unicode-width, base64, home, libc; unix tty module pulls polling/rustix-openpty/signal-hook (compiled, unused) | active | **Primary** |
| **par-term-emu-core-rust** (read) | "Everything" emulator lib + Python + streaming + mux | MIT | 111k lines src | even with `sim`: vte, image(+rayon, 10 codecs), swash, flate2, regex, serde(+json, yaml_ng), url, uuid, lru, parking_lot, smallvec, unicode-* | very active, fast churn (0.45→0.51), MSRV **1.98** | Fallback |
| wezterm-term *(memory)* | WezTerm's emulator | MIT | large | termwiz (pest, phf, fancy-regex, …), wezterm-* crates, image, lru | active but **not on crates.io** (git dep on monorepo, or `tattoy-wezterm-term` fork) | not recommended |
| vt100 (atuin fork 0.19.1, read) | Small emulator for tmux-likes | MIT | 4.9k | vte 0.15, unicode-width, itoa | active (2026-09) | too thin |
| fux-vt (read) | Bounded non-reflowing emulator | MIT | ~4k | unicode-width; edition 2024, MSRV 1.95 | active | too thin |
| ansi-rs / nativelite-ansi (read) | Tokenizer + SGR + diff | MIT | - | none | - | not an emulator |
| vterm-rs (read) | Toy emulator on ansi-rs | MIT | 850 | path dep `../uwidth-rs` (missing) | - | no |
| term-wm (read) | Window manager app; uses `term-wm-vt100` fork | MIT/Apache | - | - | - | app, not lib |
| winter-term (read) | GPU terminal; grid in `winter-render` (wgpu/glyphon/resvg), block-list in `winter-core` | MIT | - | wgpu… | - | not a lib; `grid/reflow.rs` (803 lines) is a usable reference |
| termwiz 0.23.3 (read, in registry) | Surfaces, escape parser, input encoding | MIT | - | heavy | - | not an emulator; `KeyboardEncoding::Kitty` declared but not implemented in `encode` |

### 2.2 Capability matrix

yes covered · ◐ partial · no missing · H = shepr already has it / can do it cheaply outside the core

| # | Capability | alacritty_terminal *(memory)* | par-term (read) | vt100 (read) | fux-vt (read) |
|---|---|---|---|---|---|
| 1 | VT parse → grid | yes `vte::ansi::Processor::advance(&mut term, bytes)` | yes `Terminal::process` | yes `Parser::process` | yes |
| 2 | Alt screen | yes `TermMode::ALT_SCREEN`, 1049/47/1047 | yes `is_alt_screen_active` | yes | yes (no 1047) |
| 3 | Scrollback | yes lines (`Config::scrolling_history`, max 100k); bytes→lines conversion needed | yes lines (`with_scrollback`) | yes lines | yes lines ring |
| 4 | Viewport scroll | yes `scroll_display(Scroll::Delta/Top/Bottom)`, `grid().display_offset()`, `history_size()` | ◐ none in core; host keeps offset, reads `scrollback_line` | ◐ `set_scrollback` | ◐ windows |
| 5 | Reflow | yes primary (alt not reflowed) | yes primary, alt truncates | no | no by design |
| 6 | Cursor + style | yes `grid().cursor.point`, `cursor_style()` {Block/Underline/Beam/HollowBlock, blinking}, `SHOW_CURSOR` | yes | ◐ no style | ◐ |
| 7 | Modes 1/1004/1005/1006/1007/2004/9-1003 | yes (`APP_CURSOR`, `FOCUS_IN_OUT`, `UTF8_MOUSE`, `SGR_MOUSE`, `ALTERNATE_SCROLL`, `BRACKETED_PASTE`, `MOUSE_REPORT_CLICK/DRAG/MOTION`); ◐ X10 (9) likely missing | yes (no 1007) | ◐ (no 1004/1007) | ◐ |
| 7b | 1016 SGR-pixels, 2031 colour-scheme report, ?996n | no → **H** pre-scan tracker | no → H | no | no |
| 8 | Sync output 2026 | ◐ Processor buffers BSU..ESU itself (with timeout); query via `processor.sync_timeout()`; host must `stop_sync` on expiry | yes flag + `flush_synchronized_updates` | no | no |
| 9 | Kitty kb flags | yes stack per screen (needs `Config::kitty_keyboard = true`); answers CSI ?u. **H** tracker exists anyway | yes `keyboard_flags`, auto-reset on alt exit | no | no |
| 10 | modifyOtherKeys state | ◐/no → **H** (`KittyKeyboardTracker::modify_other_keys_level`) | yes `modify_other_keys_mode` | no | no |
| 11 | Key encoding | no (lives in alacritty app) → **H** `input/encode.rs` (needs gaps closed, §3.3) | no (host) | no | no |
| 12 | Mouse encoding | no → **H** `input/encode.rs::encode_mouse_*` (dead code today, add mode filter + SGR-pixels) | yes `Terminal::report_mouse` (no pixels) | no | no |
| 13 | Focus encoding | no → H (2 constants) | yes | no | no |
| 14 | Query replies | yes DA1/DA2/DSR/CPR/DECRQM/CSI ?u/CSI 14t,18t via `Event::PtyWrite`, `Event::TextAreaSizeRequest(fn)`; yes OSC 4/10/11/12 queries via `Event::ColorRequest(idx, fmt)` (**host answers**, which is exactly what shepr wants); no XTGETTCAP → H (extend `xtgettcap.rs` to 7-bit DCS); no ?996n → H | yes DA/DSR/DECRQM/XTGETTCAP/DECRQSS/XTVERSION/XTWINOPS into `drain_responses()`; OSC colour queries answered **internally** (shepr must strip them as today) | no none | ◐ DA1/DSR only |
| 15 | OSC 7 | no → H raw-byte tracker (osc.rs already scans OSC) | yes `current_directory`, `poll_cwd_events` | no (unhandled_osc cb) | no |
| 16 | OSC 52 write | yes `Event::ClipboardStore(ty, String)` (UTF-8 only), `Config::osc52` | yes | yes callback | ◐ opt-in event |
| 17 | BEL | yes `Event::Bell` | yes `bell_count` | yes cb | ◐ |
| 19 | OSC 8 | yes `cell.hyperlink().uri()` | yes `hyperlink_id` + `get_hyperlink_url` | no | no |
| 20 | Palette/default colours | yes `term.colors()[i]` = child overrides only (`Option<Rgb>`, incl. `NamedColor::Foreground/Background/Cursor`); defaults owned by shepr, a clean fit | ◐ theme default_fg/bg are concrete; **cells store concrete colours, so "default" is not distinguishable from palette 7/0** | ◐ `Color::Default` yes | yes default |
| 21 | Cell attrs | yes bold/dim/italic/inverse/hidden/strike, underline single/double/curl/dotted/dashed, underline colour, `Color::{Named(Foreground/Background)=default, Indexed, Spec}`, wide flags `WIDE_CHAR`/`WIDE_CHAR_SPACER`/`LEADING_WIDE_CHAR_SPACER` (= ghostty Wide/SpacerTail/SpacerHead). no blink, no overline | yes all incl. blink/overline, 5 underline styles | ◐ bold/dim/italic/underline/inverse only | ◐ same as vt100 |
| 22 | Damage | yes `term.damage()` → `TermDamage::{Full, Partial(iter of LineDamageBounds)}`, `reset_damage()`; viewport-relative; used by Alacritty's renderer, so reliable | ◐ bitset set **only** in write.rs char paths, not scroll/erase/alt switch | no | yes row versions + structural generation |
| 23 | Graphemes/width | ◐ zero-width chars appended (`cell.zerowidth()`); width per codepoint (unicode-width); no ZWJ/flag clustering (mode 2027) | yes grapheme clusters, VS15/16, ZWJ, flags | ◐ combining only | ◐ combining only |
| 24 | Row text + wrap flags | yes `grid()[Line(i)][Column(j)]`, `WRAPLINE` flag on last cell; history is negative `Line` | yes `row`, `is_line_wrapped`, `scrollback_line`, `is_scrollback_wrapped` | yes `row_wrapped` | yes |
| 25 | Formatter (plain/VT, unwrap) | ◐ `bounds_to_string` (plain, joins wraps); no VT → **write own** | yes `export_text_buffer`, `export_styled_buffer`, `export_scrollback_styled` | yes `contents_formatted`, `rows_formatted` | ◐ copy only |
| 26 | Clear screen keep cursor line | ◐ call `Handler` methods directly on `Term` (parser-independent): `clear_screen(ClearMode::Saved)` + line moves | ◐ | ◐ | ◐ |
| 27 | Compression | n/a, drop | n/a | n/a | n/a |
| - | Rust/MSRV | ~1.74-1.85 (memory) | 1.98 (toolchain has 1.98.0) | 1.70 | 1.95, ed. 2024 |
| - | Hot-path perf | table-driven vte; direct grid indexing | vte; wide cells (~60+ B) | fine | fine |

Notes on the "read" rows:

* par-term: `Cell { c, combining: SmallVec<[char;4]>, fg, bg, underline_color, flags, width }`.
  `Cell::default().fg = Named(White)`, `bg = Named(Black)`, and SGR 39/49 write
  `theme.default_fg/bg` (terminal/mod.rs:721, csi tests:223). `mark_row_dirty`
  is only called from `terminal/write.rs` (and one site in mod.rs:3479). Dirty
  state is a single bitset with no Partial/Full semantics.
* vt100: `perform.rs::csi_dispatch` has no `c`/`n` arms, so DA/DSR go to
  `Callbacks::unhandled_csi`. There is no reflow. `Callbacks` covers bell, title, clipboard,
  resize request, and unhandled CSI/OSC.
* fux-vt README: "Excluded: paragraph reflow; graphics protocols; kitty
  keyboard; grapheme segmentation beyond a base glyph plus combining marks".

### 2.3 Why alacritty over par-term

1. **Default-colour semantics.** Shepr forwards "default" fg/bg to the host so the
   host theme applies (`ghostty_default_fg/bg`, `ghostty_reset_cell`), and it keeps
   palette indices unless OSC 4 redefined them (`PaletteOverrides`).
   Alacritty's `Color::Named(Foreground|Background)` and override-only
   `colors()` match this 1:1. par-term would need a sentinel-colour hack that
   conflicts with shepr writing OSC 10/11 into the core.
2. **Damage tracking** is complete and matches ghostty's RenderState
   dirty model (Full / Partial rows). par-term's is incomplete.
3. **ColorRequest events** let shepr answer OSC colour queries itself, in order.
   That removes the "strip ghostty's reply, append ours" machinery.
4. **Weight and churn.** par-term compiles an image stack and a font rasteriser, and
   it breaks its API often.
5. The fallback still has value. par-term's source is local and readable now,
   and it covers OSC 7, grapheme clusters, XTGETTCAP, and a styled export, if alacritty
   turns out to be unworkable.

---

## 3. Migration plan (alacritty_terminal)

### 3.0 Pre-step (before writing code)

1. Add `alacritty_terminal = "=0.25.x"` (pin exact; check the latest), let cargo
   **fetch** sources, and read `term/mod.rs`, `term/cell.rs`, `grid/`, `event.rs`,
   and vte `ansi.rs` from `~/.cargo/registry`. Confirm these (all from memory):
   `Term::new(Config, &impl Dimensions, listener)`, the `Processor<StdSyncHandler>`
   generic plus `sync_timeout()`/`stop_sync()`, the `Config` field names
   (`scrolling_history`, `kitty_keyboard`, `osc52`), whether `term::test::TermSize` is public,
   `Event` variants, `TermDamage` iteration, `Cell::hyperlink()`, `NamedColor` indices,
   and DECRQM/CSI ?u support.
2. If the owner's workflow forbids fetching, write against par-term instead, whose
   source is local, and accept its gaps.

### 3.1 Adapter shape: keep the `crate::ghostty` API, swap the guts

Rewrite `src/ghostty/mod.rs` (delete `bindings.rs`) so that it exposes the **same
names and signatures**. That way `pane/terminal.rs` (3.3k prod lines) compiles
nearly untouched on the first try. Rename to `src/vt/` in a second pass.

```text
pub struct Terminal {
    term: alacritty_terminal::Term<Listener>,
    parser: vte::ansi::Processor,            // holds sync-update buffer
    listener_state: Arc<Mutex<ListenerState>> or RefCell inside Listener (Term must stay Send)
    default_palette: [RgbColor; 256],        // shepr-owned defaults (alacritty stores overrides only)
    default_fg/bg: RgbColor,
    cell_px: (u32, u32), max_scrollback_bytes: usize,
    extra_modes: ExtraModes,                 // 1016, 2031, X10(9) via pre-scan
    osc7: Osc7Tracker, write_pty: Option<Box<dyn FnMut(&[u8]) + Send>>,
    color_scheme: Option<ColorScheme>,
}
struct Listener(Arc<Mutex<Vec<Event>>>);   // impl EventListener { fn send_event(&self, e) { push } }
```

`Terminal::write(bytes)` does three things:
1. Run the pre-scan trackers on the bytes: OSC 7, DECSET/DECRST 9/1016/2031, CSI ?996n,
   7-bit XTGETTCAP (moved from shepr trackers), and DECRQM for those modes.
2. `parser.advance(&mut term, bytes)`.
3. Drain the listener events **in order**. `PtyWrite(s)` goes to `write_pty(s)`.
   `ColorRequest(i, fmt)` gets `fmt(self.effective_color(i))` passed to `write_pty`, keeping the
   current ghostty behaviour, and shepr's existing override logic still runs on top. `TextAreaSizeRequest(fmt)`
   gets `fmt(WindowSize{cell px…})`. `Bell` increments `bell_count`. `ClipboardStore` is
   pushed to `clipboard_writes` (size cap). Titles are ignored because shepr tracks
   them.

Method mapping (same names as today):

| ghostty wrapper method | alacritty implementation |
|---|---|
| `new(cols, rows, max_scrollback_bytes)` | `Config { scrolling_history: bytes_to_lines(bytes, cols), kitty_keyboard: true, osc52: OnlyCopy, .. }`; `bytes_to_lines = clamp(bytes / (cols * size_of::<Cell>()), 0, 100_000)`. `0` must give zero history (test `zero_max_scrollback_disables_history`) |
| `resize(c, r, cw, ch)` | `term.resize(TermSize::new(c, r))`; store px |
| `mode_get(m)` / `mode_set` | match m to `TermMode` bits; 2026 → `parser.sync_timeout().sync_timeout().is_some()`; 1016/2031/9 → `extra_modes`; 2027 → `true` (constant); `mode_set` only in tests (1, 1004, 2027, 2048), so implement 1/1004 by feeding `CSI ? n h` |
| `kitty_keyboard_flags()` | `term.mode() & KITTY_KEYBOARD_PROTOCOL` bits → u8 (or return shepr tracker value) |
| `modify_other_keys_enabled()` | delete; use `kitty_keyboard.modify_other_keys_level() == 2` (patch 0002 dies) |
| `mouse_tracking_enabled()` | `MOUSE_MODE` any \| X10 extra |
| `active_screen()` | `ALT_SCREEN` |
| `total_rows()` / `scrollback_rows()` / `rows()` / `cols()` | `history_size()+screen_lines()` / `history_size()` / `screen_lines()` / `columns()` |
| `scrollbar()` | `total = hist+lines, len = lines, offset = hist - display_offset` |
| `scroll_viewport_*` | `scroll_display(Scroll::Bottom / Delta(-delta) / Delta(target-current))`. **Sign: ghostty delta<0 = up; alacritty Delta>0 = up** |
| `cursor_y()` | `grid().cursor.point.line.0` |
| screen coord y (0 = oldest) | `Line(y as i32 - history as i32)`; viewport y → `Line(y as i32 - display_offset as i32)` |
| `screen_text_rows_range` / `screen_cell` | iterate grid rows; `CellWide` from flags; graphemes = `[c] + zerowidth()`; `soft_wrapped` = last cell `WRAPLINE`; `wrap_continuation` = previous row's `WRAPLINE` |
| `viewport_hyperlink_uri` | `cell.hyperlink().map(|h| h.uri().to_owned())` |
| `read_text_*` / `read_ansi_*` | **own formatter** (§3.2) |
| `clear_screen()` | direct `Handler` calls on `term` (no parser): `clear_screen(ClearMode::Saved)`, then move the cursor's soft-wrapped line to the top (e.g. `scroll_up(n)` via the scroll region) and `clear_screen(ClearMode::Below)`. Or simplify to history+screen clear |
| `set_default_palette` / `default_palette` | adapter fields |
| `effective_foreground_color` / `effective_cursor_color` | `term.colors()[NamedColor::Foreground/Cursor]` (`Option`) |
| `width_px` / `height_px` | `cols*cw`, `rows*ch` |
| `take_bell_count` / `take_pwd_changes` / `take_clipboard_writes` / `set_write_pty_callback` / `set_color_scheme` | adapter state (same semantics) |
| `compression_activity` / `compress_incremental` | return `Ok(0)` / `Ok(Unsupported)` for pass 1, then delete the pane.rs task |
| `RenderState::update(&Terminal)` | copy viewport into owned `Vec<RowSnapshot{cells: Vec<SnapCell>, dirty: bool}>`: `TermDamage::Full` means recopy all and mark all dirty (`Dirty::Full`); `Partial` means recopy the damaged lines only (`Dirty::Partial`); then `term.reset_damage()`. A display-offset change forces Full. This reproduces ghostty's contract: dirty flags persist until shepr clears them (`clear_dirty`, `set_dirty(Clean)`, `clean()`) |
| `RenderState::{cols,rows,dirty,cursor,colors,clean,set_dirty}` | from the snapshot; `colors()` = overrides ∪ adapter defaults |
| `RowIterator` / `RowIter::{next,next_dirty,dirty,clear_dirty,set_dirty,selection(→None),populate_cells}` / `RowCells` / `RowCellIter::{next,select,basic_data,wide,has_hyperlink,style,content_bg_color(→None),fg_color,bg_color,grapheme_text[_into]}` | plain Rust iterators over the snapshot; keep names, drop FFI. `fg_color/bg_color` return the resolved RGB only when the style colour is None and the child set a default (mirror ghostty: `None` means use default) |
| `unicode_codepoint_width` / `unicode_grapheme_width` | `unicode-width` crate (already a dep) + `unicode-segmentation` (already a dep) |
| `encode_focus` | constants |
| `KeyEncoder` / `KeyEvent` / `MouseEncoder` / `MouseEvent` | **delete**; see §3.3 |

### 3.2 Formatter (write ourselves, ~250 lines)

Used by `extract_selection`, `visible_ansi`, `recent_*_snapshot`, the resize
replay, and **history persistence** (`recent_unwrapped_ansi` → saved → `seed_history_ansi`).
It must round-trip through our own parser:
* Plain: walk cells from start to end (inclusive, screen or viewport coords), skip
  `SpacerTail`, turn `SpacerHead` into nothing, empty cell into space, trim row ends when `trim`, and
  join soft-wrapped rows with no newline when `unwrap`. Otherwise use `\n`.
* VT: same walk, plus minimal SGR transitions (reset, 1/2/3/4:x/7/8/9, 38/48/58 with
  `5;n` or `2;r;g;b`, default → 39/49/59). Wrap OSC 8 open/close on hyperlink
  change. No cursor positioning.
* Byte-exact equality with ghostty's formatter is not required, but the tests in
  terminal.rs that assert exact strings (e.g. around l.5139-5260) will need their
  expectations regenerated.

### 3.3 Input encoding (shepr-owned; close the gaps ghostty was covering)

Today non-Char keys go to ghostty's `KeyEncoder`. With it gone,
`input::encode_terminal_key` has to cover the following. The terminal.rs tests
4092-4790 are the spec.
* **DECCKM**: unmodified arrows/Home/End use `ESC O x` when mode 1 is set. There's a dead
  `encode_cursor_key` helper. Thread `application_cursor` into the encoder, e.g.
  via a `KeyEncodeModes { kitty_flags, modify_other_keys, app_cursor, app_keypad }`
  argument built from `input_state` scalars.
* **Kitty functional keys: bug.** `try_encode_csi_u` maps arrows/Home/End/
  PgUp/PgDn/Ins/Del to **57417-57426**, which are the *keypad* (KP_*) codes.
  Correct kitty encoding: `CSI 1;mods[:ev] A/B/C/D/H/F`,
  `CSI 2/3/5/6;mods[:ev] ~`, F1-F4 `CSI 1;mods P/Q/S` (F3 = `CSI 13~`), F5-F12 `CSI n;mods ~`,
  Enter/Tab/Bksp/Esc as `13/9/127/27 u` under report-all/event-types. This path was masked
  because ghostty handled these keys. `input/parse.rs` maps the KP codes back to plain arrows,
  which hides the bug in round-trip tests.
* **modifyOtherKeys** (level from `KittyKeyboardTracker`): `CSI 27;mod;code ~`
  for modified Enter/Tab/Bksp/Esc and (level 2) modified chars. Expected by
  tests `ghostty_modified_enter_respects_existing_terminal_mode`
  (`\x1b[27;2;13~`) and `..._mode_one_preserves_shift_enter`.
* Keep the "Enter with modifiers → legacy unless negotiated" rule; it becomes the
  natural default.
* **Mouse**: use `encode_mouse_button/scroll` (already written, currently
  `#[allow(dead_code)]`). Add a motion encoder. Filter by protocol mode (X10:
  press only, no mods; 1000: no motion; 1002: drag only; 1003: any). Add SGR-pixels
  (1016): pixel coordinates when `Position::Pixels`, otherwise downgrade to cells (test l.4769).
  `pane/input.rs` shrinks to the crossterm→encoder mapping or disappears.
* **Focus**: `b"\x1b[I"` / `b"\x1b[O"`.

### 3.4 Responses and trackers shepr must own

| Item | Where |
|---|---|
| OSC 7 cwd | new small tracker (pattern of `AgentOscStateTracker`), feeding `take_pwd_changes` |
| 7-bit XTGETTCAP (`ESC P + q … ESC \`) incl. `TN` → `PANE_TERM` | extend `xtgettcap.rs` (drop the `raw_c1_intro`-only gating and the `suppress_native` dance) |
| CSI ?996n colour-scheme DSR, mode 2031 set/reset/DECRQM | tracker plus reply from `color_scheme` |
| DECSET 1016 / 9 state (+ DECRQM replies for them) | tracker |
| OSC 10/11/12/4 query replies | `ColorRequest` handling (host theme first, as today). Delete `remove_last_matching_libghostty_color_reply` and simplify `write_pty_bytes_with_ordered_responses` to "write chunk, drain replies in order" |
| XTVERSION (`CSI > q`) | optional; drop |

### 3.5 Things to drop rather than reimplement

* Incremental page compression (pane.rs `TerminalCompressionTask`, ~200 lines,
  plus `TerminalCompressionStep` in terminal.rs).
* Bounded word selection (patch 0005): test-only.
* Kitty graphics / PNG retention (patch 0007): unused.
* Grapheme-cluster mode 2027. Accept alacritty's per-codepoint widths. The
  width-normalisation guard in `ghostty_buffer_symbol_into` already handles
  metadata/symbol width disagreement. Affected tests: `render_cells_preserve_issue_453_unicode_payload_exactly`,
  flag/family emoji tests (l.4882-4930), `grapheme_cluster_mode_is_default…`.
* Exact ghostty DA/XTGETTCAP strings.
* Blink and overline attributes (alacritty has no flag for them). They render as plain text.
* Optionally, "clear screen but keep cursor line" (patch 0006). Replace it with a plain
  history+screen clear if the direct-Handler approach is awkward.

### 3.6 Effort estimate

| Area | New/changed lines | Days |
|---|---|---|
| Adapter `Terminal` + listener + mode/coord mapping + scrollbar/viewport | ~700 | 1.5-2 |
| RenderState/RowIterator shim with damage | ~300 | 1 |
| Formatter (plain + VT, unwrap/trim) | ~250 | 0.5-1 |
| Replies/trackers (§3.4) + simplifying ordered-response code | ~300 | 1 |
| Key encoder completion (DECCKM, kitty functional table, modifyOtherKeys) + mouse filter/pixels, delete `pane/input.rs` glue | ~350 | 1-1.5 |
| Drop compression, unicode width swaps, focus constants, build.rs/Cargo cleanup | ~ -400 | 0.5 |
| Test triage (≈4.2k test lines written against ghostty; expect dozens of expectation changes) | - | 2-3 |
| **Total** | | **~8-10 days** |

### 3.7 Biggest risks (first compile only after the swap)

1. **API from memory.** alacritty_terminal changes signatures between minors
   (Processor generics and sync handler, `Term::new`, `Dimensions` location, `Config`
   fields). *Mitigation:* pin an exact version and read the fetched source before writing (§3.0).
   Keep all alacritty types inside `src/ghostty/mod.rs` so compile errors stay in one file.
2. **Semantic drift that compiles fine:** scroll delta sign, `Line` negative-index
   mapping, 0- vs 1-based `Column`, `WRAPLINE` living on the *last cell* of a row, damage lines
   being viewport-relative and reset by display scroll. These show up as wrong text or
   frozen panes, not compile errors. *Mitigation:* port the wrapper tests
   (`ghostty/mod.rs` 2209-2981) first and run them before the pane tests.
3. **Sync updates.** vte buffers BSU..ESU inside `Processor` and needs a host
   call when the timeout expires. If shepr never calls `stop_sync`, a program that
   forgets ESU freezes the pane until more output arrives. *Mitigation:* on every
   `write` and every render check `sync_timeout()`, then flush once past the deadline. Keep
   `synchronized_output_epoch` bumps on transitions.
4. **Reply ordering and completeness.** Shepr's byte-ordered merge logic was
   built around ghostty. Missing replies (XTGETTCAP, ?996n, DECRQM for 1016/2031)
   make agents mis-detect capabilities or hang waiting for them. *Mitigation:* a
   table of every query shepr answered before, with a test each.
5. **Key encoding regressions.** Arrows, modifyOtherKeys, and kitty functional keys
   were ghostty's job, and shepr's fallback has the keypad-code bug. *Mitigation:*
   the existing encoding tests are the spec. Add a corpus test that round-trips through
   `input/parse.rs` and asserts that **non-keypad** arrows come out as `CSI 1;m A`.
6. **Scrollback memory.** alacritty counts lines, shepr configures bytes. Convert from
   `size_of::<Cell>()` and cols (recompute on column change via `term.set_options`).
   Otherwise the default 10 MB can turn into 100k lines × 15 panes.
7. **History persistence round-trip.** The new VT formatter output is replayed on restore
   and after resize. Any SGR/hyperlink/wrap mistake corrupts restored panes. *Mitigation:* a
   round-trip test (format, feed into a fresh Terminal, compare cells).
8. **Graphemes.** ZWJ emoji become several wide cells, so shepr-side width
   computations (copy mode, search spans) may drift from what the host shows.

---

## 4. Deletion checklist (once green)

* `vendor/libghostty-vt/` (23 MB), `vendor/libghostty-vt.vendor.json`,
  `vendor/libghostty-vt.patches.md`, `vendor/patches/libghostty-vt/` (0002, 0004,
  0005, 0006, 0007).
* `src/ghostty/bindings.rs`. Rename `src/ghostty/` to `src/vt/` (update `mod ghostty;`
  in `src/main.rs:24` and ~500 `crate::ghostty::` paths; do the rename last).
* `build.rs` entirely, plus `build = "build.rs"` in `Cargo.toml`. SHEPR_BUILD_* are read
  with `option_env!` in `src/build_info.rs`, which rustc already tracks. Also drop the external-contributor
  `cargo:warning`.
* `scripts/build_vendored_libghostty_vt.sh`, `scripts/generate_libghostty_bindings.sh`,
  `scripts/vendor_libghostty_vt.py` (then `scripts/` is empty).
* `brokkr.toml` `[gremlins] exclude = ["vendor"]` (removed in pass 2 together
  with vendored portable-pty).
* AGENTS.md sections: "Vendored libghostty-vt", Windows/Zig SDK notes, the
  `just check` patch-verification mention, and `LIBGHOSTTY_VT_*` env vars.
* `.gitignore` has no ghostty or zig entries (checked). `zig-out/` sits
  inside the vendored tree, so it goes with the tree.
* pane.rs `TerminalCompressionTask`/`run_terminal_compression_task`;
  terminal.rs `TerminalCompressionStep`, `try_compression_activity`,
  `try_compress_incremental_if_activity`.
* `pane/input.rs` ghostty event builders; `remove_last_matching_libghostty_color_reply`,
  `is_matching_libghostty_color_reply`, the `suppress_native` path in `xtgettcap.rs`.
* Tests: `build_info_contract_matches_expected_vendored_features`,
  `incremental_compression_preserves_cold_scrollback`,
  `link_target_selection_budget_is_shared_across_both_directions`,
  `row_cell_basic_data_uses_batched_vendor_reads`, the clipboard-callback FFI tests
  (`invoke_clipboard_callback`, `test_clipboard_content`).
