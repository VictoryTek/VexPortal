# VexPortal — Master Plan

Consolidated from `ANALYSIS_ARCH.md`, `ANALYSIS_BUGS.md`, `ANALYSIS_FEATURES.md`.
Duplicates across the three documents are merged into one line, tagged with every
source ID that named it. Ordering inside each tier is attack order (smallest/safest
fixes first), not source order. Full explanation, failure scenario, and exact
file:line evidence for every item lives in the three source documents — this file is
the checklist, not the write-up.

Legend: `[ARCH Xn]` / `[BUGS Xn]` / `[FEAT Fn]` = originating finding id(s).

---

## HIGH priority (23)

- [ ] **M1** — Job completion (`Event::Finished`) calls `navigation.replace()` and destroys
      the run page the user is reading, when launched from the dashboard.
      `src/app.rs:106-116`, `src/ui/window.rs:125-127,142-150`.
      [ARCH A2] [BUGS L3]
- [ ] **M2** — `Event::Output`/`Event::Finished` can arrive before `Event::Started`;
      unroutable events are dropped with no `else` and no log, so a page can hang at
      "Running…" forever. `src/app.rs:97-116`, `daemon/src/interface.rs:106-118`.
      [BUGS L1] [BUGS E2]
- [ ] **M3** — One invalid UTF-8 byte in job output silently truncates the rest of that
      stream (`lines()` treats the decode error as EOF). `daemon/src/executor.rs:161-170`.
      [BUGS L2]
- [ ] **M4** — Structured D-Bus errors (`AccessDenied`/`InvalidArgs`/`LimitsExceeded`) are
      flattened to a string in `friendly()`, then `is_declined()` re-guesses the
      classification by substring match; the `"dismissed"` arm matches nothing the
      daemon can emit. `src/dbus_client.rs:48-52,222-230`.
      [ARCH A1] [BUGS E8] [BUGS D10] (pairs naturally with M23 — polkit `is_challenge`)
- [ ] **M5** — GUI's single D-Bus command loop awaits `run_recipe` inline, so it blocks
      behind a human-paced polkit prompt — a second `Run` and every `Cancel` stalls
      behind it. `src/dbus_client.rs:178-200`.
      [ARCH A3]
- [ ] **M6** — A failed `Client::send` (channel closed) is logged and discarded; the run
      page stays "Waiting for authorization…" forever with no way to know the command
      was never queued. `src/app.rs:63-71`, `src/dbus_client.rs:104-108`.
      [BUGS L5] [BUGS E17]
- [ ] **M7** — D-Bus client connects eagerly on `Client::start()` despite the doc comment
      promising connection is "deferred to the first command". Property-caching proxy
      construction likely D-Bus-activates the root daemon just by opening the window.
      `src/dbus_client.rs:60-88,111-143`. Verify with `busctl monitor` before fixing.
      [BUGS L4]
- [ ] **M8** — Strong `Rc` cycles (`App`↔`Window`, `App`↔`RunPage`) leak every run page
      (with its ≤5000-line buffer and 32 text tags) for the process lifetime; `running`
      map entries leak outright if a job never reports `Finished`.
      `src/app.rs:23-28,154-155`, `src/ui/window.rs:13-21`, `src/ui/run_page.rs:23-35`.
      [ARCH A4] [BUGS P3]
- [ ] **M9** — Four inconsistent error-handling styles across the codebase; concrete
      worst case: every reader in `system/state.rs` swallows failures to `None`/empty
      with zero logging, while `variant.rs` next door defines a full error enum for the
      same class of read. `src/system/state.rs:56-124` vs `src/system/variant.rs:29-37`.
      [ARCH C1] [BUGS E1]
- [ ] **M10** — `AbsPath` and `FlakeRef` validators accept shell metacharacters
      (`/tmp/$(reboot)`, `/etc/nixos&reboot`); the existing injection test passes only
      by accident (probes fail on unrelated checks, not on metacharacter rejection).
      `catalog/src/format.rs:106-136,152-179`.
      [BUGS S1]
