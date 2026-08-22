# VexPortal — Feature Recommendations

Every recommendation below is anchored to code, data or metadata that **already exists**
in this repository. Nothing here requires re-architecting the GUI/daemon split, changing
the catalog format, or introducing a new privilege boundary.

Scope note: this is a forward-looking document. Defects in what is already built are in
`ANALYSIS_BUGS.md`; structural issues are in `ANALYSIS_ARCH.md`. A few items overlap
with those — where they do, it is called out, because "finish the feature" and "fix the
bug" are sometimes the same commit.

No code was modified and no build was run.

---

## Summary

| # | Priority | Feature | Anchor already in the repo |
|---|---|---|---|
| **F1** | High | Feature toggles as real switches | `SystemState.features` + `enable-feature`/`disable-feature` + `_feature_names` |
| **F2** | High | Search / command palette | 42 recipes, `title` + `blurb` + `icon` on every one |
| **F3** | High | Reattach to running jobs | `ListJobs` fully implemented daemon-side, unused |
| **F4** | High | Give the drift test something to run against | `drift::compare` + a test that never executes |
| **F5** | High | Desktop notifications for long jobs | `adw::Application` + matching `.desktop` id |
| **F6** | Med-High | Service-centric page for the server role | 7 service recipes keyed by one slug + `_server_service_names` |
| **F7** | Med-High | "Test build first" before `switch` | `build` is `switch`'s safe twin with an identical signature |
| **F8** | Med-High | Primary menu, About, keyboard shortcuts | Complete `metainfo.xml`, no menu anywhere |
| **F9** | Medium | Post-run state diff toast | `Recipe::refresh` (populated, unread) + `SystemState: Clone` |
| **F10** | Medium | Version handshake with the daemon | `Version` property implemented on both sides, called by neither |
| **F11** | Medium | Generation browser + rollback target picker | `read_generation` already parses the profile symlink |
| **F12** | Medium | Log search, save-to-file, jump-to-error | `GtkTextBuffer` + `FileDialog` both already in use |
| **F13** | Medium | Failure guidance on the result banner | journald audit exists and is unreachable from the UI |
| **F14** | Medium | Operation history | `audit.rs` writes a complete record nothing reads back |
| **F15** | Medium | `vexportal-cli` companion | Catalog carries roles that structurally cannot run the GUI |
| **F16** | Medium | Real CI | `scripts/preflight.sh` encodes the gates; `.github/` has no workflow |
| **F17** | Medium | Detect `needs_upstream` instead of hand-maintaining it | Drift machinery exists for exactly this |
| **F18** | Low-Med | Use the daemon's `Justfile` property | Implemented on both sides, unused |
| **F19** | Low-Med | Elapsed time and duration in the audit line | No timing recorded anywhere today |
| **F20** | Low-Med | "Reboot now" on the reboot-pending row | `reboot_pending` and the `reboot` recipe don't know about each other |
| **F21** | Low-Med | Actionable drift banner | `adw::Banner` supports a button; `drift_summary()` supplies the text |
| **F22** | Low-Med | Manual refresh + refresh on focus | `refresh_state()` + `state_changed()` already exist |
| **F23** | Low | Surface `Excluded::reason` | Three entries with prose reasons, never read |
| **F24** | Low | Remember window size and last category | Standard expectation; nothing persists today |

---

## 1. Partially stubbed or clearly intended, never finished

### F1 — Feature toggles as actual switches — **HIGH**

**What already exists.** Three separate pieces that have never been connected to each
other:

