//! Reading and editing the flat `/etc/nixos` side files.
//!
//! `features.nix`, `server-services.nix`, `local.nix` and the rest are attribute sets
//! with one `vexos.<option> = <literal>;` per line. The `just` recipes edit them with
//! `sed`; this does the same job on the same line shape, so a file edited here still
//! reads cleanly with `just` and the other way round.
//!
//! The rules are the ones the recipes follow:
//! - an uncommented line for the option is rewritten in place;
//! - failing that, the first commented template line is uncommented in place;
//! - failing that, a new line goes in before the file's closing brace.
//!
//! Anything after the `;` on a rewritten line (a trailing comment) is kept.
//!
//! Values are typed ([`NixValue`]) and rendered here, never passed in as Nix text. A
//! string with a quote, a `${` or a `;` in it is escaped, so an answer typed into a
//! form can never become code in a file the next rebuild evaluates as root.

use crate::fsutil;
use crate::runner::{Cmd, RunError, Runner};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("`{0}` is not a valid option name")]
    BadOption(String),
    #[error("the value for `{0}` has a control character in it")]
    BadValue(String),
    #[error("the file has no closing `}}` line to add the option before")]
    NoClosingBrace,
    #[error("`{0}` is set across several lines; edit it by hand")]
    MultiLine(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("could not check the edited file: {0}")]
    Check(#[from] RunError),
    #[error("the edit would leave a file Nix cannot parse, so nothing was written:\n{0}")]
    Invalid(String),
    #[error("the file kept changing while it was being edited; try again")]
    Busy,
}

/// A value to write: the few shapes the side files hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NixValue {
    Bool(bool),
    Int(i64),
    Str(String),
    /// `[ "a" "b" ]`
    StrList(Vec<String>),
}

fn quote(text: &str, opt: &str) -> Result<String, EditError> {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // `${` starts an antiquotation inside a Nix string, which can run any
            // expression, such as reading a file.
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            c if c.is_control() => return Err(EditError::BadValue(opt.to_string())),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

impl NixValue {
    pub fn str(text: impl Into<String>) -> Self {
        Self::Str(text.into())
    }

    /// The Nix source for this value. `opt` is only for the error message.
    pub fn render(&self, opt: &str) -> Result<String, EditError> {
        Ok(match self {
            Self::Bool(b) => b.to_string(),
            Self::Int(n) => n.to_string(),
            Self::Str(s) => quote(s, opt)?,
            Self::StrList(items) => {
                let mut out = String::from("[ ");
                for item in items {
                    out.push_str(&quote(item, opt)?);
                    out.push(' ');
                }
                out.push(']');
                out
            }
        })
    }
}

/// Option names are dotted identifiers. Refusing anything else keeps a stray character
/// from ever reaching a file the next rebuild evaluates.
fn check_option(opt: &str) -> Result<(), EditError> {
    let ok = !opt.is_empty()
        && opt
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(EditError::BadOption(opt.to_string()))
    }
}

/// The parts of a line that assigns `opt`: its indentation and what follows the `=`.
fn assignment<'a>(line: &'a str, opt: &str, commented: bool) -> Option<(&'a str, &'a str)> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let body = if commented {
        trimmed.strip_prefix('#')?.trim_start()
    } else {
        trimmed
    };
    let after_name = body.strip_prefix(opt)?.trim_start();
    let after_eq = after_name.strip_prefix('=')?;
    // `==` is a comparison, not an assignment.
    if after_eq.starts_with('=') {
        return None;
    }
    Some((indent, after_eq))
}

/// Where the value ends: the first `;` that is not inside a double-quoted string.
fn value_end(after_eq: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in after_eq.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            ';' if !in_string => return Some(i),
            _ => {}
        }
    }
    None
}

/// The value of the first uncommented assignment of `opt`, trimmed.
fn value_of<'a>(text: &'a str, opt: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (_, after_eq) = assignment(line, opt, false)?;
        let end = value_end(after_eq)?;
        Some(after_eq[..end].trim())
    })
}

/// Is there an uncommented line setting `opt` to exactly `value`?
pub fn is_set(text: &str, opt: &str, value: &NixValue) -> bool {
    match value.render(opt) {
        Ok(rendered) => value_of(text, opt) == Some(rendered.as_str()),
        Err(_) => false,
    }
}

