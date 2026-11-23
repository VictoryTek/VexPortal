//! Which model each assistant uses, and whether it is signed in.
//!
//! `vexos-ai` only starts an assistant; the model is the assistant's own setting, in its own
//! file in the user's home. This reads and edits that one setting and leaves the rest of the
//! file alone. Claude Code and OpenCode keep JSON, Codex keeps TOML (edited by line, so
//! comments and order survive). An unset model means the assistant's own default.

use super::ai::Agent;
use std::path::{Path, PathBuf};

/// Where each assistant keeps its files.
pub struct Dirs {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub opencode_config: PathBuf,
    pub opencode_data: PathBuf,
    /// `OPENCODE_CONFIG` or `OPENCODE_CONFIG_DIR` is set: OpenCode then reads settings from
    /// places this does not follow, so a model saved here might not apply.
    pub opencode_overridden: bool,
}

impl Dirs {
    /// This user's, honouring the variables each assistant honours.
    pub fn of_user() -> Self {
        let from = |var: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = glib::home_dir();
        let xdg = |var: &str, fallback: &str| from(var).unwrap_or_else(|| home.join(fallback));
        Self {
            claude: from("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")),
            codex: from("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
            opencode_config: xdg("XDG_CONFIG_HOME", ".config").join("opencode"),
            opencode_data: xdg("XDG_DATA_HOME", ".local/share").join("opencode"),
            opencode_overridden: from("OPENCODE_CONFIG").is_some()
                || from("OPENCODE_CONFIG_DIR").is_some()
                || from("OPENCODE_CONFIG_CONTENT").is_some(),
        }
    }

    /// The settings file the model lives in.
    fn settings(&self, agent: Agent) -> PathBuf {
        match agent {
            Agent::Claude => self.claude.join("settings.json"),
            Agent::Codex => self.codex.join("config.toml"),
            Agent::OpenCode => {
                // OpenCode reads the .jsonc after the .json, so it wins when both exist.
                let jsonc = self.opencode_config.join("opencode.jsonc");
                if jsonc.exists() {
                    jsonc
                } else {
                    self.opencode_config.join("opencode.json")
                }
            }
        }
    }

    /// A login file is there. `None` when this cannot tell: OpenCode newer than its
    /// `auth.json` keeps logins in a database that outlives signing out. Environment-variable
    /// keys are not seen either way.
    pub fn signed_in(&self, agent: Agent) -> Option<bool> {
        match agent {
            Agent::Claude => Some(self.claude.join(".credentials.json").exists()),
            Agent::Codex => Some(self.codex.join("auth.json").exists()),
            Agent::OpenCode => self
                .opencode_data
                .join("auth.json")
                .exists()
                .then_some(true)
                .or_else(|| (!self.opencode_data.join("opencode.db").exists()).then_some(false)),
        }
    }

    /// Every file the model may come from, strongest first. OpenCode merges its `.jsonc`
    /// over its `.json`.
    fn files(&self, agent: Agent) -> Vec<PathBuf> {
        match agent {
            Agent::OpenCode => vec![
                self.opencode_config.join("opencode.jsonc"),
                self.opencode_config.join("opencode.json"),
            ],
            _ => vec![self.settings(agent)],
        }
    }

    /// The saved model, `None` while the assistant's own default applies.
    pub fn model(&self, agent: Agent) -> Option<String> {
        if agent == Agent::OpenCode && self.opencode_overridden {
            return None;
        }
        self.files(agent).iter().find_map(|path| {
            let text = std::fs::read_to_string(path).ok()?;
            match agent {
                Agent::Codex => toml_value(&text).ok()?,
                _ => {
                    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
                    json.get("model")?.as_str().map(str::to_string)
                }
            }
        })
    }