- [ ] **M11** — `risk = "safe"` (no polkit prompt at all) includes `build` and
      `upgrade-analysis` — full root Nix evaluations, not reads. Unauthenticated local
      DoS via looping either. `catalog/src/catalog.toml:93,157`,
      `data/io.github.vexportal.policy:19-27`.
      [BUGS S2]
- [ ] **M12** — `kernel-build-log` (a follow/tail recipe) has no execution timeout: wedges
      one of 3 concurrency slots and permanently blocks daemon idle-exit.
      `catalog/src/catalog.toml:564`, `daemon/src/executor.rs:55-149`.
      [BUGS L6]
- [ ] **M13** — Output pipeline is unbatched: one D-Bus signal + main-loop wakeup +
      TextBuffer mutation per output line. Risks hitting `dbus-daemon`'s per-connection
      outgoing-message limits under a fast rebuild, which would disconnect the daemon
      mid-job. `daemon/src/executor.rs:161-190`, `src/dbus_client.rs:147-176`,
      `src/ui/run_page.rs:209-257`.
      [BUGS P1] (pairs with M20 — per-line object-server lookup)
- [ ] **M14** — Signal-subscription failures (`receive_job_output`/`receive_job_finished`)
      are discarded with no log; if `JobFinished` fails to subscribe, every run page
      hangs at "Running…" forever with zero diagnostic. `src/dbus_client.rs:142-176`.
      [BUGS E3]
- [ ] **M15** — Detached `tokio::spawn` signal pumps with dropped `JoinHandle`s (silent
      death on stream end/bus disconnect); `Client::send` uses `send_blocking` on the
      GTK main thread. `src/dbus_client.rs:104-108,147,164`.
      [ARCH C3] [BUGS E4]
- [ ] **M16** — `ListJobs` is fully implemented daemon-side "so a GUI that was restarted
      can reattach" and never called. Implement reattachment: call it on connect,
      re-attach a `RunPage` per live job via the existing `RunPage::attach`.
      `daemon/src/interface.rs:144-154`, `src/dbus_client.rs:240`,
      `src/ui/run_page.rs:144-148`.
      [ARCH D1] [FEAT F3]
- [ ] **M17** — Feature toggles render as read-only chips despite the app already having
      current state (`SystemState.features`), the valid-name list
      (`JustfileFacts.features`), and the exact recipes to flip one
      (`enable-feature`/`disable-feature`). Replace with `adw::SwitchRow` per feature
      wired to the existing `confirm_then_run` path. Fix the trailing-comment parsing
      bug in `read_features` in the same commit (a switch would render in the wrong
      position otherwise). `src/ui/dashboard.rs:107-136`, `src/system/state.rs:86-110`.
      [FEAT F1] [BUGS L8]
- [ ] **M18** — No search/command palette over the 42 cataloged recipes, despite every
      recipe already carrying `title`/`blurb`/`icon`/`category` and a reusable
      `action_row` renderer. `catalog/src/lib.rs:196-222`,
      `src/ui/category_page.rs:37-85`.
      [FEAT F2]
- [ ] **M19** — Both drift tests report `ok` by skipping (returning early) whenever
      `/etc/nixos/justfile` is absent — which is always true in the Nix build sandbox,
      so `nix build .#default`'s "runs tests in the sandbox" claim has never actually
      executed the drift comparison. Commit a captured `just --dump` fixture and run
      the comparison against it unconditionally; keep the live-host tests but make
      their skip an honest `#[ignore]`.
      `catalog/tests/drift_against_justfile.rs:17-22,88-93`.
      [BUGS E11] [FEAT F4]
- [ ] **M20** — No desktop notifications for long-running jobs (`rebuild`,
      `kernel-build-now` run 10–40 minutes); the app is already a `GApplication` with a
      matching desktop-file id, which is the entire requirement for
      `gio::Notification`. `src/main.rs:22`, `src/app.rs:106-116`.
      [FEAT F5]
- [ ] **M21** — Server-services category (`services`, `available-services`,
      `service-info`, `status`, `restart`, `disable`, `enable`) is 6 verbs on one noun,
      all keyed by the same `choice-dynamic` slug already parsed into
      `JustfileFacts.server_services`; reorganize into one row per service with a menu
      of actions instead of 6 separate cards each requiring its own dropdown pick.
      `catalog/src/catalog.toml` (`services` category), `src/just.rs:80`.
      [FEAT F6]
