# GUI Overhaul & Justfile Resync — Specification

**Feature name:** `gui_overhaul`
**Date:** 2026-10-05
**Phase:** 1 (Research & Specification)
**Upstream reference:** vexos-nix `main` @ `f6159bb` ("refactor(just): group recipes into menus; add zfs-pool and mergerfs-pool managers")
**VexPortal baseline:** `main` @ `8cf6aaa` (catalog last synced 2026-08-28)

### Decisions confirmed by the user (2026-10-05)

1. **Interactive recipes run in an embedded VTE terminal**, as the logged-in user, using a catalog-validated argv with no shell. `xdg-terminal-exec` is the fallback.
2. **State-driven pages** replace the "list of recipes, each with a Run button" model.
3. **The runtime bugs L1, L3 and L7** from `docs/ANALYSIS_BUGS.md` are in scope.

---

## 1. Current state analysis

### 1.1 The justfile changed under the catalog

vexos-nix has 24 justfile commits since the catalog was last synced. The biggest is today's `f6159bb`, which moved most recipes behind **grouped menu dispatchers**:

```just
feature action="" *args: _require-desktop-role
    @just _menu feature "Optional features" "{{action}}" "{{args}}" "list|…" "enable|…|Feature to enable (or all)" …
```

`_menu` (justfile L1790) runs `exec just "_${group}-${action}" "${args[@]}"`. Two details of `_menu` matter here:

- **With no action and no TTY, it prints usage and exits 1** (`[ -t 0 ] || { usage; exit 1; }`). With no TTY, an action marked as needing an argument also errors instead of prompting. A non-interactive caller therefore always gets a clean failure, never a hang.
- **Extra args are joined into one string and re-split on whitespace** (`read -r -a args <<< "$argstr"`). An argument routed through a dispatcher must not contain whitespace.

The catalog's recipe names no longer exist. The drift check reports them as `Missing`, so `JustfileFacts::is_available` **hides them**. On a host that has rebuilt since `f6159bb`, about half the portal has silently disappeared.

| Catalog `name` (today) | Justfile now | New invocation (argv after `just`) | Hidden impl. recipe (drift target) |
|---|---|---|---|
| `variant`, `rebuild`, `build`, `rollback`, `rollforward`, `upgrade-analysis`, `reboot`, `shutdown`, `fix-flake`, `secrets-init`, `backup-now`, `backup-plex`, `restore-plex`, `set-hostname`, `ssh`, `setup-tailscale`, `reset-defaults` | unchanged public | same | same |
| `switch` | public; legacy NVIDIA is now **`nvidia-legacy580`** (catalog still says `535`) | `switch …` | `switch` |
| `switch-bootloader` | removed | `bootloader switch [target]` | `_bootloader-switch` |
| `switch-bootloader-cleanup` | removed | `bootloader cleanup` | `_bootloader-cleanup` |
| `features` | removed | `feature list` | `_feature-list` |
| `enable-feature` / `disable-feature` | removed | `feature enable X` / `feature disable X` | `_feature-enable` / `_feature-disable` |
| `services` | removed (now an alias of info) | `service list` | `_service-list` |
| `available-services` | removed | `service available` | `_service-available` |
| `service-info` | removed | `service info [svc]` | `_service-info` |
| `status` / `restart` / `disable` | removed | `service status X` / `restart X` / `disable X` | `_service-status` / `_service-restart` / `_service-disable` |
| `enable` (terminal) | removed | `service enable X` | `_service-enable` |
| `enable-plex-pass` / `disable-plex-pass` | removed | `plex-pass enable` / `plex-pass disable` | `_plex-pass-enable` / `_plex-pass-disable` |
| `create-zfs-pool` | removed | `zfs-pool` (interactive menu: create/destroy/import/replace/scrub) | `zfs-pool` |
| `create-mergerfs-pool` | removed | `mergerfs-pool` (interactive menu) | `mergerfs-pool` |
| `attach-remote-storage` | removed; **now also on desktop/htpc** | `remote-storage attach` | `_remote-storage-attach` |
| `enable-kill-switch` / `disable-kill-switch` | removed; **roles now desktop/htpc/stateless**, needs the `vpn` feature | `kill-switch on` / `kill-switch off` | `_kill-switch-on` / `_kill-switch-off` |
| `harmonia-info` | removed | `cache harmonia` | `_cache-harmonia` |
| `attic-push` / `attic-bootstrap` | removed | `cache push [cache]` / `cache bootstrap [cache]` | `_cache-push` / `_cache-bootstrap` |
| `kernel-build-status` / `-log` / `-now` | removed | `kernel status` / `kernel log [name]` / `kernel now [name]` | `_kernel-status` / `_kernel-log` / `_kernel-now` |
| `setup-rdp` | **deleted upstream** | — | drop from catalog |

