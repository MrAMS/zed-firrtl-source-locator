use std::fs;

use zed_extension_api::{
    self as zed, current_platform, download_file, github_release_by_tag_name, make_file_executable,
    set_language_server_installation_status, Architecture, DownloadedFileType, GithubRelease,
    LanguageServerInstallationStatus, Os, Result,
};

const SERVER_BIN_NAME: &str = "firrtl-source-locator-server";
const EXTENSION_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_REPOSITORY: &str = "MrAMS/zed-firrtl-source-locator";
const RELEASE_TAG_PREFIX: &str = "v";
const SERVER_PATH_ENV: &str = "FIRRTL_SOURCE_LOCATOR_SERVER";

struct FirrtlSourceLocatorExtension;

impl FirrtlSourceLocatorExtension {
    fn fail<T>(language_server_id: &zed::LanguageServerId, message: String) -> Result<T> {
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Failed(message.clone()),
        );
        Err(message)
    }

    fn release_tag() -> String {
        format!("{RELEASE_TAG_PREFIX}{EXTENSION_VERSION}")
    }

    fn platform_target(platform: Os, arch: Architecture) -> Option<&'static str> {
        match (platform, arch) {
            (Os::Linux, Architecture::X8664) => Some("x86_64-unknown-linux-gnu"),
            (Os::Linux, Architecture::Aarch64) => Some("aarch64-unknown-linux-gnu"),
            (Os::Mac, Architecture::X8664) => Some("x86_64-apple-darwin"),
            (Os::Mac, Architecture::Aarch64) => Some("aarch64-apple-darwin"),
            (Os::Windows, Architecture::X8664) => Some("x86_64-pc-windows-msvc"),
            _ => None,
        }
    }

    fn binary_name(platform: Os) -> &'static str {
        if platform == Os::Windows {
            "firrtl-source-locator-server.exe"
        } else {
            SERVER_BIN_NAME
        }
    }

    fn archive_type(platform: Os) -> DownloadedFileType {
        if platform == Os::Windows {
            DownloadedFileType::Zip
        } else {
            DownloadedFileType::GzipTar
        }
    }

    fn release_asset_name(platform: Os, target: &str) -> String {
        let extension = if platform == Os::Windows {
            "zip"
        } else {
            "tar.gz"
        };
        format!("{SERVER_BIN_NAME}-{target}.{extension}")
    }

    fn install_dir(target: &str) -> String {
        format!("{SERVER_BIN_NAME}-v{EXTENSION_VERSION}-{target}")
    }

    fn binary_from_path(worktree: &zed::Worktree, binary_name: &str) -> Option<String> {
        worktree
            .which(SERVER_BIN_NAME)
            .or_else(|| worktree.which(binary_name))
            .or_else(|| worktree.which(&format!("{SERVER_BIN_NAME}.exe")))
    }

    fn use_override_binary(
        language_server_id: &zed::LanguageServerId,
        binary_name: &str,
    ) -> Result<Option<String>> {
        let Ok(path) = std::env::var(SERVER_PATH_ENV) else {
            return Ok(None);
        };

        if fs::metadata(&path).is_ok() {
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::None,
            );
            return Ok(Some(path));
        }

        Self::fail(
            language_server_id,
            format!(
                "`{SERVER_PATH_ENV}` points to a missing server binary: `{path}`. \
                 Set `{SERVER_PATH_ENV}` to a valid `{binary_name}` path, or unset it to use PATH/GitHub release binaries."
            ),
        )
    }

    fn release_by_tag(
        language_server_id: &zed::LanguageServerId,
        release_tag: &str,
    ) -> Result<GithubRelease> {
        github_release_by_tag_name(GITHUB_REPOSITORY, release_tag).map_err(|err| {
            let message = format!(
                "Failed to fetch GitHub release `{release_tag}` from `{GITHUB_REPOSITORY}`. \
                 Ensure network access is available and the release exists. Original error: {err}"
            );
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Failed(message.clone()),
            );
            message
        })
    }

    fn language_server_binary_path(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        let (platform, arch) = current_platform();
        let binary_name = Self::binary_name(platform);

        if let Some(path) = Self::use_override_binary(language_server_id, binary_name)? {
            return Ok(path);
        }

        if let Some(path) = Self::binary_from_path(worktree, binary_name) {
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::None,
            );
            return Ok(path);
        }

        let Some(target) = Self::platform_target(platform, arch) else {
            return Self::fail(
                language_server_id,
                format!(
                    "Unsupported platform {:?}-{:?}. Supported platforms: Linux (x86_64/aarch64), macOS (x86_64/aarch64), Windows (x86_64).",
                    platform, arch
                ),
            );
        };

        let install_dir = Self::install_dir(target);
        let binary_path = format!("{install_dir}/{binary_name}");

        if fs::metadata(&binary_path).is_ok() {
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::None,
            );
            return Ok(binary_path);
        }

        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let release_tag = Self::release_tag();
        let release = Self::release_by_tag(language_server_id, &release_tag)?;

        let asset_name = Self::release_asset_name(platform, target);
        let asset = match release.assets.iter().find(|asset| asset.name == asset_name) {
            Some(asset) => asset,
            None => {
                let available_assets = release
                    .assets
                    .iter()
                    .map(|asset| asset.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Self::fail(
                    language_server_id,
                    format!(
                        "Release `{release_tag}` does not contain `{asset_name}`. Available assets: [{available_assets}]"
                    ),
                );
            }
        };

        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Downloading,
        );

        if let Err(err) = download_file(
            &asset.download_url,
            &install_dir,
            Self::archive_type(platform),
        ) {
            return Self::fail(
                language_server_id,
                format!(
                    "Failed to download `{asset_name}` from `{}` into `{install_dir}`: {err}",
                    asset.download_url
                ),
            );
        }

        if platform != Os::Windows {
            if let Err(err) = make_file_executable(&binary_path) {
                return Self::fail(
                    language_server_id,
                    format!("Failed to make `{binary_path}` executable: {err}"),
                );
            }
        }

        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );

        Ok(binary_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tag_matches_extension_version() {
        assert_eq!(
            FirrtlSourceLocatorExtension::release_tag(),
            format!("v{EXTENSION_VERSION}")
        );
    }

    #[test]
    fn release_asset_name_matches_expected_by_platform() {
        assert_eq!(
            FirrtlSourceLocatorExtension::release_asset_name(Os::Linux, "x86_64-unknown-linux-gnu"),
            "firrtl-source-locator-server-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            FirrtlSourceLocatorExtension::release_asset_name(Os::Windows, "x86_64-pc-windows-msvc"),
            "firrtl-source-locator-server-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn platform_target_mapping_is_stable() {
        assert_eq!(
            FirrtlSourceLocatorExtension::platform_target(Os::Linux, Architecture::X8664),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            FirrtlSourceLocatorExtension::platform_target(Os::Linux, Architecture::Aarch64),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            FirrtlSourceLocatorExtension::platform_target(Os::Mac, Architecture::X8664),
            Some("x86_64-apple-darwin")
        );
        assert_eq!(
            FirrtlSourceLocatorExtension::platform_target(Os::Mac, Architecture::Aarch64),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            FirrtlSourceLocatorExtension::platform_target(Os::Windows, Architecture::X8664),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            FirrtlSourceLocatorExtension::platform_target(Os::Windows, Architecture::X86),
            None
        );
    }
}

impl zed::Extension for FirrtlSourceLocatorExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary_path = self.language_server_binary_path(language_server_id, worktree)?;

        Ok(zed::Command {
            command: binary_path,
            args: vec![],
            env: Default::default(),
        })
    }
}

zed::register_extension!(FirrtlSourceLocatorExtension);