- [ ] **M22** — Add a "Test build first" option to `switch`'s confirmation dialog: `build`
      is the identical-signature, safe dry-run twin of `switch` and the confirm-dialog
      plumbing to reuse it already exists.
      `catalog/src/catalog.toml:93,123`, `src/ui/category_page.rs:106-145`.
      [FEAT F7]
- [ ] **M23** — No primary menu, About dialog, or keyboard shortcuts anywhere in the
      GUI, despite a complete, correct `metainfo.xml` already installed that
      `adw::AboutDialog::from_appdata()` can consume directly.
      `data/io.github.vexportal.metainfo.xml`.
      [FEAT F8]

---

## MEDIUM priority (33)

- [ ] **M24** — Terminal escape hatch (`enable`, `create-zfs-pool`,
      `create-mergerfs-pool`, `attach-remote-storage`, `secrets-init`) bypasses the
      daemon entirely: no catalog validation, no polkit tier, no journald audit record,
      and builds a shell string in the GUI. `src/ui/category_page.rs:156-231`.
      [ARCH A5] [BUGS S5]
- [ ] **M25** — `activate()` checks `recipe.terminal` before params/confirm, so
      `enable`'s required `service` param is silently dropped (`just enable` runs with
      no argument) and the two `destructive` terminal recipes (`create-zfs-pool`,
      `create-mergerfs-pool`) skip confirmation entirely despite their pill tooltip
      promising one. `src/ui/category_page.rs:88-103`, `catalog/src/catalog.toml:339-353`.
      [ARCH A6] [BUGS L7] [BUGS D6]
- [ ] **M26** — `Cancel` (`allow_active=yes`, no prompt) has no ownership model — any
      locally active user can cancel any job, including another session's in-flight
      `nixos-rebuild`. `daemon/src/interface.rs:122-142`, `daemon/src/cancel.rs:11-16`.
      [ARCH A7] [BUGS S4]
- [ ] **M27** — Concurrency-limit check and job-map insert are separated by an
      unbounded polkit `await`, so `MAX_CONCURRENT_JOBS` does not hold under
      concurrent requests. `daemon/src/interface.rs:88-116`.
      [ARCH A8] [BUGS S3]
