# VexPortal — Architecture & Structure Analysis

Scope: architecture, structure, consistency, abandoned work, and dependencies.
Not covered here: correctness bugs unrelated to structure, UI/UX polish, test coverage depth.

Codebase surveyed in full: 4,606 lines of Rust across three workspace members
(`vexportal` GUI, `vexportal-catalog`, `vexportal-daemon`), plus `catalog/src/catalog.toml`
(670 lines, 42 recipes), `data/`, `nix/`, `flake.nix`, `build.rs`, `scripts/preflight.sh`.

No code was modified and no build was run — this is a read-only analysis.

---

## Summary

| # | Priority | Finding |
|---|----------|---------|
| A1 | High | Structured D-Bus errors flattened to a string, then re-parsed by substring match |
| A2 | High | A finishing job destroys the run page the user is watching |
| A3 | High | GUI D-Bus command loop serializes behind a human-interactive polkit prompt |
| C1 | High | Four different error-handling styles for the same class of failure |
| D1 | High | Job reattachment (`ListJobs`) fully built on both sides, never wired up |
| A4 | Med-High | `Rc` reference cycles between `App` ↔ `Window` and `App` ↔ `RunPage`; `running` map leaks |
| A5 | Medium | Terminal escape hatch bypasses daemon, polkit and audit, and builds a shell string in the GUI |
| A6 | Medium | `enable` is terminal-only *and* declares a required parameter that is silently dropped |
| A7 | Medium | `Cancel` has no ownership model — any active user can cancel any job |
| A8 | Medium | Concurrency limit checked outside the lock, across a minutes-long `await` |
| A9 | Medium | Daemon's `Justfile` property exists so the GUI need not guess; the GUI guesses anyway |
| A12 | Medium | `JustfileFacts` read once at startup; never refreshed after a rebuild changes the justfile |
| B1 | Medium | D-Bus wire constants duplicated across crates instead of living in the shared crate |
| B3 | Medium | `/etc/nixos/justfile` hardcoded in four places |
| B4 | Medium | `just --dump` invocation copy-pasted three times; the third copy has already drifted |
| C2 | Medium | Two competing "recipes for this role" implementations; the catalog's is dead |
| C3 | Medium | Detached `tokio::spawn` pumps with dropped `JoinHandle`s; `send_blocking` on the GTK main loop |
| C4 | Medium | Per-recipe `refresh` metadata is inert; the GUI re-reads everything after every job |
| D2–D5 | Medium | `refresh`, `Version`/`Justfile` properties, `Excluded::reason`, `VEXOS_ASSUME_YES` all dead |
| E1–E3 | Medium | `anyhow`, `serde` (GUI), `serde_json` (daemon) declared and never used |
| *(15 more)* | Low–Med | See sections below |

---

## 1. Architectural anti-patterns and design problems

### A1 — Structured D-Bus errors are flattened to a string and re-parsed by substring match — **HIGH**

