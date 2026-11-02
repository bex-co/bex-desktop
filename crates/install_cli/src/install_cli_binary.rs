use super::register_zed_scheme;
use anyhow::Result;
use gpui::{AppContext as _, AsyncApp, Context, PromptLevel, Window, actions};
use release_channel::ReleaseChannel;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use util::ResultExt;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::notifications::{DetachAndPromptErr, NotificationId};
use workspace::{Toast, Workspace};

actions!(
    cli,
    [
        /// Installs the `bex-desktop` CLI tool to the system PATH.
        InstallCliBinary,
    ]
);

const CANT_INSTALL_DOCS_URL: &str = "https://zed.dev/docs/macos#cant-install-cli";

/// The terminal command name. `bex` belongs to the bex platform CLI, which
/// execs this command for `bex edit <path>`; see
/// docs/adrs/ADR002-cli-command-bex-desktop.md.
const CLI_COMMAND_NAME: &str = "bex-desktop";
const BIN_DIR: &str = "/usr/local/bin";
// TODO: remove the legacy link migration a few releases after the rename (ADR002).
const LEGACY_LINK_PATH: &str = "/usr/local/bin/bex";

struct InstalledCli {
    path: PathBuf,
    /// Whether a legacy `/usr/local/bin/bex` link into this app was removed.
    migrated_legacy_link: bool,
}

/// Whether the symlink at `link_path` with target `link_target` points into
/// `owned_root` (this app's bundle, or the CLI binary itself for unbundled
/// dev builds). Relative targets resolve against the link's directory.
fn link_points_into(link_path: &Path, link_target: &Path, owned_root: &Path) -> bool {
    let resolved_target = match link_path.parent() {
        Some(link_dir) => link_dir.join(link_target),
        None => link_target.to_path_buf(),
    };
    resolved_target.starts_with(owned_root)
}

/// The `.app` bundle containing `cli_path`, or `cli_path` itself when the
/// CLI does not run from a bundle.
fn owned_root(cli_path: &Path) -> &Path {
    cli_path
        .ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .is_some_and(|extension| extension == "app")
        })
        .unwrap_or(cli_path)
}

/// Returns true only when `legacy_link_path` is a symlink into this app;
/// regular files and links to anything else (e.g. the bex platform CLI) are
/// never touched.
async fn is_our_legacy_link(legacy_link_path: &Path, cli_path: &Path) -> bool {
    let Ok(link_target) = smol::fs::read_link(legacy_link_path).await else {
        return false;
    };
    let root = owned_root(cli_path);
    if link_points_into(legacy_link_path, &link_target, root) {
        return true;
    }
    let (Ok(canonical_target), Ok(canonical_root)) = (
        smol::fs::canonicalize(legacy_link_path).await,
        smol::fs::canonicalize(root).await,
    ) else {
        return false;
    };
    canonical_target.starts_with(canonical_root)
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy())
}