- [ ] **M28** — Daemon's `Justfile` D-Bus property exists precisely so the GUI need not
      guess the path, and the GUI hardcodes `/etc/nixos/justfile` anyway (4 places
      total, including the daemon's own default and the drift test).
      `daemon/src/interface.rs:178-183`, `src/just.rs:13`, `daemon/src/config.rs:8`,
      `catalog/tests/drift_against_justfile.rs:15`.
      [ARCH A9] [ARCH B3] [FEAT F18]
- [ ] **M29** — `JustfileFacts` (dropdown choices, availability filter, drift banner) is
      read once at startup and never refreshed — the one operation guaranteed to
      change the justfile (a successful `rebuild`) can't update VexPortal's view of it.
      `src/app.rs:133`, `src/ui/window.rs:142-150`.
      [ARCH A12] (cross-ref M42 for the dashboard-only refresh)
- [ ] **M30** — D-Bus wire-protocol constants (bus name, object path, interface name,
      stream tags) are duplicated by hand across the GUI and daemon crates instead of
      living once in the shared `vexportal-catalog` crate.
      `daemon/src/executor.rs:20-24`, `src/ui/run_page.rs:260-261`,
      `src/dbus_client.rs:232-236`.
      [ARCH B1]
- [ ] **M31** — The `just --dump --dump-format json` invocation is copy-pasted 3 times;
      the third copy is missing the `status.success()` check its siblings have, so a
      failing `just` there reports a misleading "variable is empty" failure instead of
      the real error. `src/just.rs:37-47`, `catalog/tests/drift_against_justfile.rs:24-33,94-109`.
      [ARCH B4] [BUGS E12]
- [ ] **M32** — Dead public catalog API: `Catalog::for_role` (test-only caller) and
      `Catalog::categories_for_role` (zero callers) duplicate `App::visible_in`/
      `visible_categories`, which additionally filter by host availability.
      `catalog/src/lib.rs:326-328,339-348`, `src/app.rs:41-56`.
      [ARCH C2] [ARCH D7] [BUGS D7]
- [ ] **M33** — `Recipe::refresh` (per-recipe state keys to re-read) is populated on 7
      recipes and read by nothing; the GUI instead re-reads all system state after
      every job. Wire it into a post-run state-diff toast ("Generation 360 → 361",
      "gaming enabled") instead of deleting it.
      `catalog/src/lib.rs:209-211`, `src/app.rs:106-116`, `src/ui/window.rs:129-131`.
      [ARCH C4] [FEAT F9]
- [ ] **M34** — Argument validation is split across 3 layers with 3 different
      rulesets: the GUI dialog checks only required-fields, the catalog crate is
      linked but its `validate::build` is never called from the GUI, and format/range
      errors round-trip through the daemon instead of pointing at the field.
      `src/ui/arg_dialog.rs:99-113`, `catalog/src/validate.rs:73-183`.
      [ARCH C5]
- [ ] **M35** — Audit coverage has holes: the `LimitsExceeded` rejection and the cancel
      authorization failure are not logged via `audit::rejected`, though the module's
      own doc presents journald as the complete record.
      `daemon/src/interface.rs:89-93,133-138`.
      [ARCH C8] [BUGS E10]
- [ ] **M36** — Add a daemon `Version` mismatch check: property is implemented and
      declared on both sides and read by neither, despite `catalog.toml` being
      compiled into both binaries separately (a real skew hazard, not boilerplate).
      `daemon/src/interface.rs:173-176`, `src/dbus_client.rs:248-249`.
      [ARCH D3] [FEAT F10]
- [ ] **M37** — `Excluded::reason` (3 catalog entries with prose reasons) is parsed and
      never displayed; surface it as an expander so users looking for `update`/`deploy`
      get an answer instead of concluding VexPortal is broken.
      `catalog/src/lib.rs:244-249`, `catalog/src/catalog.toml:658-670`.
      [ARCH D4] [FEAT F23]
- [ ] **M38** — `VEXOS_ASSUME_YES`/`needs_upstream` is hand-maintained across 6 recipes
      for a contract that doesn't exist upstream yet; extend `drift::compare` to detect
      it automatically from the justfile's recipe bodies instead.
      `daemon/src/config.rs:62-67`, `catalog/src/lib.rs:216-219`.
      [ARCH D5] [FEAT F17]
- [ ] **M39** — `check_consistency` misses duplicate category ids, duplicate parameter
      names within a recipe, a second `Secret` parameter (silently overwrites the
      first), and `destructive` without `confirm` — each currently a silent runtime
      defect rather than a caught build-time one.
      `catalog/src/lib.rs:278-315`.
      [ARCH D9] [BUGS L20]
- [ ] **M40** — Remove unused dependencies: `anyhow` (workspace + GUI, zero call
      sites), `serde` (GUI, only `serde_json::Value` is actually used), `serde_json`
      (daemon, zero call sites). Add `cargo-machete` to `scripts/preflight.sh` so this
      stays fixed. `Cargo.toml:19,36,39`, `daemon/Cargo.toml:18`.
      [ARCH E1] [ARCH E2] [ARCH E3] [BUGS D17]
- [ ] **M41** — Interior optional parameters are passed as literal `""` rather than
      omitted, so `just switch "" amd` binds `role = ""` instead of falling back to the
      justfile's own default for a non-final optional slot.
      `catalog/src/validate.rs:121-143`.
      [BUGS L9]
- [ ] **M42** — `JustDump` deserialization assumes an unversioned, non-guaranteed `just
      --dump` shape; a format change degrades the whole app at once (empty dropdowns,
      drift banner, "recipe not available" for things that are). No version pin
      anywhere between the devShell's `just` and the daemon's runtime `PATH` lookup.
      `catalog/src/drift.rs:14-49`, `src/just.rs:68-84`.
      [BUGS L10]
- [ ] **M43** — `RefCell` borrow (from `borrow_mut()` as the `if let` scrutinee) is held
      live across `state_changed()`'s full dashboard rebuild — a latent
      "already mutably borrowed" panic one added callback away.
      `src/app.rs:106-116`.
      [BUGS L11]
- [ ] **M44** — ANSI parser drops only the `ESC` of a non-CSI escape sequence (e.g. an
      OSC title sequence), leaking the rest of its body as literal noise into the log
      view — exactly what the parser exists to prevent.
      `src/ui/ansi.rs:108-119`.
      [BUGS L12]
- [ ] **M45** — `path_row`'s single subtitle slot is overwritten by help text, then
      permanently overwritten again once a file is chosen; "no file chosen" state and
      help text both become unrecoverable.
      `src/ui/arg_dialog.rs:184-215`.
      [BUGS L13]
- [ ] **M46** — `must_exist` path check is TOCTOU (`Path::exists()` re-resolved by root
      an unbounded time later, no symlink/ownership constraint).
      `daemon/src/interface.rs:81-86`.
      [BUGS S6]
- [ ] **M47** — Secret is never zeroized; persists in ≥3 unwiped heap allocations
      (`Invocation.stdin` String, the `format!` temporary, the zbus message buffer)
      inside a root process.
      `daemon/src/executor.rs:102-108`, `catalog/src/validate.rs:18-20`.
      [BUGS S7]
- [ ] **M48** — `emit_output` does an object-server lookup and async `RwLock` read per
      output line from two concurrent pump tasks; build the `SignalEmitter` once in
      `spawn()` instead.
      `daemon/src/executor.rs:172-190`.
      [BUGS P2]
- [ ] **M49** — `just --dump` runs synchronously on the GTK main thread inside
      `app::build`, blocking the window from appearing until it returns.
      `src/just.rs:36-47`, `src/app.rs:133`.
      [BUGS P4]
- [ ] **M50** — Failed secret-stdin write (`let _ = stdin.write_all(...)`) doesn't abort
      the job; the recipe proceeds as if the password arrived, then fails opaquely.
      `daemon/src/executor.rs:102-108`.
      [BUGS E5]
- [ ] **M51** — `let _ = events.send(...)` in 4 places; a closed channel is
      indistinguishable from a successful send, so the pumps keep looping into the
      void with no shutdown signal in either direction.
      `src/dbus_client.rs:151-157,168-173,192,214-219`.
      [BUGS E6]
- [ ] **M52** — Failed `JobOutput`/`JobFinished` signal emission from the daemon is
      silently dropped — precisely the failure mode that causes a run page to hang
      forever, and it leaves nothing in the journal that's supposed to be the audit
      trail. `daemon/src/executor.rs:172-190`.
      [BUGS E7]
- [ ] **M53** — `Result<_, String>` in `auth::check` and `Config::from_args` collapses
      distinct daemon failures ("polkit unreachable" vs "polkit rejected the subject")
      into one opaque `fdo::Error::Failed`, in a crate where every other error type is
      a `thiserror` enum. `daemon/src/auth.rs:16-36`, `daemon/src/config.rs:27`.
      [BUGS E9]
- [ ] **M54** — No generation browser or rollback-target picker, despite
      `read_generation` already demonstrating the privilege-free technique (parse the
      `system-<n>-link` profile symlink) that would generalize to listing all
      generations. `src/system/state.rs:63-72`.
      [FEAT F11]
- [ ] **M55** — Run page has no log search, no save-to-file, no jump-to-first-error,
      despite the `stderr` tag already marking exactly the lines a jump-to-error would
      need and a working `FileDialog::save` flow already existing elsewhere in the GUI.
      `src/ui/run_page.rs:119-129,168-171`, `src/ui/arg_dialog.rs:217-221`.
      [FEAT F12]
- [ ] **M56** — Failure banner ("The output above says why") gives no actionable next
      step; `adw::Banner` supports an action button and the app already knows how to
      open a terminal and run `rollback`.
      `src/ui/run_page.rs:181-183`.
      [FEAT F13]
- [ ] **M57** — No operation-history view; `audit.rs` writes a complete record that is
      only reachable via `journalctl` and typically not readable by an unprivileged
      desktop user. Needs a small polkit-gated daemon method
      (`GetHistory`) since the daemon idle-exits and can't hold it in memory.
      `daemon/src/audit.rs`.
      [FEAT F14]
- [ ] **M58** — No CLI companion, despite the catalog carrying ~20 recipes for
      `headless-server`/`vanilla` roles that structurally cannot run the GUI at all.
      A `vexportal-cli` workspace member would reuse `vexportal-catalog` and the
      existing D-Bus surface unchanged.
      [FEAT F15]
- [ ] **M59** — No CI workflow exists (`.github/` has none); `nix flake check` and
      `scripts/preflight.sh` independently define "checked" and disagree (flake check
      never runs fmt/clippy). Add `checks.fmt`/`checks.clippy` derivations and a GitHub
      Actions workflow running `nix flake check`.
      `flake.nix:37`, `scripts/preflight.sh:13-23`.
      [ARCH E7] [FEAT F16]

---

## LOW priority (36)

- [ ] **M60** — Daemon idle-shutdown polls a deadline it already knows (12 wakeups/idle
      minute); use `sleep_until(last_activity + timeout)` instead.
      `daemon/src/main.rs:29-30,85-90`, `daemon/src/lifecycle.rs`.
      [ARCH A10] [BUGS P8]
- [ ] **M61** — Only `run_recipe` marks daemon activity; `cancel`/`list_jobs` don't, so
      a client that only polls/cancels can have the daemon idle-exit from under it.
      `daemon/src/interface.rs:113`.
      [ARCH A11]
- [ ] **M62** — Constant placement inconsistent (`STDERR` defined below its `impl`
      block) and referenced via an unnecessary absolute path from inside its own
      module. `src/ui/run_page.rs:16,169,261`.
      [ARCH B2]
- [ ] **M63** — `color_tag_name` duplicates `Color::tag` character-for-character; the
      two must be hand-kept in sync or tag lookups silently fail at runtime. Make
      `Color::tag` public and delete the duplicate.
      `src/ui/ansi.rs:64-74`, `src/ui/run_page.rs:310-320`.
      [ARCH B5] [BUGS D16]
- [ ] **M64** — Two import paths for `gio` used inconsistently (`gio::` direct vs
      `gtk::gio::`). `src/main.rs:19` vs `src/ui/arg_dialog.rs:210,218,220`.
      [ARCH B6]
- [ ] **M65** — Naming: `append_plain` actually forces the stderr tag (backwards from
      its name); `box_` trailing-underscore; `Window::root()` returns an
      `adw::ApplicationWindow` from a type also called `Window`.
      `src/ui/run_page.rs:204-206`, `src/ui/window.rs:137-139,154`.
      [ARCH B7]
- [ ] **M66** — `.github/docs/subagent_docs/` holds process artifacts in GitHub's config
      directory with no actual workflows present; `docs/vexos-nix-prompt.md` is a chat
      handoff prompt addressed to a different repository, checked into this one.
      [ARCH B8]
- [ ] **M67** — `HashMap<_, ()>` used as a set in `check_consistency`; `HashSet` reads
      correctly instead. `catalog/src/lib.rs:279-289`.
      [ARCH B9]
- [ ] **M68** — Empty string overloaded to mean 3 different things in
      `Field::value()` ("left blank", "leave unchanged" sentinel, "no file chosen");
      cannot express a legitimately empty answer.
      `src/ui/arg_dialog.rs:22-42,96`.
      [ARCH C6]
- [ ] **M69** — Inconsistent log-level defaults (GUI `warn`, daemon `info`) with almost
      no `debug`-level call sites in either, so `RUST_LOG=debug` yields nothing extra.
      `src/main.rs:15`, `daemon/src/main.rs:34`.
      [ARCH C7] [BUGS E16]
- [ ] **M70** — Dead CSS: `.vex-hero` (no caller) and `.vex-risk.medium` (`risk_pill`
      returns `None` for `Risk::Medium`, so the class is never applied).
      `data/style.css:29-33,51-54`.
      [ARCH D8] [BUGS D9]
- [ ] **M71** — GUI's tokio dependency inherits the daemon's full feature set
      (`process`, `signal`, `io-util`, `rt-multi-thread`) via workspace unification,
      though the GUI only uses `rt` + `spawn`. `Cargo.toml:24`.
      [ARCH E4]
- [ ] **M72** — `flake-utils` pulled in for a single `eachDefaultSystem` call, which
      exposes `packages.default` on darwin systems the package can never build on
      (GTK4/libadwaita/polkit/`/etc/nixos`, `meta.platforms = linux`).
      `flake.nix:6,10`, `nix/package.nix:77`.
      [ARCH E5]
- [ ] **M73** — `rust-toolchain.toml` takes effect only via a bare `rustup` shim on
      `PATH` — exactly the failure mode `CLAUDE.md`'s FORBIDDEN COMMANDS section exists
      to prevent — while every real build path (devShell, `rustPlatform`) ignores it.
      `rust-toolchain.toml`.
      [ARCH E6]
- [ ] **M74** — `nix/module.nix`'s `justfile` option is `types.path` (copies the file
      into the Nix store at eval time if written as a path literal) where it means
      `types.str` (a live filesystem path the running daemon reads).
      `nix/module.nix:24-32`.
      [ARCH E8]
