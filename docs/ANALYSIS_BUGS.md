# VexPortal — Bugs & Code Quality Analysis

Scope: logic errors, security, performance, dead code, error handling.
Not covered here: architecture and module layout (see `ANALYSIS_ARCH.md`).

Whole workspace read: 4,606 lines of Rust across `vexportal`, `vexportal-catalog` and
`vexportal-daemon`, plus `catalog/src/catalog.toml` (42 recipes), `data/style.css`,
`data/*.policy`/`.conf`/`.service`, `nix/`, `flake.nix`, `build.rs`.

No code was modified and no build was run — this is a read-only analysis.

---

## Summary of the sharp ones

| # | Priority | Finding |
|---|----------|---------|
| L1 | High | Job output and completion can arrive before `Started`; lines are dropped, page can hang forever |
| L2 | High | One invalid UTF-8 byte silently truncates the rest of a job's output stream |
| L3 | High | Completing a job destroys the run page the user is reading |
| L4 | High | D-Bus connect is eager, not lazy as documented — activates the root daemon on every GUI launch |
| S1 | High | `AbsPath` and `FlakeRef` accept shell metacharacters; the test that appears to cover this passes by accident |
| E1 | High | Every reader in `system/state.rs` swallows failures with no log line |
| E2 | High | Unroutable output events are dropped with no else branch and no log |
| L5 | Med-High | A failed `Client::send` leaves the run page pending forever |
| L6 | Med-High | `kernel-build-log` is a follow/tail recipe with no timeout — wedges a concurrency slot permanently |
| S2 | Med-High | `risk = "safe"` means *no polkit prompt*, and includes `build` and `upgrade-analysis` |
| P1 | Med-High | One D-Bus signal + main-loop iteration + TextBuffer insert per output line, unbatched |
| E3 | Med-High | Signal-subscription failures discarded; failing to subscribe hangs every run page silently |

Full detail below: **22 logic bugs**, **9 security findings**, **11 performance findings**,
**17 dead-code findings**, **17 error-handling findings**.

---

## 1. Logic errors and likely bugs

### L1 — Output and completion events can arrive before `Started`; output is silently dropped and the page can hang forever — **HIGH**