**New upstream surface not in the catalog:**

| New invocation | Notes |
|---|---|
| `remote-storage detach` | interactive |
| `vpn login` | `sudo vexos-vpn login`, prompts for credentials, so it runs in the terminal |
| `vpn selftest`, `vpn up`, `vpn down`, `vpn status`, `vpn regions`, `vpn region X`, `vpn protocol X` | non-interactive |
| `agent` | launches the AI assistant TUI; terminal, runs as the user |
| `diagnose [target]` | AI diagnosis; terminal, runs as the user |
| `nas-sync-status`, `nas-sync-now name` | server; non-interactive |
| `restore-service name [snapshot]` | server; destructive; honours `VEXOS_ASSUME_YES` |
| `prune-services` | private, deliberately not surfaced → `[[excluded]]` is not needed because it is private |

**`needs_upstream` flags that are now stale:** `restore-plex` (L2108 honours `VEXOS_ASSUME_YES`), `fix-flake` and `set-hostname` (both use `_confirm`, and the GUI always passes a name). `reset-defaults` uses `_confirm`, but it moves to the terminal lane (§1.2), so its flag goes too. `switch` stays: its prompts fire only for empty fields, and the new switch form (§3.4) always supplies role, GPU, DE and VM platform as applicable.

### 1.2 Why options do not work (root causes, verified)

| # | Symptom | Root cause | Evidence |
|---|---|---|---|
| R1 | Every "Open" (terminal) option fails or does nothing useful | `spawn_terminal` tries `kgx`, `gnome-terminal`, `ptyxis`, `xterm`. VexOS ships **Ghostty** on every role and explicitly excludes `xterm` (`modules/gnome.nix:194`). It also runs `just <name>` with **no parameters**, e.g. `just enable` with no service, and those names no longer exist. | `src/ui/category_page.rs:150-227`; vexos-nix `modules/packages-common.nix`, `gnome-*.nix` |
| R2 | Half the cards vanished after the latest vexos-nix rebuild | Catalog names → `Drift::Missing` → hidden | §1.1 |
| R3 | "Copy SSH Key", "Set Up Tailscale" and "Reset Desktop" run but do the wrong thing | The daemon runs recipes as **root** with `HOME=/root` and no `USER`. `ssh` generates and copies **root's** key, and `ssh-copy-id` needs a remote password typed on a TTY. `setup-tailscale` runs `"$USER"` under `set -u`, so it dies with an unbound variable or makes root the operator. `reset-defaults` resets **root's** dconf. | justfile L1249-1276, L1284-1307, L1131-1147; `daemon/src/config.rs:55-74` |
| R4 | Switch Role to a legacy NVIDIA card fails | Catalog offers `nvidia-legacy535`; the only valid value is `nvidia-legacy580` | justfile L252, `flake.nix` outputs |
| R5 | Fast read-only actions sometimes spin forever or show no output | **L1**: `JobOutput`/`JobFinished` can arrive before `Started`; the events are dropped | `docs/ANALYSIS_BUGS.md` L1 |
| R6 | The result page vanishes the moment a job finishes (from the dashboard) | **L3**: `state_changed()` does `navigation.replace(&[dashboard])` over the pushed run page | `docs/ANALYSIS_BUGS.md` L3 |
| R7 | Destructive pool wizards launch with no confirmation | **L7**: terminal path skips `confirm_then_run` | `docs/ANALYSIS_BUGS.md` L7 |

### 1.3 Why it feels clunky (UX analysis)

