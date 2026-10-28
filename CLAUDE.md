# CLAUDE.md
**VexPortal** — Desktop GUI application + privileged system daemon (Nix-flake-packaged, NixOS-targeted). Languages: Rust (edition 2021). Frameworks: GTK4 (gtk4-rs 0.9), libadwaita (0.7), glib/gio, zbus 5 (D-Bus), tokio. Package manager(s): Cargo (crates.io) for Rust dependencies; Nix flake for native/system dependencies (gtk4, libadwaita, glib, dbus) and distribution.

## Commands

Build:
- `nix develop -c cargo build --workspace`
- `nix build .#default`

Test:
- `nix develop -c cargo fmt --all -- --check`
- `nix develop -c cargo check --workspace`
- `nix develop -c cargo clippy --workspace --all-targets`
- `nix develop -c cargo test --workspace`
- `nix flake check`
- `nix build .#default` when the change touches packaging, `nix/package.nix`, `nix/module.nix`, `data/`, or affects what ships in the final closure

Preflight (final gate): `bash scripts/preflight.sh`
Fast verify (also run by the Stop hook): `bash scripts/preflight.sh`

## Repository

- Key directories:
  - `src/` — the `vexportal` GUI binary: `app.rs`, `dbus_client.rs`, `just.rs`, `system/` (variant + state), `ui/` (GTK4/libadwaita widgets: dashboard, category pages, run page, arg dialogs)
  - `daemon/src/` — the privileged `vexportal-daemon`: `interface.rs` (D-Bus surface), `auth.rs` (polkit), `executor.rs`, `cancel.rs`, `audit.rs` (journald), `lifecycle.rs`, `config.rs`
  - `catalog/src/` — shared `vexportal-catalog` library: `catalog.toml` (recipe metadata: titles, descriptions, icons, categories, roles, risk, argument widgets), `lib.rs`, `drift.rs`, `format.rs`, `validate.rs`
  - `catalog/tests/` — the drift tests (against a fixture and against a real installed justfile)
  - `data/` — desktop entry, polkit policy, D-Bus system service/policy files, AppStream metainfo, icons, GResource XML, stylesheet
  - `nix/` — `package.nix` (`rustPlatform.buildRustPackage`), `module.nix` (NixOS module wiring the app, daemon, polkit actions, and D-Bus policy)
  - `docs/` — project documentation
- Architecture: **Privilege-separated client/daemon over D-Bus.** The GTK4/libadwaita GUI (`vexportal`) never runs as root; it reads role/variant state locally and calls the system daemon (`io.github.vexportal.Daemon`, running as root) over `zbus` for anything that executes a recipe. The daemon revalidates every recipe and argument against the compiled-in `vexportal-catalog` before `exec`-ing `just` directly (never through a shell), gates execution through polkit at one of three risk tiers (`safe` / `medium` / `destructive`), and audits to journald. The catalog crate is the single source of truth shared by both binaries, cross-checked at test time against the justfile actually installed on a built host (drift detection).
- CI environment: none configured — `.github/` contains no workflows. `scripts/preflight.sh` is the local gate.
- OS requirements: Linux-only. The GUI links GTK4/libadwaita/glib via `pkg-config`; the daemon talks to the system D-Bus bus and polkit, and reads `/etc/nixos/vexos-variant`. None of this is portable to non-Linux hosts.
- Build layout constraints: This is a 3-member Cargo workspace (`.` = `vexportal` GUI, `catalog` = `vexportal-catalog` shared lib, `daemon` = `vexportal-daemon`). Cargo invocations should target `--workspace` to cover all three members. Building requires GTK4/libadwaita dev files on `PKG_CONFIG_PATH`, which only `nix develop` (not `nix shell`, not a bare host toolchain) reliably provides.
- Large disk side-effects: `nix build .#default` and `nix develop` populate the local Nix store on first use (toolchain + gtk4/libadwaita closure); subsequent runs reuse cached store paths. Not destructive, just a one-time fetch/build cost.
- Other constraints:
  - `catalog/tests/drift_against_justfile.rs` compares the compiled-in catalog against a real `/etc/nixos/justfile` using a real `just` binary. Neither exists in a sandboxed build or a plain dev checkout, so the test detects this and self-skips with a message — a skip here is expected, not a failure, and must not be reported as one.
  - The daemon (`vexportal-daemon`) is D-Bus-activated, runs as root, and is gated by polkit; it cannot be meaningfully exercised by just running the binary outside a real system bus + polkit + systemd-unit context.
- Special constraints:
  - Treat a skipped `catalog_matches_the_installed_justfile` drift test (missing `/etc/nixos/justfile`) as expected off-host, not a failure.
  - Never propose changes that let the GUI construct a shell command line or bypass the daemon's catalog-validated argv path — this is a deliberate security boundary (see README "Privilege" section), not an oversight.
  - Secrets (e.g. the RDP password) travel to the daemon's child process via stdin, never argv — preserve this if touching `executor.rs`.