/// Attempts to install the CLI symlink and migrate a legacy `bex` link.
/// Returns `None` if the user dismissed the macOS administrator
/// authentication prompt. Returns an error if the install could not be
/// completed, most commonly because the user is not an admin.
async fn install_script(cx: &AsyncApp) -> Result<Option<InstalledCli>> {
    let cli_path = cx.update(|cx| cx.path_for_auxiliary_executable("cli"))?;
    let bin_dir_path = Path::new(BIN_DIR);
    let link_path = bin_dir_path.join(CLI_COMMAND_NAME);
    let legacy_link_path = Path::new(LEGACY_LINK_PATH);

    let mut needs_link = smol::fs::read_link(&link_path).await.ok().as_ref() != Some(&cli_path);
    if needs_link && smol::fs::symlink_metadata(&link_path).await.is_ok() {
        anyhow::bail!(
            "{} already belongs to another command. Choose a different path for the Bex editor CLI.",
            link_path.display()
        );
    }
    let migrated_legacy_link = is_our_legacy_link(legacy_link_path, &cli_path).await;
    let mut needs_legacy_removal = migrated_legacy_link;

    if needs_link
        && smol::fs::unix::symlink(&cli_path, &link_path)
            .await
            .log_err()
            .is_some()
    {
        needs_link = false;
    }
    if !needs_link
        && needs_legacy_removal
        && smol::fs::remove_file(legacy_link_path)
            .await
            .log_err()
            .is_some()
    {
        needs_legacy_removal = false;
    }
    if !needs_link && !needs_legacy_removal {
        return Ok(Some(InstalledCli {
            path: link_path,
            migrated_legacy_link,
        }));
    }

    // The remaining changes could not be made without escalating, so use
    // osascript with admin privileges. The legacy link is re-checked to still
    // be a symlink and is only removed after the new link exists.
    let mut shell_commands = Vec::new();
    if needs_link {
        shell_commands.push(format!("mkdir -p {}", shell_quote(bin_dir_path)));
        shell_commands.push(format!(
            "ln -s {} {}",
            shell_quote(&cli_path),
            shell_quote(&link_path)
        ));
    }
    if needs_legacy_removal {
        let legacy = shell_quote(legacy_link_path);
        shell_commands.push(format!("{{ test ! -L {legacy} || rm {legacy}; }}"));
    }
    let output = smol::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                "do shell script \"{}\" with administrator privileges",
                shell_commands.join(" && "),
            ),
        ])
        .output()
        .await?;

    if output.status.success() {
        return Ok(Some(InstalledCli {
            path: link_path,
            migrated_legacy_link,
        }));
    }

    // osascript reports "User canceled." (error -128) when the administrator
    // prompt is dismissed. Treat that as a cancellation rather than a failure
    // so we don't show an error the user already chose to avoid.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("User canceled") || stderr.contains("-128") {
        if needs_link {
            return Ok(None);
        }
        // The new link is in place; only the legacy cleanup was declined.
        return Ok(Some(InstalledCli {
            path: link_path,
            migrated_legacy_link: false,
        }));
    }

    // The privileged write failed, most commonly because the user is not an
    // admin.
    anyhow::bail!("error running osascript: {}", stderr.trim());
}