- **Every operation is a sibling row with a Run button.** "List Features", "Enable Feature" and "Disable Feature" are three rows, each opening a dropdown dialog, when what the user wants is a switch per feature. The same applies to services (8 rows plus a dropdown) and the VPN kill switch (two rows that are logically one switch).
- **No state is visible where you act on it.** To learn whether `plex` is enabled you run "Service Status", read a log, go back, then pick "Disable Service" and choose `plex` from a 45-item dropdown.
- **Running anything navigates away** to a full-page log, even a 50 ms read. Read-only results ("Service Access Info", "Show Active Variant") are dumped as raw terminal text.
- **Edits that need a rebuild give no follow-through.** Enabling a feature says nothing about the rebuild it needs.
- **The categories mirror the old justfile**, not the user's goals. VPN is split between "Network & Remote" and nothing, AI has no home, and remote storage is server-only in the GUI but not upstream.
- **`Switch Role or GPU` is a flat form** that shows DE and VM-platform fields that only apply to some combinations, and silently relies on the justfile prompting for the rest.

---

## 2. Problem definition & success criteria

**Goal:** every operation the vexos-nix justfile offers for a role is reachable from VexPortal, and every one of them works:

- non-interactive ones complete through the daemon,
- interactive or user-context ones run in an in-app terminal as the user,
- and the main things a user manages (features, services, VPN) are shown as **state with controls**, not as recipe launchers.

Verifiable success criteria:

1. Drift check against vexos-nix `f6159bb`'s justfile reports **zero catalog defects** (`Unlisted`, `Parameters`, `Requiredness`). This is verified with a new fixture test (§4 step 2) that does not need a VexOS host.
2. Every catalog entry has an explicit `mode` (`daemon` or `terminal`). Every recipe that reads stdin, `$USER`, `$HOME`, or dconf, or that needs a TTY, is `terminal`. Each such recipe is annotated in the catalog with the reason.
3. Terminal-mode entries spawn `just` with an argv validated by the same `validate::build` the daemon uses. No shell string is constructed anywhere (`grep -rn '"-lc"\|"-c"' src/` returns nothing).
4. Destructive actions in **both** modes pass a confirmation dialog. `check_consistency` rejects `risk = "destructive"` without `confirm`.
5. Unit tests cover L1: events for an unknown job id are buffered and replayed on `Started`. L3 is fixed by construction (job views no longer live in the navigation stack), and a test asserts that `state_changed` never touches a job view.
6. Each Phase 3 build-validation command passes, and so does `nix build .#default`, because packaging changes.

---

## 3. Proposed solution architecture

### 3.1 Three execution lanes

```
                 ┌─────────────── GUI (user) ────────────────┐
  Local read ──► │ features.nix, server-services.nix,        │  unprivileged file reads +
                 │ `vexos-vpn status --json`, just --dump     │  `vexos-vpn … --json`
                 ├───────────────────────────────────────────┤
  Terminal   ──► │ VTE pane: argv = just --justfile … <cmd>  │  runs AS THE USER; sudo prompts
                 │ (validated by catalog; no shell)          │  appear inline, exactly like CLI
                 └───────────────┬───────────────────────────┘
                                 │ D-Bus RunRecipe(id, args)
                 ┌───────────────▼───────────────────────────┐
  Daemon     ──► │ vexportal-daemon (root, polkit, audit)    │  unchanged security model
                 └───────────────────────────────────────────┘
```

- **Daemon lane** (unchanged security model): non-interactive recipes that need root and act on the system, such as rebuild, switch, feature toggle, service status/restart/disable, VPN up/down/region/protocol/kill switch, caches, kernel now/status, power, hostname, rollback, backups and restores.
- **Terminal lane** (new, replaces `spawn_terminal`): recipes that are interactive wizards or that act on the user's own account. The lane uses an embedded `vte4::Terminal` and calls `spawn_async` with argv `["just", "--justfile", "/etc/nixos/justfile", "--working-directory", "/etc/nixos", <command…>, <args…>]` and the user's environment, plus `VEXPORTAL=1`. It **never** sets `VEXOS_ASSUME_YES`: the human answers. Privileged steps go through the recipe's own `sudo`, so the GUI gains no privilege. If VTE spawn fails, the fallback is `xdg-terminal-exec --dir=/etc/nixos --title=<title> -- <same argv>`.
- **Local read lane**: state the user can already read: `/etc/nixos/features.nix`, `/etc/nixos/server-services.nix`, `vexos-vpn status --json` and `vexos-vpn regions --json` (documented as working without sudo for the users group), and `just --dump`.