/// The boolean in an uncommented `opt = true;` or `opt = false;` line, if there is one.
pub fn get_bool(text: &str, opt: &str) -> Option<bool> {
    match value_of(text, opt)? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Undo the escapes [`quote`] writes.
fn unquote(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The string in an uncommented `opt = "...";` line, if there is one.
pub fn get_str(text: &str, opt: &str) -> Option<String> {
    let value = value_of(text, opt)?;
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    Some(unquote(inner))
}

/// Split into lines, each keeping its own line ending.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A line without its ending, and the ending.
fn body_and_ending(line: &str) -> (&str, &str) {
    match line.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (line, ""),
    }
}

/// Set `opt` to `value`.
pub fn set(text: &str, opt: &str, value: &NixValue) -> Result<String, EditError> {
    check_option(opt)?;
    let value = value.render(opt)?;
    let lines = split_lines(text);

    // An uncommented line is rewritten in place. Every such line is, so an option can
    // never end up set twice.
    let mut rewritten = false;
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);
    for line in &lines {
        let (body, ending) = body_and_ending(line);
        if let Some((indent, after_eq)) = assignment(body, opt, false) {
            let end = value_end(after_eq).ok_or_else(|| EditError::MultiLine(opt.to_string()))?;
            let tail = &after_eq[end + 1..];
            out.push(format!("{indent}{opt} = {value};{tail}{ending}"));
            rewritten = true;
        } else {
            out.push((*line).to_string());
        }
    }
    if rewritten {
        return Ok(out.concat());
    }

    // Only a commented template line: uncomment the first one.
    for (i, line) in lines.iter().enumerate() {
        let (body, ending) = body_and_ending(line);
        if let Some((indent, after_eq)) = assignment(body, opt, true) {
            let Some(end) = value_end(after_eq) else {
                continue;
            };
            let tail = &after_eq[end + 1..];
            let mut out: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
            out[i] = format!("{indent}{opt} = {value};{tail}{ending}");
            return Ok(out.concat());
        }
    }

    // Neither: add the option before the closing brace.
    let close = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('}'))
        .ok_or(EditError::NoClosingBrace)?;
    let new_line = format!("  {opt} = {value};\n");
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);
    for (i, line) in lines.iter().enumerate() {
        if i == close {
            out.push(new_line.clone());
        }
        out.push((*line).to_string());
    }
    Ok(out.concat())
}

