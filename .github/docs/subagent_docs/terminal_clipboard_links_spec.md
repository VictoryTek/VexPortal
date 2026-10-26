# Built-in terminal: clipboard, context menu, clickable links — spec

## Request
Claude Code's sign-in cannot be finished from VexPortal's built-in terminal (`agent` action →
`just agent` → `vexos-ai` → `exec claude`, in `src/ui/terminal_view.rs`):
1. Its "paste code here" prompt needs a terminal paste, and the `vte4::Terminal` has no copy or
   paste shortcuts and no context menu (VTE leaves both to the host).
2. It cannot open the sign-in URL in the browser, though `xdg-open https://claude.ai` works from
   GNOME Console. Fallback wanted: clickable URLs in the terminal.

## Current state
- `terminal_view::open` builds a bare `vte4::Terminal` (scrollback, scroll-on-output, CSS class).
  No key controller, no gesture, no menu, no match regexes, `allow-hyperlink` left at its default
  (off).
- `spawn` calls `spawn_async(PtyFlags::DEFAULT, …, envv, SpawnFlags::SEARCH_PATH, …)` with
  `envv = terminal::environment()` (`VEXPORTAL=1`, plus `VEXOS_AI_NONINTERACTIVE=1` for `ai-*`).
- vte4 0.8.0 is used with no features; the flake's nixpkgs (nixos-26.05) ships VTE 0.84.1.

## Findings on problem 2 (browser) — cause NOT yet identified
- **`envv` does not replace the environment.** VTE `src/vtepty.cc` `spawn_context_from_args` only
  calls `set_no_inherit_environ()` when `VTE_SPAWN_NO_PARENT_ENVV` is in the spawn flags;
  VexPortal passes only `G_SPAWN_SEARCH_PATH`. `src/spawn.cc` `merge_environ` then copies
  `g_get_environ()`, overlays `envv`, and sets `TERM`, `COLORTERM`, `VTE_VERSION`, `PWD`
  (prepends `TERMINFO_DIRS`). Nothing is unset. So the child sees VexPortal's own environment.
- **vexos-ai changes nothing relevant**: `launch()` runs `ensure_terminal` (returns at once in a
  TTY), `cd`s, optionally sets `CLAUDE_CONFIG_DIR`, then `exec claude`. No `env -i`, no sandbox.
  The `agent` recipe (`vexos-nix/just/ai.just`) is `@vexos-ai`.
- **VexPortal's own environment differs from Console's only by what launched it and by the
  wrapGAppsHook4 wrapper** (`nix/package.nix`): `--prefix PATH` (just, xdg-terminal-exec),
  `--prefix XDG_DATA_DIRS`, and the hook's own `GIO_EXTRA_MODULES` / `GDK_PIXBUF_MODULE_FILE` /
  gsettings schema dirs, all of which leak into the child. Prefixes keep the session's entries, so
  `xdg-open` should still resolve; a leaked GIO module set is a candidate for breaking
  `xdg-open` → `gio open`, but that is unconfirmed.
- The user's diagnostic (child `/proc/<pid>/environ` vs `env`) was **not run**. Per the request
  ("first find the cause; don't guess"), the root-cause fix is deferred until that output exists.
  This change ships only the clickable-link fallback for problem 2; the final report gives the
  user the diagnostic commands.

## Design
### Pure logic (`src/terminal.rs`, unit-tested)
- `enum ClipboardKey { Copy, Paste }` and `clipboard_key(gdk::Key, gdk::ModifierType)`:
  Ctrl+Shift+C → Copy; Ctrl+Shift+V, Shift+Insert (and keypad Insert) → Paste. Only
  Ctrl/Shift/Alt/Super/Meta are compared, so Caps Lock / Num Lock do not defeat it.
- `URL_PATTERN`: a PCRE2 pattern for `http(s)://` URLs that does not swallow trailing
  punctuation (`.`, `,`, `)` …).
- `link_to_open(&str) -> Option<&str>`: only `http://` / `https://` (scheme case-insensitive) are
  opened. OSC 8 links come from whatever runs in the terminal, so `file:`, custom schemes etc. are
  refused rather than handed to the desktop.

### Wiring (`src/ui/terminal_view.rs`)
- `gtk::EventControllerKey` in `PropagationPhase::Capture` on the terminal, so VTE never sees the
  keys. Copy is always consumed (copying only `if has_selection()`): letting an empty-selection
  Ctrl+Shift+C fall through would send ^C and interrupt the program. Copy uses
  `copy_clipboard_format(Format::Text)`; paste uses `paste_clipboard()` (VTE applies bracketed
  paste when the program asked for it).
- Context menu through VTE's own API (`set_context_menu_model`, VTE ≥ 0.76), so VTE owns
  right-click, the Menu key, popover parenting and mouse-tracking bypass. Items Copy / Paste /
  Select All, backed by a `term.*` `gio::SimpleActionGroup` inserted on the terminal; `term.copy`
  is enabled from `selection-changed`.
- Links: `set_allow_hyperlink(true)` (OSC 8) and `match_add_regex(URL_PATTERN)` with
  `PCRE2_MULTILINE | PCRE2_UTF | PCRE2_UCP | PCRE2_CASELESS` (VTE requires MULTILINE for match
  regexes), cursor `pointer`. A capture-phase primary-button `gtk::GestureClick` opens the link
  under the pointer on **Ctrl+click** (`check_hyperlink_at`, then `check_match_at`), claiming the
  sequence; opening is `gtk::UriLauncher` with the app window as parent, a toast on failure.
  - Deviation from the request's "on click": a plain click would make it impossible to drag-select
    the URL (to copy it, now that copying works) and races VTE's own click gesture. Ctrl+click is
    the GNOME Terminal convention; the status label says so.
- Status label gains "Paste with Ctrl+Shift+V. Ctrl+click a link to open it."

### Dependencies
- `vte4 = { version = "0.8", features = ["v0_76"] }` — enables `set_context_menu_model`
  (0.76) and `check_hyperlink_at` / `check_match_at` (0.70) on the existing crate. No new crate;
  `Cargo.lock` does not record features. Runtime VTE in nixos-26.05 is 0.84.1.
- `gtk::UriLauncher` needs GTK 4.10; `gtk4` already has `v4_12`.
- PCRE2 flag values from `pcre2.h`: CASELESS 0x8, MULTILINE 0x400, UCP 0x20000, UTF 0x80000.

## Files
`Cargo.toml`, `src/terminal.rs`, `src/ui/terminal_view.rs`. No catalog, daemon, or packaging
change. The security boundary is untouched: the argv/exec path is unchanged.

## Verification
Phase 3 commands via `nix develop -c` (run in WSL, where nix is installed). Unit tests for
`clipboard_key` and `link_to_open`. No `nix build` needed (no packaging change). The GUI itself
cannot be exercised here (no display); paste/link behaviour must be confirmed on the host.

## Risks
- Non-Latin keyboard layouts produce other keyvals for C/V; Shift+Insert and the context menu
  still work there.
- The URL regex cannot be unit-tested without a VTE instance; VTE rejects an invalid pattern at
  runtime (logged, terminal still works).
- Problem 2's root cause remains open until the diagnostic is run.