**Security boundary statement (for the reviewer).** The terminal lane does not construct a shell command line. It execs `just` directly with an argv vector, with every element taken from the catalog or validated by `vexportal_catalog::validate`. It runs with the user's own privileges, as the existing `spawn_terminal` escape hatch already did, so it cannot bypass the daemon's argv path for anything privileged. The daemon still rejects `mode = "terminal"` entries (`ValidationError::TerminalOnly`), and secrets still never touch argv.

### 3.2 Catalog schema v2

`catalog.toml` entries change from "a recipe name" to "an action with an argv prefix":

```toml
[[action]]
id        = "feature-enable"            # stable id; what the GUI sends and the daemon allowlists
command   = ["feature", "enable"]       # argv after `just`; params are appended
implements = "_feature-enable"          # justfile recipe whose params the drift check compares
title     = "Enable Feature"
blurb     = "…"
icon      = "list-add-symbolic"
page      = "features"                  # replaces `category`
roles     = ["desktop", "htpc", "server", "vanilla"]
risk      = "medium"
mode      = "daemon"                    # or "terminal"; replaces `terminal = true`
rebuild   = true                        # success marks "rebuild needed" (replaces refresh=["features"])
mode_reason = "…"                       # required when mode = "terminal": why it can't be a form

  [[action.params]]
  name = "feature"
  …
```

Rust changes in `catalog/src/lib.rs`:

