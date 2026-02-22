use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use zed_extension_api::{
    self as zed, process::Command as ProcessCommand, set_language_server_installation_status,
    LanguageServerInstallationStatus, Result,
};

const SERVER_BIN_NAME: &str = "firrtl-source-locator-server";
const STABLE_TOOLCHAIN: &str = "stable";
const MIN_RUSTC: (u32, u32, u32) = (1, 75, 0);
const SERVER_SOURCE_DIR: &str = "server-src";
const OVERRIDE_MANIFEST_ENV: &str = "FIRRTL_SOURCE_LOCATOR_SERVER_MANIFEST";

const BUNDLED_SERVER_CARGO_TOML: &str = include_str!("../server/Cargo.toml");
const BUNDLED_SERVER_CARGO_LOCK: &str = include_str!("../server/Cargo.lock");
const BUNDLED_SERVER_MAIN_RS: &str = include_str!("../server/src/main.rs");

struct FirrtlSourceLocatorExtension {
    validated_worktrees: HashSet<u64>,
}

impl FirrtlSourceLocatorExtension {
    fn fail<T>(language_server_id: &zed::LanguageServerId, message: String) -> Result<T> {
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Failed(message.clone()),
        );
        Err(message)
    }

    fn command_env(worktree: &zed::Worktree) -> Vec<(String, String)> {
        let mut env = worktree.shell_env();
        if !env.iter().any(|(key, _)| key == "RUSTUP_TOOLCHAIN") {
            env.push(("RUSTUP_TOOLCHAIN".to_string(), STABLE_TOOLCHAIN.to_string()));
        }
        env
    }

    fn tool_path(worktree: &zed::Worktree, name: &str) -> Option<String> {
        worktree
            .which(name)
            .or_else(|| worktree.which(&format!("{name}.exe")))
    }

    fn run_process(
        program: &str,
        args: &[&str],
        env: &[(String, String)],
    ) -> Result<zed::process::Output> {
        let mut cmd = ProcessCommand::new(program)
            .args(args.iter().copied())
            .envs(env.iter().cloned());
        cmd.output()
            .map_err(|err| format!("Failed to execute `{program}`: {err}"))
    }

    fn summarize_output(bytes: &[u8]) -> String {
        let mut text = String::from_utf8_lossy(bytes).trim().to_string();
        if text.len() > 700 {
            text.truncate(700);
            text.push_str("\n... (truncated)");
        }
        text
    }

    fn parse_rustc_version(text: &str) -> Option<(u32, u32, u32)> {
        let raw = text.split_whitespace().nth(1)?;
        let mut parts = raw.split('.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next()?.parse::<u32>().ok()?;
        let patch = parts
            .next()
            .map(|value| {
                value
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect::<String>()
            })
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        Some((major, minor, patch))
    }

    fn write_if_changed(path: &Path, content: &str) -> Result<()> {
        if let Ok(existing) = fs::read_to_string(path) {
            if existing == content {
                return Ok(());
            }
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "Failed to create directory `{}`: {}",
                    parent.to_string_lossy(),
                    err
                )
            })?;
        }

        fs::write(path, content)
            .map_err(|err| format!("Failed to write `{}`: {}", path.to_string_lossy(), err))?;

        Ok(())
    }

    fn ensure_bundled_server_source(language_server_id: &zed::LanguageServerId) -> Result<String> {
        if let Ok(value) = std::env::var(OVERRIDE_MANIFEST_ENV) {
            let override_path = PathBuf::from(&value);
            if override_path.is_file() {
                return Ok(value);
            }
            return Self::fail(
                language_server_id,
                format!(
                    "`{OVERRIDE_MANIFEST_ENV}` points to a missing file: `{}`.",
                    override_path.to_string_lossy()
                ),
            );
        }

        let base_dir = match std::env::current_dir() {
            Ok(path) => path,
            Err(err) => {
                return Self::fail(
                    language_server_id,
                    format!(
                        "Failed to determine extension working directory for local compilation: {}",
                        err
                    ),
                );
            }
        };
        let server_dir = base_dir.join(SERVER_SOURCE_DIR);
        let manifest_path = server_dir.join("Cargo.toml");
        let lock_path = server_dir.join("Cargo.lock");
        let main_path = server_dir.join("src").join("main.rs");

        if let Err(err) = Self::write_if_changed(&manifest_path, BUNDLED_SERVER_CARGO_TOML) {
            return Self::fail(language_server_id, err);
        }
        if let Err(err) = Self::write_if_changed(&lock_path, BUNDLED_SERVER_CARGO_LOCK) {
            return Self::fail(language_server_id, err);
        }
        if let Err(err) = Self::write_if_changed(&main_path, BUNDLED_SERVER_MAIN_RS) {
            return Self::fail(language_server_id, err);
        }

        Ok(manifest_path.to_string_lossy().to_string())
    }

    fn is_version_at_least(found: (u32, u32, u32), expected: (u32, u32, u32)) -> bool {
        found.0 > expected.0
            || (found.0 == expected.0
                && (found.1 > expected.1 || (found.1 == expected.1 && found.2 >= expected.2)))
    }

    fn validate_local_build(
        &self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
        manifest_path: &str,
        env: &[(String, String)],
    ) -> Result<()> {
        let Some(_rustc_path) = Self::tool_path(worktree, "rustc") else {
            return Self::fail(
                language_server_id,
                "Rust compiler not found. Install Rust via rustup (https://rustup.rs) and restart Zed."
                    .to_string(),
            );
        };

        let cargo_version_output = Self::run_process("cargo", &["--version"], env)?;
        if cargo_version_output.status != Some(0) {
            let stderr = Self::summarize_output(&cargo_version_output.stderr);
            return Self::fail(
                language_server_id,
                format!(
                    "`cargo --version` failed.\n{stderr}\n\nPlease verify your Rust toolchain installation and PATH."
                ),
            );
        }

        let rustc_version_output = Self::run_process("rustc", &["--version"], env)?;
        if rustc_version_output.status != Some(0) {
            let stderr = Self::summarize_output(&rustc_version_output.stderr);
            return Self::fail(
                language_server_id,
                format!(
                    "`rustc --version` failed.\n{stderr}\n\nPlease verify your Rust toolchain installation and PATH."
                ),
            );
        }

        let rustc_version_text = Self::summarize_output(&rustc_version_output.stdout);
        if let Some(version) = Self::parse_rustc_version(&rustc_version_text) {
            if !Self::is_version_at_least(version, MIN_RUSTC) {
                return Self::fail(
                    language_server_id,
                    format!(
                        "Rust version too old: `{rustc_version_text}`.\nNeed rustc >= {}.{}.{}.\nRun `rustup update stable` and restart Zed.",
                        MIN_RUSTC.0, MIN_RUSTC.1, MIN_RUSTC.2
                    ),
                );
            }
        }

        let check_args = [
            "check",
            "--manifest-path",
            manifest_path,
            "--bin",
            SERVER_BIN_NAME,
        ];
        let check_output = Self::run_process("cargo", &check_args, env)?;
        if check_output.status != Some(0) {
            let stderr = Self::summarize_output(&check_output.stderr);
            let stdout = Self::summarize_output(&check_output.stdout);
            let details = if !stderr.is_empty() { stderr } else { stdout };
            let mut message = format!(
                "Failed to compile `{SERVER_BIN_NAME}` locally.\n{details}\n\nTry running this command in the project root:\n`cargo check --manifest-path {manifest_path} --bin {SERVER_BIN_NAME}`"
            );
            message.push_str("\nIf it still fails, update Rust (`rustup update stable`) and check network access to crates.io.");
            return Self::fail(language_server_id, message);
        }

        Ok(())
    }

    fn start_command(
        cargo_path: String,
        manifest_path: String,
        env: Vec<(String, String)>,
    ) -> zed::Command {
        zed::Command {
            command: cargo_path,
            args: vec![
                "run".to_string(),
                "--manifest-path".to_string(),
                manifest_path,
                "--bin".to_string(),
                SERVER_BIN_NAME.to_string(),
            ],
            env,
        }
    }
}

impl zed::Extension for FirrtlSourceLocatorExtension {
    fn new() -> Self {
        Self {
            validated_worktrees: HashSet::new(),
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let Some(cargo_path) = Self::tool_path(worktree, "cargo") else {
            return Self::fail(
                language_server_id,
                "`cargo` not found in PATH. Install Rust via rustup (https://rustup.rs) and restart Zed."
                    .to_string(),
            );
        };

        let manifest_path = Self::ensure_bundled_server_source(language_server_id)?;

        let env = Self::command_env(worktree);
        let worktree_id = worktree.id();

        if !self.validated_worktrees.contains(&worktree_id) {
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::CheckingForUpdate,
            );

            self.validate_local_build(language_server_id, worktree, &manifest_path, &env)?;

            self.validated_worktrees.insert(worktree_id);
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::None,
            );
        }

        Ok(Self::start_command(cargo_path, manifest_path, env))
    }
}

zed::register_extension!(FirrtlSourceLocatorExtension);