- [ ] **M75** — `declined()`/`failed_to_start()` leave `State.finished == false` (only
      `finished()` sets it), an unreliable invariant for any future reader.
      `src/ui/run_page.rs:151-206`.
      [BUGS L14]
- [ ] **M76** — Dashboard is constructed twice on every launch (sidebar's initial
      `select_row` fires the selection handler, then `Window::new` calls
      `show_category` again); category pages are also fully rebuilt with no caching on
      every sidebar click. `src/ui/window.rs:45-62,91-93,111-122`.
      [BUGS L15] [BUGS P6]
- [ ] **M77** — `Variant::parse` has no component boundary (`vexos-desktopish` parses as
      a valid Desktop variant); the longest-first role sort is both unstable and solves
      a shadowing case that can't actually occur.
      `src/system/variant.rs:53-73`.
      [BUGS L16]
- [ ] **M78** — An empty-but-set `VEXPORTAL_VARIANT=` env var makes a real host claim
      "not built yet" instead of falling through to the real file.
      `src/system/variant.rs:41-43`.
      [BUGS L17]
- [ ] **M79** — `choices()` doesn't dedupe catalog `extra` entries against the dynamic
      justfile-sourced list; an overlap renders the same value twice in a dropdown.
      `src/just.rs:88-99`.
      [BUGS L18]