- `Recipe` → `Action` with the fields `id`, `command: Vec<String>`, `implements: String`, `mode: Mode { Daemon, Terminal }`, `rebuild: bool`, `mode_reason: Option<String>`, and `page` instead of `category`. Remove `terminal`, `needs_upstream` and `refresh`, since nothing will read them (Simplicity First).
- `Category` → `Page`. Pages are the sidebar entries in §3.3.
- `Catalog::action(id)`, `in_page(page, role)` and `pages_for_role(role)` replace the recipe/category accessors.
- New `[[feature]]` table with `name`, `title`, `description`, `default_on` (sunshine = true; mirrors `_feature-list`'s `_check sunshine on`), and optional `roles`.
- `DynamicSource` gains `ServiceCatalog`, parsed from the `_service_catalog` assignment (`Group|name|description` lines). This gives the services page groups and descriptions straight from the justfile.
- `check_consistency` additionally rejects:
  - (a) `risk = destructive` without `confirm` (L7);
  - (b) `mode = terminal` without `mode_reason`;
  - (c) any param of an action whose `command` has more than one element (i.e. routed through `_menu`) using a format that admits whitespace (`AbsPath`, `FlakeRef`, `Secret`), because `_menu` re-splits on whitespace;
  - (d) duplicate `id`.

`validate::build` keeps its contract and returns `Invocation { argv: command ++ params, … }`. `Invocation::just_args()` returns `command` followed by positional args, with trailing empties trimmed (existing behaviour). Terminal-mode actions are validated with a new `validate::build_for_terminal`. It shares the same code path and differs only in that it accepts `mode = terminal` and rejects `daemon`, so the GUI cannot be tricked into running a root-lane action unprivileged.

`drift::compare` changes:

- Compare params against `dump.recipes[action.implements]`, not `action.command[0]`.
- `Missing` is keyed on `implements`.
- `Unlisted` considers a public recipe "listed" if any action's `command[0]` equals it. Dispatchers like `feature` are covered by their actions.

The D-Bus interface keeps `RunRecipe(s recipe, a{ss} args)`. The `recipe` string is now the action `id`. No signature change, so `data/` policy and service files are untouched.

### 3.3 Information architecture (sidebar)

Pages are shown only when they have at least one action for the role and the host's justfile has it (existing behaviour):

| Page | Roles | Contents |
|---|---|---|
| **Overview** | all | Identity (role badge, GPU, host, generation, flake age); **Rebuild-needed banner** (§3.5); reboot-pending row; primary buttons *Rebuild* and *Switch Role…*; drift banner. |
| **System** | all | Groups: *Build* (Rebuild, Test Build…, Switch Role…); *Generations* (Roll Back, Roll Forward, Analyse Upgrade…); *Bootloader* (Migrate to Limine → Finish Migration, with step 2 disabled until step 1 has run and the user has rebooted; the subtitle explains why); *Identity* (Hostname `AdwEntryRow` with an Apply button); *Power* (Reboot, Shut Down). |
| **Features** | desktop, htpc, server, vanilla | One `AdwSwitchRow` per `_feature_names` entry, with title and description from `[[feature]]`. State is the explicit value in features.nix, otherwise `default_on`; defaulted rows say "(default)" in the subtitle. Toggling runs `feature-enable`/`feature-disable` via the daemon. The switch is insensitive with an `AdwSpinner` suffix while running, reverts on failure with a toast, and marks a rebuild needed on success. *Advanced* group: Repair Feature Wiring. *AI Assistant* group (shown if feature `ai` is on): Open Assistant (terminal), Diagnose… (terminal). |
| **VPN** | desktop, htpc, stateless | Shown only if `vexos-vpn` is on PATH; otherwise a status page that offers to enable the `vpn` feature (desktop/htpc). Status card from `vexos-vpn status --json` (state, region, protocol, kill switch), refreshed every 5 s while visible. Connect/Disconnect button; Kill Switch `AdwSwitchRow` (off = destructive confirm); Region `AdwComboRow` from `regions --json` plus `auto`; Protocol combo (wireguard/openvpn). Account group: Sign In… (terminal: `vpn login`), Test Login. *Tailscale* group: Set Up Tailscale (terminal, user). |
| **Services** | server, headless-server | `AdwPreferencesGroup` per `_service_catalog` group, with a search entry. One row per service: description subtitle, "Enabled" pill if `vexos.server.<name>.enable = true` in server-services.nix, chevron. Activating a row pushes a **service detail page**: Access Info (inline result, §3.6), Check Status, Restart, Disable (enabled services) or Enable… (terminal, since the wizard asks questions) for disabled ones; Plex detail adds a *Plex Pass transcoding* switch. Header actions: Access info for all. |
| **Storage & Backup** | server roles; remote storage also desktop/htpc | *Backups*: Run Backup Now, Restore a Service… (service dropdown + optional snapshot; destructive confirm), NAS Sync Status, Run NAS Sync…; *Plex*: Back Up / Restore Plex; *Pools*: ZFS Pool Manager, Bulk Pool Manager (terminal, destructive confirm); *Remote storage*: Attach…, Detach… (terminal). |
| **Cache & Kernel** | server roles | Harmonia status, Push to Attic…, Bootstrap Attic…; Kernel: Build Now…, Build Status, Follow Build Log (terminal lane, so follow/tail ends when the user closes it; fixes L6 by construction). |
| **Tools** | per action | Copy SSH Key… (terminal, user), Reset Desktop to Defaults (terminal, user, destructive confirm), Set Up Encrypted Secrets (terminal, server). |
| **Activity** | all | List of jobs from this session (running first) with status icon, elapsed time, and an Open button that shows the job view (§3.6). |

The header bar gets an **activity button** (spinner while any job runs, with a count) that opens Activity.

### 3.4 Switch Role dialog (replaces the flat form)

An `adw::Dialog` with an `AdwPreferencesPage`, where fields appear conditionally:

1. Role combo: desktop, stateless, htpc, server (GUI), headless server, vanilla. Pre-selected to the current role.
2. GPU combo: amd, nvidia (latest), nvidia legacy 580, intel, vm. Pre-selected from the current variant.
3. Desktop environment (only if role = desktop): gnome, cosmic, hyprland. Pre-selected from `vexos.desktop.environment` in features.nix, default gnome.
4. VM platform (only if GPU = vm): qemu, virtualbox. Pre-selected from `vexos.vm.platform`, default qemu.
5. Collapsed *Advanced* expander: flake override.

The dialog **always** sends role and GPU, and sends `de`/`vmp` when the field is visible. The justfile then never reaches a `read`. A summary line ("Will rebuild as vexos-desktop-amd with GNOME") sits above a destructive-styled *Switch* button that runs through the existing confirm. *Test Build…* reuses the same dialog in "build" mode: it hides DE/VM and uses the `build` action.

### 3.5 "Rebuild needed" model

A rebuild is needed when `max(mtime(features.nix), mtime(server-services.nix), mtime(storage-pool.nix if present))` is later than the `lstat` mtime of `/run/current-system`. That symlink is replaced on every activation. This needs no new state, survives app restarts, and covers edits made from the CLI too. It is recomputed after every job with `rebuild = true` finishes, and on window focus. The banner is an `adw::Banner` on Overview, Features and Services: "Changes are waiting for a rebuild — *Rebuild now*". Its button starts the `rebuild` action.

### 3.6 Job model and views (fixes L1, L3)

- `App` keeps `jobs: Vec<Rc<Job>>`, where `Job { id, action, args_summary, state: Pending|Running|Done(i32)|Failed(String)|Declined, log: gtk::TextBuffer, started_at }`. The buffer lives on the `Job`, not the view, so any number of views can show it and closing a view loses nothing.
- **L1 fix:** `App.orphans: HashMap<String, Vec<Event>>` buffers `Output`/`Finished` for job ids not yet known. On `Started`, the buffered events are replayed in order. Orphans older than 30 s are dropped with a `log::warn!`.
- **L3 fix:** job views are never pushed onto the page `NavigationView`. A job opens in an `adw::Dialog` (content ~720×520) containing the existing log view, status row and Cancel, which is the reused `RunPage` widget code renamed `JobView`. `state_changed()` only rebuilds pages, and job dialogs are unaffected.
- **Inline results for reads:** `risk = safe` actions launched from a page (Access Info, Check Status, Build Status, Test Login, NAS Sync Status) open the same job dialog. They don't get a separate rendering path: one view, plain log text (Simplicity First).
- **State-changing actions launched from a control** (feature switch, kill switch, region combo, service Restart/Disable) **do not open a dialog.** The control shows a spinner and the result arrives as a toast. Failures get a "Details" button that opens the job dialog.
- **Terminal jobs** open a `TerminalView` in an `adw::Dialog` (content ~800×560) holding a `vte4::Terminal`. A header subtitle shows the exact command (`just service enable plex`). On `child-exited`, the status row shows the exit code, the terminal stays readable, and a Close button appears. Terminal jobs appear in Activity like daemon jobs. Their log is the VTE scrollback, and the Activity row just re-presents the dialog while it is alive.

### 3.7 L7 and confirmation

`activate()` routes **both** modes through `confirm_then_run` / `confirm_then_open`. Together with the new `check_consistency` rule (destructive ⇒ confirm), no destructive action in either lane can start without a dialog.

---

## 4. Implementation steps (each with verification)

Work is ordered so the tree builds and tests pass after every step.

1. **Catalog schema v2 + resync** (`catalog/src/lib.rs`, `catalog.toml`, `validate.rs`, `drift.rs`, `format.rs` untouched).
   Rewrite `catalog.toml` per §1.1 and §3.3:
   - all actions with `command`/`implements`/`mode`/`page`;
   - the `[[feature]]` table (gaming, development, print3d, virtualization, sunshine (default_on), vpn, kernel, ai);
   - `nvidia-legacy580`;
   - drop `setup-rdp`;
   - remove stale `needs_upstream`;
   - mark ssh, setup-tailscale, reset-defaults, service-enable, zfs-pool, mergerfs-pool, remote-storage attach/detach, secrets-init, vpn-login, agent, diagnose and kernel-log as `terminal`, each with `mode_reason`.

   Roles for kill-switch/vpn: desktop, htpc, stateless. Remote storage: desktop, htpc, server, headless-server.
   → verify: `nix develop -c cargo test -p vexportal-catalog` (existing tests updated + new consistency tests for rules a–d).
2. **Drift fixture test.** Commit `catalog/tests/fixtures/justfile-f6159bb.json` (output of `just --dump --dump-format json` on the vexos-nix justfile at `f6159bb`; generated in the devShell, which provides `just`). Add a test asserting `compare()` yields no catalog defects against it.
   → verify: test passes; deliberately renaming one `implements` makes it fail.
3. **Daemon accepts action ids.** Update `daemon/src/interface.rs`/`audit.rs` for the renamed types. No D-Bus signature change, and `executor.rs` is unchanged apart from types (stdin/secret path preserved).
   → verify: `cargo test --workspace`; `validate` tests show `feature-enable` → `["feature","enable","gaming"]`, and that a terminal action is rejected by `build`.
4. **Job model + L1 buffering** (`src/app.rs`, new `src/job.rs`). Move the log buffer onto `Job`; add orphan buffering.
   → verify: unit test feeding `Output`, `Finished` and then `Started` ends with Done(0) and all lines in the buffer.
5. **Job dialog** (`src/ui/run_page.rs` → `src/ui/job_view.rs`). Present it as `adw::Dialog`; remove `Window::push` usage for jobs (L3).
   → verify: build; `state_changed` no longer replaces a stack containing job views (covered by construction; manual check in step 11).
6. **Terminal lane** (`src/ui/terminal_view.rs`, `src/terminal.rs`). Add `vte4 = "0.8"` (gtk4 ^0.9 / glib ^0.20, matches our lockfile). Add `validate::build_for_terminal`. Use the VTE `spawn_async` argv, with the `xdg-terminal-exec` fallback. Delete `spawn_terminal`/`open_in_terminal`.
   → verify: build; `grep -rn '"-lc"' src/` is empty; unit test for argv construction.
7. **State readers** (`src/system/state.rs`, new `src/system/services.rs`, `src/system/vpn.rs`):
   - parse server-services.nix (`vexos.server.<name>.enable = true`, tolerant of trailing comments; fixes L8 for features too);
   - run `vexos-vpn status --json` / `regions --json` off the main thread via `gio::Subprocess` (async, so the UI never blocks; serde_json is already a dependency);
   - rebuild-needed mtime check.

   → verify: unit tests on sample file contents and sample JSON.
8. **Pages** (`src/ui/pages/{overview,system,features,vpn,services,service_detail,storage,cache,tools,activity}.rs`). Replace `dashboard.rs` and `category_page.rs`, reusing `action_row`, `risk_pill` and `badge`. Add the switch-role dialog (`src/ui/switch_dialog.rs`). The generic `arg_dialog.rs` remains for simple parameter forms (hostname is inline, but e.g. Push to Attic… uses it).
   → verify: build, clippy clean.
9. **Window/sidebar** (`src/ui/window.rs`). New page list, activity header button, banner plumbing.
10. **Packaging** (`nix/package.nix`):
    - add `vte-gtk4` to `buildInputs`;
    - add `xdg-terminal-exec` and `just` to the wrapper PATH via `gappsWrapperArgs+=(--prefix PATH : ${lib.makeBinPath [ just xdg-terminal-exec ]})`. The GUI already runs `just --dump`; this pins it instead of trusting PATH.

    Check `flake.nix` devShell inputs and add `vte-gtk4` there as well. `Cargo.toml`: libadwaita feature `v1_5` → `v1_6` (for `AdwSpinner`, `AdwButtonRow`); nixpkgs 26.05 ships libadwaita 1.9.3, and crate 0.7.2 exposes `v1_6`.
    → verify: `nix build .#default`.
11. **Visual check** with `cage` + `grim` per CLAUDE.md, using `VEXPORTAL_VARIANT=vexos-desktop-amd` and `vexos-server-amd`. Screenshots go in the review doc.
12. **Docs**: update README "How it works"/"Privilege" to describe the terminal lane; update the catalog header comment.

## 5. Dependencies

| Dependency | Version | Verified how | Notes |
|---|---|---|---|
| `vte4` (crate) | 0.8.0 | crates.io API: requires `gtk4 ^0.9`, `glib ^0.20` (lockfile: gtk4 0.9.7) | 0.9/0.10 require gtk4 0.10, so pin 0.8 |
| `vte-gtk4` (nixpkgs) | 0.84.1 | nixos MCP | native lib for vte4 |
| `xdg-terminal-exec` (nixpkgs) | 0.14.3 | nixos MCP; already a runtime input of vexos-nix's `vexos-ai` | fallback launcher; `--dir=`, `--title=` options |
| `libadwaita` crate | 0.7.2, feature `v1_6` | crates.io feature list `v1_1..v1_7` | nixpkgs 26.05 libadwaita 1.9.3 |
| `vexos-vpn` CLI | vexos-nix `pkgs/vexos-vpn` | read source: `status --json`, `regions --json` work for users group | runtime only, optional |

**Context7:** the MCP server failed to connect this session (CONNECT_TIMEOUT). The APIs were instead verified against crates.io metadata, the GNOME/libadwaita docs, and the vte4-rs docs (sources below). Phase 2 should retry Context7 for `vte4` before writing the spawn code.

## 6. Configuration changes

- `nix/package.nix`: `vte-gtk4` buildInput; wrapper PATH prefix (step 10).
- `flake.nix` devShell: `vte-gtk4`.
- No D-Bus, polkit, or `nix/module.nix` changes. The daemon interface is unchanged, and `VEXOS_ASSUME_YES` behaviour is unchanged.

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Big-bang rewrite of the UI | Steps ordered so each builds/tests green; catalog + bug fixes (1–5) deliver value even before new pages. |
| `_menu` whitespace re-split corrupts args | `check_consistency` rule (c); all dispatcher params are slugs. |
| Calling public dispatchers couples us to `_menu` | The dispatchers are the documented public API (`just vpn up`). The fixture drift test catches renames of the `implements` targets. |
| Feature default mismatch (sunshine) | `default_on` in catalog mirrors `_feature-list`; covered by a test asserting the catalog's feature names equal the fixture's `_feature_names`. |
| VPN JSON shape changes | Deserialize with `#[serde(default)]` on every field; on parse failure the page shows "VPN status unavailable" and keeps controls working. |
| Terminal lane perceived as a privilege bypass | It runs as the user with an exec'd argv (no shell), identical in privilege to the user typing the command; `build_for_terminal` refuses daemon-lane actions. Documented in README. |
| VTE missing on a host | `nix/package.nix` links it, so it is always present in the closure. The `xdg-terminal-exec` fallback is kept for spawn errors. |
| mtime heuristic false positive (file touched without change) | Acceptable: banner offers a rebuild, which is harmless; documented. |
| Kill-switch "always" mode asks for a password via `vexos-vpn` | Resolved: `cmd_killswitch off` is `systemctl stop vexos-killswitch.service`; the password prompt is the polkit rule for unprivileged users. Run as root by the daemon, it does not prompt, so `kill-switch-off` stays in the daemon lane (`risk = destructive`, confirm). |

## 8. Build validation (Phase 3 commands — all safe per CLAUDE.md)

1. `nix develop -c cargo fmt --all -- --check`
2. `nix develop -c cargo check --workspace`
3. `nix develop -c cargo clippy --workspace --all-targets`
4. `nix develop -c cargo test --workspace` (the `catalog_matches_the_installed_justfile` skip is expected off-host)
5. `nix build .#default` (required: packaging changes)

No FORBIDDEN COMMANDS are used.

## 9. Out of scope / upstream asks (vexos-nix)

- `_vpn-login` could pass `--stdin` when `VEXPORTAL=1`, which would let VPN sign-in become a form with the secret on stdin. Until then it stays terminal.
- A machine-readable `feature list --json` / `service list --json` would replace VexPortal's file parsing.
- The prior-analysis items not listed in §1.2 (S1–S9, P1–P11, the rest of L/E) are untouched by this change.

## 10. Sources

1. vexos-nix `justfile` @ `f6159bb`, read directly (`_menu` L1790-1847, grouped menus L1849-1930, `switch` L117-430, `_feature-list` L1456-1490, `_service_catalog` L1709).
2. vexos-nix `pkgs/vexos-vpn` (status/regions `--json`, `login --stdin`, user-group permissions).
3. GNOME HIG — Switches: https://developer.gnome.org/hig/patterns/controls/switches.html
4. GNOME HIG — Boxed Lists (one control per row, max two; button rows; expander rows): https://developer.gnome.org/hig/patterns/containers/boxed-lists.html
5. Libadwaita 1.6 release notes (AdwSpinner, AdwButtonRow, AdwBottomSheet): https://blogs.gnome.org/alicem/2024/09/13/libadwaita-1-6/
6. vte4-rs docs — `Pty`/`Terminal` spawn API: https://world.pages.gitlab.gnome.org/Rust/vte4-rs/stable/latest/docs/vte4/struct.Pty.html and https://docs.rs/vte4
7. VTE `Terminal.spawn_with_fds_async`: https://gnome.pages.gitlab.gnome.org/vte/gtk4/method.Terminal.spawn_with_fds_async.html
8. xdg-terminal-exec (reference implementation of the proposed XDG Default Terminal Execution spec): https://github.com/Vladimir-csp/xdg-terminal-exec
9. Cockpit services page (state-first service list → detail page with actions; prior art for the Services page): https://docs.oracle.com/en/operating-systems/oracle-linux/cockpit/cockpit-services.html
10. crates.io API metadata for `vte4` 0.8.0 / 0.9.0 and `libadwaita` 0.7.2.
11. VexPortal `docs/ANALYSIS_BUGS.md` (L1, L3, L6, L7, L8).
