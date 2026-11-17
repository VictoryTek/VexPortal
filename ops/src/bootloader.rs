//! Migrating a UEFI host from systemd-boot to Limine, in two steps with a reboot between.
//!
//! The first step installs Limine next to the existing entry and puts it first. The
//! second removes the old entry and its files, and only runs once this boot demonstrably
//! came from the Limine entry, so there is always a bootloader that works.

use crate::fsutil;
use crate::nixedit::{self, NixValue};
use crate::rebuild::{boot_command, flake_base};
use crate::runner::{Cmd, Runner};
use crate::sidefile::{self, Changes};
use crate::system::{read_variant, refuse_legacy};
use crate::{activate, capture, fail, stream, Ctx};
use std::path::{Path, PathBuf};

/// Where the first step records the entries it found, for the second step to remove.
const OLD_ENTRIES_FILE: &str = ".vexos-bootloader-migration-old-entries";

/// The label NixOS's systemd-boot installer gives its own entry. Matched exactly, never
/// as a substring: `boot-discovery.nix` registers entries such as `NixOS/systemd-boot [tag]`
/// for other disks, which are the dual-boot entries this feature exists to keep.
const SYSTEMD_BOOT_LABEL: &str = "Linux Boot Manager";

const BOOTLOADER_SEED: &str =
    "# /etc/nixos/bootloader.nix\n# Written by VexPortal (Migrate to Limine).\n{\n}\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootEntry {
    /// Four hex digits, upper case.
    pub number: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootState {
    pub current: Option<String>,
    pub order: Vec<String>,
    pub entries: Vec<BootEntry>,
}

impl BootState {
    pub fn limine(&self) -> Option<&BootEntry> {
        self.entries
            .iter()
            .find(|e| e.label.to_ascii_lowercase().contains("limine"))
    }

    fn systemd_boot(&self) -> Vec<&BootEntry> {
        self.entries
            .iter()
            .filter(|e| e.label.eq_ignore_ascii_case(SYSTEMD_BOOT_LABEL))
            .collect()
    }

    fn has(&self, number: &str) -> bool {
        self.entries.iter().any(|e| e.number == number)
    }
}

fn hex4(text: &str) -> Option<String> {
    (text.len() == 4 && text.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| text.to_ascii_uppercase())
}

/// Parse plain `efibootmgr` output: `BootCurrent`, `BootOrder` and one line per entry.
pub fn parse_efibootmgr(text: &str) -> BootState {
    let mut state = BootState::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BootCurrent:") {
            state.current = hex4(rest.trim());
        } else if let Some(rest) = line.strip_prefix("BootOrder:") {
            state.order = rest.split(',').filter_map(|n| hex4(n.trim())).collect();
        } else if let Some(rest) = line.strip_prefix("Boot") {
            let Some(number) = rest.get(..4).and_then(hex4) else {
                continue;
            };
            let after = &rest[4..];
            let after = after.strip_prefix('*').unwrap_or(after);
            // `Boot0001* Label` follows the number with whitespace; a longer word such as
            // `BootNext` is not an entry.
            if !after.starts_with(char::is_whitespace) {
                continue;
            }
            // Verbose output adds the device path after a tab.
            let label = after.split('\t').next().unwrap_or("").trim().to_string();
            state.entries.push(BootEntry { number, label });
        }
    }
    state
}

async fn read_boot_state<R: Runner>(ctx: &Ctx<'_, R>) -> Result<BootState, i32> {
    let out = match ctx.runner.run(&Cmd::new("efibootmgr")).await {
        Ok(out) => out,
        Err(crate::runner::RunError::Cancelled) => {
            ctx.sink.error("Cancelled.");
            return Err(1);
        }
        Err(_) => {
            return Err(fail(
                ctx.sink,
                "efibootmgr was not found. Is this a UEFI system?",
            ))
        }
    };
    if !out.success() {
        return Err(fail(ctx.sink, format!("efibootmgr: {}", out.stderr.trim())));
    }
    Ok(parse_efibootmgr(&out.stdout))
}

fn bootloader_is_limine(nixos_dir: &Path) -> bool {
    ["bootloader.nix", "flake.nix"].iter().any(|f| {
        std::fs::read_to_string(nixos_dir.join(f))
            .ok()
            .and_then(|text| nixedit::get_str(&text, "vexos.bootloader"))
            .is_some_and(|v| v == "limine")
    })
}