    /// Save `model`, or clear it with `None`. Every other setting in the file is kept. A
    /// file that cannot be read back safely is refused, never overwritten. Clearing also
    /// clears the weaker of OpenCode's two files, or its model would show through.
    pub fn set_model(&self, agent: Agent, model: Option<&str>) -> Result<(), String> {
        if let Some(m) = model {
            valid(m)?;
        }
        if agent == Agent::OpenCode && self.opencode_overridden {
            return Err(
                "OPENCODE_CONFIG, OPENCODE_CONFIG_DIR or OPENCODE_CONFIG_CONTENT is set, so \
                 OpenCode may read its model from somewhere else. Set the model there by hand."
                    .into(),
            );
        }
        let targets = if model.is_some() {
            vec![self.settings(agent)]
        } else {
            self.files(agent)
        };
        let mut planned = Vec::new();
        for path in targets {
            planned.extend(self.plan(agent, &path, model)?);
        }
        // Nothing is written until every file has been read and found editable, so a refusal
        // never leaves one file changed and another not.
        for write in planned {
            write.commit()?;
        }
        Ok(())
    }

    fn plan(
        &self,
        agent: Agent,
        path: &Path,
        model: Option<&str>,
    ) -> Result<Option<Write>, String> {
        let existing = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{} cannot be read: {e}", path.display())),
        };
        let updated = match agent {
            Agent::Codex => toml_with_model(existing.as_deref().unwrap_or(""), model, path)?,
            _ => json_with_model(existing.as_deref(), model, path)?,
        };
        if existing.as_deref() == Some(updated.as_str()) || (existing.is_none() && model.is_none())
        {
            return Ok(None);
        }
        Ok(Some(Write {
            path: path.to_path_buf(),
            had_file: existing.is_some(),
            text: updated,
        }))
    }
}

/// A checked change, ready to save.
struct Write {
    path: PathBuf,
    had_file: bool,
    text: String,
}

impl Write {
    fn commit(self) -> Result<(), String> {
        let path = &self.path;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        if self.had_file {
            vexportal_ops::fsutil::backup(path).map_err(|e| format!("backup failed: {e}"))?;
        }
        let mode = vexportal_ops::fsutil::mode_of(path).unwrap_or(0o600);
        vexportal_ops::fsutil::write_atomic(path, self.text.as_bytes(), mode)
            .map_err(|e| format!("{} cannot be written: {e}", path.display()))
    }
}

/// Names the assistants accept are plain: this keeps one from breaking the file it is
/// written into.
fn valid(model: &str) -> Result<(), String> {
    let ok = !model.is_empty()
        && model.len() <= 100
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(format!("'{model}' is not a model name."))
    }
}

fn json_with_model(
    existing: Option<&str>,
    model: Option<&str>,
    path: &Path,
) -> Result<String, String> {
    let mut json: serde_json::Value = match existing {
        None => serde_json::json!({}),
        Some(text) if text.trim().is_empty() => serde_json::json!({}),
        Some(text) => serde_json::from_str(text).map_err(|_| {
            format!(
                "{} has comments or is not plain JSON, so it is left alone. Set the model in \
                 that file by hand.",
                path.display()
            )
        })?,
    };
    let Some(object) = json.as_object_mut() else {
        return Err(format!("{} is not a JSON object.", path.display()));
    };
    match model {
        Some(m) => {
            object.insert("model".into(), m.into());
        }
        None => {
            object.remove("model");
        }
    }
    let mut text = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

/// The top-level `model = "…"` line: index and value. Only lines before the first table
/// count, because a `model` inside `[profiles.x]` is not the default.
fn toml_model_line(text: &str) -> (Option<(usize, String)>, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = None;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with('[') {
            end = i;
            break;
        }
        let Some(rest) = t.strip_prefix("model") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.split('#').next().unwrap_or("").trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        if found.is_none() {
            found = Some((i, value.to_string()));
        }
    }
    (found, end)
}

/// The real top-level `model`, from a TOML parse. An error when the file is not valid TOML
/// or `model` is not a string.
fn toml_value(text: &str) -> Result<Option<String>, String> {
    let table: toml::Table = text.parse().map_err(|_| "not valid TOML".to_string())?;
    match table.get("model") {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err("`model` is not a string".to_string()),
    }
}

/// Every setting except the top-level `model`, parsed.
fn others(text: &str) -> Option<toml::Table> {
    let mut table: toml::Table = text.parse().ok()?;
    table.remove("model");
    Some(table)
}