- [ ] **M80** — A relative `--justfile` argument yields an empty (not `/`) working
      directory, producing an opaque ENOENT instead of a clear config error.
      `daemon/src/config.rs:44-48`.
      [BUGS L19]
- [ ] **M81** — Cancel is a kill-by-pid whose failure path logs at `debug!` (filtered
      out by the daemon's `info` default), so a cancel that silently no-ops looks
      identical to a successful one; also carries a (narrow) pid-recycling hazard as
      root. `daemon/src/cancel.rs:33-47`.
      [BUGS L21] [BUGS S9]
- [ ] **M82** — Stylesheet uses `:root`/`var()` (GTK ≥4.16) while the crate's declared
      feature floor is `v4_12`; the role badge silently loses its gradient below 4.16.
      `data/style.css:9-22`, `Cargo.toml:43-44`.
      [BUGS L22]
- [ ] **M83** — `unreachable!()` for the `Secret` widget case sits inside a root D-Bus
      service; the invariant holds today but a future reorder turns malformed input
      into a daemon panic instead of a typed error.
      `catalog/src/validate.rs:180`.
      [BUGS S8]
- [ ] **M84** — 32 `GtkTextTag`s are constructed per run page from an entirely static
      tag set; build one shared `GtkTextTagTable` for the process instead.
      `src/ui/run_page.rs:263-308`.
      [BUGS P5]
- [ ] **M85** — `visible_categories()` builds and discards a full `Vec<&Recipe>` per
      category just to check `is_empty()`; `Iterator::any` avoids the allocation.
      `src/app.rs:41-56`.
      [BUGS P7]
- [ ] **M86** — `spawn_terminal` allocates all 4 candidate argv vectors up front before
      trying the first (which succeeds on essentially every GNOME desktop).
      `src/ui/category_page.rs:178-220`.
      [BUGS P9]
- [ ] **M87** — A fresh `DBusProxy` is constructed on every `run_recipe` purely for the
      audit uid lookup; build it once in `Daemon::new`.
      `daemon/src/interface.rs:194-200`.
      [BUGS P10]
- [ ] **M88** — Linear `iter().find()` scans on `Catalog::recipe`/`category` hot paths —
      irrelevant at 42 recipes, noted only so it's a conscious choice if the catalog
      grows. `catalog/src/lib.rs:317-323`.
      [BUGS P11]
- [ ] **M89** — File-chooser cancellation and a real portal failure are
      indistinguishable (`let Ok(file) = result else { return }`); a non-local URI with
      no local path is also silently dropped. `src/ui/arg_dialog.rs:210-216`.
      [BUGS E13]
- [ ] **M90** — `spawn_terminal` reports success (`spawn().is_ok()`) for a terminal
      binary that launched and immediately exited (no display, broken profile); the
      informative fallback dialog is never reached in that case. Child is also never
      `wait()`ed, leaving a zombie per launch.
      `src/ui/category_page.rs:222-230`.
      [BUGS E14]
- [ ] **M91** — Raw D-Bus error names (`org.freedesktop.DBus.Error.ServiceUnknown`) can
      reach the user's dialog for a `MethodError` with no message body — the function's
      own comment says this is "not what anyone wants to read in a dialog" right before
      doing it. `src/dbus_client.rs:227`.
      [BUGS E15]
- [ ] **M92** — Trivial cleanups bundle: unnecessary `Rc<Self>` receivers where `&self`
      suffices (`src/app.rs:63,77`); `let _ = app;` parameter-silencing instead of
      removal (`src/ui/category_page.rs:172`); always-true `visible_page().is_some()`
      guard (`src/ui/window.rs:146`); unreachable defensive re-lookups that silently
      `return` instead of asserting (`src/ui/category_page.rs:13,89,139`).
      [ARCH D8] [BUGS D11] [BUGS D13] [BUGS D14] [BUGS D15]
- [ ] **M93** — No elapsed-time display on the run page, and no duration recorded in
      the audit log — cheap on both sides, and completes the "who ran what, how long"
      story the journal already half-answers.
      `src/ui/run_page.rs:58-63`, `daemon/src/audit.rs:19-25`.
      [FEAT F19]
- [ ] **M94** — No "reboot now" action on the dashboard's reboot-pending row, despite
      both the state (`reboot_pending`) and the recipe (`reboot`) already existing —
      they've simply never been introduced to each other.
      `src/ui/dashboard.rs:94-102`, `catalog/src/catalog.toml:637`.
      [FEAT F20]
- [ ] **M95** — Drift banner has no action button despite `adw::Banner` supporting one
      and `drift_summary()` already distinguishing the one-click-fixable case
      ("host hasn't rebuilt" → offer `rebuild`) from the catalog-defect case.
      `src/just.rs:120-152`, `src/ui/dashboard.rs:185-188`.
      [FEAT F21]
- [ ] **M96** — No manual refresh button and no refresh-on-window-focus, despite
      `refresh_state()`/`state_changed()` already existing and being called from
      exactly one place (post-job). Pair with M29 to also refresh `JustfileFacts`.
      `src/app.rs:58-60,111-114`.
      [FEAT F22]
- [ ] **M97** — Window size and last-selected sidebar category aren't persisted between
      launches; `Window.current` already tracks the latter in memory.
      `src/ui/window.rs:20`.
      [FEAT F24]

---

**Totals:** 23 high · 33 medium · 36 low · 5 explicitly-not-recommended items noted in
`ANALYSIS_FEATURES.md` (scheduled operations) are intentionally excluded here.