pub async fn switch<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match switch_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn switch_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    if ctx
        .args
        .get("target")
        .map(String::as_str)
        .unwrap_or("limine")
        != "limine"
    {
        return Err(fail(
            ctx.sink,
            "only Limine is supported as a migration target",
        ));
    }
    if !ctx.nixos_dir.join("flake.nix").is_file() {
        return Err(fail(
            ctx.sink,
            "/etc/nixos/flake.nix not found. Run this on an installed vexos-nix host.",
        ));
    }
    if !ctx.root.join("sys/firmware/efi").is_dir() {
        return Err(fail(
            ctx.sink,
            "the Limine migration is only supported on UEFI systems",
        ));
    }
    if refuse_legacy(ctx) {
        return Err(1);
    }
    let Some(target) = read_variant(ctx.nixos_dir) else {
        return Err(fail(
            ctx.sink,
            "/etc/nixos/vexos-variant is missing, so the active flake target is unknown",
        ));
    };
    if target.starts_with("vexos-vanilla-") {
        return Err(fail(
            ctx.sink,
            "the vanilla role does not support vexos.bootloader: its configuration sets \
             boot.loader.systemd-boot directly, so this change would break evaluation",
        ));
    }
    if bootloader_is_limine(ctx.nixos_dir) {
        ctx.sink
            .info("Already configured for Limine. Nothing to do.");
        return Ok(());
    }

    let before = read_boot_state(ctx).await?;

    // Record the systemd-boot entries now. Once Limine's installer has run both entries
    // exist side by side and "which one is old" is no longer obvious.
    let old: Vec<String> = before
        .systemd_boot()
        .iter()
        .map(|e| e.number.clone())
        .collect();
    let record = ctx.nixos_dir.join(OLD_ENTRIES_FILE);
    let mut text = old.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    fsutil::write_atomic(&record, text.as_bytes(), 0o644).map_err(|e| {
        fail(
            ctx.sink,
            format!("could not write {}: {e}", record.display()),
        )
    })?;

    ctx.sink
        .info("Writing /etc/nixos/bootloader.nix for Limine…");
    let mut changes = Changes::default();
    sidefile::set_option(
        ctx.runner,
        &mut changes,
        &ctx.nixos_dir.join("bootloader.nix"),
        BOOTLOADER_SEED,
        "vexos.bootloader",
        &NixValue::str("limine"),
    )
    .await
    .map_err(|e| fail(ctx.sink, e))?;

    let base = match flake_base(ctx).await {
        Ok(base) => base,
        Err(code) => {
            changes.restore(ctx.sink).await;
            return Err(code);
        }
    };
    ctx.sink.info("Rebuilding and installing Limine…");
    match stream(ctx, &boot_command(&base, &target)).await {
        Ok(0) => {}
        Ok(code) | Err(code) => {
            // Nothing was installed, so the setting goes back with it.
            changes.restore(ctx.sink).await;
            return Err(code);
        }
    }
    activate::activate(ctx).await?;

    let after = read_boot_state(ctx).await?;
    let Some(limine) = after.limine() else {
        ctx.sink.error(
            "The configuration was applied, but no Limine entry was found in the firmware. The \
             old systemd-boot entry was not touched. Investigate before rebooting, and do not \
             finish the migration.",
        );
        return Err(1);
    };

    // Limine first, every other entry kept in its place.
    let mut order = vec![limine.number.clone()];
    order.extend(after.order.iter().filter(|n| **n != limine.number).cloned());
    let out = capture(ctx, &Cmd::new("efibootmgr").args(["-o", &order.join(",")])).await?;
    if !out.success() {
        return Err(fail(
            ctx.sink,
            format!("efibootmgr -o: {}", out.stderr.trim()),
        ));
    }
    ctx.sink.info(&format!(
        "✓ Boot order updated: Limine (Boot{}) is first.",
        limine.number
    ));
    ctx.sink.info(
        "Limine is installed. The old systemd-boot entry and its files are still in place as a \
         fallback. Reboot and check that Limine boots, then run Finish Limine Migration.",
    );
    activate::mark_reboot_required(ctx.root);
    Ok(())
}

/// The mount point of the EFI system partition: the vfat filesystem mounted at one of the
/// places NixOS puts it. Read from the mount table, not assumed to be `/boot`.
pub fn esp_mount(root: &Path) -> Option<PathBuf> {
    let mounts = std::fs::read_to_string(root.join("proc/mounts")).ok()?;
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (_device, point, fstype) = (fields.next()?, fields.next()?, fields.next()?);
        (fstype == "vfat" && ["/boot", "/boot/efi", "/efi"].contains(&point))
            .then(|| root.join(point.trim_start_matches('/')))
    })
}