/// Edit the `model` line, then parse the result: if it does not say exactly what was asked
/// (a multi-line string or a quoted key fooled the line scan), nothing is written.
fn toml_with_model(text: &str, model: Option<&str>, path: &Path) -> Result<String, String> {
    let refuse = |why: &str| {
        format!(
            "{} {why}, so it is left alone. Set the model in that file by hand.",
            path.display()
        )
    };
    let now = toml_value(text).map_err(|e| refuse(&format!("is not usable ({e})")))?;
    if now.as_deref() == model {
        return Ok(text.to_string());
    }
    let (found, end) = toml_model_line(text);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match (found, model) {
        (Some((i, _)), Some(m)) => lines[i] = format!("model = \"{m}\""),
        (Some((i, _)), None) => {
            lines.remove(i);
        }
        (None, Some(m)) => {
            // Before the first table, after any leading comments.
            lines.insert(end, format!("model = \"{m}\""));
        }
        (None, None) => {}
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    match toml_value(&out) {
        Ok(now) if now.as_deref() == model && others(text) == others(&out) => Ok(out),
        _ => Err(refuse("has a layout this cannot edit safely")),
    }
}

/// What the dropdown offers, as (value, label). The first entry is always "unset".
/// Model names change often, so a saved value that is not here is shown as it is.
pub fn choices(agent: Agent) -> &'static [(&'static str, &'static str)] {
    match agent {
        Agent::Claude => &[
            ("", "Account default"),
            ("opus", "Opus (latest)"),
            ("sonnet", "Sonnet (latest)"),
            ("haiku", "Haiku (latest)"),
            ("claude-fable-5-2", "Fable 5.2"),
            ("claude-opus-5-5", "Opus 5.5"),
            ("claude-sonnet-5-5", "Sonnet 5.5"),
            ("claude-haiku-5-5", "Haiku 5.5"),
        ],
        Agent::Codex => &[("", "Account default"), ("gpt-6.1-sol", "GPT-6.1 Sol")],
        Agent::OpenCode => &[
            ("", "Provider default"),
            ("anthropic/claude-opus-5-5", "Anthropic Opus 5.5"),
            ("anthropic/claude-sonnet-5-5", "Anthropic Sonnet 5.5"),
            ("anthropic/claude-haiku-5-5", "Anthropic Haiku 5.5"),
            ("openai/gpt-6.1-sol", "OpenAI GPT-6.1 Sol"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vexportal_ops::fsutil::testing::TempDir;

    fn dirs(t: &TempDir) -> Dirs {
        Dirs {
            claude: t.path().join("claude"),
            codex: t.path().join("codex"),
            opencode_config: t.path().join("oc-config"),
            opencode_data: t.path().join("oc-data"),
            opencode_overridden: false,
        }
    }

    #[test]
    fn nothing_saved_means_the_default() {
        let t = TempDir::new();
        let d = dirs(&t);
        for a in Agent::ALL {
            assert_eq!(d.model(a), None);
        }
    }

    #[test]
    fn claude_keeps_its_other_settings() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.claude).unwrap();
        std::fs::write(
            d.claude.join("settings.json"),
            r#"{"permissions":{"deny":["x"]},"model":"opus"}"#,
        )
        .unwrap();
        assert_eq!(d.model(Agent::Claude).as_deref(), Some("opus"));
        d.set_model(Agent::Claude, Some("claude-sonnet-5-5"))
            .unwrap();
        assert_eq!(d.model(Agent::Claude).as_deref(), Some("claude-sonnet-5-5"));
        let text = std::fs::read_to_string(d.claude.join("settings.json")).unwrap();
        assert!(text.contains("\"deny\""));
        assert!(d.claude.join("settings.json.bak").exists() || has_backup(&d.claude));
        d.set_model(Agent::Claude, None).unwrap();
        assert_eq!(d.model(Agent::Claude), None);
        let text = std::fs::read_to_string(d.claude.join("settings.json")).unwrap();
        assert!(text.contains("\"deny\"") && !text.contains("model"));
    }

    fn has_backup(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".bak"))
    }

    #[test]
    fn a_missing_file_is_created_and_clearing_it_creates_nothing() {
        let t = TempDir::new();
        let d = dirs(&t);
        d.set_model(Agent::OpenCode, None).unwrap();
        assert!(!d.opencode_config.exists());
        d.set_model(Agent::OpenCode, Some("anthropic/claude-opus-5-5"))
            .unwrap();
        assert_eq!(
            d.model(Agent::OpenCode).as_deref(),
            Some("anthropic/claude-opus-5-5")
        );
    }

    #[test]
    fn a_file_with_comments_is_refused_not_overwritten() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.opencode_config).unwrap();
        let file = d.opencode_config.join("opencode.jsonc");
        std::fs::write(&file, "{ // mine\n \"theme\": \"x\" }").unwrap();
        assert!(d
            .set_model(Agent::OpenCode, Some("openai/gpt-6.1-sol"))
            .is_err());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{ // mine\n \"theme\": \"x\" }"
        );
    }

    fn p() -> &'static Path {
        Path::new("config.toml")
    }

    #[test]
    fn codex_edits_only_the_top_level_line() {
        let text = "# mine\nmodel = \"a\" # why\napproval = \"x\"\n\n[profiles.p]\nmodel = \"b\"\n";
        assert_eq!(toml_value(text).unwrap().as_deref(), Some("a"));
        let set = toml_with_model(text, Some("gpt-6.1-sol"), p()).unwrap();
        assert_eq!(
            set,
            "# mine\nmodel = \"gpt-6.1-sol\"\napproval = \"x\"\n\n[profiles.p]\nmodel = \"b\"\n"
        );
        let cleared = toml_with_model(text, None, p()).unwrap();
        assert!(!cleared.contains("\"a\"") && cleared.contains("model = \"b\""));
    }

    #[test]
    fn codex_inserts_before_the_first_table_and_ignores_table_models() {
        let text = "[profiles.p]\nmodel = \"b\"\n";
        assert_eq!(toml_value(text).unwrap(), None);
        assert_eq!(
            toml_with_model(text, Some("m"), p()).unwrap(),
            "model = \"m\"\n[profiles.p]\nmodel = \"b\"\n"
        );
        assert_eq!(
            toml_with_model("", Some("m"), p()).unwrap(),
            "model = \"m\"\n"
        );
        // Another key that starts with "model" is not the model.
        let other = "model_provider = \"x\"\n";
        assert_eq!(toml_value(other).unwrap(), None);
        assert_eq!(
            toml_with_model(other, Some("m"), p()).unwrap(),
            "model_provider = \"x\"\nmodel = \"m\"\n"
        );
    }

    #[test]
    fn codex_text_the_line_scan_could_misread_is_refused_not_rewritten() {
        // A model-looking line inside a multi-line string.
        let in_string =
            "developer_instructions = \"\"\"\nmodel = \"example\"\n\"\"\"\nmodel = \"old\"\n";
        assert!(toml_with_model(in_string, Some("m"), p()).is_err());
        // A quoted key would otherwise gain a duplicate.
        assert!(toml_with_model("\"model\" = \"old\"\n", Some("m"), p()).is_err());
        // Not TOML at all.
        assert!(toml_with_model("model = = \n", Some("m"), p()).is_err());
        assert!(toml_with_model("model = 5\n", Some("m"), p()).is_err());
    }

    #[test]
    fn codex_never_changes_another_setting_while_editing_the_model() {
        let text = "developer_instructions = \"\"\"\nmodel = \"example\"\nKeep this\n\"\"\"\n";
        // No top-level model: clearing changes nothing; setting is refused, because the
        // line scan would have hit the one inside the string.
        assert_eq!(toml_with_model(text, None, p()).unwrap(), text);
        assert!(toml_with_model(text, Some("m"), p()).is_err());
    }

    #[test]
    fn opencode_reset_clears_both_files_and_reads_the_stronger_first() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.opencode_config).unwrap();
        let (json, jsonc) = (
            d.opencode_config.join("opencode.json"),
            d.opencode_config.join("opencode.jsonc"),
        );
        std::fs::write(&json, r#"{"model":"openai/gpt-6.1-sol"}"#).unwrap();
        // A .jsonc with no model lets the .json's show through.
        std::fs::write(&jsonc, "{}").unwrap();
        assert_eq!(
            d.model(Agent::OpenCode).as_deref(),
            Some("openai/gpt-6.1-sol")
        );
        std::fs::write(&jsonc, r#"{"model":"anthropic/claude-opus-5-5"}"#).unwrap();
        assert_eq!(
            d.model(Agent::OpenCode).as_deref(),
            Some("anthropic/claude-opus-5-5")
        );
        d.set_model(Agent::OpenCode, None).unwrap();
        assert_eq!(d.model(Agent::OpenCode), None);
    }

    #[test]
    fn json_key_order_survives_because_opencode_rules_are_order_sensitive() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.opencode_config).unwrap();
        let file = d.opencode_config.join("opencode.json");
        std::fs::write(
            &file,
            r#"{"permission":{"bash":{"git *":"allow","*":"deny"}}}"#,
        )
        .unwrap();
        d.set_model(Agent::OpenCode, Some("openai/gpt-6.1-sol"))
            .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.find("git *").unwrap() < text.find("\"*\"").unwrap());
    }

    #[test]
    fn a_reset_that_one_file_refuses_changes_neither() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.opencode_config).unwrap();
        let jsonc = d.opencode_config.join("opencode.jsonc");
        std::fs::write(&jsonc, r#"{"model":"openai/gpt-6.1-sol"}"#).unwrap();
        std::fs::write(
            d.opencode_config.join("opencode.json"),
            "{ // note\n \"model\": \"anthropic/claude-opus-5-5\" }",
        )
        .unwrap();
        assert!(d.set_model(Agent::OpenCode, None).is_err());
        assert_eq!(
            std::fs::read_to_string(&jsonc).unwrap(),
            r#"{"model":"openai/gpt-6.1-sol"}"#
        );
    }

    #[test]
    fn names_that_could_break_a_file_are_refused() {
        let t = TempDir::new();
        let d = dirs(&t);
        for bad in ["", "a b", "x\"y", "x\ny", "a;b"] {
            assert!(d.set_model(Agent::Codex, Some(bad)).is_err(), "{bad:?}");
        }
        assert!(!d.codex.exists());
    }

    #[test]
    fn opencode_edits_the_jsonc_when_both_files_exist() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.opencode_config).unwrap();
        std::fs::write(d.opencode_config.join("opencode.json"), "{}").unwrap();
        std::fs::write(
            d.opencode_config.join("opencode.jsonc"),
            r#"{"model":"openai/gpt-6.1-sol"}"#,
        )
        .unwrap();
        assert_eq!(
            d.model(Agent::OpenCode).as_deref(),
            Some("openai/gpt-6.1-sol")
        );
        d.set_model(Agent::OpenCode, Some("anthropic/claude-opus-5-5"))
            .unwrap();
        let json = std::fs::read_to_string(d.opencode_config.join("opencode.json")).unwrap();
        assert_eq!(json, "{}");
        assert_eq!(
            d.model(Agent::OpenCode).as_deref(),
            Some("anthropic/claude-opus-5-5")
        );
    }

    #[test]
    fn opencode_with_a_config_override_is_not_edited() {
        let t = TempDir::new();
        let mut d = dirs(&t);
        d.opencode_overridden = true;
        assert!(d
            .set_model(Agent::OpenCode, Some("openai/gpt-6.1-sol"))
            .is_err());
        assert_eq!(d.model(Agent::OpenCode), None);
        assert!(!d.opencode_config.exists());
    }

    #[test]
    fn sign_in_is_a_login_file() {
        let t = TempDir::new();
        let d = dirs(&t);
        for a in Agent::ALL {
            assert_eq!(d.signed_in(a), Some(false));
        }
        std::fs::create_dir_all(&d.claude).unwrap();
        std::fs::write(d.claude.join(".credentials.json"), "{}").unwrap();
        assert_eq!(d.signed_in(Agent::Claude), Some(true));
        assert_eq!(d.signed_in(Agent::Codex), Some(false));
    }

    #[test]
    fn opencode_with_only_a_database_is_not_reported_either_way() {
        let t = TempDir::new();
        let d = dirs(&t);
        std::fs::create_dir_all(&d.opencode_data).unwrap();
        std::fs::write(d.opencode_data.join("opencode.db"), "").unwrap();
        assert_eq!(d.signed_in(Agent::OpenCode), None);
        std::fs::write(d.opencode_data.join("auth.json"), "{}").unwrap();
        assert_eq!(d.signed_in(Agent::OpenCode), Some(true));
    }

    #[test]
    fn every_agent_offers_unset_first() {
        for a in Agent::ALL {
            assert_eq!(choices(a)[0].0, "");
        }
    }
}