/// Turn an uncommented `opt = true;` into `false`, keeping the line's spacing and any
/// trailing comment. `None` if no line had it enabled.
pub fn disable(text: &str, opt: &str) -> Result<Option<String>, EditError> {
    check_option(opt)?;
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in split_lines(text) {
        let (body, ending) = body_and_ending(line);
        if let Some((indent, after_eq)) = assignment(body, opt, false) {
            if let Some(end) = value_end(after_eq) {
                let value = &after_eq[..end];
                if value.trim() == "true" {
                    // Everything between `=` and `;` except the word itself stays put.
                    let lead = &value[..value.len() - value.trim_start().len()];
                    let trail = &value[value.trim_end().len()..];
                    let name_and_eq = &body[indent.len()..body.len() - after_eq.len()];
                    out.push_str(&format!(
                        "{indent}{name_and_eq}{lead}false{trail}{}{ending}",
                        &after_eq[end..]
                    ));
                    changed = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    Ok(changed.then_some(out))
}

/// Whether the file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    Changed,
    Unchanged,
}

/// How many times an edit starts over because something outside this process changed
/// the file underneath it.
const ATTEMPTS: usize = 3;

/// One lock per file being edited, so two operations in this process take turns.
fn lock_for(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.entry(path.to_path_buf()).or_default().clone()
}

/// Apply `edit` to the file at `path`, check the result still parses, and replace the
/// file atomically, leaving the old content in `path.bak`.
///
/// `edit` returns `None` for "nothing to change", which leaves the file untouched.
///
/// Two operations in this process editing one file take turns, held across the whole
/// read, check and write; otherwise each would read the old text and the later write
/// would drop the earlier change. Something outside the process (`just`, a person with
/// an editor) cannot be locked out, so just before writing the file is read again, and
/// if it is no longer what the edit started from, the edit is redone on the new content.
pub async fn edit_file<R: Runner>(
    runner: &R,
    path: &Path,
    mut edit: impl FnMut(&str) -> Result<Option<String>, EditError>,
) -> Result<Edited, EditError> {
    let lock = lock_for(path);
    let _turn = lock.lock().await;

    for _ in 0..ATTEMPTS {
        let original = std::fs::read_to_string(path)?;
        let Some(updated) = edit(&original)? else {
            return Ok(Edited::Unchanged);
        };
        if updated == original {
            return Ok(Edited::Unchanged);
        }

        // A file that does not parse would break the next rebuild, so the check comes
        // first.
        let parse = runner
            .run(
                &Cmd::new("nix-instantiate")
                    .args(["--parse", "-"])
                    .stdin(updated.clone().into_bytes()),
            )
            .await?;
        if !parse.success() {
            return Err(EditError::Invalid(parse.stderr.trim().to_string()));
        }

        if std::fs::read_to_string(path)? != original {
            continue;
        }
        let mode = fsutil::mode_of(path)?;
        fsutil::backup(path)?;
        fsutil::write_atomic(path, updated.as_bytes(), mode)?;
        return Ok(Edited::Changed);
    }
    Err(EditError::Busy)
}

/// Under the file's lock, replace its content with `replacement` (or remove it, for
/// `None`) if it still holds exactly `expected`. `Ok(false)` means it has changed and
/// was left alone. Edits take the same lock, so nothing can slip in between the check
/// and the write.
pub async fn replace_if_unchanged(
    path: &Path,
    expected: &str,
    replacement: Option<&str>,
) -> std::io::Result<bool> {
    let lock = lock_for(path);
    let _turn = lock.lock().await;
    if std::fs::read_to_string(path).ok().as_deref() != Some(expected) {
        return Ok(false);
    }
    match replacement {
        Some(text) => fsutil::write_atomic(path, text.as_bytes(), fsutil::mode_of(path)?)?,
        None => std::fs::remove_file(path)?,
    }
    Ok(true)
}

/// [`set`] on a file.
pub async fn set_in_file<R: Runner>(
    runner: &R,
    path: &Path,
    opt: &str,
    value: &NixValue,
) -> Result<Edited, EditError> {
    edit_file(runner, path, |text| set(text, opt, value).map(Some)).await
}

/// [`disable`] on a file.
pub async fn disable_in_file<R: Runner>(
    runner: &R,
    path: &Path,
    opt: &str,
) -> Result<Edited, EditError> {
    edit_file(runner, path, |text| disable(text, opt)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testing::TempDir;
    use crate::runner::testing::{block_on, FakeRunner};

    const FILE: &str = "{ ... }:\n{\n  vexos.features.gaming.enable = true;\n  # vexos.features.ai.enable = false;\n  vexos.hostname = \"box\";  # the name\n}\n";

    fn s(text: &str) -> NixValue {
        NixValue::str(text)
    }

    #[test]
    fn an_uncommented_line_is_rewritten_in_place_and_keeps_its_comment() {
        let out = set(FILE, "vexos.hostname", &s("new")).unwrap();
        assert!(
            out.contains("  vexos.hostname = \"new\";  # the name\n"),
            "{out}"
        );
        assert_eq!(out.matches("vexos.hostname").count(), 1);
    }

    #[test]
    fn a_commented_template_line_is_uncommented_in_place() {
        let out = set(FILE, "vexos.features.ai.enable", &NixValue::Bool(true)).unwrap();
        assert!(
            out.contains("  vexos.features.ai.enable = true;\n"),
            "{out}"
        );
        assert!(!out.contains("# vexos.features.ai"));
        // Neighbouring lines are untouched.
        assert!(out.contains("vexos.features.gaming.enable = true;"));
    }

    #[test]
    fn a_missing_option_is_added_before_the_closing_brace() {
        let out = set(FILE, "vexos.features.vpn.enable", &NixValue::Bool(true)).unwrap();
        assert!(
            out.ends_with(
                "  vexos.hostname = \"box\";  # the name\n  vexos.features.vpn.enable = true;\n}\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn the_closing_brace_need_not_be_the_last_line() {
        let text = "{\n  vexos.a = 1;\n}\n# trailing note\n";
        let out = set(text, "vexos.b", &NixValue::Int(2)).unwrap();
        assert_eq!(
            out,
            "{\n  vexos.a = 1;\n  vexos.b = 2;\n}\n# trailing note\n"
        );
    }

    #[test]
    fn a_file_without_a_closing_brace_is_refused() {
        assert!(matches!(
            set("# nothing here\n", "vexos.a", &NixValue::Int(1)),
            Err(EditError::NoClosingBrace)
        ));
    }

    #[test]
    fn a_longer_option_name_is_not_mistaken_for_the_prefix() {
        let text = "{\n  vexos.server.arr.enable = true;\n}\n";
        let out = set(text, "vexos.server.arr", &NixValue::Int(1)).unwrap();
        // Not a match, so it was added rather than rewriting the longer name.
        assert!(out.contains("vexos.server.arr.enable = true;"));
        assert!(out.contains("vexos.server.arr = 1;"));
    }

    #[test]
    fn a_semicolon_inside_a_string_does_not_end_the_value() {
        let text = "{\n  vexos.motd = \"a;b\";  # keep\n}\n";
        let out = set(text, "vexos.motd", &s("c")).unwrap();
        assert_eq!(out, "{\n  vexos.motd = \"c\";  # keep\n}\n");
        assert_eq!(get_str(text, "vexos.motd").as_deref(), Some("a;b"));
    }

    #[test]
    fn an_option_set_across_lines_is_refused_rather_than_ignored() {
        let text = "{\n  vexos.list = [\n    \"a\"\n  ];\n}\n";
        assert!(matches!(
            set(text, "vexos.list", &NixValue::StrList(vec![])),
            Err(EditError::MultiLine(_))
        ));
    }

    #[test]
    fn option_names_are_checked() {
        assert!(matches!(
            set(FILE, "vexos.a; evil", &NixValue::Int(1)),
            Err(EditError::BadOption(_))
        ));
        assert!(matches!(
            set(FILE, "", &NixValue::Int(1)),
            Err(EditError::BadOption(_))
        ));
    }

    #[test]
    fn text_can_never_become_nix_code() {
        // Each of these would still parse if written raw, which is why the parse check
        // alone is not enough.
        for hostile in [
            "x\"; users.users.root.initialHashedPassword = \"",
            "${builtins.readFile /etc/shadow}",
            "back\\slash\"",
            "a;b",
            "$notanti",
        ] {
            let out = set(FILE, "vexos.hostname", &s(hostile)).unwrap();
            // Still exactly one assignment, and it reads back as the same text.
            assert_eq!(out.matches("vexos.hostname").count(), 1, "{hostile}: {out}");
            assert_eq!(
                get_str(&out, "vexos.hostname").as_deref(),
                Some(hostile),
                "{hostile}"
            );
            let line = out.lines().find(|l| l.contains("vexos.hostname")).unwrap();
            // Every `${` must be escaped: an odd number of backslashes before the `$`.
            for (i, _) in line.match_indices("${") {
                let slashes = line[..i].chars().rev().take_while(|c| *c == '\\').count();
                assert!(slashes % 2 == 1, "unescaped antiquotation: {line}");
            }
        }
    }

    #[test]
    fn control_characters_are_refused() {
        for bad in ["a\nb", "a\rb", "a\0b", "a\tb"] {
            assert!(
                matches!(
                    set(FILE, "vexos.hostname", &s(bad)),
                    Err(EditError::BadValue(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn values_render_as_nix() {
        assert_eq!(NixValue::Bool(false).render("o").unwrap(), "false");
        assert_eq!(NixValue::Int(-3).render("o").unwrap(), "-3");
        assert_eq!(
            NixValue::StrList(vec!["a".into(), "b\"c".into()])
                .render("o")
                .unwrap(),
            r#"[ "a" "b\"c" ]"#
        );
        assert_eq!(NixValue::StrList(vec![]).render("o").unwrap(), "[ ]");
    }

    #[test]
    fn crlf_endings_survive() {
        let text = "{\r\n  vexos.a = 1;\r\n}\r\n";
        let out = set(text, "vexos.a", &NixValue::Int(2)).unwrap();
        assert_eq!(out, "{\r\n  vexos.a = 2;\r\n}\r\n");
    }

    #[test]
    fn is_set_ignores_comments_and_other_values() {
        let on = NixValue::Bool(true);
        let off = NixValue::Bool(false);
        assert!(is_set(FILE, "vexos.features.gaming.enable", &on));
        assert!(!is_set(FILE, "vexos.features.gaming.enable", &off));
        assert!(!is_set(FILE, "vexos.features.ai.enable", &off));
    }

    #[test]
    fn disable_flips_true_and_keeps_spacing_and_comment() {
        let text = "{\n  vexos.x.enable   =  true ;  # on\n}\n";
        let out = disable(text, "vexos.x.enable").unwrap().unwrap();
        assert_eq!(out, "{\n  vexos.x.enable   =  false ;  # on\n}\n");
    }

    #[test]
    fn disable_reports_nothing_to_do() {
        let text = "{\n  vexos.x.enable = false;\n  # vexos.y.enable = true;\n}\n";
        assert_eq!(disable(text, "vexos.x.enable").unwrap(), None);
        assert_eq!(disable(text, "vexos.y.enable").unwrap(), None);
    }

    #[test]
    fn edit_file_checks_the_result_writes_it_and_keeps_a_backup() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", FILE);
        let runner = FakeRunner::new().ok("nix-instantiate", &["--parse", "-"]);

        let result = block_on(set_in_file(&runner, &path, "vexos.hostname", &s("new"))).unwrap();

        assert_eq!(result, Edited::Changed);
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"new\""));
        assert_eq!(
            std::fs::read_to_string(dir.join("features.nix.bak")).unwrap(),
            FILE
        );
        // The candidate was checked on stdin, not as an argument.
        let call = &runner.calls()[0];
        assert!(String::from_utf8(call.stdin.clone().unwrap())
            .unwrap()
            .contains("\"new\""));
    }

    #[test]
    fn edit_file_writes_nothing_when_nix_rejects_the_result() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", FILE);
        let runner =
            FakeRunner::new().failing("nix-instantiate", &["--parse", "-"], 1, "syntax error");

        let err = block_on(set_in_file(&runner, &path, "vexos.hostname", &s("new"))).unwrap_err();

        assert!(matches!(err, EditError::Invalid(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE);
        assert!(!dir.join("features.nix.bak").exists());
    }

    #[test]
    fn edit_file_skips_the_check_and_the_write_when_nothing_changes() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", FILE);
        let runner = FakeRunner::new();

        let result = block_on(disable_in_file(&runner, &path, "vexos.features.ai.enable")).unwrap();

        assert_eq!(result, Edited::Unchanged);
        assert!(runner.calls().is_empty());
        assert!(!dir.join("features.nix.bak").exists());
    }

    #[test]
    fn edit_file_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.write("features.nix", FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let runner = FakeRunner::new().ok("nix-instantiate", &[]);

        block_on(set_in_file(&runner, &path, "vexos.hostname", &s("new"))).unwrap();

        assert_eq!(crate::fsutil::mode_of(&path).unwrap(), 0o640);
    }

    #[test]
    fn a_change_made_while_editing_is_not_lost() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", FILE);
        let runner = FakeRunner::new().ok("nix-instantiate", &[]);

        // Another operation edits the file after this one has read it, on the first
        // attempt only.
        let mut first = true;
        let other = FILE.replace("\"box\"", "\"theirs\"");
        let result = block_on(edit_file(&runner, &path, |text| {
            if std::mem::take(&mut first) {
                std::fs::write(&path, &other).unwrap();
            }
            set(text, "vexos.features.ai.enable", &NixValue::Bool(true)).map(Some)
        }))
        .unwrap();

        assert_eq!(result, Edited::Changed);
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now.contains("\"theirs\""), "the other edit was lost: {now}");
        assert!(now.contains("vexos.features.ai.enable = true;"), "{now}");
    }

    #[test]
    fn edits_made_at_the_same_time_all_survive() {
        let dir = TempDir::new();
        let path = dir.write(
            "features.nix",
            "{
}
",
        );
        let runner = FakeRunner::new().ok("nix-instantiate", &[]);

        // Eight threads each add their own option. The pause after reading is what
        // makes them overlap; without the per-file lock most of the edits are lost.
        std::thread::scope(|scope| {
            for n in 0..8 {
                let (runner, path) = (&runner, &path);
                scope.spawn(move || {
                    let opt = format!("vexos.opt{n}");
                    let result = block_on(edit_file(runner, path, |text| {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        set(text, &opt, &NixValue::Bool(true)).map(Some)
                    }));
                    assert_eq!(result.unwrap(), Edited::Changed);
                });
            }
        });

        let now = std::fs::read_to_string(&path).unwrap();
        for n in 0..8 {
            assert!(now.contains(&format!("vexos.opt{n} = true;")), "{now}");
        }
    }

    #[test]
    fn a_file_that_never_settles_is_an_error_not_a_loop() {
        let dir = TempDir::new();
        let path = dir.write("features.nix", FILE);
        let runner = FakeRunner::new().ok("nix-instantiate", &[]);

        let mut n = 0;
        let err = block_on(edit_file(&runner, &path, |text| {
            n += 1;
            std::fs::write(&path, format!("{FILE}# {n}\n")).unwrap();
            set(text, "vexos.hostname", &s("x")).map(Some)
        }))
        .unwrap_err();

        assert!(matches!(err, EditError::Busy), "{err}");
    }
}