### Project-specific rules

- GUI verification: GNOME Shell refuses `org.gnome.Shell.Screenshot.Screenshot` from an ordinary process, so a running VexPortal window cannot be screenshotted directly on this desktop. Confirming a UI change renders requires a nested wlroots compositor, e.g. `nix shell nixpkgs#cage nixpkgs#grim --command bash -c 'cage -- ./result/bin/vexportal & sleep 5; WAYLAND_DISPLAY=wayland-1 grim shot.png'`. `VEXPORTAL_VARIANT=<role>` (e.g. `vexos-server-amd`) can be set to preview another role's page layout without rebuilding or changing the host's actual role.
- `nix shell` itself is not forbidden — it is the documented way to pull in one-off tools (e.g. `nix shell nixpkgs#cage nixpkgs#grim` for GUI screenshot verification). Only using it as a substitute for `nix develop` to build/test this workspace is forbidden.

## Forbidden commands

Never run these. They are also blocked in `.claude/settings.json` where possible.

### Universal (all repositories)

- `git add` (any form)
- `git commit` (any form, including `--amend`)
- `git push` (any form, including `--force`)
- `git stash` (any form, including `pop`, `apply`, `drop`)
- Any other git command that stages, commits, pushes, or stashes changes

What gets staged, committed, pushed, or stashed is the user's decision; the commit message at the end of every change is what the user runs. Read-only git (`status`, `log`, `diff`, `show`) is allowed.

### Project-specific

- `cargo build` / `cargo test` / `cargo check` (bare, invoked directly without the flake devShell) — reason: this workspace links against GTK4, libadwaita, and glib via `pkg-config`. The project's own `flake.nix` devShell is what puts these on `PKG_CONFIG_PATH`; whatever `cargo`/`rustc` happens to resolve on a bare `PATH` is not guaranteed to see them (and on this project's target OS, NixOS, an unwrapped PATH toolchain routinely fails outright). Use instead: `nix develop -c cargo build --workspace` / `nix develop -c cargo test --workspace` / `nix develop -c cargo check --workspace`
- `nix shell nixpkgs#cargo -c cargo build` (or any `cargo` build/test invoked through `nix shell` rather than `nix develop`) — reason: `nix shell` does not export `PKG_CONFIG_PATH`, so anything linking gtk4/libadwaita fails to find them even though `cargo` itself runs. Use instead: `nix develop -c cargo build --workspace`
- `nixos-rebuild switch` / `nixos-rebuild boot` — reason: applies a configuration to the live host; applying is user-initiated. Use instead: `nix build .#default` or `nixos-rebuild dry-build`

## How to work

### Task size

State the tier in one line before starting.

- **Tier 1 — ask or explore** (questions, reading code, anything that changes no files): just answer. No commit message.
- **Tier 2 — standard change** (bug fix, small feature, config or docs change, tests; a few files, no new dependency, no change to a public interface, schema, or build/CI config): research → brief plan in chat (goal, assumptions, docs consulted, steps with how each is verified) → implement → verify → preflight → commit message.
- **Tier 3 — major change** (new dependency; many files or modules; public interface, schema, or migration; security-sensitive code; build, CI, or deploy config; hard to reverse): in plan mode: research → full plan (including docs consulted) → **wait for approval** (the user approving the plan) → implement → verify → independent review → preflight → commit message.

If a task fits two tiers, use the larger. If a Tier 2 task grows, stop, say so, and continue as Tier 3. The user can ask for more or less process ("just do it", "full process"); verification, forbidden commands, and the commit message still apply.

Plan mode: for Tier 3, and before any new feature or design decision with real tradeoffs, enter plan mode yourself if you can; otherwise ask the user to switch (Shift+Tab) before planning. With the `opusplan` model setting, planning then runs on Opus and implementation on Sonnet.

### Principles

1. **Think before coding.** State your assumptions. If a request has several valid readings, present them or ask; never pick one silently. If a simpler approach exists, say so.
2. **Simplicity first.** Write the minimum code that solves the problem: no extra features, no abstractions for single-use code, no unrequested configurability, no handling for cases that cannot happen. If 200 lines could be 50, rewrite it.
3. **Surgical changes.** Every changed line must trace to the request. Do not refactor or tidy adjacent code; match the existing style. Remove only the imports, variables, and functions your change orphaned. Mention unrelated dead code instead of deleting it.
4. **Goal-driven.** Turn the task into a check you can run before you start: bug fix → a failing test first, then make it pass; new behaviour → tests for it; refactor → tests pass before and after. For multi-step work, list the steps with "→ verify: …" for each.

### Done means verified

