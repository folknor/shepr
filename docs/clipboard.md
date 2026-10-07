# Clipboard

shepr captures the mouse (`mouse_capture = true` under `[ui]`, the default),
so the host terminal does not make its own text selections inside the shepr
window. shepr selects text itself and copies it to the clipboard of the machine
you are sitting at: directly when the TUI runs there, through the host terminal
when it runs over SSH (see below).

## Copying

- **Mouse selection.** Drag to select, or double-click to select a word. With
  `copy_on_select = true` (the default) the text is copied when the mouse button
  is released, and the highlight is cleared. With `copy_on_select = false` the
  selection stays until `ctrl+c` copies and clears it.
- **Copy mode.** `prefix+[` selects and copies with the keyboard.
- **Programs in panes.** A program that stores text with OSC 52 to the
  clipboard (`c`) or primary-selection (`p` or `s`) target has its text
  forwarded through the server to the TUI, which copies it the same way. The
  TUI stores these copies in the background, one at a time; when a program
  copies again before the previous copy is stored, only its newest waiting copy
  is kept.

`copy_on_select` lives under `[ui]` in `client.toml`.

## Both selections

Every copy sets both the clipboard and the primary selection. The clipboard is
what `ctrl+v` pastes, and what a remote desktop session such as RDP syncs to the
remote side. The primary selection is what middle-click pastes. One selection in
shepr is therefore ready for either kind of paste, with no separate copy step.

## How the text reaches the clipboard

The TUI decides once, at launch, from its own environment:

- **Local session.** When none of `SSH_CONNECTION`, `SSH_TTY` or
  `VSCODE_IPC_HOOK_CLI` is set, shepr runs a clipboard helper:
  - On Wayland (`WAYLAND_DISPLAY` set) it uses `wl-copy`, then `wl-copy --primary`.
  - On X11 (`DISPLAY` set) it uses `xclip` or `xsel`, once for the clipboard
    and once for the primary selection.

  The copy counts as done once the clipboard helper takes the text. Setting the
  primary selection is best effort after that. If no helper is installed, or
  every one fails, shepr falls back to OSC 52.
- **SSH or VS Code remote.** When any of those variables is set, shepr writes
  OSC 52 to the host terminal, which sets the clipboard on the machine you are
  sitting at. It writes two sequences, one for the clipboard (`c`) and one for
  the primary selection (`p`).

With OSC 52, whether the copy lands is up to the host terminal. Some terminals
need OSC 52 enabled in their settings, and some honour only the clipboard target
and ignore the primary one. shepr cannot observe either.

## Pasting

- **Into a pane.** Paste with your terminal's own paste shortcut. The terminal
  sends the text to shepr, which passes it to the focused pane.
- **Middle-click.** Because shepr captures the mouse, a middle-click in a pane
  goes to shepr, not to the terminal's paste. shepr forwards it only to a
  program that asked for mouse events, and otherwise ignores it. Most terminals
  still paste the primary selection on shift+middle-click while an application
  captures the mouse.
- **Into shepr's own prompts.** In copy-mode search and in overlay prompts,
  `ctrl+v` reads through the same clipboard route selected when shepr starts.
  A local helper route reads with `wl-paste`, `xclip` or `xsel`. An SSH or VS
  Code remote route writes with OSC 52, which has no portable read operation in
  shepr; prompt paste therefore inserts nothing on that route. In particular,
  X forwarding does not make shepr read the remote display's clipboard.