pub async fn cleanup<R: Runner>(ctx: &Ctx<'_, R>) -> i32 {
    match cleanup_inner(ctx).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

async fn cleanup_inner<R: Runner>(ctx: &Ctx<'_, R>) -> Result<(), i32> {
    let state = read_boot_state(ctx).await?;
    let Some(limine) = state.limine() else {
        return Err(fail(
            ctx.sink,
            "no Limine entry found, so there is nothing to clean up",
        ));
    };
    if state.current.as_deref() != Some(limine.number.as_str()) {
        ctx.sink.error(&format!(
            "This session did not boot through the Limine entry (booted: {}, Limine: {}).",
            state.current.as_deref().unwrap_or("unknown"),
            limine.number
        ));
        return Err(fail(
            ctx.sink,
            "reboot, pick Limine, and run this again once you have confirmed it boots. The \
             systemd-boot fallback is not removed until that is proven",
        ));
    }
    ctx.sink.info(&format!(
        "Confirmed: this session booted through Limine (Boot{}).",
        limine.number
    ));

    let record = ctx.nixos_dir.join(OLD_ENTRIES_FILE);
    let recorded = std::fs::read_to_string(&record);
    let numbers: Vec<String> = match &recorded {
        // A recorded number is only a hint. Firmware reuses numbers, so an entry is removed
        // only if it still carries the systemd-boot label now; otherwise it belongs to
        // something else (another OS installed since) and is left alone.
        Ok(text) => text
            .lines()
            .filter_map(|l| hex4(l.trim()))
            .filter(|n| {
                state
                    .systemd_boot()
                    .iter()
                    .any(|e| e.number == *n && e.number != limine.number)
            })
            .collect(),
        Err(_) => {
            ctx.sink.info(
                "No record of the old entries was found; removing any entry labelled \
                 \"Linux Boot Manager\" instead.",
            );
            state
                .systemd_boot()
                .iter()
                .map(|e| e.number.clone())
                .collect()
        }
    };

    for number in &numbers {
        if !state.has(number) {
            continue;
        }
        ctx.sink
            .info(&format!("Removing the old systemd-boot entry Boot{number}"));
        let out = capture(ctx, &Cmd::new("efibootmgr").args(["-b", number, "-B"])).await?;
        if !out.success() {
            return Err(fail(
                ctx.sink,
                format!("efibootmgr -B: {}", out.stderr.trim()),
            ));
        }
    }
    if recorded.is_ok() {
        let _ = std::fs::remove_file(&record);
    }

    match esp_mount(ctx.root) {
        Some(esp) => {
            ctx.sink.info(&format!("Removing the old systemd-boot files from {}…", esp.display()));
            for leftover in [esp.join("EFI/systemd"), esp.join("loader")] {
                if leftover.exists() {
                    std::fs::remove_dir_all(&leftover).map_err(|e| {
                        fail(ctx.sink, format!("could not remove {}: {e}", leftover.display()))
                    })?;
                }
            }
        }
        None => ctx.sink.info(
            "The EFI system partition is not mounted at /boot, /boot/efi or /efi, so its old files \
             were left in place.",
        ),
    }
    ctx.sink.info("✓ Cleanup complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activate::tests::{point, system};
    use crate::runner::testing::{block_on, FakeRunner};
    use crate::runner::Output;
    use crate::testkit::Kit;

    const BOTH: &str = "BootCurrent: 0001\nTimeout: 1 seconds\nBootOrder: 0000,0002,0001\n\
Boot0000* Linux Boot Manager\nBoot0001* Limine\nBoot0002* NixOS/systemd-boot [other]\n";
    const ONLY_OLD: &str = "BootCurrent: 0000\nBootOrder: 0000\nBoot0000* Linux Boot Manager\n";
    const BOOTED_OLD: &str = "BootCurrent: 0000\nBootOrder: 0001,0000\n\
Boot0000* Linux Boot Manager\nBoot0001* Limine\n";

    fn out(text: &str) -> Output {
        Output {
            code: 0,
            stdout: text.into(),
            stderr: String::new(),
        }
    }

    #[test]
    fn efibootmgr_output_is_parsed() {
        let state = parse_efibootmgr(BOTH);
        assert_eq!(state.current.as_deref(), Some("0001"));
        assert_eq!(state.order, ["0000", "0002", "0001"]);
        assert_eq!(state.entries.len(), 3);
        assert_eq!(state.limine().unwrap().number, "0001");
        // Only the exact label is the old entry; the other disk's entry is not.
        let old: Vec<&str> = state
            .systemd_boot()
            .iter()
            .map(|e| e.number.as_str())
            .collect();
        assert_eq!(old, ["0000"]);
    }

    #[test]
    fn verbose_lines_lose_their_device_path_and_odd_lines_are_skipped() {
        let state = parse_efibootmgr(
            "BootNext: 0003\nBoot0003* Limine\tHD(1,GPT)/File(\\EFI\\limine.efi)\nBootFFFF* x\n",
        );
        assert_eq!(state.entries[0].label, "Limine");
        assert_eq!(state.entries[1].number, "FFFF");
        assert_eq!(state.entries.len(), 2);
    }

    #[test]
    fn the_esp_comes_from_the_mount_table() {
        let kit = Kit::new();
        std::fs::create_dir_all(kit.root.join("proc")).unwrap();
        std::fs::write(
            kit.root.join("proc/mounts"),
            "/dev/nvme0n1p2 / ext4 rw 0 0\n/dev/nvme0n1p1 /boot/efi vfat rw 0 0\n/dev/sdb1 /boot ext4 rw 0 0\n",
        )
        .unwrap();
        assert_eq!(esp_mount(kit.root.path()), Some(kit.root.join("boot/efi")));
        std::fs::write(
            kit.root.join("proc/mounts"),
            "/dev/sda1 /data vfat rw 0 0\n",
        )
        .unwrap();
        assert_eq!(esp_mount(kit.root.path()), None);
    }

    fn host(kit: &Kit) {
        kit.nixos_file("flake.nix", "{ }\n");
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        std::fs::create_dir_all(kit.root.join("sys/firmware/efi")).unwrap();
    }

    fn switch_runner(listing: &str, top: &Path) -> FakeRunner {
        FakeRunner::new()
            .on("efibootmgr", &[], out(listing))
            .ok("efibootmgr", &["-o"])
            .ok("git", &[])
            .ok("nix-instantiate", &["--parse"])
            .ok("nixos-rebuild", &["boot"])
            .ok(
                &top.join("bin/switch-to-configuration")
                    .display()
                    .to_string(),
                &["switch"],
            )
    }

    #[test]
    fn switching_records_old_entries_writes_the_setting_and_puts_limine_first() {
        let kit = Kit::new().with_args(&[("target", "limine")]);
        host(&kit);
        let old = system(&kit, "old", Some("display-manager.service"));
        let new = system(&kit, "new", Some("display-manager.service"));
        point(&kit, &old, &new);
        let runner = switch_runner(BOTH, &new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);

        let recorded = std::fs::read_to_string(kit.nixos.join(OLD_ENTRIES_FILE)).unwrap();
        assert_eq!(recorded, "0000\n");
        let setting = std::fs::read_to_string(kit.nixos.join("bootloader.nix")).unwrap();
        assert!(
            setting.contains("vexos.bootloader = \"limine\";"),
            "{setting}"
        );
        assert!(runner
            .lines()
            .contains(&"efibootmgr -o 0001,0000,0002".to_string()));
    }

    #[test]
    fn a_failed_build_takes_the_bootloader_setting_back() {
        let kit = Kit::new().with_args(&[("target", "limine")]);
        host(&kit);
        let runner = FakeRunner::new()
            .on("efibootmgr", &[], out(ONLY_OLD))
            .ok("git", &[])
            .ok("nix-instantiate", &["--parse"])
            .failing("nixos-rebuild", &["boot"], 1, "boom");
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 1);
        assert!(!kit.nixos.join("bootloader.nix").exists());
    }

    #[test]
    fn no_limine_entry_after_the_build_is_reported_and_leaves_the_old_one() {
        let kit = Kit::new().with_args(&[("target", "limine")]);
        host(&kit);
        let old = system(&kit, "old", None);
        let new = system(&kit, "new", None);
        point(&kit, &old, &new);
        let runner = switch_runner(ONLY_OLD, &new);
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("no Limine entry was found"));
        assert!(runner.lines().iter().all(|l| !l.contains(" -B")));
    }

    #[test]
    fn switch_refuses_bios_vanilla_legacy_and_the_already_migrated() {
        // BIOS
        let kit = Kit::new();
        kit.nixos_file("flake.nix", "{ }\n");
        kit.nixos_file("vexos-variant", "vexos-desktop-amd\n");
        let runner = FakeRunner::new();
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("UEFI"));

        // vanilla
        let kit = Kit::new();
        host(&kit);
        kit.nixos_file("vexos-variant", "vexos-vanilla-amd\n");
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 1);
        assert!(kit.sink.text().contains("vanilla"));

        // already limine
        let kit = Kit::new();
        host(&kit);
        kit.nixos_file("bootloader.nix", "{\n  vexos.bootloader = \"limine\";\n}\n");
        assert_eq!(block_on(switch(&kit.ctx(&runner))), 0);
        assert!(kit.sink.text().contains("Already configured"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn cleanup_refuses_until_this_boot_came_from_limine() {
        let kit = Kit::new();
        let runner = FakeRunner::new()
            .on("efibootmgr", &[], out(BOOTED_OLD))
            .ok("efibootmgr", &["-b"]);
        assert_eq!(block_on(cleanup(&kit.ctx(&runner))), 1);
        assert!(kit
            .sink
            .text()
            .contains("did not boot through the Limine entry"));
        assert!(runner.lines().iter().all(|l| !l.contains("-B")));
    }

    #[test]
    fn cleanup_removes_recorded_entries_and_the_esp_leftovers() {
        let kit = Kit::new();
        kit.nixos_file(OLD_ENTRIES_FILE, "0000\n");
        std::fs::create_dir_all(kit.root.join("proc")).unwrap();
        std::fs::write(kit.root.join("proc/mounts"), "/dev/x /boot vfat rw 0 0\n").unwrap();
        std::fs::create_dir_all(kit.root.join("boot/EFI/systemd")).unwrap();
        std::fs::create_dir_all(kit.root.join("boot/EFI/limine")).unwrap();
        std::fs::create_dir_all(kit.root.join("boot/loader")).unwrap();
        let runner = FakeRunner::new()
            .on("efibootmgr", &[], out(BOTH))
            .ok("efibootmgr", &["-b"]);
        assert_eq!(block_on(cleanup(&kit.ctx(&runner))), 0);
        assert!(runner
            .lines()
            .contains(&"efibootmgr -b 0000 -B".to_string()));
        assert!(!kit.root.join("boot/EFI/systemd").exists());
        assert!(!kit.root.join("boot/loader").exists());
        assert!(
            kit.root.join("boot/EFI/limine").exists(),
            "Limine's own files stay"
        );
        assert!(!kit.nixos.join(OLD_ENTRIES_FILE).exists());
    }

    #[test]
    fn cleanup_never_removes_limines_own_entry_or_a_malformed_number() {
        let kit = Kit::new();
        kit.nixos_file(OLD_ENTRIES_FILE, "0001\n../../x\n00 00\n");
        let runner = FakeRunner::new()
            .on("efibootmgr", &[], out(BOTH))
            .ok("efibootmgr", &["-b"]);
        assert_eq!(block_on(cleanup(&kit.ctx(&runner))), 0);
        assert!(runner.lines().iter().all(|l| !l.contains("-B")));
    }

    #[test]
    fn a_recorded_number_that_now_belongs_to_another_entry_is_left_alone() {
        let kit = Kit::new();
        // 0002 was recorded as systemd-boot, but is now another OS's entry.
        kit.nixos_file(
            OLD_ENTRIES_FILE,
            "0002
",
        );
        let runner = FakeRunner::new()
            .on("efibootmgr", &[], out(BOTH))
            .ok("efibootmgr", &["-b"]);
        assert_eq!(block_on(cleanup(&kit.ctx(&runner))), 0);
        assert!(runner.lines().iter().all(|l| !l.contains("-B")));
    }

    #[test]
    fn without_a_record_only_the_exact_systemd_boot_label_is_removed() {
        let kit = Kit::new();
        let runner = FakeRunner::new()
            .on("efibootmgr", &[], out(BOTH))
            .ok("efibootmgr", &["-b"]);
        assert_eq!(block_on(cleanup(&kit.ctx(&runner))), 0);
        let removed: Vec<String> = runner
            .lines()
            .into_iter()
            .filter(|l| l.contains("-B"))
            .collect();
        assert_eq!(removed, ["efibootmgr -b 0000 -B"]);
    }
}