pub fn install_cli_binary(window: &mut Window, cx: &mut Context<Workspace>) {
    const LINUX_PROMPT_DETAIL: &str = "If you installed Bex from our official release, the CLI is installed as ~/.local/bin/bex-desktop; add ~/.local/bin to your PATH.\n\nIf you installed Bex from a different source like your package manager, then you may need to create an alias/symlink manually.";

    cx.spawn_in(window, async move |workspace, cx| {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            let prompt = cx.prompt(
                PromptLevel::Warning,
                "CLI should already be installed",
                Some(LINUX_PROMPT_DETAIL),
                &["OK"],
            );
            cx.background_spawn(prompt).detach();
            return Ok(());
        }
        let installed = match install_script(cx.deref()).await {
            Ok(Some(installed)) => installed,
            // The user dismissed the administrator prompt; nothing to do.
            Ok(None) => return Ok(()),
            Err(error) => {
                log::error!("failed to install the bex-desktop CLI: {error:#}");
                workspace.update(cx, |workspace, cx| {
                    struct CliInstallFailed;

                    workspace.show_notification(
                        NotificationId::unique::<CliInstallFailed>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    "You can add `bex-desktop` to your PATH manually.",
                                    cx,
                                )
                                .with_title("Couldn't install the Bex CLI")
                                .more_info_message("Show me how")
                                .more_info_url(CANT_INSTALL_DOCS_URL)
                            })
                        },
                    );
                })?;
                return Ok(());
            }
        };

        workspace.update_in(cx, |workspace, _, cx| {
            if installed.migrated_legacy_link {
                struct MigratedLegacyCli;

                workspace.show_notification(
                    NotificationId::unique::<MigratedLegacyCli>(),
                    cx,
                    |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(
                                format!(
                                    "Installed `{CLI_COMMAND_NAME}` to {} and removed the old \
                                     `{LEGACY_LINK_PATH}` link: the `bex` command now belongs to \
                                     the bex platform CLI. Open files from your terminal with \
                                     `{CLI_COMMAND_NAME} <path>`, or `bex edit <path>` if the \
                                     bex platform CLI is installed.",
                                    installed.path.display()
                                ),
                                cx,
                            )
                            .with_title("The Bex terminal command is now `bex-desktop`")
                        })
                    },
                );
                return;
            }

            struct InstalledZedCli;

            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<InstalledZedCli>(),
                    format!(
                        "Installed `{CLI_COMMAND_NAME}` to {}. You can launch {} from your terminal.",
                        installed.path.display(),
                        ReleaseChannel::global(cx).display_name()
                    ),
                ),
                cx,
            )
        })?;
        register_zed_scheme(cx).await.log_err();
        Ok(())
    })
    .detach_and_prompt_err("Cannot install the bex-desktop CLI", window, cx, |_, _, _| None);
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI_PATH: &str = "/Applications/Bex.app/Contents/MacOS/cli";

    fn is_ours(link_target: &str, cli_path: &str) -> bool {
        let cli_path = Path::new(cli_path);
        link_points_into(
            Path::new(LEGACY_LINK_PATH),
            Path::new(link_target),
            owned_root(cli_path),
        )
    }

    #[test]
    fn owned_root_is_the_app_bundle() {
        assert_eq!(
            owned_root(Path::new(CLI_PATH)),
            Path::new("/Applications/Bex.app")
        );
        assert_eq!(
            owned_root(Path::new("/repo/target/debug/cli")),
            Path::new("/repo/target/debug/cli")
        );
    }

    #[test]
    fn legacy_link_into_this_bundle_is_ours() {
        assert!(is_ours(CLI_PATH, CLI_PATH));
        assert!(is_ours(
            "/Applications/Bex.app/Contents/MacOS/bex",
            CLI_PATH
        ));
        assert!(is_ours("/repo/target/debug/cli", "/repo/target/debug/cli"));
    }

    #[test]
    fn legacy_link_elsewhere_is_foreign() {
        assert!(!is_ours("/opt/homebrew/Cellar/bex/1.0.0/bin/bex", CLI_PATH));
        assert!(!is_ours("/Users/me/.local/bin/bex", CLI_PATH));
        assert!(!is_ours(
            "/Applications/Bex Preview.app/Contents/MacOS/cli",
            CLI_PATH
        ));
        assert!(!is_ours(
            "/Applications/Bex.app.old/Contents/MacOS/cli",
            CLI_PATH
        ));
        assert!(!is_ours("bex-platform", CLI_PATH));
        assert!(!is_ours(
            "/repo/target/debug/cli-other",
            "/repo/target/debug/cli"
        ));
    }

    #[test]
    fn legacy_link_target_is_checked_without_following_regular_files() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let bundle_cli = temp_dir.path().join("Bex.app/Contents/MacOS/cli");
        std::fs::create_dir_all(bundle_cli.parent().expect("cli has a parent"))
            .expect("create bundle");
        std::fs::write(&bundle_cli, "").expect("write cli");
        let platform_cli = temp_dir.path().join("platform-bex");
        std::fs::write(&platform_cli, "").expect("write platform cli");

        let ours = temp_dir.path().join("ours");
        std::os::unix::fs::symlink(&bundle_cli, &ours).expect("link ours");
        let foreign_link = temp_dir.path().join("foreign-link");
        std::os::unix::fs::symlink(&platform_cli, &foreign_link).expect("link foreign");
        let regular_file = temp_dir.path().join("regular");
        std::fs::write(&regular_file, "").expect("write regular file");
        let missing = temp_dir.path().join("missing");

        smol::block_on(async {
            assert!(is_our_legacy_link(&ours, &bundle_cli).await);
            assert!(!is_our_legacy_link(&foreign_link, &bundle_cli).await);
            assert!(!is_our_legacy_link(&regular_file, &bundle_cli).await);
            assert!(!is_our_legacy_link(&missing, &bundle_cli).await);
        });
    }
}