**Files:** [src/app.rs:77-118](src/app.rs#L77-L118), [daemon/src/interface.rs:106-118](daemon/src/interface.rs#L106-L118), [daemon/src/executor.rs:41-45](daemon/src/executor.rs#L41-L45), [src/dbus_client.rs:145-176](src/dbus_client.rs#L145-L176)

The daemon starts the child **before** it replies to `run_recipe`:

```rust
let handle = executor::spawn(job_id.clone(), invocation, &self.config, connection.clone());
self.jobs.lock().await.insert(job_id.clone(), handle);
Ok(job_id)                                   // reply sent only now
```

`executor::spawn` immediately begins pumping stdout/stderr into `JobOutput` signals. So
signals for a job can be **put on the bus before the method reply is**.

On the GUI side the reply and the signals are handled by **two independent tokio tasks**
that both push into one `async_channel`: the `serve()` command loop (`dbus_client.rs:185`)
produces `Event::Started`, while the spawned pumps (`:147`, `:164`) produce
`Event::Output` / `Event::Finished`. Nothing orders them.

`App::handle` routes strictly by map membership:

```rust
Event::Output { job_id, stream, line } => {
    if let Some(page) = self.running.borrow().get(&job_id) {
        page.append(stream, &line);
    }
}                                            // no else — the line is discarded
```

Two concrete failures:

1. **Every line emitted before `Started` is processed is discarded**, with no buffer, no
   log and no `else`. For a fast recipe this is the first output the user would see. The
   comment at `dbus_client.rs:140-141` — *"Subscribe before any job can start, so no
   output is missed"* — is about the *subscription*; the loss happens later, in routing.
2. If `Finished` wins the race outright, `running.borrow_mut().remove(&job_id)` finds
   nothing, the event is dropped, and `Started` then inserts the page into `running`
   **after** the job has already ended. The page spins at "Running…" forever, the Cancel
   button stays live against a dead job, and the entry leaks in `running` permanently
   (the daemon's `reap()` will have dropped its side).

This is not theoretical for the 11 `risk = "safe"` recipes, which take **no polkit
prompt at all** (`data/io.github.vexportal.policy:19-27`) and can complete in
milliseconds — `variant`, `features`, `services`, `status`, `harmonia-info`,
`kernel-build-status`.

The fix is to buffer events for unknown job ids (or key the page by `request_id` and
have the daemon echo it), not to reorder the daemon.

### L2 — One invalid UTF-8 byte silently truncates the rest of a job's output — **HIGH**

**File:** [daemon/src/executor.rs:161-170](daemon/src/executor.rs#L161-L170)

```rust
let mut lines = BufReader::new(reader).lines();
// `next_line` splits on newlines and drops invalid UTF-8, which is what a log
// view wants; a recipe emitting binary is not a case worth carrying.
while let Ok(Some(line)) = lines.next_line().await {
```

The comment states the opposite of what the code does. `tokio::io::Lines::next_line`
is `String`-based and returns `Err(ErrorKind::InvalidData)` on invalid UTF-8 — it does
not drop it. `while let Ok(...)` treats that `Err` exactly like end-of-stream: the loop
**exits**, the pump task ends, and **every remaining byte on that stream is lost** for
the rest of the job.

Nothing reports it. `run()` then awaits the pump handle at `:139-140`, gets `Ok(())`,
and the job continues to completion — so the user sees output stop dead partway through
a rebuild and then a "Finished" with no explanation.

Invalid UTF-8 in a build log is not exotic: a path with a non-UTF-8 filename, a
progress renderer emitting raw bytes, or a compiler echoing a source file in another
encoding all produce it. `BufReader::split(b'\n')` + `String::from_utf8_lossy` is what
the comment describes and what a log view actually wants.

### L3 — Completing a job destroys the run page the user is reading — **HIGH**

**Files:** [src/app.rs:106-116](src/app.rs#L106-L116), [src/ui/window.rs:125-127](src/ui/window.rs#L125-L127), [src/ui/window.rs:142-150](src/ui/window.rs#L142-L150)

```rust
Event::Finished { job_id, exit_code } => {
    if let Some(page) = self.running.borrow_mut().remove(&job_id) {
        page.finished(exit_code);        // writes "Finished" / "Failed (exit code N)"
        self.refresh_state();
        if let Some(window) = self.window.borrow().as_ref() {
            window.state_changed();      // -> navigation.replace(&[dashboard])
        }
```

`Window::push` never updates `current`, so a job launched from a dashboard quick action
leaves `current == "__dashboard"` while the run page sits on top of the stack.
`state_changed()` then calls `navigation.replace(&[dashboard])`, which replaces the
**entire** stack — tearing the run page out at the exact instant it printed the result.

Launched from a category page (`current` is the category id) the same job does not do
this. Two visually identical buttons, opposite behaviour, and the failing one is the
dashboard — the default landing page.

### L4 — The D-Bus client connects eagerly despite documenting lazy activation — **HIGH**

**Files:** [src/dbus_client.rs:60-88](src/dbus_client.rs#L60-L88), [src/dbus_client.rs:111-143](src/dbus_client.rs#L111-L143), [src/dbus_client.rs:248-252](src/dbus_client.rs#L248-L252)

```rust
/// Start the D-Bus thread. Connecting is deferred to the first command so that
/// launching VexPortal does not activate a root daemon before it is needed.
pub fn start() -> Self {
```

Nothing defers. `start()` spawns the thread, which immediately calls `serve()`, which
immediately does `Connection::system().await`, then `DaemonProxy::new(&connection)`,
then both `receive_*` subscriptions — **all before the first `commands.recv().await`**.

The proxy declares two `#[zbus(property)]` methods (`version`, `justfile`). zbus's
`proxy` macro enables property caching by default, so proxy construction issues
`org.freedesktop.DBus.Properties.GetAll` at the destination — a bus call to
`io.github.vexportal.Daemon`, which is D-Bus-activated. The stated design goal ("does
not activate a root daemon before it is needed") is therefore inverted: **merely opening
the window starts the root daemon**, which then sits resident for the full 180-second
`IDLE_TIMEOUT` (`daemon/src/main.rs:29`).

The eager-connect half is unambiguous from the code regardless of caching behaviour;
the activation consequence follows from zbus's default and is worth confirming with
`busctl monitor` before choosing the fix (either build the proxy lazily, or disable
property caching on the proxy).

### L5 — A failed `Client::send` leaves the run page pending forever — **MEDIUM-HIGH**

**Files:** [src/app.rs:63-71](src/app.rs#L63-L71), [src/dbus_client.rs:104-108](src/dbus_client.rs#L104-L108), [src/dbus_client.rs:70-79](src/dbus_client.rs#L70-L79)

```rust
pub fn run(self: &Rc<Self>, recipe: &Recipe, args: HashMap<String, String>, page: RunPage) {
    ...
    self.pending.borrow_mut().insert(request_id, page);   // inserted first
    self.client.run(request_id, &recipe.name, args);      // may fail, returns ()
}
```

`Client::send` returns nothing and only logs on failure:

```rust
if let Err(e) = self.commands.send_blocking(command) {
    error!("the D-Bus thread is gone: {e}");
}
```

The page is already in `pending`, and only `Event::Started` / `Event::Failed` ever
remove it. If the channel is closed there will be neither, so the page sits at
*"Waiting for authorization…"* forever with a live spinner.

This is reachable: if `tokio::runtime::Builder::build()` fails at `dbus_client.rs:70-79`
the thread logs and **returns**, dropping `command_rx`. From that point **every** action
in the app hangs at "Waiting for authorization…", and the only evidence is one
`error!` line that the GUI's default `warn` filter does print but nobody sees.
`send` should return `Result` and `run()` should fail the page.

### L6 — `kernel-build-log` is a follow/tail recipe with no timeout — **MEDIUM-HIGH**

**Files:** [catalog/src/catalog.toml:564](catalog/src/catalog.toml#L564), [daemon/src/interface.rs:22-24](daemon/src/interface.rs#L22-L24), [daemon/src/executor.rs:55-149](daemon/src/executor.rs#L55-L149), [daemon/src/main.rs:97-108](daemon/src/main.rs#L97-L108)

`kernel-build-log` — *"Follow a custom kernel build's output live"* — is a recipe that
by definition does not terminate. There is no per-job timeout anywhere in the executor.

Consequences:

- It permanently occupies one of `MAX_CONCURRENT_JOBS = 3` slots.
- `has_running_jobs()` returns true forever, so the daemon **never idle-exits** and a
  root process stays resident indefinitely — defeating the entire rationale in
  `daemon/src/lifecycle.rs:3-6`.
- Closing the GUI does not stop it. There is no client-disconnect handling, no
  `NameOwnerChanged` watch, and no reattach path (`ListJobs` is unused), so after the
  window closes the job is unreachable and uncancellable short of killing the daemon.
- Three launches wedge the portal: every subsequent `run_recipe` returns
  `LimitsExceeded`, and the user has no UI that shows why.

`risk = "safe"` for this recipe also means it takes **no polkit prompt** to get into
that state.

### L7 — Terminal recipes skip confirmation entirely; the destructive tooltip is false — **MEDIUM**

**Files:** [src/ui/category_page.rs:88-103](src/ui/category_page.rs#L88-L103), [src/ui/mod.rs:45-49](src/ui/mod.rs#L45-L49), [catalog/src/catalog.toml:418](catalog/src/catalog.toml#L418), [catalog/src/catalog.toml:428](catalog/src/catalog.toml#L428)

```rust
fn activate(app: &Rc<App>, window: &Window, recipe_name: &str) {
    ...
    if recipe.terminal {
        open_in_terminal(app, window, recipe);
        return;                       // confirm_then_run is never reached
    }
```

`Recipe::confirm` is therefore unreachable for all five terminal recipes. Two of them —
`create-zfs-pool` and `create-mergerfs-pool` — are `risk = "destructive"` **and declare
no `confirm`**, so they get the destructive pill whose tooltip reads:

> "Destroys data or removes a protection — **asks for confirmation first**"

and a red `destructive-action` button that, on a single click, launches a terminal
already running `just create-zfs-pool` with no VexPortal confirmation of any kind. The
UI states a guarantee it does not provide, for the two operations most able to destroy a
disk. Either `check_consistency` should reject `destructive` without `confirm`, or
`activate()` should confirm before the terminal launch (preferably both).

### L8 — `read_features` reports an enabled feature as disabled when the line has a trailing comment — **MEDIUM**

**File:** [src/system/state.rs:86-110](src/system/state.rs#L86-L110)

```rust
let enabled = value
    .trim_start_matches([' ', '='])
    .trim_end_matches(';')
    .trim()
    == "true";
```

For `vexos.features.gaming.enable = true;` this yields `"true"` — correct. For
`vexos.features.gaming.enable = true; # enabled 2026-01` the value is
`" = true; # enabled 2026-01"`; `trim_end_matches(';')` finds no trailing `;` to strip,
the comparison against `"true"` fails, and the dashboard shows the feature as
**disabled** while it is in fact on.

The failure mode is the dangerous direction: silently wrong state presented as fact, on
the page whose entire job is telling the user what this machine is. Splitting on `#`
before the comparison, or matching `starts_with("true")` after trimming, fixes it.

Related, same function: `features` is a `Vec<(String, bool)>` with no de-duplication, so
a repeated key renders two chips for one feature.

### L9 — Interior optional parameters are passed as `""` instead of being omitted — **MEDIUM**

**File:** [catalog/src/validate.rs:121-143](catalog/src/validate.rs#L121-L143)

```rust
if value.is_empty() {
    if param.required { return Err(...); }
    argv.push(String::new());          // placeholder
    continue;
}
...
while argv.last().is_some_and(String::is_empty) { argv.pop(); }
```

Trailing empties are correctly dropped so `just` applies the recipe's own defaults. But
an **interior** blank is passed through as a literal empty string, which is not the same
thing: `just switch "" amd` binds `role = ""`, it does not fall back to the justfile's
declared default for `role`.

The comment at `:138-140` justifies keeping the placeholder positionally, which is
correct, but does not acknowledge that the value semantics differ. It is currently
benign only because the justfile defaults for `switch` happen to be `""` themselves.
Any recipe with a non-empty justfile default in a non-final optional slot will silently
run with an empty argument instead of that default. The catalog's own `default` field is
the intended mitigation and is not enforced — `check_consistency` does not require an
interior optional to carry one.

### L10 — `JustDump` deserialization is brittle against `just`'s unversioned dump format — **MEDIUM**

**Files:** [catalog/src/drift.rs:14-49](catalog/src/drift.rs#L14-L49), [src/just.rs:68-84](src/just.rs#L68-L84)

`JustParam { name: String, default: Option<String> }` and
`JustAssignment { value: String }` assume `just --dump --dump-format json` emits plain
strings for defaults and assignment values. That format carries no version field, is not
part of `just`'s stability guarantee, and has changed shape across releases (defaults
have been represented as structured expression objects, not bare strings).

If the shape changes, `serde_json::from_str` fails on a **type mismatch** — which
`#[serde(default)]` does not protect against — and the whole thing degrades at once:

- `JustfileFacts.error` is set, so the dashboard shows *"Could not read this host's
  justfile: could not read `just --dump` output: …"*.
- Both dynamic dropdowns (`_feature_names`, `_server_service_names`) render **empty**,
  making every `choice-dynamic` parameter unselectable.
- `available` is empty, so `is_available` returns `true` for everything
  (`src/just.rs:106-108`) and recipes the host does not actually have are offered
  anyway, failing later at the daemon.

Nothing pins the `just` version: the devShell takes `pkgs.just` (`flake.nix:25`) and the
daemon resolves `just` from a fixed `PATH` at runtime (`daemon/src/config.rs:58-61`),
which on a NixOS host is whatever `/run/current-system/sw/bin/just` happens to be. The
two can be different versions.

### L11 — `RefCell` borrows held live across arbitrary widget callbacks — **MEDIUM**

**File:** [src/app.rs:106-116](src/app.rs#L106-L116)

```rust
if let Some(page) = self.running.borrow_mut().remove(&job_id) {
    page.finished(exit_code);
    self.refresh_state();
    window.state_changed();      // rebuilds the entire dashboard widget tree
}
```

The temporary `RefMut` from `borrow_mut()` is the scrutinee of the `if let`, so in
edition 2021 it lives for the **whole block**. `running` is therefore mutably borrowed
while `state_changed()` constructs a fresh dashboard — action rows, click closures,
banner, feature chips — any of which touching `app.running` would panic with
*"already mutably borrowed"*.

It does not panic today because none of that path reads `running`. It is one added
callback away from doing so, in a GTK app where widget construction routinely runs user
code. Binding `let page = self.running.borrow_mut().remove(&job_id);` on its own line
drops the borrow first. `Event::Started` and `Event::Failed` (`:80`, `:89`) have the same
shape.

### L12 — The ANSI parser keeps the body of non-CSI escape sequences it claims to drop — **MEDIUM**

**File:** [src/ui/ansi.rs:108-119](src/ui/ansi.rs#L108-L119)

```rust
// Only CSI sequences appear in practice; anything else is dropped along with
// the escape that introduced it.
if chars.peek() != Some(&'[') {
    continue;
}
```

`continue` drops **only the ESC**. The following characters are consumed by the next
loop iteration and pushed straight into `text`. For an OSC title sequence —
`ESC ] 0 ; building nixos BEL`, which terminal-aware build tooling emits routinely —
the user sees the literal string `]0;building nixos` in the log view, plus a stray BEL
control character inserted into a `GtkTextBuffer`.

The module doc (`ansi.rs:3-6`) says the whole point is that *"the log view would
otherwise show raw `[32m` noise"*. It handles the CSI case and leaves the OSC case
producing exactly the noise it set out to remove.

### L13 — `path_row` destroys the help text and the "no file chosen" state — **MEDIUM**

**File:** [src/ui/arg_dialog.rs:184-215](src/ui/arg_dialog.rs#L184-L215)

```rust
.subtitle(if must_exist { "No file chosen" } else { "Default location" })
.build();
if let Some(help) = &param.help {
    row.set_subtitle(help);              // overwrites the state text
}
...
row.set_subtitle(&display);              // overwrites the help text, permanently
```

One subtitle slot is being used for three different things. If `help` is set the user
never sees "No file chosen", so a required path that has not been picked looks
indistinguishable from one that has. Once a file *is* chosen the help text is gone for
the life of the dialog, with no way to get it back.

The `Field::Path` value is also unrecoverable from the widget — it lives only in the
`Rc<RefCell<Option<String>>>` (`:182`), so nothing can re-render the row correctly.

### L14 — `declined()` and `failed_to_start()` leave `State.finished == false` — **LOW-MEDIUM**

**File:** [src/ui/run_page.rs:151-206](src/ui/run_page.rs#L151-L206)

`finished()` sets `state.finished = true` before calling `stop()`; `declined()` and
`failed_to_start()` call `stop()` only. The `State` struct then claims the page is
unfinished when it demonstrably is.

`request_cancel` guards on `!state.finished` (`:190`), and is currently unreachable in
those paths because `stop()` desensitises the button and `job_id` is `None`. But the
field is now unreliable for any future reader, and the two-flag invariant (`job_id`,
`finished`) is enforced in one of three exit paths.

### L15 — The dashboard is constructed twice on every launch — **LOW-MEDIUM**

**File:** [src/ui/window.rs:45-62](src/ui/window.rs#L45-L62), [src/ui/window.rs:91-93](src/ui/window.rs#L91-L93)

`build_sidebar()` is evaluated inside the split-view builder (`:46`). At its end it calls
`list.select_row(Some(&row))`, which fires `connect_row_selected` → `show_category(DASHBOARD)`
→ builds the dashboard and calls `navigation.replace()`.

Then `Window::new` explicitly calls `this.show_category(DASHBOARD)` again at `:60`,
discarding the tree just built and constructing a second one — including a second
`SystemState` borrow, a second drift banner and a second full set of quick-action rows
with their closures.

### L16 — `Variant::parse` has no component boundary, and its sort is both unnecessary and unstable — **LOW-MEDIUM**

**File:** [src/system/variant.rs:53-73](src/system/variant.rs#L53-L73)

```rust
let mut roles = Role::ALL;
roles.sort_by_key(|r| std::cmp::Reverse(r.as_str().len()));
for role in roles {
    if let Some(gpu) = rest.strip_prefix(role.as_str()) {
        let gpu = gpu.strip_prefix('-').unwrap_or(gpu);
```

Two issues:

- `strip_prefix` does not require the match to end at a `-`, so `vexos-desktopish`
  parses as `role = Desktop`, `gpu = "ish"` and renders as a valid Desktop host. A
  typo in `/etc/nixos/vexos-variant` silently selects the wrong recipe set instead of
  producing `Unrecognised`.
- The comment claims the longest-first sort prevents `headless-server` being shadowed by
  `server`. It cannot: `"headless-server-amd"` does not start with `"server"`, so no
  shadowing is possible in either order. Meanwhile `sort_by_key` is an unstable sort and
  `desktop`/`vanilla` are both 7 characters, so their relative order is unspecified —
  harmless here, but the code is defending against the wrong thing.

### L17 — `VEXPORTAL_VARIANT=` (set but empty) makes a real host claim it is not VexOS — **LOW**

**File:** [src/system/variant.rs:41-43](src/system/variant.rs#L41-L43)

```rust
if let Ok(override_value) = std::env::var(OVERRIDE_ENV) {
    return Self::parse(override_value.trim());
}
```

An empty-but-present variable takes the branch and `parse("")` returns
`Unrecognised("")`. `app::build` maps that to `variant = None`
(`src/app.rs:134-140`) and the dashboard renders the full "Not a VexOS host yet" status
page (`dashboard.rs:155-169`) on a machine that is one. `env::var` returning `Ok("")` is
the normal result of `VEXPORTAL_VARIANT= vexportal` or an exported-then-cleared
variable. Guarding with `.filter(|v| !v.trim().is_empty())` restores the fallthrough.

### L18 — `choices()` does not de-duplicate `extra` against the dynamic list — **LOW**

**File:** [src/just.rs:88-99](src/just.rs#L88-L99)

`extra` (catalog-supplied, e.g. `all`) is concatenated with the justfile's variable with
no dedup. If the justfile's `_server_service_names` or `_feature_names` ever contains a
value the catalog also lists as `extra`, the dropdown shows it twice and the two entries
select different indices for the same value.

### L19 — A relative `--justfile` yields an empty working directory and an opaque spawn failure — **LOW**

**File:** [daemon/src/config.rs:44-48](daemon/src/config.rs#L44-L48)

```rust
pub fn working_directory(&self) -> &Path {
    self.justfile.parent().unwrap_or(Path::new("/"))
}
```

`Path::new("justfile").parent()` is `Some("")`, not `None`, so the `unwrap_or` fallback
never fires for a bare filename. `Command::current_dir("")` then fails at spawn with a
bare ENOENT surfaced as *"VexPortal could not start `just`"* (`executor.rs:90-98`), with
no indication that the working directory is the problem. Only root sets this argv, so
the impact is limited to a misconfigured unit file — but that is precisely when a clear
error matters most. `.filter(|p| !p.as_os_str().is_empty())` before the `unwrap_or`.

### L20 — `check_consistency` misses three classes of catalog defect — **LOW**

**File:** [catalog/src/lib.rs:278-315](catalog/src/lib.rs#L278-L315)

It rejects unknown categories, duplicate recipe names, empty role lists and
required-after-optional. It does not reject:

- **Duplicate category ids.** Two categories with the same `id` produce two sidebar
  rows; the index-based row→id mapping at `window.rs:80-89` then routes the second row to
  the first category's content.
- **Duplicate parameter names within a recipe.** `answers` is a `BTreeMap` keyed by
  name, so both params receive the same value, and `recipe.param(name)`
  (`lib.rs:229-231`) returns only the first — the second is validated against the wrong
  widget.
- **A second `Secret` parameter.** `validate::build` assigns
  `stdin = Some(value)` unconditionally (`validate.rs:116`), so a second secret silently
  overwrites the first. Only the *test* `secrets_are_never_positional`
  (`lib.rs:384-399`) would catch this, and tests do not run in the daemon.

Also unchecked: `destructive` without `confirm` (see **L7**), and `terminal = true`
together with a declared parameter (which is unreachable — see `ANALYSIS_ARCH.md` A6).

### L21 — Cancel is a kill-by-pid that can silently no-op — **LOW**

**File:** [daemon/src/cancel.rs:33-47](daemon/src/cancel.rs#L33-L47)

```rust
let Some(pid) = child.id() else {
    debug!("no pid to signal — the child has already been reaped");
    return;
};
let result = unsafe { libc::kill(-(pid as i32), signal) };
```

`tokio::process::Child::id()` returns `None` once the child has been reaped. The failure
path logs at `debug!`, which the daemon's `info` default filter
(`daemon/src/main.rs:34`) **discards** — so a cancel that does nothing is completely
silent, while `JobHandle::request_cancel` has already returned `true` and the GUI has
already shown "Cancelling…".

Separately, signalling by pid as root carries the classic pid-recycling hazard: if the
group is gone and the pid has been reused, `kill(-pid, SIGKILL)` targets an unrelated
process group with root privileges. The window is narrow here (the cancel branch runs
while `wait()` is still outstanding) but `pidfd_send_signal` removes the class entirely.

### L22 — The stylesheet uses a GTK ≥4.16 CSS feature while the crate targets 4.12 — **LOW**

**Files:** [data/style.css:9-22](data/style.css#L9-L22), [Cargo.toml:43-44](Cargo.toml#L43-L44)

```css
:root {
  --vex-magenta: #c74ded;
  --vex-magenta-dim: #a63bc4;
}
.vex-role-badge {
  background: linear-gradient(135deg, var(--vex-magenta), var(--vex-magenta-dim));
```

`:root` and CSS custom properties / `var()` arrived in GTK's CSS engine in **4.16**.
The crate declares `gtk4 = { features = ["v4_12"] }` and `libadwaita = { features =
["v1_5"] }`, i.e. a stated floor of GTK 4.12, where `var()` does not resolve and the
`background` declaration is dropped — the role badge renders with no gradient and white
text on the default background, i.e. invisible.

nixpkgs `nixos-26.05` ships GTK well past 4.16, so the packaged build is fine; the
inconsistency bites anyone building against the declared minimum. Either raise the
feature gate to `v4_16` or use libadwaita's `@define-color`, which works on both.

---

## 2. Security vulnerabilities and unsafe patterns

### S1 — `AbsPath` and `FlakeRef` accept shell metacharacters, and the test that appears to cover this passes by accident — **HIGH**

**File:** [catalog/src/format.rs:106-136](catalog/src/format.rs#L106-L136), test at [catalog/src/format.rs:152-179](catalog/src/format.rs#L152-L179)

The module doc states the purpose plainly:

> These checks exist anyway as a second line: they keep a malformed value from reaching
> a recipe that *does* interpolate it into a `bash` body (`{{service}}` inside a
> `#!/usr/bin/env bash` recipe is a real interpolation)

Two validators do not provide that property.

```rust
fn is_abs_path(v: &str) -> bool {
    v.starts_with('/') && !v.split('/').any(|c| c == "..")
}
```

Everything that starts with `/` and has no `..` component is accepted, including spaces
and every shell metacharacter. `/tmp/backup.tar.gz; reboot`, `/tmp/$(reboot)`,
`` /tmp/`id` `` and `/var/lib/x && rm -rf /` all pass. The only filter is the generic
control-character check at `:46-48`.

```rust
fn is_flake_ref(v: &str) -> bool {
    let path_like = v == "." || v.starts_with('/') || v.starts_with("./");
    ...
    && v.chars().all(|c| c.is_ascii_alphanumeric()
        || matches!(c, '.' | '/' | '-' | '_' | ':' | '+' | '?' | '=' | '&' | '#'))
```

`&` and `#` are in the allowlist. `/etc/nixos&reboot` is `path_like`, passes the
charset, and is accepted — `&` backgrounds the first command and runs the second.

**The test does not catch this.** `shell_metacharacters_are_rejected_everywhere`
(`:152-179`) asserts every format rejects seven probes, and it passes — but for
`AbsPath` it passes because the probes happen to fail the *leading-slash* check
(`"; rm -rf /"` starts with `;`) or the `..` check (`"../../etc/shadow"`), never because
metacharacters were rejected. Add `/tmp/$(reboot)` or `/tmp/a;reboot` to the probe list
and the suite goes red immediately. A green test asserting a property the code does not
have is worse than no test.

**Impact** depends on whether any reached recipe interpolates these parameters into a
bash body — which the module doc asserts is a real pattern in this justfile. Where it
does, this is command injection executed by a **root** daemon. The parameters concerned
are `restore-plex`'s `tarball` (`Widget::Path`, `catalog.toml:400`) and `switch`/`build`'s
`flake` (`Format::FlakeRef`). Both currently sit behind `auth_admin`-class polkit
actions, which bounds *who* can reach it but not what happens once they do — and
`build` is `risk = "safe"` (**S2**), i.e. no prompt at all.

Fix: restrict `AbsPath` to a conservative charset (alphanumerics, `/ . - _ +`, space)
and drop `&`, `#`, `?` from `FlakeRef` unless a real flake URL in this catalog needs
them.

### S2 — `risk = "safe"` means *no authentication*, and the safe tier includes full root Nix builds — **MEDIUM-HIGH**

**Files:** [data/io.github.vexportal.policy:19-27](data/io.github.vexportal.policy#L19-L27), [catalog/src/lib.rs:85-91](catalog/src/lib.rs#L85-L91), [catalog/src/catalog.toml:93](catalog/src/catalog.toml#L93), [catalog/src/catalog.toml:157](catalog/src/catalog.toml#L157), [data/io.github.vexportal.Daemon.conf:10-17](data/io.github.vexportal.Daemon.conf#L10-L17)

`Risk::Safe` maps to `io.github.vexportal.run-readonly`, whose policy is
`<allow_active>yes</allow_active>` — no prompt, no password, no record beyond the
journal. The D-Bus policy allows `context="default"` to send to the daemon, so any
locally active session can invoke it directly with `busctl` without going through
VexPortal at all.

The `safe` tier is documented as *"Reads state only"*. Eleven recipes carry it, and two
of them are not reads:

| Recipe | Blurb | What it actually does |
|---|---|---|
| `build` | "Dry-run build of a role and GPU variant without switching to it" | A full `nixos-rebuild build` — evaluation, substitution, and compilation as root |
| `upgrade-analysis` | "Test this configuration against a newer nixpkgs without changing anything" | A full evaluation against a different nixpkgs, with network fetches |

"Changes nothing on the system" is not the same as "costs nothing". Both consume
unbounded root CPU, disk (Nix store writes) and network, both take user-supplied
`role`/`variant` parameters, and both are reachable with **zero authentication** by any
locally active user. Looping `build` is a straightforward local denial-of-service that
fills `/nix/store` and saturates the machine, and the audit trail records it as a
routine safe operation.

`kernel-build-log` in the same tier is the unbounded-duration variant (**L6**).

Either move `build` and `upgrade-analysis` to `medium`, or add a fourth tier for
"changes nothing but costs a lot" with `auth_admin_keep`.

### S3 — The concurrency cap is not enforced under concurrency — **MEDIUM**

**File:** [daemon/src/interface.rs:88-116](daemon/src/interface.rs#L88-L116)

```rust
self.reap().await;
if self.jobs.lock().await.len() >= MAX_CONCURRENT_JOBS { ... }   // lock dropped
...
if !auth::check(connection, &caller, action).await ... { ... }   // unbounded wait
...
self.jobs.lock().await.insert(job_id.clone(), handle);           // lock retaken
```

Check and insert are separated by a polkit round trip that, for `auth_admin` actions,
blocks on a human typing a password. N callers can all observe `len() < 3` and all
insert. For the `safe` tier there is no prompt at all, so the window is just the
scheduling gap — and N concurrent `busctl` calls can trivially exceed the cap.

This is the bound that is supposed to contain **S2**, and it does not hold. It also
makes `active_jobs` and the idle logic report values the daemon never intended to allow.

### S4 — `Cancel` performs no ownership check — **MEDIUM**

**Files:** [daemon/src/interface.rs:122-142](daemon/src/interface.rs#L122-L142), [daemon/src/cancel.rs:11-16](daemon/src/cancel.rs#L11-L16), [data/io.github.vexportal.policy:50-58](data/io.github.vexportal.policy#L50-L58)

`io.github.vexportal.cancel` is `allow_active = yes` — no prompt. The method then
cancels whatever job id it is handed:

```rust
let jobs = self.jobs.lock().await;
Ok(jobs.get(job_id).is_some_and(JobHandle::request_cancel))
```

`JobHandle` records `job_id` and `recipe` and **not the caller**, so the daemon cannot
check ownership even in principle. Any locally active user — including a second
concurrent session — can SIGTERM then SIGKILL another user's in-flight
`nixos-rebuild` process group. Job ids are not secret either: `ListJobs`
(`interface.rs:144-154`) enumerates them and is subject to no polkit check at all.

Interrupting a `nixos-rebuild switch` mid-activation is not a clean no-op; the
`run_recipe` path is carefully tiered by risk and the path that *stops* those same
operations is unauthenticated.

### S5 — The terminal escape hatch runs entirely outside the security architecture — **MEDIUM**

**File:** [src/ui/category_page.rs:156-231](src/ui/category_page.rs#L156-L231)

```rust
let command = format!("just {}", recipe.name);
...
format!("{command}; exec bash")     // passed to `bash -lc`
```

Five recipes take this path — `enable`, `create-zfs-pool`, `create-mergerfs-pool`,
`attach-remote-storage`, `secrets-init` — and none of them touch the daemon. No catalog
argv validation, no polkit tier (two of them are `destructive`), and **no journald audit
record**. `data/io.github.vexportal.metainfo.xml:18-20` promises *"every operation is
recorded in the journal"*; for these five, nothing is.

It is also the one place the GUI builds a shell command line. It is safe today only
because `command` interpolates `recipe.name` from the compiled-in catalog — an
incidental property, not an enforced one. The `-l` login shell additionally means the
recipe runs under the user's profile with an environment the daemon path deliberately
clears (`daemon/src/config.rs:51-73`), so the same recipe behaves differently depending
on which button started it.

### S6 — `must_exist` is a TOCTOU existence check with no constraint on the target — **LOW-MEDIUM**

**File:** [daemon/src/interface.rs:81-86](daemon/src/interface.rs#L81-L86)

```rust
for path in &invocation.must_exist {
    if !std::path::Path::new(path).exists() { ... }
}
```

`Path::exists()` follows symlinks, does not check ownership, mode or file type, and is
re-resolved by the root recipe an unbounded time later — after a polkit prompt that may
take minutes. A user-writable directory in the path (`/tmp/plex.tar.gz` is the value in
the test at `validate.rs:317-325`) lets a local attacker swap the target between the
check and the root recipe's use of it.

The check's stated purpose is a better error message, and the recipe would fail on a bad
file anyway — so the severity is bounded. But it reads like a validation gate and is not
one. Opening the file once and passing an fd, or at minimum rejecting symlinks and
world-writable parents, would make it mean what it looks like.

### S7 — The secret is never zeroized and lives in several root-process copies — **LOW-MEDIUM**

**Files:** [daemon/src/executor.rs:102-108](daemon/src/executor.rs#L102-L108), [catalog/src/validate.rs:18-20](catalog/src/validate.rs#L18-L20)

```rust
if let Some(secret) = invocation.stdin.as_deref() {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{secret}\n").as_bytes()).await;
```

The project takes real care to keep secrets out of argv, `ps`, the journal and the audit
line (`validate.rs:18-20`, `audit.rs:4-5`, `lib.rs:187-193`) — and then leaves the
plaintext in at least three heap allocations inside a root process, none of them wiped:
the `String` in `Invocation.stdin`, the `format!` temporary, and the zbus message buffer
the `a{ss}` argument was deserialized from. All are freed without zeroing, so the
plaintext persists in the daemon's freed heap until reuse and lands in any core dump or
`/proc/<pid>/mem` read.

`zeroize::Zeroizing<String>` on the `stdin` field, and writing the bytes without the
`format!` temporary, closes most of it. The zbus buffer is harder and worth documenting
as an accepted limit rather than left implicit.

### S8 — `unreachable!()` inside a root D-Bus service — **LOW**

**File:** [catalog/src/validate.rs:180](catalog/src/validate.rs#L180)

```rust
Widget::Secret => unreachable!("secrets are handled before check_value"),
```

The invariant holds today — `build()` `continue`s on `is_secret()` at `:109-119` before
reaching `check_value`. But this is a panic reachable from remote input in a process
running as root on the system bus: any future reordering turns a malformed request into
a daemon abort. Since `run_recipe` is `async` and zbus catches panics per-method
inconsistently across versions, the blast radius is unclear — which is itself the
argument for returning a `ValidationError` instead.

### S9 — The single `unsafe` block signals by pid as root — **LOW**

**File:** [daemon/src/cancel.rs:40](daemon/src/cancel.rs#L40)

```rust
let result = unsafe { libc::kill(-(pid as i32), signal) };
```

Correctly written (negative pid targets the group established by `process_group(0)`),
and it is the only `unsafe` in the workspace. The residual hazard is pid recycling —
see **L21**. Worth noting only because it runs as root and the failure mode is
"SIGKILL an unrelated process group".

---

## 3. Performance problems

### P1 — One D-Bus signal, one main-loop wakeup and one TextBuffer mutation per output line — **MEDIUM-HIGH**

**Files:** [daemon/src/executor.rs:161-170](daemon/src/executor.rs#L161-L170), [src/dbus_client.rs:147-176](src/dbus_client.rs#L147-L176), [src/ui/run_page.rs:209-257](src/ui/run_page.rs#L209-L257)

Every single line of recipe output travels the full pipeline individually:

1. `pump` emits one `JobOutput` D-Bus signal per line.
2. The GUI's signal task allocates an `Event::Output` (three `String`s) and pushes it to
   an unbounded channel.
3. `glib::spawn_future_local` wakes the GTK main loop and calls `App::handle`.
4. `append_line` runs `ansi::parse` (allocating a `Vec<Segment>` and a `String` per
   segment), inserts into the `GtkTextBuffer`, then calls `trim()` and `scroll_to_end()`.
5. `scroll_to_end` **creates and immediately deletes a `GtkTextMark`** for every line
   (`:252-256`).

There is no batching, coalescing or rate limiting at any stage. `nixos-rebuild` and
`nix build` routinely emit thousands of lines per second.

Two concrete consequences beyond CPU:

- **`dbus-daemon` enforces per-connection outgoing limits** (`max_outgoing_bytes`,
  `max_replies_per_connection` and friends). A daemon that floods signals faster than
  the bus can deliver them can be **disconnected by the bus**, which kills the daemon's
  connection mid-rebuild and leaves the GUI hanging forever with no `JobFinished`.
- The unbounded `async_channel` (`dbus_client.rs:65`) grows without limit if the GTK main
  loop falls behind, so the backlog becomes memory rather than backpressure.

Batching lines into a small time window (say 50 ms) or a line-count threshold before
emitting collapses all five costs at once.

### P2 — `emit_output` does an object-server lookup and an async lock acquisition per line — **MEDIUM**

**File:** [daemon/src/executor.rs:172-190](daemon/src/executor.rs#L172-L190)

```rust
async fn emit_output(connection: &Connection, job_id: &str, stream: u32, line: &str) {
    if let Ok(iface) = connection
        .object_server()
        .interface::<_, Daemon>(OBJECT_PATH)
        .await
```

`interface::<_, Daemon>()` resolves the path through the object server's node map and
acquires an async `RwLock` read guard on the interface — per output line, from two
concurrent pump tasks, at the line rates in **P1**. The `SignalEmitter` could be built
once in `spawn()` and moved into each pump.

### P3 — `RunPage` and its ≤5000-line buffer leak for the process lifetime — **MEDIUM**

**Files:** [src/app.rs:23-28](src/app.rs#L23-L28), [src/ui/run_page.rs:23-35](src/ui/run_page.rs#L23-L35), [src/ui/window.rs:13-21](src/ui/window.rs#L13-L21)

Two strong `Rc` cycles with no `Weak` anywhere in the workspace:

- `App.window: RefCell<Option<Window>>` → `Window.app: Rc<App>`
- `App.pending` / `App.running: HashMap<_, RunPage>` → `RunPage.app: Rc<App>`

Every `RunPage` ever created is retained, each holding a `GtkTextBuffer` with up to
`MAX_LINES = 5000` lines of scrollback (`run_page.rs:16`) plus 32 `GtkTextTag`s
(**P5**). Growth is unbounded in the number of recipes run per session.

On top of the cycle, `running` entries leak outright whenever a job never reports
`Finished` — the daemon crashing, being SIGKILLed, idle-exiting, or the **L1** race —
because `Event::Finished` is the only thing that removes them, and there is no timeout
or `NameOwnerChanged` watch.

### P4 — `just --dump` is spawned synchronously on the GTK main thread before the window appears — **MEDIUM**

**Files:** [src/just.rs:36-47](src/just.rs#L36-L47), [src/app.rs:133](src/app.rs#L133)

`JustfileFacts::read()` runs `std::process::Command::…output()` — a blocking fork/exec
plus a full `just` parse of a ~1400-line justfile — inside `app::build`, on the GTK main
thread, before `window.present()`. Startup is blocked for the duration.

It is also the *only* time it runs (see `ANALYSIS_ARCH.md` A12), so the cost is paid once
but the data goes stale immediately after any rebuild.

### P5 — 32 `GtkTextTag`s constructed per run page — **LOW-MEDIUM**

**File:** [src/ui/run_page.rs:263-308](src/ui/run_page.rs#L263-L308)

`register_tags` builds 7 colours × 4 style variants + 3 colourless variants + `stderr`
= 32 tags, each a `GObject`, for **every** `RunPage`. The tag set is entirely static.
A single shared `GtkTextTagTable` passed to `TextBuffer::new(Some(&table))` builds it
once for the process. Combined with **P3** (pages are never freed), the tags accumulate.

### P6 — Redundant page construction — **LOW-MEDIUM**

**Files:** [src/ui/window.rs:45-62](src/ui/window.rs#L45-L62), [src/ui/window.rs:111-122](src/ui/window.rs#L111-L122)

The dashboard is built twice at startup (**L15**). Beyond that, `show_category` rebuilds
the complete widget tree on every sidebar click with no caching — each rebuild walks the
catalog, allocates a `Vec<&Recipe>`, and constructs an `ActionRow` with badges, a risk
pill and a closure per recipe. At ~10 recipes per category this is cheap; it is listed
because there is no mechanism to stop it growing.

### P7 — `visible_categories()` builds and discards a `Vec` per category — **LOW**

**File:** [src/app.rs:41-56](src/app.rs#L41-L56)

```rust
pub fn visible_categories(&self) -> Vec<&Category> {
    self.catalog.categories.iter()
        .filter(|c| !self.visible_in(&c.id).is_empty())
```

`visible_in` walks all 42 recipes, filters by category, role and justfile availability,
and **collects into a `Vec`** — which is then thrown away after a single `is_empty()`
call, once per category. `Iterator::any` avoids the allocation entirely.

### P8 — The daemon polls a deadline it already knows — **LOW**

**Files:** [daemon/src/main.rs:29-30](daemon/src/main.rs#L29-L30), [daemon/src/main.rs:85-90](daemon/src/main.rs#L85-L90)

A 15-second `tokio::time::sleep` in the select loop wakes a root process 12 times per
idle minute; each wake takes a `Mutex`, and on the `is_idle()` branch also does an
object-server lookup and a second lock in `has_running_jobs`. `IdleTracker` stores
`last_activity`; `sleep_until(last_activity + timeout)` reset on `mark_active` expresses
the same policy with one timer and no polling.

### P9 — `spawn_terminal` builds all four candidate argument vectors up front — **LOW**

**File:** [src/ui/category_page.rs:178-220](src/ui/category_page.rs#L178-L220)

The `[(&str, Vec<String>); 4]` array allocates four `Vec<String>`s and ~18 `String`s
(three of them `format!`-ed) before the loop tries the first candidate, which succeeds
on essentially every GNOME desktop. Trivial in absolute terms, listed because it is on a
user-interactive path and a lazy `match` costs nothing.

### P10 — A fresh `DBusProxy` per job start for the audit uid — **LOW**

**File:** [daemon/src/interface.rs:194-200](daemon/src/interface.rs#L194-L200)

`uid_of` constructs `fdo::DBusProxy::new(connection)` on every `run_recipe`, purely to
call `GetConnectionUnixUser`. The proxy is stateless and could be built once in
`Daemon::new`.

### P11 — Linear scans on the hot lookup paths — **LOW**

**File:** [catalog/src/lib.rs:317-323](catalog/src/lib.rs#L317-L323)

`Catalog::recipe` and `Catalog::category` are `iter().find()` over `Vec`s, called on
every action-row click, every `confirm_then_run`, every `arg_dialog` submit and once per
recipe per page build. At 42 recipes and 8 categories this is genuinely irrelevant —
noted only so it is a conscious choice rather than an oversight if the catalog grows.

---

## 4. Dead code, redundant code, code that does nothing

### Dead feature mechanisms

| # | Priority | Item |
|---|---|---|
| D1 | Medium | **`Recipe::refresh`** — [catalog/src/lib.rs:209-211](catalog/src/lib.rs#L209-L211), populated on 7 recipes (`catalog.toml:90,131,181,191,214,232,605`). No code reads it. `app.rs:111` re-reads *all* state after *every* job instead. |
| D2 | Medium | **`ListJobs`** — fully implemented in [daemon/src/interface.rs:144-154](daemon/src/interface.rs#L144-L154) *"so a GUI that was restarted can reattach"*, declared in the GUI proxy at [src/dbus_client.rs:240](src/dbus_client.rs#L240). No caller; no reattach path exists. |
| D3 | Medium | **`Version` / `Justfile` properties** — [daemon/src/interface.rs:173-183](daemon/src/interface.rs#L173-L183), declared at [src/dbus_client.rs:248-252](src/dbus_client.rs#L248-L252). Implemented on both sides, called by neither. `Justfile` exists specifically so the GUI need not hardcode the path — and `src/just.rs:13` hardcodes it. |
| D4 | Medium | **`Excluded::reason`** — [catalog/src/lib.rs:244-249](catalog/src/lib.rs#L244-L249), three entries at `catalog.toml:658-670`. `drift::compare` reads only `.name` ([drift.rs:148](catalog/src/drift.rs#L148)). |
| D5 | Medium | **`VEXOS_ASSUME_YES` / `VEXPORTAL` env vars** — [daemon/src/config.rs:62-67](daemon/src/config.rs#L62-L67). No reader exists upstream; the comment and `needs_upstream` (6 recipes) both acknowledge it. |
| D6 | Low-Med | **`Recipe::confirm` for terminal recipes** — unreachable, since `activate()` returns before `confirm_then_run` (**L7**). |
| D7 | Low-Med | **`Catalog::categories_for_role`** — [catalog/src/lib.rs:339-348](catalog/src/lib.rs#L339-L348), zero callers. **`Catalog::for_role`** — [:326-328](catalog/src/lib.rs#L326-L328), called only from a test at `:365`. |

### Dead CSS

| # | Priority | Item |
|---|---|---|
| D8 | Low | **`.vex-hero`** — [data/style.css:29-33](data/style.css#L29-L33). No Rust code calls `add_css_class("vex-hero")`. |
| D9 | Low | **`.vex-risk.medium`** — [data/style.css:51-54](data/style.css#L51-L54). `risk_pill` returns `None` for `Risk::Medium` ([src/ui/mod.rs:44](src/ui/mod.rs#L44)), so the class is never applied by any path. |

### Unreachable branches and no-op statements

| # | Priority | Item |
|---|---|---|
| D10 | Low | **`is_declined`'s `"dismissed"` arm** — [src/dbus_client.rs:51](src/dbus_client.rs#L51). No message the daemon can construct contains that word; every rejection path goes through `fdo::Error::AccessDenied("Not authorized …")` or a `ValidationError` display string. |
| D11 | Low | **`let _ = app;`** — [src/ui/category_page.rs:172](src/ui/category_page.rs#L172). The `app` parameter of `open_in_terminal` is unused and silenced rather than removed, along with its call-site argument at `:94`. |
| D12 | Low | **`AuthorizationResult::is_challenge` / `details`** — [daemon/src/auth.rs:64-68](daemon/src/auth.rs#L64-L68), both `#[allow(dead_code)]`. `is_challenge` is exactly the signal that would replace the string matching in **E8**. |
| D13 | Low | **`self: &Rc<Self>` receivers that never use the `Rc`** — [src/app.rs:63](src/app.rs#L63) (`run`) and [:77](src/app.rs#L77) (`handle`). Neither clones the `Rc`; `&self` suffices. |
| D14 | Low | **`visible_page().is_some()` guard** — [src/ui/window.rs:146](src/ui/window.rs#L146). Always true after `Window::new` has run `show_category`, so the condition never affects control flow. |
| D15 | Low | **Unreachable defensive paths in `category_page`** — `map_or("VexOS", …)` at [:13](src/ui/category_page.rs#L13) for a category id that always exists (it came from `visible_categories`), and the `catalog.recipe(recipe_name)` re-lookups at [:89](src/ui/category_page.rs#L89) and [:139](src/ui/category_page.rs#L139) for a name that was read out of the catalog moments earlier. Each silently `return`s, so a real lookup failure would be invisible. |

### Duplicated logic

| # | Priority | Item |
|---|---|---|
| D16 | Low-Med | **`color_tag_name` duplicates `Color::tag`** — [src/ui/run_page.rs:310-320](src/ui/run_page.rs#L310-L320) is character-for-character [src/ui/ansi.rs:64-74](src/ui/ansi.rs#L64-L74). The tag names produced by `register_tags` must match those requested by `Style::tag_name()` or `insert_with_tags_by_name` fails at runtime with a GTK warning and unstyled text; two hand-maintained copies is the fragile way to guarantee that. Making `Color::tag` public deletes one. |

### Unused dependencies

| # | Priority | Item |
|---|---|---|
| D17 | Medium | **`anyhow`** — declared at [Cargo.toml:19](Cargo.toml#L19) (workspace) and [Cargo.toml:39](Cargo.toml#L39) (GUI); zero occurrences in any `.rs` file. **`serde`** — [Cargo.toml:36](Cargo.toml#L36), no `serde::` path or derive anywhere in `src/`. **`serde_json`** — [daemon/Cargo.toml:18](daemon/Cargo.toml#L18), no occurrence in `daemon/src/`. Each of the three crates declares at least one dependency it does not use; `cargo-machete` in `scripts/preflight.sh` would keep it fixed. |

---

## 5. Error handling: missing, inconsistent, or silently swallowed

### E1 — Every reader in `system/state.rs` swallows its failure with no log line — **HIGH**

**File:** [src/system/state.rs:56-124](src/system/state.rs#L56-L124)

All five readers convert any failure to `None` / empty:

```rust
fn read_features() -> Vec<(String, bool)> {
    let Ok(contents) = std::fs::read_to_string(FEATURES_FILE) else {
        return Vec::new();
    };
```
```rust
fn read_generation() -> Option<u32> {
    let target = std::fs::read_link(SYSTEM_PROFILE).ok()?;
    let name = target.file_name()?.to_str()?;
    name.strip_prefix("system-")?.strip_suffix("-link")?.parse().ok()
}
```

There is not a single `log::` call in the file. A permission error, a malformed
`features.nix`, a renamed profile symlink and a corrupt `flake.lock` all render
identically: the row simply **vanishes from the dashboard**, and `features_group`
returns `None` so the whole Features section disappears (`dashboard.rs:107-110`).

The user is shown a dashboard that looks complete and is not, on the page whose only
purpose is to state what this machine is. `read_generation` chains five `?`s so even the
author cannot tell which step failed.

This is also internally inconsistent: `src/system/variant.rs:29-37` defines a
three-variant `thiserror` enum for the same class of operation — reading one file in
`/etc/nixos` for the same dashboard — and surfaces the reason to the user.

### E2 — Unroutable output and completion events are dropped with no `else` and no log — **HIGH**

**File:** [src/app.rs:97-116](src/app.rs#L97-L116)

```rust
Event::Output { job_id, stream, line } => {
    if let Some(page) = self.running.borrow().get(&job_id) {
        page.append(stream, &line);
    }
}
```

No `else`, no `log::warn!`. This is the mechanism behind **L1**: output for a job that
has not yet been inserted into `running` is discarded, and a `Finished` that loses the
race is discarded, leaving the page live forever. A single `warn!("dropping output for
unknown job {job_id}")` would have made **L1** obvious in testing instead of presenting
as "the first few lines are sometimes missing".

### E3 — Signal-subscription failures are discarded — **MEDIUM-HIGH**

**File:** [src/dbus_client.rs:142-176](src/dbus_client.rs#L142-L176)

```rust
let output = proxy.receive_job_output().await;
let finished = proxy.receive_job_finished().await;

if let Ok(mut output) = output { ... }
if let Ok(mut finished) = finished { ... }
```

Both `Err` cases are dropped on the floor — no log, no event, no fallback. If the
`JobFinished` subscription fails, the GUI still connects, still accepts commands, still
starts jobs, and **every run page hangs at "Running…" forever** because the completion
signal has nowhere to arrive. There is no diagnostic anywhere to explain it.

The `let Ok(args) = signal.args() else { continue }` inside both pumps (`:150`, `:167`)
has the same shape: a malformed signal is skipped silently, so a wire-format mismatch
between GUI and daemon presents as "output randomly missing".

### E4 — Detached tasks whose termination is unobservable — **MEDIUM-HIGH**

**File:** [src/dbus_client.rs:147](src/dbus_client.rs#L147), [src/dbus_client.rs:164](src/dbus_client.rs#L164)

Both signal pumps are `tokio::spawn`ed and their `JoinHandle`s dropped immediately. If a
stream ends — daemon exit, bus disconnect, `NameOwnerChanged` — the task returns and
nothing notices. The GUI keeps a `Client` that looks healthy, keeps accepting commands,
and silently stops receiving anything.

The daemon deliberately exits after 180 s idle (`daemon/src/main.rs:29`), so a
mid-session owner change is an **expected** event in normal operation, not an edge case.
Nothing re-establishes the subscriptions afterwards.

### E5 — A failed secret write runs the recipe anyway — **MEDIUM**

**File:** [daemon/src/executor.rs:102-108](daemon/src/executor.rs#L102-L108)

```rust
let _ = stdin.write_all(format!("{secret}\n").as_bytes()).await;
let _ = stdin.shutdown().await;
```

If the write fails — EPIPE because the recipe exited early, a short write, ENOSPC on the
pipe — the job proceeds regardless. The recipe's `read -rsp` then gets EOF, takes its
default, and (per the project's own table in `docs/vexos-nix-prompt.md:20-30`) either
silently does the wrong thing or exits 1 under `set -e`. The user sees an unexplained
failure for `setup-rdp` and no indication the password never arrived.

### E6 — `let _ = events.send(...)` in four places — **MEDIUM**

**File:** [src/dbus_client.rs:151-157](src/dbus_client.rs#L151-L157), [:168-173](src/dbus_client.rs#L168-L173), [:192](src/dbus_client.rs#L192), [:214-219](src/dbus_client.rs#L214-L219)

A closed event channel — the receiving side gone — is indistinguishable from a
successful send. The pumps keep looping and keep sending into the void rather than
shutting down. Since the receiver lives in `App` which lives forever (**P3**), this
cannot currently fire, but it means there is no shutdown signal in either direction.

### E7 — Failed signal emission from the daemon is silently dropped — **MEDIUM**

**File:** [daemon/src/executor.rs:172-190](daemon/src/executor.rs#L172-L190)

```rust
let _ = Daemon::job_output(iface.signal_emitter(), job_id, stream, line).await;
...
let _ = Daemon::job_finished(iface.signal_emitter(), job_id, exit_code).await;
```

Plus the outer `if let Ok(iface)` with no `else`. A failed `JobFinished` emission is
precisely the "GUI hangs forever" case (**E3**), and it is discarded without so much as
a `warn!`. Given **P1**'s risk of hitting the bus's outgoing-message limits, emission
failure under load is a realistic scenario, not a theoretical one — and the daemon would
record nothing at all about it in the journal it is supposed to be the audit trail for.

### E8 — Structured D-Bus errors flattened to a string, then re-parsed by substring — **MEDIUM**

**File:** [src/dbus_client.rs:222-230](src/dbus_client.rs#L222-L230), [:48-52](src/dbus_client.rs#L48-L52)

```rust
zbus::Error::MethodError(_, Some(message), _) => message.clone(),   // name discarded
...
message.contains("Not authorized") || message.contains("dismissed")
```

The daemon classified the failure precisely one process away — `AccessDenied`,
`InvalidArgs`, `LimitsExceeded` — and `friendly()` throws the discriminator away so
`is_declined()` can guess it back from prose. Rewording the string literal at
`interface.rs:102` silently breaks the declined-vs-failed UX with no type error and no
test. It also conflates "you dismissed the prompt" with "you are not an administrator",
which need different advice.

### E9 — Stringly-typed `Result<_, String>` collapses distinct daemon failures — **MEDIUM**

**File:** [daemon/src/auth.rs:16-36](daemon/src/auth.rs#L16-L36), [daemon/src/interface.rs:96-98](daemon/src/interface.rs#L96-L98)

`auth::check` returns `Result<bool, String>` with two distinct failures —
`"could not reach polkit: …"` and `"polkit CheckAuthorization failed: …"` — that the
caller can only funnel into a single opaque `fdo::Error::Failed`. "polkit is not running
on this machine" and "polkit rejected the subject" reach the user as the same generic
error, in a crate where every other error type is a `thiserror` enum.

`Config::from_args` (`config.rs:27`) has the same shape.

### E10 — Audit coverage has holes on the rejection paths — **MEDIUM**

**File:** [daemon/src/interface.rs:89-93](daemon/src/interface.rs#L89-L93), [:133-138](daemon/src/interface.rs#L133-L138)

`audit::rejected` is called for validation failures (`:72`), missing files (`:83`) and
polkit denials (`:100`) — but **not** for the `LimitsExceeded` rejection at `:89-93`, and
**not** for the cancel authorization failure at `:133-138`. `audit.rs:1-5` presents
journald as the complete record of *"what the portal was asked to do"*, and
`audit::rejected`'s own doc says a rejection *"means either a bug or something else on
the bus"* — exactly the case a concurrency-limit flood (**S3**) would produce, and
exactly the case that leaves no trace.

### E11 — Both drift tests report green having verified nothing — **MEDIUM**

**File:** [catalog/tests/drift_against_justfile.rs:17-22](catalog/tests/drift_against_justfile.rs#L17-L22), [:88-93](catalog/tests/drift_against_justfile.rs#L88-L93)

```rust
if !Path::new(JUSTFILE).exists() {
    eprintln!("skipping: {JUSTFILE} not present (not a built VexOS host)");
    return;                         // test PASSES
}
```

The skip is deliberate and documented, but it is implemented as a **pass**, not a skip —
`cargo test` prints `ok` for both. Drift detection is the project's primary safety net
against the catalog and the justfile diverging, and it is a silent no-op in exactly the
environment the README advertises as running it:

> `nix build .#default   # package, runs tests in the sandbox`

The sandbox has no `/etc/nixos/justfile`, so `nix build` has never once executed the
drift comparison. `#[ignore]` plus an explicit `--ignored` run on a host, or a
`VEXPORTAL_REQUIRE_JUSTFILE=1` env check that turns the skip into a failure in CI, makes
the coverage honest.

### E12 — The second drift test omits the status check its sibling makes — **LOW-MEDIUM**

**File:** [catalog/tests/drift_against_justfile.rs:94-109](catalog/tests/drift_against_justfile.rs#L94-L109)

```rust
let Ok(output) = Command::new("just").args([...]).output() else {
    eprintln!("skipping: could not run `just`");
    return;
};
let dump = JustDump::parse(&String::from_utf8_lossy(&output.stdout)).unwrap();
```

`Ok(output)` only means the process was spawned, not that it succeeded — unlike the
sibling test, which asserts `output.status.success()` at `:43-47`. A `just` that exits
non-zero (syntax error in the justfile, missing import) yields empty stdout, `parse`
succeeds on `""`... or panics on `.unwrap()`, and either way the reported failure is
*"`_feature_names` is empty or missing from the justfile"* instead of the actual `just`
error, which is discarded with `stderr` unread.

### E13 — File-chooser errors are indistinguishable from user cancellation — **LOW-MEDIUM**

**File:** [src/ui/arg_dialog.rs:210-216](src/ui/arg_dialog.rs#L210-L216)

```rust
let finish = move |result: Result<gtk::gio::File, glib::Error>| {
    let Ok(file) = result else { return };
    let Some(path) = file.path() else { return };
```

Both early returns are silent. The first conflates "user pressed Cancel" (expected) with
a real portal failure; the second silently drops a non-local file (an `smb://` or
`sftp://` URI has no local path) so the row keeps saying "No file chosen" and the user
has no idea why their selection did not take.

### E14 — `spawn_terminal` reports success for a terminal that never appeared — **LOW**

**File:** [src/ui/category_page.rs:222-230](src/ui/category_page.rs#L222-L230)

```rust
if std::process::Command::new(program).args(&args).spawn().is_ok() {
    return true;
}
```

`spawn().is_ok()` means the `execve` succeeded, nothing more. A terminal binary that
exists but exits immediately — no `$DISPLAY`/`$WAYLAND_DISPLAY`, missing runtime dir,
broken profile — still returns `true`, so the user gets the toast *"Opened a terminal
running `just create-zfs-pool`"* and no terminal. The fallback dialog with the
copy-pasteable command (`:162-170`) is never reached in the case it exists for.

The `Child` is also dropped without `wait()`, leaving a zombie for each launch.

### E15 — Raw D-Bus error names can reach the user — **LOW**

**File:** [src/dbus_client.rs:227](src/dbus_client.rs#L227)

```rust
zbus::Error::MethodError(name, None, _) => name.to_string(),
```

A `MethodError` with no message body surfaces as
`org.freedesktop.DBus.Error.ServiceUnknown` in the run page's banner — which the
function's own doc comment (`:222-223`) identifies as *"not what anyone wants to read in
a dialog"* before doing it anyway for that arm.

### E16 — GUI logging is effectively silent about local failures — **LOW**

**File:** [src/main.rs:15](src/main.rs#L15)

Default filter is `warn`, and the GUI's `log` calls are almost entirely `error!` with
two `warn!` — so the level is not the constraint, the missing call sites are. Combined
with **E1** and **E2**, the practical result is a GUI that reports nothing about a failed
state read, a dropped output line, a failed signal subscription, or a dead D-Bus thread.
`RUST_LOG=debug` yields nothing extra because there is nothing at `debug`.

### E17 — `Client::send` failure is logged and the caller proceeds as if queued — **LOW**

**File:** [src/dbus_client.rs:104-108](src/dbus_client.rs#L104-L108)

Covered in **L5**; listed here as the error-handling defect that causes it. `send`
returns `()`, so `App::run` has no way to know the command was never queued and no way
to fail the page it already registered in `pending`.

---

## Suggested order

Ordered by risk × effort, not by section:

1. **L1 + E2** — buffer or key events so output cannot be dropped and `Finished` cannot
   lose the race. One change, and it fixes the most user-visible defect in the app.
2. **L2** — swap `lines()` for `split(b'\n')` + `from_utf8_lossy`. Three lines, removes
   silent output truncation.
3. **L3** — track the pushed run page so `state_changed` cannot replace the stack under
   it.
4. **S1** — tighten `is_abs_path` and `is_flake_ref`, and add `/tmp/$(reboot)` to the
   metacharacter probe list so the test stops passing by accident.
5. **S2 + S3 + L6** — reclassify `build` / `upgrade-analysis` off the unauthenticated
   tier, hold the jobs lock across the check-and-insert, and add a job timeout. Together
   they close the local-DoS path.
6. **E1 + E3 + E7** — add logging to the three places that currently swallow the
   failures that make the app appear to hang.
7. **L4** — confirm with `busctl monitor` whether launching the GUI activates the
   daemon; if so, build the proxy lazily.
8. **P1 + P2** — batch output lines before they hit the bus. This is the one performance
   change with a correctness payoff (bus disconnection under load).
9. **L7 + L20** — make `check_consistency` reject `destructive` without `confirm` and
   `terminal` with params, and confirm before terminal launches.
10. **D1–D17** — delete the dead mechanisms or finish them; run `cargo-machete` in
    `scripts/preflight.sh`.