- `SystemState.features: Vec<(String, bool)>` — the *current* on/off state of every
  optional module, parsed from `/etc/nixos/features.nix` ([src/system/state.rs:86-110](src/system/state.rs#L86-L110)).
- `enable-feature` and `disable-feature` recipes, each taking one `feature` parameter
  backed by `DynamicSource::Features` ([catalog/src/catalog.toml:207](catalog/src/catalog.toml#L207), [:225](catalog/src/catalog.toml#L225)).
- `JustfileFacts.features` — the *list of valid feature names*, read from the justfile's
  `_feature_names` variable ([src/just.rs:79](src/just.rs#L79)).

The app therefore knows what the features are, which are on, and exactly how to change
one — and renders it as **decoration**:

```rust
for (name, enabled) in &state.features {
    let label = gtk::Label::new(Some(name));
    label.add_css_class(if *enabled { "vex-risk" } else { "vex-badge" });
```
— [src/ui/dashboard.rs:119-134](src/ui/dashboard.rs#L119-L134)

A read-only chip in a `FlowBox`. To turn on `gaming`, a user looking straight at a chip
that says `gaming` must navigate to Features, find "Enable Feature", open the argument
dialog, and pick `gaming` from a dropdown.

**The feature.** Replace the chip `FlowBox` with one `adw::SwitchRow` per feature
(libadwaita 1.4+, available under the crate's `v1_5` gate). Title = the feature name,
subtitle = "Takes effect on the next rebuild", `active` = the parsed boolean. On toggle,
route to the existing `category_page::confirm_then_run` with the corresponding recipe
and `{"feature": name}` — the identical path an argument dialog would produce, so risk
tier, polkit and audit all behave unchanged. Set the switch insensitive while the job is
in flight and re-read state on completion (`refresh_state()` already does this).

Pair with **F9** so the toast confirms the change landed.

**Why it is worth it.** This is the single most obvious missing interaction in the app,
it needs no new daemon surface, no new catalog field, and no new validation — every
component is already built and tested. Roughly a 60-line change to one function.

Related caveat from `ANALYSIS_BUGS.md`: `read_features` mis-parses a line with a
trailing comment (L8), which would make a switch render in the wrong position. Fix that
first — it becomes user-visible the moment the chip becomes a control.

### F3 — Reattach to running jobs — **HIGH**

**What already exists.** `ListJobs` is complete on the daemon side, with the intent
stated in its own doc comment:

```rust
/// Running jobs as `(job_id, recipe)`, so a GUI that was restarted can reattach.
async fn list_jobs(&self) -> fdo::Result<Vec<(String, String)>> {
```
— [daemon/src/interface.rs:144-154](daemon/src/interface.rs#L144-L154)

It reaps stale handles, returns live ones, and is declared in the GUI's proxy trait at
[src/dbus_client.rs:240](src/dbus_client.rs#L240). Nothing calls it. `JobHandle` already
carries `job_id` and `recipe` ([daemon/src/cancel.rs:11-16](daemon/src/cancel.rs#L11-L16))
for exactly this purpose.

**The feature.** On startup, after the connection is established, call `ListJobs`. For
each result, look the recipe up in the catalog and construct a `RunPage` already attached
to that job id (`RunPage::attach` exists and does precisely this —
[src/ui/run_page.rs:144-148](src/ui/run_page.rs#L144-L148)), then surface it as a
dashboard banner: *"`rebuild` is still running — Watch"*. Signals for the job already
flow; the page starts receiving output from the moment of attach.

Output produced *before* the reattach is genuinely gone, since the daemon does not
buffer. Two honest options:

- **v1 (small):** attach and prepend one line — *"Reattached. Earlier output is in
  `journalctl -u vexportal-daemon`."*
- **v2:** add a bounded ring buffer per job in the executor (say 5,000 lines, matching
  `MAX_LINES`) and a `GetJobLog(job_id) -> Vec<(u32, String)>` method to replay it.

**Why it is worth it.** `rebuild` and `kernel-build-now` run for tens of minutes. The
daemon deliberately outlives the GUI, and today closing the window orphans the job
permanently: no output, no Cancel, no indication on next launch that the machine is
mid-rebuild. The daemon half is finished; only the client call is missing.

This also directly resolves `ANALYSIS_ARCH.md` D1 and gives `ListJobs` its first caller.

### F10 — Version handshake with the daemon — **MEDIUM**

**What already exists.** `Version` is implemented as a D-Bus property returning
`env!("CARGO_PKG_VERSION")` ([daemon/src/interface.rs:173-176](daemon/src/interface.rs#L173-L176))
and declared in the GUI proxy ([src/dbus_client.rs:248-249](src/dbus_client.rs#L248-L249)).
Neither side uses it.

**The feature.** On connect, compare `proxy.version().await` against the GUI's own
`CARGO_PKG_VERSION`. On mismatch, raise a dashboard banner: *"VexPortal 0.2.0 is talking
to vexportal-daemon 0.1.0. Some operations may be rejected — run `nixos-rebuild switch`
or reboot to update the daemon."*

**Why it is worth it.** This is a real hazard specific to this design, not boilerplate.
`catalog.toml` is `include_str!`-compiled into **both** binaries
([catalog/src/lib.rs:22](catalog/src/lib.rs#L22)). A version skew — trivially produced by
`nixos-rebuild test`, by the daemon still running an old store path, or by a partial
update — means the GUI offers recipes the daemon's catalog has never heard of. The user
gets `AccessDenied: 'x' is not a recipe VexPortal knows about`, which reads like a
permissions problem and is not. The banner turns an inexplicable error into an
actionable one, for perhaps 15 lines of code.

### F17 — Detect `needs_upstream` instead of hand-maintaining it — **MEDIUM**

**What already exists.** Six recipes carry `needs_upstream = true`
([catalog/src/lib.rs:216-219](catalog/src/lib.rs#L216-L219)), rendered as a badge whose
tooltip explains that the recipe still stops for a prompt
([src/ui/category_page.rs:55-62](src/ui/category_page.rs#L55-L62)). The daemon sets
`VEXOS_ASSUME_YES=1` for a contract nothing upstream honours yet
([daemon/src/config.rs:62-65](daemon/src/config.rs#L62-L65)).

The whole `drift` module exists to stop the catalog making hand-maintained claims about
the justfile that have gone stale — and `needs_upstream` is exactly such a claim.

**The feature.** `just --dump --dump-format json` carries recipe bodies. Add a `body`
field to `JustRecipe` ([catalog/src/drift.rs:27-35](catalog/src/drift.rs#L27-L35)) and a
new `Drift` variant: a recipe marked `needs_upstream` whose body **does** reference
`VEXOS_ASSUME_YES` is a stale catalog flag, and one *not* marked whose body contains a
bare `read` is a missing flag. Then drop the badge automatically once upstream lands
support, without a catalog edit.

**Why it is worth it.** It converts the project's most awkward outstanding dependency —
a flag that must be manually cleared across six recipes when vexos-nix changes — into
something the existing drift check reports on its own. Confirm the exact JSON field name
for the body against the installed `just` before relying on it; the shape is not
versioned (see `ANALYSIS_BUGS.md` L10).

### F18 — Use the daemon's `Justfile` property — **LOW-MEDIUM**

**What already exists.** The daemon exposes the justfile path it will actually execute,
with the reason spelled out:

```rust
/// The justfile this daemon runs, so the GUI can read the same one for its
/// dropdowns and drift check instead of guessing.
```
— [daemon/src/interface.rs:178-183](daemon/src/interface.rs#L178-L183)

The GUI declares the property ([src/dbus_client.rs:251-252](src/dbus_client.rs#L251-L252))
and then hardcodes `/etc/nixos/justfile` anyway ([src/just.rs:13](src/just.rs#L13)).

**The feature.** Read the property on connect and pass the result to
`JustfileFacts::read`. `programs.vexportal.justfile` is a real NixOS option
([nix/module.nix:24-32](nix/module.nix#L24-L32)); any deployment that sets it currently
gets a GUI whose dropdowns, availability filter and drift banner describe a *different
file* than the daemon runs, with no warning.

Sequencing note: this makes justfile facts depend on the D-Bus connection, so it pairs
naturally with fixing the eager/lazy connect confusion (`ANALYSIS_BUGS.md` L4) and with
moving `just --dump` off the main thread (P4).

### F23 — Surface `Excluded::reason` — **LOW**

**What already exists.** Three catalog entries with human-written reasons that nothing
reads:

```toml
[[excluded]]
name = "update"
reason = "Updates and upgrades are handled by Up, not VexPortal."
```
— [catalog/src/catalog.toml:658-670](catalog/src/catalog.toml#L658-L670); `drift::compare`
uses only `.name` ([catalog/src/drift.rs:148](catalog/src/drift.rs#L148)).

**The feature.** An `adw::ExpanderRow` at the bottom of the relevant category — *"3
operations are available from the command line"* — listing each excluded recipe with its
reason and the `just <name>` invocation. Users who know vexos-nix's justfile will look
for `update` and conclude VexPortal is broken; the answer is already written down and
simply not displayed.

---

## 2. Natural complements to the existing code and data models

### F2 — Search / command palette — **HIGH**

**What already exists.** 42 recipes across 8 categories, every one carrying `title`,
`blurb`, `icon` and `category` ([catalog/src/lib.rs:196-222](catalog/src/lib.rs#L196-L222)).
`App::visible_in` already does role + availability filtering
([src/app.rs:41-47](src/app.rs#L41-L47)), and `category_page::action_row` already renders
a recipe as a row independent of which page it lands on
([src/ui/category_page.rs:37-85](src/ui/category_page.rs#L37-L85)) — the dashboard's
quick actions reuse it verbatim.

**The feature.** A `gtk::SearchEntry` in the header (Ctrl+F, plus type-to-search) that
switches the content area to a flat `PreferencesGroup` of `action_row`s matching the
query against `title` and `blurb`, across every category the current role can see. The
blurbs are full sentences written for humans, so substring matching over them is
genuinely useful — searching "kill switch", "plex", "kernel" or "generation" all land.

**Why it is worth it.** Around 80 lines, entirely reusing existing rendering, and it is
the difference between a 42-item app you browse and one you drive. Users who know what
they want should not have to guess which of eight categories the author filed it under —
`fix-flake` is in Features, `attic-push` is in Cache, `ssh` is in Network.

### F6 — Service-centric page for the server role — **MEDIUM-HIGH**

**What already exists.** The `services` category is seven recipes, and **six of them are
verbs applied to the same noun**:

| Recipe | Param | Risk |
|---|---|---|
| `service-info` | `service` | safe |
| `status` | `service` | safe |
| `restart` | `service` | medium |
| `disable` | `service` | medium |
| `enable` | `service` | medium (terminal) |
| `services` / `available-services` | — | safe |

Every `service` parameter is `choice-dynamic` backed by `DynamicSource::ServerServices`,
and the list is already parsed at startup into `JustfileFacts.server_services`
([src/just.rs:80](src/just.rs#L80)).

**The feature.** Replace the flat card list with one row per service from
`server_services`: name, an enabled/disabled indicator, and a `gtk::MenuButton` offering
Status / Info / Restart / Disable — each running the corresponding recipe with
`{"service": name}` pre-filled, through the unchanged confirm→run path. Keep
`available-services` as a footer action.

**Why it is worth it.** Today, restarting Jellyfin means: open Server Services, find
"Restart Service", open a dialog, select `jellyfin` from a dropdown, confirm. Six
interactions to express one. The catalog already models these as one noun with six
verbs; the UI just hasn't been reorganised around that. It also makes `enable`'s
unreachable `service` parameter (`ANALYSIS_ARCH.md` A6) meaningful again — the row knows
which service it is, so the terminal launch can pass it.

### F7 — "Test build first" before `switch` — **MEDIUM-HIGH**

**What already exists.** `build` and `switch` take **identical parameter lists** —
`role`, `variant`, `flake` — and differ only in risk:

```
build-deploy   build    safe     role,variant,flake
build-deploy   switch   medium   role,variant,flake
```

`build`'s blurb is literally *"Dry-run build of a role and GPU variant without switching
to it."* The confirmation dialog machinery already exists and already knows the args
([src/ui/category_page.rs:106-145](src/ui/category_page.rs#L106-L145)).

**The feature.** Add a third response to `switch`'s confirmation dialog: **Test build
first**. It runs `build` with the same `HashMap<String, String>`, shows the normal run
page, and on exit code 0 offers "Build succeeded — switch now?" wired to the original
`switch` invocation. On failure, the user has the error and has changed nothing.

**Why it is worth it.** Changing a machine's role and GPU variant is the highest-stakes
routine operation in the app, and the catalog already ships the safe rehearsal for it
with a signature that matches exactly. This is a genuine safety feature that requires no
new recipe, no new validation and no new privilege — just wiring two existing catalog
entries to each other.

Note the interaction with `ANALYSIS_BUGS.md` S2: `build` is currently `risk = "safe"`,
which means *no polkit prompt at all* for a full root Nix build. If it moves to `medium`
as recommended there, this flow costs one extra authentication — worth it.

### F11 — Generation browser and rollback target picker — **MEDIUM**

**What already exists.** The app already reads generation state from a world-readable
symlink without privileges, and explains why that works:

```rust
/// The profile symlink is `system-<n>-link`, which gives the generation number without
/// taking the profile lock that `nix-env --list-generations` needs root for.
fn read_generation() -> Option<u32> {
    let target = std::fs::read_link(SYSTEM_PROFILE).ok()?;
```
— [src/system/state.rs:63-72](src/system/state.rs#L63-L72)

`reboot_pending` already compares `/run/booted-system` against `/run/current-system`
([:74-82](src/system/state.rs#L74-L82)). `rollback` and `rollforward` exist as
zero-argument, step-at-a-time recipes.

**The feature.** A Generations page: read the directory `/nix/var/nix/profiles/`, glob
`system-*-link`, and list each generation with its number and symlink mtime, marking
which is **current** and which is **booted**. Even purely read-only this is valuable on
NixOS. Then wire `rollback` from the row for generation N-1, and show plainly how many
steps back a target is.

**Why it is worth it.** The same technique the code already uses for one number
generalises to the list, with no privileges and no daemon call. "Which generation am I
on, which did I boot, and what can I go back to" is the central question of NixOS system
management, and the dashboard currently answers one third of it.

### F12 — Log search, save-to-file, and jump-to-error — **MEDIUM**

**What already exists.** The run page has a `GtkTextBuffer` with up to 5,000 lines
([src/ui/run_page.rs:16](src/ui/run_page.rs#L16)), a Copy button that already extracts
the whole buffer ([:119-129](src/ui/run_page.rs#L119-L129)), a `stderr` tag applied to
every error line ([:168-171](src/ui/run_page.rs#L168-L171)), and a working `gtk::FileDialog`
save flow demonstrated in `arg_dialog::path_row`
([src/ui/arg_dialog.rs:217-221](src/ui/arg_dialog.rs#L217-L221)).

**The feature.** Three additions to the run page header:

- **Search** — a `gtk::SearchBar` using `TextIter::forward_search`, with next/previous.
- **Save output…** — a `FileDialog::save` writing the buffer to a `.log`, defaulting the
  name to `<recipe>-<timestamp>.log`.
- **Jump to first error** — iterate the `stderr` tag's ranges and scroll to the first.
  The tag is already applied to exactly the right lines; nothing new needs detecting.

**Why it is worth it.** A failed `nixos-rebuild` produces thousands of lines whose one
useful line is somewhere in the middle. Today the only affordance is Copy-all and paste
elsewhere. "Jump to first error" in particular is nearly free — the styling pass already
identified those lines.

### F9 — Post-run state diff toast — **MEDIUM**

**What already exists.** `Recipe::refresh` is a per-recipe list of state keys —
`generation`, `variant`, `features`, `hostname` — populated on seven recipes and read by
nothing ([catalog/src/lib.rs:209-211](catalog/src/lib.rs#L209-L211);
`catalog.toml:90, 131, 181, 191, 214, 232, 605`). `SystemState` derives `Clone` and
`Default` and every field is comparable ([src/system/state.rs:11-23](src/system/state.rs#L11-L23)).
`Window::toast` exists and is used exactly once ([src/ui/window.rs:129-131](src/ui/window.rs#L129-L131)).

**The feature.** In `Event::Finished`, snapshot `SystemState` before `refresh_state()`,
diff after, and toast the specific change: *"Generation 360 → 361"*, *"Reboot now
pending"*, *"gaming enabled"*, *"Hostname is now vexos-office"*. `refresh` tells you
which fields that recipe was expected to move, so the diff can be scoped and a
*missing* change can be reported too — *"`enable-feature` finished but `features.nix` is
unchanged"* is a genuinely useful warning.

**Why it is worth it.** It closes the loop on every state-changing operation with
concrete evidence, and it is the only proposal here that gives `Recipe::refresh` — an
existing, deliberately designed, entirely unused field — a reason to exist.

### F19 — Elapsed time, and duration in the audit line — **LOW-MEDIUM**

**What already exists.** The run page header has a spinner and a status label
([src/ui/run_page.rs:58-63](src/ui/run_page.rs#L58-L63)). `audit::finished` records only
the exit code ([daemon/src/audit.rs:19-25](daemon/src/audit.rs#L19-L25)).

**The feature.** A `glib::timeout_add_seconds_local` ticking an elapsed label next to
the status ("Running… 4:12"), frozen at the final duration on completion. Daemon side,
capture an `Instant` in `executor::spawn` and log it: `job {job} finished successfully in
7m12s`.

**Why it is worth it.** Cheap on both sides, and it makes the journal answer "how long
do rebuilds take on this machine" — which is the question that follows every "who
rebuilt this at 3am" that `audit.rs:1-5` already answers.

---

## 3. Gaps a user of an app like this would expect

### F5 — Desktop notifications for long jobs — **HIGH**

**What already exists.** The application is an `adw::Application`
([src/main.rs:22](src/main.rs#L22)), i.e. a `GApplication`, whose id `io.github.vexportal`
matches the installed desktop file `io.github.vexportal.desktop` — which is the entire
requirement for `gio::Notification` to work. `Event::Finished` already knows the recipe
and the exit code ([src/app.rs:106-116](src/app.rs#L106-L116)).

**The feature.** On job completion, if the window is not focused
(`window.is_active()`), call `application.send_notification()` with a title of the
recipe's `title` and a body of "Finished" or "Failed (exit code N)", using the app icon.
Add `X-GNOME-UsesNotifications=true` to the desktop entry so it appears in GNOME's
notification settings.

**Why it is worth it.** A `rebuild` or `kernel-build-now` runs for 10–40 minutes.
Nobody watches a progress log for 40 minutes; they alt-tab away and forget. Perhaps 15
lines of code for the single highest perceived-value addition in this document.
Pairs with F9 so the notification can carry the actual outcome ("Generation 361").

### F8 — Primary menu, About dialog, and keyboard shortcuts — **MEDIUM-HIGH**

**What already exists.** A complete, correct `metainfo.xml` with description, license,
homepage, bugtracker, developer and release entries
([data/io.github.vexportal.metainfo.xml](data/io.github.vexportal.metainfo.xml)) — and
no way to see any of it. There is no `gtk::MenuButton`, no `gio::Menu`, and no
`set_accels_for_action` call anywhere in `src/`.

**The feature.**

- A hamburger `MenuButton` in the header with **About VexPortal**, **Keyboard
  Shortcuts**, **Quit**. `adw::AboutDialog::from_appdata()` (libadwaita 1.5, already the
  declared feature level) builds the About dialog *directly from the installed metainfo*
  — no duplicated strings.
- Accelerators for what exists: `Ctrl+F` search (F2), `Ctrl+R` rebuild, `Ctrl+W` back /
  close, `F5` refresh (F22), `Ctrl+Q` quit, `Escape` to leave a run page.
- A `gtk::ShortcutsWindow`.

**Why it is worth it.** This is the baseline GNOME HIG surface that every user will look
for and not find. `from_appdata` in particular makes the About dialog nearly free given
the metainfo is already written and already installed by `nix/package.nix:54-55`.

### F13 — Failure guidance on the result banner — **MEDIUM**

**What already exists.** On failure the run page shows:

```rust
self.banner.set_title("This operation did not complete. The output above says why.");
```
— [src/ui/run_page.rs:181-183](src/ui/run_page.rs#L181-L183)

Meanwhile `audit.rs` writes a complete journald record that the UI never mentions, and
`spawn_terminal` already knows how to open a terminal
([src/ui/category_page.rs:175-231](src/ui/category_page.rs#L175-L231)).

**The feature.** `adw::Banner` supports an action button. Give the failure banner one,
and offer contextually:

- **Save output…** / **Copy output** (F12).
- **Open journal** — `spawn_terminal("journalctl -u vexportal-daemon -n 200")`, reusing
  the existing launcher.
- For a failed `rebuild` or `switch` specifically: **Roll back**, wired to the `rollback`
  recipe.

**Why it is worth it.** The current message tells the user to read output they may not
understand and gives them nothing to do next. Every one of these actions already exists
somewhere in the codebase; none of them is reachable from the moment the user needs it.

### F14 — Operation history — **MEDIUM**

**What already exists.** `daemon/src/audit.rs` writes a genuinely complete record —
who started what, at what risk, with a redacted argument line, and how it ended — with
the stated purpose:

> Everything lands in the journal under the daemon's unit, so `journalctl -u
> vexportal-daemon` is the answer to "who rebuilt this machine at 3am".

Nothing reads it back. There is no history view anywhere in the GUI.

**The feature.** A "Recent operations" section on the dashboard: recipe, timestamp,
outcome, duration (F19). The honest implementation is a polkit-gated daemon method —
`GetHistory(limit) -> Vec<(timestamp, recipe, exit_code)>` — reading its own unit's
journal, because an unprivileged desktop user is generally **not** in `systemd-journal`
or `adm` and cannot read a system unit's logs directly. The daemon can, and it already
has a `safe`-tier polkit action pattern to gate it with.

**Why it is worth it.** The audit trail is one of the project's stated selling points
(`metainfo.xml:18-20`) and is currently accessible only to someone who already knows the
`journalctl` incantation and has the group membership to run it. Note the in-memory
alternative does not work here: the daemon idle-exits after 180 seconds
([daemon/src/main.rs:29](daemon/src/main.rs#L29)), so journald really is the store.

### F20 — "Reboot now" on the reboot-pending row — **LOW-MEDIUM**

**What already exists.** `SystemState.reboot_pending` is computed
([src/system/state.rs:74-82](src/system/state.rs#L74-L82)) and rendered as a passive
informational row ([src/ui/dashboard.rs:94-102](src/ui/dashboard.rs#L94-L102)). A
`reboot` recipe exists in the catalog for every role
([catalog/src/catalog.toml:637](catalog/src/catalog.toml#L637)).

**The feature.** Add a suffix button to that row wired to `confirm_then_run` for
`reboot`. Two things that already exist and have never been introduced to each other.

### F22 — Manual refresh and refresh on window focus — **LOW-MEDIUM**

**What already exists.** `App::refresh_state()` and `Window::state_changed()` are both
implemented and are called from exactly one place — after a job finishes
([src/app.rs:58-60](src/app.rs#L58-L60), [src/app.rs:111-114](src/app.rs#L111-L114)).

**The feature.** A refresh button in the dashboard header, plus a
`connect_is_active_notify` handler that refreshes when the window regains focus.

**Why it is worth it.** System state changes from outside VexPortal constantly — a
terminal `nixos-rebuild`, an automatic upgrade timer, an edit to `features.nix`. Today
the dashboard is frozen at whatever it read at launch until the user happens to run a job
through the portal. Both functions already exist; only the triggers are missing.

Note this does **not** refresh `JustfileFacts`, which is read once and never again
(`ANALYSIS_ARCH.md` A12) — refreshing that too is the more valuable half.

### F21 — Actionable drift banner — **LOW-MEDIUM**

**What already exists.** `drift_summary()` produces a well-written, correctly
differentiated message ([src/just.rs:120-152](src/just.rs#L120-L152)) — distinguishing
"this host has not rebuilt recently" from "VexPortal is out of step with the justfile" —
displayed in an `adw::Banner` with no button ([src/ui/dashboard.rs:185-188](src/ui/dashboard.rs#L185-L188)).

**The feature.** `adw::Banner` supports a button. For the *host is behind* case, offer
**Rebuild** wired to the `rebuild` recipe — that is literally the fix, and the recipe is
right there. For the *catalog defect* case, offer **Report** opening the bugtracker URL
already declared in `metainfo.xml`.

**Why it is worth it.** The message already diagnoses the problem precisely and then
leaves the user to work out the remedy. One of the two cases has a one-click fix already
in the catalog.

### F24 — Remember window size and last category — **LOW**

Nothing persists between launches. Standard expectation: save width/height/maximised
and the last-selected sidebar category (`Window.current` already tracks it —
[src/ui/window.rs:20](src/ui/window.rs#L20)) via `gio::Settings` or a small JSON file in
`glib::user_config_dir()`. `serde_json` is already a dependency of the GUI.

---

## 4. Integrations and automations the structure is already set up for

### F4 — Give the drift test something to run against — **HIGH**

**What already exists.** `drift::compare` is a careful, well-tested comparison engine
([catalog/src/drift.rs:141-208](catalog/src/drift.rs#L141-L208)) with four drift
categories and a correct distinction between catalog defects and a stale host. It is the
project's primary safety net against the catalog and the justfile silently diverging.

**It has never executed in CI or in a package build.** Both tests return early — and
report `ok` — when `/etc/nixos/justfile` is absent:

```rust
if !Path::new(JUSTFILE).exists() {
    eprintln!("skipping: {JUSTFILE} not present (not a built VexOS host)");
    return;                                    // test PASSES
}
```
— [catalog/tests/drift_against_justfile.rs:19-22](catalog/tests/drift_against_justfile.rs#L19-L22)

The Nix build sandbox has no `/etc/nixos/justfile`, so `nix build .#default` — which the
README describes as *"runs tests in the sandbox"* — has never once run the comparison.

**The feature.** Commit a captured dump as a fixture:

- `catalog/tests/fixtures/justfile-dump.json`, produced by
  `just --justfile /etc/nixos/justfile --dump --dump-format json` on a real VexOS host.
- A `scripts/refresh-drift-fixture.sh` to regenerate it, so updating is one command.
- A test that runs **unconditionally** against the fixture, asserting zero catalog
  defects.
- Keep the existing live-host tests, but convert their skip into a real skip:
  `#[ignore]` with an explicit `--ignored` run, or a `VEXPORTAL_REQUIRE_JUSTFILE=1`
  environment check that turns absence into a failure in CI.

**Why it is worth it.** Highest value-per-line item in this document. The comparison
engine is already written and already correct; it simply has no input. A committed
fixture means every `nix build`, every `cargo test`, and every PR actually checks that
the 42 catalog entries still match the justfile — which is the one invariant the whole
design depends on. It also makes `catalog.toml`'s own instruction (*"Keep this in sync
with the justfile. `cargo test -p vexportal-catalog` compares the two"*) true.

### F16 — Real CI — **MEDIUM**

**What already exists.** `scripts/preflight.sh` already encodes exactly the right gates
— `cargo fmt --check`, `cargo check --workspace`, `cargo clippy --workspace
--all-targets`, `cargo test --workspace`, each through `nix develop -c`
([scripts/preflight.sh:13-23](scripts/preflight.sh#L13-L23)). `.github/` exists but
contains **no workflows** — only three markdown documents under
`.github/docs/subagent_docs/`.

Meanwhile `flake.nix` defines `checks.default = self.packages.${system}.default`
([flake.nix:37](flake.nix#L37)), so `nix flake check` builds the package and runs tests
but never runs fmt or clippy. Two "run all the checks" entry points, two different
definitions.

**The feature.**

- Add `checks.fmt` and `checks.clippy` derivations to the flake so `nix flake check` and
  `scripts/preflight.sh` agree on what "checked" means.
- A `.github/workflows/ci.yml` running `nix flake check` on push and PR, with a Nix
  install action and a store cache.
- Once F4 lands, the drift comparison runs there too — which is the point.

**Why it is worth it.** The gates are already written and already correct; only the
trigger is missing. Also worth moving the process documents out of `.github/` (GitHub's
config directory) into `docs/` so a workflow file is not mistaken for them.

### F15 — `vexportal-cli` companion binary — **MEDIUM**

**What already exists.** Two pieces of evidence that this was anticipated:

1. The catalog assigns roles `headless-server` and `vanilla` to roughly twenty recipes —
   including everything in `services`, `storage` and `cache` — while the README states
   plainly that those roles **do not get VexPortal** because they have no display. The
   catalog carries carefully maintained metadata for machines that structurally cannot
   run the GUI.
2. An `ssh` recipe with a `target` parameter exists in every role's catalog
   ([catalog/src/catalog.toml:495](catalog/src/catalog.toml#L495)).

**The feature.** A fourth workspace member: a small `vexportal-cli` binary speaking the
same D-Bus interface. `vexportal-cli list`, `vexportal-cli run restart jellyfin`,
`vexportal-cli jobs`, `vexportal-cli logs <job>`. It reuses `vexportal-catalog` verbatim
for validation and help text, and the `pkttyagent` text polkit agent handles
authentication over SSH.

**Why it is worth it.** It makes the headless-role catalog data mean something, it gives
`ListJobs` a second consumer (reinforcing F3), and it is a natural fit for a workspace
that is already three members with a shared catalog crate. It needs no daemon changes at
all — the D-Bus surface is complete.

Deliberately excluded from scope: anything that would let the CLI bypass the daemon or
construct a command line. It is a thin client, exactly like the GUI.

### Scheduled operations — **NOT RECOMMENDED (noted for completeness)**

`backup-now` and `backup-plex` invite the thought of a schedule, and
`nix/module.nix` already generates a systemd unit, so `programs.vexportal.backup.schedule`
generating a timer would be mechanically easy.

It is listed here to be argued *against*: scheduling belongs in vexos-nix's own NixOS
configuration, alongside every other timer on the machine, not behind a GUI's module.
Adding it would make VexPortal a second, competing place where system automation is
declared — and the project has been consistent so far about being a front end to the
justfile rather than a source of system configuration in its own right.

---

## Suggested sequencing

**First — the ones with the best value-per-line:**

1. **F4** (drift fixture) — the safety net that has never run. Foundational; do it before
   the catalog grows further.
2. **F5** (notifications) — ~15 lines, largest perceived improvement.
3. **F1** (feature switches) — one function, and it fixes the app's most obvious gap.
   Fix `read_features`' comment-parsing bug (`ANALYSIS_BUGS.md` L8) in the same commit.
4. **F8** (menu + About + shortcuts) — `from_appdata` makes most of it free.

**Second — the substantial ones:**

5. **F2** (search) — reuses existing rendering entirely.
6. **F3** (reattach) — finish the daemon-side work that is already done. Do this after
   the `Event::Output` routing race (`ANALYSIS_BUGS.md` L1) is fixed, since reattach
   depends on the same routing path.
7. **F6** (service page) — the largest UX reframing, and the one that most changes how
   the server role feels.
8. **F7** (test build first) — safety, at near-zero cost.

**Third — polish and reach:**

9. **F9**, **F10**, **F12**, **F13**, **F19**, **F20**, **F21**, **F22** — each small
   and independent; good candidates for filling out a release.
10. **F11** (generations), **F14** (history), **F16** (CI), **F15** (CLI) — each is a
    day or more of work with a clear payoff.
11. **F17**, **F18**, **F23**, **F24** — worthwhile cleanups that finish existing
    mechanisms.