- Do not call work finished until the build and tests have actually run and you have reported the real output. Never write "passes" for something you did not run.
- After a scripted or multi-file edit, confirm it applied (`git diff`) before running checks. Formatters can change lines between your edit and the check.
- Final gate: run the preflight command; the exit code must be 0. If none exists, run the build and test commands instead, say so, and offer to create a preflight script (do not create it unasked). A failure means the work is not finished.
- A Stop hook (`.claude/hooks/verify.sh`) re-runs the fast verify command when files have changed and blocks finishing on failure. Treat its report as a failed verification: fix it and re-run.
- Mechanical bugs in tooling or scripts that block a check (a shell error, a typo, a broken path) are fixed without asking, reported in one line, and listed in the commit message. Anything that changes behaviour still needs the user.
- If your first attempt at a bug fix fails, stop and re-plan before the next attempt (plan mode for Tier 3, a revised plan in chat for Tier 2). Do not stack more edits on an approach that did not work.
- Do not create spec, plan, or review files in the repository unless asked.

### Review findings

- Tier 3: before preflight, have the `reviewer` subagent review the diff. Give it the approved plan and the changed files, not your conclusions. If it is unavailable, review `git diff` yourself and say it was a self-review.
- Fix every in-scope finding, CRITICAL or RECOMMENDED, without asking. Then re-run the checks and the review. Repeat until the review has nothing left that needs a code change.
- Ask the user before fixing a finding only if the fix would change behaviour the user specified, add or upgrade a dependency, touch state outside the repository, or go beyond the task's scope.
- After 3 fix cycles that still fail (a failed review or preflight, fixed, failed again), STOP and report everything. Do not loop silently.
- Done means nothing in scope is left to fix. Do not end with "not implemented", "known issues", or "things I didn't touch" lists. Anything genuinely out of scope gets one line.

### Outside the repository

- Never change state outside the repository without asking: containers, volumes, database roles or passwords, images, installed packages, system or user configuration. Reading it is fine.
- If the harness or a permission rule denies a command, do not work around it with another command. Say what was denied and ask.

### Do not guess state

Before saying what is committed, pushed, or up to date, check: `git status`; `git log --oneline origin/$(git branch --show-current)..HEAD` (empty output means fully pushed); `stat <lockfile>`. Never tell the user to push, commit, or update without checking first.

### Research before changing

For Tier 2 and Tier 3, research comes before the plan, and the plan shows what you found.

1. **Read the code first.** Read the files you will change and the nearest existing code that does something similar. Reuse its patterns and helpers.
2. **Check current docs for every external library, framework, CLI, or API the change uses or touches, including ones already in the lock file.** Look up the version in use (the manifest or lock file says which). Use the Context7 MCP if available (resolve the library, then query its docs); otherwise the official documentation. Do not rely on memory for signatures, options, flags, or default behaviour. When a failure involves a library, search its docs and issue tracker for the error before choosing a fix. You may hand lookups to the `docs` subagent (if present); it returns quoted excerpts with source and version. Check the excerpt supports its answer before relying on it.
3. **Tier 3 only:** for any dependency being added or upgraded, also read its release notes or changelog and check for deprecations and known advisories. When choosing between libraries, compare at least two.
4. **Show it.** The plan has a line `Docs consulted:` naming each source and what it settled. Write `Docs consulted: none needed — <reason>` only when the change uses no external library or API (pure internal logic, comments, docs, formatting). If a lookup fails or the docs contradict the code, say so and ask; do not guess.

### Communication

- Lead with the answer or result. Use short sentences and plain words.
- Include only what the user needs: the result, what changed, what they must decide, and real check output. Do not restate the request, narrate your steps, or summarize what you just showed.
- Prefer plain wording to jargon and abbreviations. If a technical term is needed, explain it once.
- Use a list only for three or more parallel items. No headings in short replies.
- This does not shorten required content: the plan, the `Docs consulted:` line, failure output, and the commit message stay complete.

### Commit message (final step of every change)

Every change that modifies files ends with a commit message, however small (a one-line typo fix included). It is the last thing in your reply. You never run `git commit`; the user does.

The user commits all changed files at once, so give exactly **one** commit message covering every uncommitted change in the working tree, including work from earlier tasks that has not been committed yet. Never split the work into several messages.

Format, ready to paste into `git commit`:

```
<type>(<scope>): <description — MAX 72 characters total>

<PARAGRAPH EXPLAINING WHAT CHANGED AND WHY>

Modified Files:
- path/to/file1
- path/to/file2
- path/to/file3

✔ Build successful
✔ Tests passed
✔ Review approved
✔ Preflight passed
```

Valid types: `feat`, `fix`, `chore`, `refactor`, `docs`, `test`, `perf`. Example first line: `fix(network): disable swap on ZFS server roles`

Inside the message: no "Commit Message" heading, no "Edited" summary, no diff statistics, nothing outside the template. List new files under Modified Files too.

Checkmark lines appear only for checks that actually ran and passed on the final code in this session; never include a line for a skipped check. `✔ Review approved` means the Tier 3 review returned PASS (write `✔ Review approved (self-review)` if no independent reviewer was used). Omit the Preflight line if no preflight exists.

After the message, if every check that ran passed, add one line:
> All checks passed. Code is ready to push to GitHub.