**Files:** [src/dbus_client.rs:222-230](src/dbus_client.rs#L222-L230), [src/dbus_client.rs:48-52](src/dbus_client.rs#L48-L52), [daemon/src/interface.rs:71-104](daemon/src/interface.rs#L71-L104), [src/ui/run_page.rs:151-166](src/ui/run_page.rs#L151-L166)

The daemon classifies every failure precisely: `AccessDenied` for an unknown or
terminal-only recipe, `InvalidArgs` for a bad value or a missing file, `LimitsExceeded`
for the concurrency cap, `AccessDenied` again for a polkit refusal.

`friendly()` then throws the error **name** away and keeps only the human message:

```rust
zbus::Error::MethodError(_, Some(message), _) => message.clone(),
```

and `is_declined()` recovers a classification by grepping that prose:

```rust
message.contains("Not authorized") || message.contains("dismissed")
```

Three problems follow:

1. The classification already existed one process away and was deliberately discarded.
   Rewording `"Not authorized to run this operation"` in `interface.rs:102` silently
   breaks the declined-vs-failed UI with nothing to catch it — no type, no test.
2. `"dismissed"` never appears in any message the daemon can emit. That arm is
   speculative and dead.
3. It conflates two different situations. A user who dismissed the password prompt and
   a user who is not an administrator both land on
   `"This operation needs administrator authorization to run."` (`run_page.rs:163-164`).
   The first needs "try again"; the second needs "ask an admin".

polkit itself returns the discriminator — `is_challenge` — and the daemon already
deserializes it and then discards it (see **D6**).

### A2 — A finishing job destroys the run page the user is watching — **HIGH**

**Files:** [src/app.rs:106-116](src/app.rs#L106-L116), [src/ui/window.rs:142-150](src/ui/window.rs#L142-L150), [src/ui/category_page.rs:147-151](src/ui/category_page.rs#L147-L151)

On `JobFinished` the app calls, in order: `page.finished(exit_code)` — which writes the
result the user has been waiting for — then `refresh_state()`, then
`window.state_changed()`. `state_changed()` does:

```rust
if current == DASHBOARD && self.navigation.visible_page().is_some() {
    self.navigation.replace(&[page]);   // whole stack, not a push
}
```

`Window::push` (`window.rs:125-127`) never updates `current`. So when a job is launched
from a dashboard quick action, `current` is still `__dashboard` while the run page sits
on top of the stack — and `replace(&[dashboard])` tears that run page out at the exact
instant it printed "Finished" or "Failed (exit code 1)".

Launched from a *category* page the same job does not do this, because `current` is the
category id. The behaviour of watching a rebuild finish therefore depends on which of
two identical-looking buttons started it.

### A3 — The GUI's D-Bus command loop serializes behind a human-interactive call — **HIGH**

**File:** [src/dbus_client.rs:178-200](src/dbus_client.rs#L178-L200)

```rust
while let Ok(command) = commands.recv().await {
    match command {
        Command::Run { .. } => {
            let event = match proxy.run_recipe(&recipe, args).await { ... };
```

`run_recipe` does not return until the daemon has finished `auth::check`, which blocks
on the desktop's polkit agent showing a password dialog — an unbounded, human-paced
wait. Everything behind it in the channel is stalled: a second `Run`, and **every
`Cancel`**.

The consequences are concrete. Two quick actions started in sequence leave the second
sitting at "Waiting for authorization…" with no prompt on screen and no explanation
until the first is answered. And `Cancel`, the documented escape hatch, cannot be
delivered while any request anywhere is awaiting authorization.

The runtime is already multi-task capable (`tokio::spawn` is used at `:147` and `:164`);
`Run` dispatch simply is not spawned.

### A4 — Reference cycles and a leaking job map — **MEDIUM-HIGH**

**Files:** [src/app.rs:23-28](src/app.rs#L23-L28), [src/app.rs:154-155](src/app.rs#L154-L155), [src/ui/window.rs:13-21](src/ui/window.rs#L13-L21), [src/ui/run_page.rs:23-35](src/ui/run_page.rs#L23-L35)

Two strong cycles, no `Weak` anywhere in the codebase:

- `App.window: RefCell<Option<Window>>` → `Window.app: Rc<App>`
- `App.pending`/`App.running: HashMap<_, RunPage>` → `RunPage.app: Rc<App>`

`App` and every `RunPage` ever created are leaked for the process lifetime. On a
desktop app that is survivable, but it is unbounded in the number of recipes run, and
each `RunPage` retains a `TextBuffer` holding up to `MAX_LINES = 5000` lines
(`run_page.rs:16`).

Separately, `running` (`app.rs:26`) is only ever drained by `Event::Finished`
(`app.rs:107`). If the daemon crashes, is SIGKILLed, or idle-exits mid-job, no
`JobFinished` arrives: the entry stays forever, the spinner spins forever, and the user
is never told. There is no timeout, no `NameOwnerChanged` watch, and no reconnect —
`drain_with_error` (`dbus_client.rs:205-220`) only covers a connection that failed at
*startup*, never one that drops mid-session. After such a drop the `serve()` loop keeps
accepting commands against a dead proxy.

### A5 — The terminal escape hatch bypasses the whole security architecture — **MEDIUM**

**File:** [src/ui/category_page.rs:156-231](src/ui/category_page.rs#L156-L231)

```rust
format!("{command}; exec bash")   // handed to `bash -lc`
```

This is the one place the GUI constructs a shell command line, which the project's own
design notes identify as the boundary that must not be crossed. It is safe *today* —
`command` is `format!("just {}", recipe.name)` and the name comes from the compiled-in
catalog — but the safety is incidental, not structural, and one future interpolation of
a user value into `command` turns it into shell injection with nothing in the way.

More significant: this path runs **entirely outside the daemon**. No catalog argv
validation, no polkit risk tier, no journald audit record. The recipes routed through
it are `secrets-init`, `create-zfs-pool`, `create-mergerfs-pool`,
`attach-remote-storage` and `enable` — arguably the highest-consequence entries in the
catalog. `data/io.github.vexportal.metainfo.xml:18-20` promises "every operation is
recorded in the journal"; for these five, none is.

Secondary problems in the same function:

- Four terminal emulators are hardcoded (`kgx`, `gnome-terminal`, `ptyxis`, `xterm`)
  rather than resolved through `gio::AppInfo` or the desktop portal.
- `spawn().is_ok()` at `:223-228` is treated as "launched". It is true for a binary
  that exists and dies one millisecond later (no display, missing bash) — the user then
  gets the toast "Opened a terminal running `just …`" and no terminal.
- The child is never waited on, so each launch leaves a zombie until it exits.

### A6 — `enable` is terminal-only *and* declares a required parameter that is silently dropped — **MEDIUM**

**Files:** [catalog/src/catalog.toml:339-353](catalog/src/catalog.toml#L339-L353), [src/ui/category_page.rs:88-103](src/ui/category_page.rs#L88-L103), [src/ui/category_page.rs:156-157](src/ui/category_page.rs#L156-L157)

`enable` is `terminal = true` and declares a required `choice-dynamic` parameter
`service`. `activate()` checks `recipe.terminal` **before** it looks at
`recipe.params`, so the dialog is never built, and `open_in_terminal` runs
`just enable` — with no service argument at all.

The declared widget is unreachable dead data. The root cause is that
`Recipe::params` is being used for two incompatible purposes: the **justfile
signature** (needed by `drift::compare` at `drift.rs:166-185`, which is why the param
must be declared) and the **GUI form spec** (consumed by `arg_dialog`). One field
cannot be both.

### A7 — `Cancel` has no ownership model — **MEDIUM**

**Files:** [daemon/src/interface.rs:122-142](daemon/src/interface.rs#L122-L142), [daemon/src/cancel.rs:11-16](daemon/src/cancel.rs#L11-L16), [data/io.github.vexportal.policy:50-58](data/io.github.vexportal.policy#L50-L58)

`Cancel` gates on `io.github.vexportal.cancel`, whose policy is `allow_active = yes` —
no prompt for any locally active session. It then cancels *any* job id handed to it.
`JobHandle` records `job_id` and `recipe` but never the caller, so the daemon has no way
to check ownership even if it wanted to.

On a multi-session desktop (or with `ListJobs` enumerating ids for free — see **D1**)
any active user can SIGTERM another user's in-flight `nixos-rebuild`. The
`run_recipe` path is carefully authenticated per risk tier; the path that *stops* those
same operations is not.

### A8 — Concurrency limit checked outside the lock that would enforce it — **MEDIUM**

**File:** [daemon/src/interface.rs:88-116](daemon/src/interface.rs#L88-L116)

```rust
self.reap().await;
if self.jobs.lock().await.len() >= MAX_CONCURRENT_JOBS { ... }   // lock released here
...
if !auth::check(connection, &caller, action).await ... { ... }   // minutes
...
self.jobs.lock().await.insert(job_id.clone(), handle);           // lock retaken here
```

The check and the insert are separated by an `await` on a human-paced polkit prompt.
N callers can all pass the check and all insert, so `MAX_CONCURRENT_JOBS = 3` bounds
nothing under concurrency — which is precisely the case it exists for
(`interface.rs:22-24` says "past that, a caller is looping").

### A9 — The `Justfile` property exists so the GUI need not guess; the GUI guesses anyway — **MEDIUM**

**Files:** [daemon/src/interface.rs:178-183](daemon/src/interface.rs#L178-L183), [src/dbus_client.rs:248-252](src/dbus_client.rs#L248-L252), [src/just.rs:13](src/just.rs#L13), [nix/module.nix:24-32](nix/module.nix#L24-L32)

The daemon exposes `Justfile` with the explicit comment *"so the GUI can read the same
one for its dropdowns and drift check instead of guessing"*. The GUI declares it in the
proxy. **Nothing calls it.** `src/just.rs:13` hardcodes `/etc/nixos/justfile`.

The mechanism is complete on both sides and connected on neither. It is not cosmetic:
`programs.vexportal.justfile` is a real NixOS option, so any deployment that sets it
gets a GUI whose dropdowns, availability filter and drift banner all describe a
different file than the one the daemon will actually execute — with no warning that the
two diverged.

### A12 — Justfile facts are read once and never refreshed — **MEDIUM**

**Files:** [src/app.rs:133](src/app.rs#L133), [src/app.rs:109-114](src/app.rs#L109-L114), [src/ui/window.rs:142-150](src/ui/window.rs#L142-L150)

`JustfileFacts::read()` runs exactly once, during `app::build`. `refresh_state()`
re-reads `SystemState` only. But `/etc/nixos/justfile` is *a copy made by the last
rebuild* — that is the stated premise of the whole `available`/`Missing`-drift design
(`src/just.rs:20-26`).

So the one operation guaranteed to change the justfile — a successful `rebuild` or
`switch`, run from inside VexPortal — is the one thing that cannot update VexPortal's
view of it. The "N operations are hidden — this host has not rebuilt since vexos-nix
added them" banner persists after the rebuild that fixed it, and the newly available
recipes stay hidden until the app is restarted. `state_changed()` also never rebuilds
the sidebar, so `visible_categories()` is likewise frozen at startup.

### A10 — Idle shutdown polls a deadline it already knows — **LOW-MEDIUM**

**Files:** [daemon/src/main.rs:29-30](daemon/src/main.rs#L29-L30), [daemon/src/main.rs:85-90](daemon/src/main.rs#L85-L90), [daemon/src/lifecycle.rs](daemon/src/lifecycle.rs)

The daemon wakes every 15 seconds to ask a mutex whether 180 seconds have elapsed —
12 wakeups per idle minute of a root process, plus a lock acquisition and an object
server lookup each time. `IdleTracker` stores `last_activity + timeout`; a
`tokio::time::sleep_until` reset by `mark_active` expresses the same policy with one
timer and no polling.

### A11 — Only `run_recipe` marks activity — **LOW**

**File:** [daemon/src/interface.rs:113](daemon/src/interface.rs#L113)

`idle.mark_active()` is called from `run_recipe` alone. `cancel` and `list_jobs` do not
touch it. A client using `ListJobs` to track work (the reattach story of **D1**) does
not hold the daemon up, and a client that only cancels can have the daemon exit from
under it mid-conversation. `has_running_jobs` covers in-flight work but not idle
clients.

---

## 2. Structural inconsistencies

### B1 — Wire-protocol constants duplicated across crates — **MEDIUM**

**Files:** [daemon/src/executor.rs:20-24](daemon/src/executor.rs#L20-L24), [src/ui/run_page.rs:260-261](src/ui/run_page.rs#L260-L261), [src/dbus_client.rs:232-236](src/dbus_client.rs#L232-L236), [daemon/src/main.rs:26](daemon/src/main.rs#L26), [catalog/src/lib.rs:85-91](catalog/src/lib.rs#L85-L91)

```rust
// daemon/src/executor.rs
pub const STREAM_STDOUT: u32 = 0;
pub const STREAM_STDERR: u32 = 1;

// src/ui/run_page.rs
/// Matches `executor::STREAM_STDERR` in the daemon.
pub const STDERR: u32 = 1;
```

A comment is the only thing holding the two halves of a wire protocol together.
The same applies to the interface name, bus name and object path, each written twice
(`daemon/src/main.rs:26` + `daemon/src/executor.rs:20` vs `src/dbus_client.rs:232-236`).

`vexportal-catalog` exists specifically to be the shared source of truth between the two
binaries and already carries the polkit action ids (`Risk::polkit_action`) — but no
other part of the D-Bus contract. The natural home for a `Stream` enum, the bus name,
the object path and the interface name is that crate, where a type mismatch would be a
compile error.

Related: `stream: u32` is passed raw across three layers (`interface.rs:157-163`,
`dbus_client.rs:37-41`, `run_page.rs:168-171`) where a two-variant enum would do.

### B2 — Constant placement and self-referential paths — **LOW-MEDIUM**

**File:** [src/ui/run_page.rs:16](src/ui/run_page.rs#L16), [src/ui/run_page.rs:169](src/ui/run_page.rs#L169), [src/ui/run_page.rs:261](src/ui/run_page.rs#L261)

`MAX_LINES` is at the top of the file, module convention; `STDERR` is at line 261,
below the `impl`, in the middle of free functions. And it is referenced from inside its
own module by absolute path:

```rust
let tag = (stream == crate::ui::run_page::STDERR).then_some("stderr");
```

`STDERR` alone is in scope. The absolute path reads as if it were importing from
elsewhere.

### B3 — `/etc/nixos/justfile` hardcoded in four places — **MEDIUM**

[src/just.rs:13](src/just.rs#L13) · [daemon/src/config.rs:8](daemon/src/config.rs#L8) · [catalog/tests/drift_against_justfile.rs:15](catalog/tests/drift_against_justfile.rs#L15) · [nix/module.nix:26](nix/module.nix#L26)

And `/etc/nixos` separately at [src/just.rs:41](src/just.rs#L41),
[daemon/src/config.rs:46-48](daemon/src/config.rs#L46-L48),
[src/ui/category_page.rs:181](src/ui/category_page.rs#L181), `:203`, `:217`,
[catalog/tests/drift_against_justfile.rs:29](catalog/tests/drift_against_justfile.rs#L29), `:98`.

The daemon's copy is the authoritative one (it is what actually gets executed) and is
already exported over D-Bus. See **A9** — the duplication is what makes the unwired
property harmless-looking and quietly wrong.

### B4 — The `just --dump` invocation is copy-pasted three times, and has already drifted — **MEDIUM**

[src/just.rs:37-47](src/just.rs#L37-L47) · [catalog/tests/drift_against_justfile.rs:24-33](catalog/tests/drift_against_justfile.rs#L24-L33) · [catalog/tests/drift_against_justfile.rs:94-104](catalog/tests/drift_against_justfile.rs#L94-L104)

The same seven-argument `just` invocation, three times. It belongs beside
`JustDump::parse` in `catalog/src/drift.rs`, which is already the shared owner of the
dump format.

The drift is not hypothetical — the third copy is already inconsistent with the other
two. `drift_against_justfile.rs:43-47` asserts `output.status.success()`;
`drift_against_justfile.rs:94-108` does not, and hands the (empty) stdout of a failed
`just` straight to `JustDump::parse`. When `just` fails there, the test reports
`"_feature_names is empty or missing from the justfile"` instead of the actual error.

### B5 — The colour→tag-name mapping is written twice, identically — **LOW-MEDIUM**

[src/ui/ansi.rs:64-74](src/ui/ansi.rs#L64-L74) (`Color::tag`, private) and
[src/ui/run_page.rs:310-320](src/ui/run_page.rs#L310-L320) (`color_tag_name`, free function).

Character-for-character the same match. `register_tags` (`run_page.rs:263-308`) must
produce exactly the tag names `Style::tag_name` (`ansi.rs:24-36`) will later ask for, or
`insert_with_tags_by_name` fails at runtime with a GTK warning and unstyled text. Two
independently maintained hand-written matches is the fragile way to guarantee that;
making `Color::tag` public and deleting `color_tag_name` makes it structural.

### B6 — Two import paths for the same crate — **LOW**

`gio::resources_register_include!` at [src/main.rs:19](src/main.rs#L19) versus
`gtk::gio::File` / `gtk::gio::Cancellable` at
[src/ui/arg_dialog.rs:210](src/ui/arg_dialog.rs#L210), `:218`, `:220`. `gio` is a
direct dependency (`Cargo.toml:45`); the re-export path is used anyway in one file.

### B7 — Naming — **LOW**

- [src/ui/run_page.rs:204-206](src/ui/run_page.rs#L204-L206): `append_plain` is not
  plain — it forces the `stderr` tag. It is called only for error text
  (`failed_to_start`, `:156`), so the name is exactly backwards from what it does.
- [src/ui/window.rs:154](src/ui/window.rs#L154): `box_` — a trailing underscore to dodge
  the keyword, in a file where every other local reads normally.
- [src/ui/window.rs:137-139](src/ui/window.rs#L137-L139): `Window::root()` returns an
  `adw::ApplicationWindow` from a type also called `Window`; call sites read
  `window.root()` where they mean "the real window".

### B8 — Repository layout — **LOW**

- `.github/docs/subagent_docs/` holds three agent-workflow markdown documents while
  `.github/` contains **no workflows at all**. `.github/` is GitHub's configuration
  directory; process artifacts in it will be mistaken for CI config.
- `docs/vexos-nix-prompt.md` (161 lines) is a chat handoff prompt addressed to a
  *different repository* (`~/Projects/vexos-nix`), checked into this one. It contains
  genuinely valuable empirical findings — the `read`-at-EOF behaviour table — that
  belong in `README.md` or a design note, not in a prompt.

### B9 — Collection choices in `check_consistency` — **LOW**

[catalog/src/lib.rs:279-289](catalog/src/lib.rs#L279-L289)

```rust
let known: Vec<&str> = ...;              // then linear `known.contains(...)` per recipe
let mut seen: HashMap<&str, ()> = ...;   // a HashMap used as a set
```

`HashSet` for both. Trivial in cost at 42 recipes; it is listed because
`HashMap<_, ()>` signals to a reader that the value type once mattered.

---

## 3. Inconsistent patterns

### C1 — Four error-handling styles for the same class of failure — **HIGH**

| Style | Where |
|---|---|
| Typed `thiserror` enum | [catalog/src/lib.rs:261-267](catalog/src/lib.rs#L261-L267), [catalog/src/validate.rs:48-66](catalog/src/validate.rs#L48-L66), [catalog/src/format.rs:14-24](catalog/src/format.rs#L14-L24), [src/system/variant.rs:29-37](src/system/variant.rs#L29-L37) |
| Stringly-typed `Result<_, String>` | [daemon/src/auth.rs:16](daemon/src/auth.rs#L16), [daemon/src/config.rs:27](daemon/src/config.rs#L27) |
| Error-as-a-struct-field | [src/just.rs:32](src/just.rs#L32), formatted into prose at [src/just.rs:49-76](src/just.rs#L49-L76) |
| Silently swallowed | all of [src/system/state.rs:56-124](src/system/state.rs#L56-L124) |

The fourth is the sharpest. Every reader in `state.rs` converts any failure to
`None`/empty via `.ok()?` with **no log line whatsoever**:

```rust
fn read_features() -> Vec<(String, bool)> {
    let Ok(contents) = std::fs::read_to_string(FEATURES_FILE) else {
        return Vec::new();
    };
```

A permission problem or a malformed `features.nix` renders as "this host has no
features" and the Features group is simply omitted from the dashboard
(`dashboard.rs:107-110`). Meanwhile `variant.rs`, reading a file in the same directory
for the same dashboard, defines a three-variant error enum and surfaces the reason.
Same layer, same kind of input, opposite policy — and the module doc
(`system/mod.rs:3-6`) presents them as one uniform thing.

`daemon/src/auth.rs:16` returning `Result<bool, String>` in a crate that otherwise
carries typed errors is the same inconsistency in the daemon: the caller at
`interface.rs:96-98` can only do `.map_err(fdo::Error::Failed)`, collapsing "polkit is
unreachable" and "CheckAuthorization failed" into one opaque `Failed`.

### C2 — Two competing "recipes for this role" implementations, one dead — **MEDIUM**

[catalog/src/lib.rs:339-348](catalog/src/lib.rs#L339-L348) (`Catalog::categories_for_role`) versus
[src/app.rs:41-56](src/app.rs#L41-L56) (`App::visible_in` / `App::visible_categories`).

They compute the same thing except that the `App` version also filters on
`facts.is_available()`. Because the availability filter is mandatory in practice, the
catalog version has **zero callers** — the whole method is dead. `Catalog::for_role`
(`lib.rs:326-328`) is likewise called only from a test (`lib.rs:365`).

The layering question was never settled: the catalog crate models "which recipes exist
for a role", the GUI models "which recipes exist for a role *on this host*", and the
first is redundant the moment the second exists.

Related inefficiency: `visible_categories` calls `visible_in` per category
(`app.rs:54`), each of which walks all recipes and rebuilds a `Vec`, only to check
`is_empty()`.

### C3 — Async patterns — **MEDIUM**

**File:** [src/dbus_client.rs](src/dbus_client.rs)

The overall model — tokio on its own thread, `async_channel` to the GTK main loop — is
sound and well documented (`:3-6`). Inside it, three things are inconsistent:

- `Client::send` uses `send_blocking` (`:105`) and is called from GTK button handlers on
  the main loop. The channel is unbounded so it does not block today, but a blocking
  call on the UI thread is a latent freeze one bounded-channel change away.
- The two signal pumps are `tokio::spawn`ed and their `JoinHandle`s dropped
  (`:147`, `:164`). If either stream ends or errors the task exits silently and the GUI
  simply stops receiving output, with no event, no log, and no way to notice.
- `if let Ok(mut output) = output` / `if let Ok(mut finished) = finished` (`:145`, `:162`)
  discard the subscription error entirely. Failing to subscribe to `JobFinished` means
  every run page hangs at "Running…" forever — logged nowhere.

Meanwhile `Run` dispatch, which is the one thing that *should* be spawned, is not
(**A3**).

### C4 — Two competing "something changed" mechanisms — **MEDIUM**

`Recipe::refresh` ([catalog/src/lib.rs:209-211](catalog/src/lib.rs#L209-L211)) is a
per-recipe list of state keys to re-read, populated on seven recipes
(`catalog.toml:90, 131, 181, 191, 214, 232, 605`) with values `generation`, `variant`,
`features`, `hostname`.

No code reads the field. [src/app.rs:109-114](src/app.rs#L109-L114) instead re-reads
*all* system state after *every* job, including read-only ones that changed nothing.
The designed fine-grained mechanism is inert data, and its existence makes the coarse
one look deliberate rather than provisional.

### C5 — Validation split across three layers with three different rulesets — **LOW-MEDIUM**

| Layer | Checks | File |
|---|---|---|
| GUI dialog | required-fields only | [src/ui/arg_dialog.rs:99-113](src/ui/arg_dialog.rs#L99-L113) |
| Shared crate | formats, choices, positional argv, secrets | [catalog/src/validate.rs:73-183](catalog/src/validate.rs#L73-L183) |
| Daemon | path existence | [daemon/src/interface.rs:81-86](daemon/src/interface.rs#L81-L86) |

The GUI links `vexportal-catalog` and never calls `validate::build`. Its own comment
states the goal:

```rust
// Check here rather than letting the daemon reject it: a missing required
// field should point at the field, not come back as a D-Bus error.
```

That goal is met for missing fields and missed for everything else. A malformed
hostname, an out-of-range NixOS version, a non-absolute path — each takes a full D-Bus
round trip, a polkit-tier decision it will never reach, and comes back as a red banner
on a run page rather than an error on the field that caused it.

### C6 — Empty string doing the work of `Option` — **LOW**

[src/ui/arg_dialog.rs:22-42](src/ui/arg_dialog.rs#L22-L42), filtered at `:96`.

`Field::value()` returns `String`, and `""` is overloaded to mean three things:
"user left it blank", "the optional-choice sentinel *Leave unchanged* was selected"
(`:29-31`), and "no file chosen" (`:39`). The caller then drops every empty value.
A legitimately empty answer cannot be expressed. `Invocation::stdin`
(`validate.rs:20`) uses `Option<String>` for exactly this distinction one layer down.

### C7 — Log levels — **LOW**

GUI defaults to `warn` ([src/main.rs:15](src/main.rs#L15)); daemon to `info`
([daemon/src/main.rs:34](daemon/src/main.rs#L34)). The GUI's `log` calls are almost all
`error!`, with two `warn!`. Combined with **C1**, the practical effect is that the GUI
emits nothing at all about the state readers that fail silently — the `warn` default
is not the reason, the missing call sites are.

### C8 — Audit coverage is uneven — **LOW**

[daemon/src/interface.rs:71-104](daemon/src/interface.rs#L71-L104) calls
`audit::rejected` for validation failures, missing files and polkit denials — but the
concurrency-limit rejection at `:89-93` returns without an audit line, and `cancel`'s
authorization failure at `:133-138` does not audit either, though `audit::cancelled`
exists (`audit.rs:27-29`) and is called from the executor. The audit module's own doc
(`audit.rs:1-5`) presents journald as the complete record of "what the portal was asked
to do".

---

## 4. Half-implemented or abandoned

### D1 — Job reattachment — **HIGH**

[daemon/src/interface.rs:144-154](daemon/src/interface.rs#L144-L154) ·
[src/dbus_client.rs:240](src/dbus_client.rs#L240)

`ListJobs` is fully implemented in the daemon — reaps, returns `(job_id, recipe)` —
with the stated purpose *"so a GUI that was restarted can reattach"*. It is declared in
the GUI's proxy trait. **Nothing calls it, and no reattach path exists anywhere in the
GUI.**

This is the largest gap between designed and built behaviour in the repo, and it is
load-bearing: the daemon deliberately outlives the GUI, and a `rebuild` can run for
tens of minutes. A user who closes the window (or whose GUI crashes) has no way back to
a running job — the output is gone, the Cancel button is gone, and the next launch shows
an idle portal above a machine mid-rebuild. The daemon side is done; only the client
side is missing.

### D2 — `Recipe::refresh` — **MEDIUM**

See **C4**. Declared, documented, populated on seven recipes, read by nothing.

### D3 — `Version` and `Justfile` D-Bus properties — **MEDIUM**

[daemon/src/interface.rs:173-183](daemon/src/interface.rs#L173-L183) ·
[src/dbus_client.rs:248-252](src/dbus_client.rs#L248-L252)

Implemented in the daemon, declared in the GUI proxy, called by neither. `Version` would
be the obvious guard against a GUI and daemon from different generations talking to each
other — a real hazard given that the catalog is compiled into both and drift between
them is the thing the whole `drift` module exists to detect. `Justfile` is **A9**.

### D4 — `Excluded::reason` — **MEDIUM**

[catalog/src/lib.rs:244-249](catalog/src/lib.rs#L244-L249) ·
[catalog/src/catalog.toml:658-670](catalog/src/catalog.toml#L658-L670) ·
[catalog/src/drift.rs:148](catalog/src/drift.rs#L148)

Three entries carry a `reason`; `compare` reads only `.name`. The reasons are good
documentation ("Updates and upgrades are handled by Up, not VexPortal") but nothing
surfaces or even asserts them. An exclusion with an empty or stale reason is
indistinguishable from a good one — which matters, because an exclusion is how a recipe
gets permanently hidden from the drift check.

### D5 — `VEXOS_ASSUME_YES` and the `needs_upstream` recipes — **MEDIUM**

[daemon/src/config.rs:62-67](daemon/src/config.rs#L62-L67) ·
[catalog/src/lib.rs:216-219](catalog/src/lib.rs#L216-L219) ·
[docs/vexos-nix-prompt.md:20-30](docs/vexos-nix-prompt.md#L20-L30)

The daemon sets `VEXOS_ASSUME_YES=1` for a contract that does not exist upstream — its
own comment says so. `VEXPORTAL=1` has no reader either. Six recipes carry
`needs_upstream = true` (`switch`, `fix-flake`, `restore-plex`, `setup-rdp`,
`set-hostname`, `reset-defaults`) and are shipped as runnable through the form path with
an explanatory badge (`category_page.rs:55-62`).

By the project's own empirically verified table in `docs/vexos-nix-prompt.md`, three of
those six (`reset-defaults`, `restore-plex`, `setup-rdp`) use a plain `read` under
`set -euo pipefail` and will therefore **exit 1** when stdin hits EOF. They are offered
anyway. The badge text says the recipe "will either take the prompt's default answer or
stop with an error" — accurate, but it is a known-broken path presented as a working
one, and nothing distinguishes the three that degrade gracefully from the three that
cannot work at all.

This is an honest, well-documented blocked dependency rather than abandonment — but it
is unfinished work shipped in the default UI, and the distinction is not encoded
anywhere a future change could act on.

### D6 — `AuthorizationResult.is_challenge` — **LOW-MEDIUM**

[daemon/src/auth.rs:62-69](daemon/src/auth.rs#L62-L69)

```rust
#[allow(dead_code)]
pub is_challenge: bool,
```

Deserialized from polkit and explicitly silenced. It is exactly the discriminator that
would let the daemon tell the GUI "the user was asked and declined" versus "the user is
not an administrator" as a *typed* result — removing the need for the string matching in
**A1** entirely. The field is already on the wire; only the plumbing to `is_declined` is
missing.

### D7 — Dead public catalog API — **LOW**

`Catalog::categories_for_role` (zero callers) and `Catalog::for_role` (test-only) —
see **C2**.

### D8 — Unused parameter silenced rather than removed — **LOW**

[src/ui/category_page.rs:156](src/ui/category_page.rs#L156), `:172`

```rust
fn open_in_terminal(app: &Rc<App>, window: &Window, recipe: &Recipe) {
    ...
    let _ = app;
}
```

`let _ = app;` at the end of the function to suppress the warning, rather than dropping
the parameter and its two call-site arguments.

### D9 — `check_consistency` validates recipes but not categories — **LOW**

[catalog/src/lib.rs:278-315](catalog/src/lib.rs#L278-L315)

It catches unknown categories, duplicate recipe names, empty role lists and misordered
optionals. It does **not** catch duplicate *category* ids (which would render a
duplicate sidebar entry that the index-based row mapping at `window.rs:80-89` would then
resolve to the wrong page), unknown `refresh` keys (moot — **C4**), or the
terminal-recipe-with-params contradiction of **A6**. `every_category_is_used` exists as
a test (`lib.rs:372-382`) but duplicate-id does not.

---

## 5. Dependencies

### E1 — `anyhow` is declared twice and used nowhere — **MEDIUM**

[Cargo.toml:19](Cargo.toml#L19) (workspace) and [Cargo.toml:39](Cargo.toml#L39) (root package).

Zero occurrences of `anyhow` in any `.rs` file in the workspace. Worse than dead weight:
it sits directly beside `thiserror` in both dependency lists, advertising an
`anyhow`-for-applications / `thiserror`-for-libraries strategy the code does not follow
(see **C1**, where the application layer uses stringly-typed errors and silent
swallowing instead).

### E2 — `serde` is a direct GUI dependency and unused there — **MEDIUM**

[Cargo.toml:36](Cargo.toml#L36)

No `serde::` path and no `#[derive(Serialize/Deserialize)]` anywhere in `src/`. The only
serde-family use in the GUI is `serde_json::Value` at
[src/system/state.rs:115](src/system/state.rs#L115), which needs `serde_json`, not
`serde` + `derive`.

### E3 — `serde_json` is a direct daemon dependency and unused there — **MEDIUM**

[daemon/Cargo.toml:18](daemon/Cargo.toml#L18)

No occurrence in `daemon/src/`. The daemon's only serde use is
`#[derive(serde::Deserialize)]` on `AuthorizationResult`
([daemon/src/auth.rs:62](daemon/src/auth.rs#L62)), which needs `serde` — declared
separately at `:16` and correctly used.

E1–E3 together mean each of the three crates declares at least one dependency it does
not use. This is worth a `cargo-udeps` / `cargo-machete` step in
`scripts/preflight.sh`, which currently gates on fmt/check/clippy/test only.

### E4 — The GUI inherits the daemon's tokio feature set — **LOW-MEDIUM**

[Cargo.toml:24](Cargo.toml#L24)

```toml
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "process", "io-util", "sync", "time", "signal"] }
```

Both binaries take this one workspace entry. The GUI uses `rt` (a `new_current_thread`
runtime) and `tokio::spawn` — nothing else. `process`, `signal`, `io-util` and
`rt-multi-thread` exist for the daemon's executor and are compiled into the GUI binary
regardless, because workspace-level dependency sharing unifies features. Splitting the
GUI's tokio entry to `features = ["rt"]` is the accurate declaration.

### E5 — `flake-utils` for one call, exposing non-buildable systems — **LOW**

[flake.nix:6](flake.nix#L6), [flake.nix:10](flake.nix#L10), [nix/package.nix:77](nix/package.nix#L77)

`flake-utils` is a whole flake input for a single `eachDefaultSystem`. That call exposes
`packages.default` on `aarch64-darwin` and `x86_64-darwin`, where `meta.platforms =
lib.platforms.linux` — and where GTK4/libadwaita/polkit/`/etc/nixos` make the package
meaningless regardless. A hand-rolled `forAllSystems` over the two Linux systems drops
the input and stops advertising builds that cannot succeed.

### E6 — `rust-toolchain.toml` is a trap, not a pin — **LOW**

[rust-toolchain.toml](rust-toolchain.toml) · [flake.nix:19-20](flake.nix#L19-L20) · [nix/package.nix:13](nix/package.nix#L13)

```toml
[toolchain]
channel = "stable"
```

Every supported build path supplies its own toolchain: the devShell provides
`pkgs.cargo`/`pkgs.rustc`, and the package uses `rustPlatform`. Neither consults this
file. It takes effect only when a `rustup` shim is on `PATH` — which is exactly the
failure mode `CLAUDE.md`'s FORBIDDEN COMMANDS section exists to prevent, and which
this environment is already known to hit. The file makes a bare `cargo build` look
supported. Deleting it, or pinning a real version that matches nixpkgs, both beat the
current state.

### E7 — `nix flake check` and `scripts/preflight.sh` disagree on what "checked" means — **LOW**

[flake.nix:37](flake.nix#L37) · [scripts/preflight.sh:13-23](scripts/preflight.sh#L13-L23)

```nix
checks.default = self.packages.${system}.default;
```

`nix flake check` therefore builds the package and runs `cargo test` in the sandbox —
but never `cargo fmt --check` and never `clippy`, both of which `preflight.sh` treats as
mandatory gates. Two "run all the checks" entry points, two different definitions, and
`CLAUDE.md` lists both as test commands. A `checks.fmt` / `checks.clippy` derivation
would reconcile them, and would also give the project real CI once a workflow exists —
`.github/` currently has none.

### E8 — `module.nix` uses `types.path` where it means `types.str` — **LOW**

[nix/module.nix:24-26](nix/module.nix#L24-L26)

```nix
justfile = lib.mkOption {
  type = lib.types.path;
  default = "/etc/nixos/justfile";
```

`types.path` accepts a Nix *path literal*. A user who writes
`programs.vexportal.justfile = /etc/nixos/justfile;` — the spelling the type invites —
gets the file **copied into the Nix store at evaluation time**, and the daemon then
permanently pointed at that frozen snapshot rather than the live file the next rebuild
will replace. The option's own description says it names the file the daemon runs
"recipes from" on the running system; `types.str` is what that means.

---

## Recommended order of attack

Nothing here is a rewrite. Grouped by what unblocks what:

1. **A2** (run page destroyed on finish) and **A3** (serialized polkit dispatch) — both
   are small, both are user-visible on the primary path, and neither needs a design
   decision.
2. **D1 + A9 + D3** together — wiring `ListJobs`, `Justfile` and `Version` is one
   coherent piece of work that finishes an architecture already fully built on the
   daemon side.
3. **A1 + D6** together — plumbing `is_challenge` through as a typed result deletes the
   string matching rather than patching it.
4. **B1 + B3 + B4 + B5** — move the shared contract into `vexportal-catalog` and delete
   four duplications. Mechanical, and it is what the shared crate is for.
5. **C1** — decide one error policy per layer. The specific ask is that
   `system/state.rs` stop swallowing failures silently while `system/variant.rs` next
   door reports them.
6. **A6 + C5** — settle whether `Recipe::params` describes the justfile signature or the
   GUI form, since it currently cannot correctly do both.
7. **E1–E3** — three unused dependencies, plus a `cargo-machete` line in
   `scripts/preflight.sh` so it stays fixed.
