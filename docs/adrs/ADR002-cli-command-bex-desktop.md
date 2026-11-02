# ADR002: Install the editor CLI as `bex-desktop`

- **Status:** Accepted
- **Date:** 2026-11-02
- **Scope:** the terminal command name of the bex-desktop editor launcher (Zed's `zed` CLI), the stable binary locations other tools may rely on, and the one-time migration of existing installs

## Context

bex-desktop inherits Zed's editor-launcher CLI (`crates/cli`), which the fork renamed to `bex`. The bex platform CLI (the Go Render-CLI launcher in [bex-co/bex](https://github.com/bex-co/bex), `lego/cli/`) is also called `bex`. Both install a `bex` into the same places:

| Installer                                 | Path                       | How                                                                        |
| ----------------------------------------- | -------------------------- | -------------------------------------------------------------------------- |
| bex-desktop "Install CLI" action (macOS)  | `/usr/local/bin/bex`       | symlink to `Bex.app/Contents/MacOS/cli` (`crates/install_cli`)             |
| bex-desktop `script/install.sh` (Linux)   | `~/.local/bin/bex`         | symlink to `~/.local/bex*.app/bin/bex`                                     |
| bex-desktop `script/install.sh` (macOS)   | `~/.local/bin/bex`         | symlink to `/Applications/Bex*.app/Contents/MacOS/cli`                     |
| bex platform CLI `scripts/install-bex.sh` | `~/.local/bin/bex`         | `mv -f` of the downloaded binary — silently replaces the desktop's symlink |
| bex platform CLI Homebrew formula         | `$(brew --prefix)/bin/bex` | regular binary                                                             |

What `bex` runs therefore depends on install order and `PATH` order.

## Decision

1. **`bex` belongs to the bex platform CLI.** The desktop's editor launcher is installed on `PATH` as **`bex-desktop`**. The app name ("Bex"), bundle ids (`co.bex.Bex*`), the `bex://` URL scheme, the `--bex`/`--zed` flags, settings/data directories, and `bex_*` crates are unchanged.
2. **The platform CLI opens the editor by exec'ing `bex-desktop`.** It adds `bex edit <path>…` and a path-shaped shorthand `bex <path>` (an argument that is `.`, starts with `/`, `./`, `../`, `~`, or names an existing path) that pass all remaining arguments through unchanged.
3. **No `bex` alias is kept** on macOS/Linux; an alias would recreate the collision.

### Stable contract for the platform CLI

The platform CLI resolves the editor launcher in this order and execs the first hit:

1. `bex-desktop` on `PATH` — `/usr/local/bin/bex-desktop` (macOS in-app "Install CLI"), `~/.local/bin/bex-desktop` (`script/install.sh` on Linux and macOS).
2. macOS app bundle: `<app>/Contents/MacOS/cli`, where `<app>` is `/Applications/Bex.app` or `~/Applications/Bex.app` (channel builds: `Bex Preview.app`, `Bex Nightly.app`, `Bex Dev.app`; bundle id `co.bex.Bex[-Preview|-Nightly|-Dev]` for a Spotlight lookup). An auto-updated install may still live at `Zed.app`; match by bundle id rather than name when it matters. `Contents/MacOS/bex` is the editor binary itself, **not** the launcher — never exec it with CLI arguments.
3. Linux tarball install: `~/.local/bex.app/bin/bex` (channel builds `~/.local/bex-<channel>.app/bin/bex`; builds before 0.139 shipped `bin/cli`). The bundle-internal file stays named `bin/bex` and is never on `PATH`.

These locations are stable; changing any of them requires a new ADR and a platform CLI release.

Argument passthrough: the launcher accepts everything the desktop CLI does, including `path`, `path:line`, `path:line:col`, `-n`/`--new`, `-a`/`--add`, `-e`/`--existing`, `-w`/`--wait` (exit status follows the editor), `-` (read stdin), `--diff OLD NEW`, and URLs with `bex://`, `zed://`, `file://`, `http(s)://`, or `ssh://user@host/path` prefixes. The platform CLI must forward argv verbatim and must not resolve paths itself — the launcher resolves relative paths against its own working directory, which it inherits from the exec.

`bex-desktop --completions <shell>` names completions `bex-desktop` regardless of which file was executed, so they never shadow the platform CLI's `bex` completions.

## Migration

One-time and conservative: a legacy `bex` is removed **only** if it is a symlink into this app; regular files and symlinks to anything else (the platform CLI, Homebrew) are never touched.

- **macOS "Install CLI" action** (`crates/install_cli/src/install_cli_binary.rs`): creates `/usr/local/bin/bex-desktop` (refusing to replace a foreign file, as before), then removes `/usr/local/bin/bex` if its target — as stored, or after canonicalization — lies inside the running app's `.app` bundle (for an unbundled dev build: equals the `cli` binary). When privileges are needed, a single administrator prompt creates the new link and removes the old one, re-checking that it is still a symlink. A one-time notification explains that the command is now `bex-desktop` and that `bex edit <path>` also works. There is no startup auto-install, so existing users migrate the next time they run "Install CLI".
- **`script/install.sh`**: links `~/.local/bin/bex-desktop`, then removes `~/.local/bin/bex` if it is a symlink to `$HOME/.local/bex*.app/bin/{bex,cli}` or `/Applications/Bex*.app/Contents/MacOS/cli`, and prints a one-line notice.
- **`script/uninstall.sh`**: removes both `~/.local/bin/bex-desktop` and a legacy `~/.local/bin/bex` that points into the uninstalled app.

**Removal:** the migration code is marked `TODO` and is deleted a few releases after the rename ships, once installs predating it are no longer expected.

## Consequences

- `bex` is unambiguous on macOS and Linux; install order no longer matters.
- Muscle memory breaks once: users who typed `bex .` now type `bex-desktop .` or, with the platform CLI installed, keep typing `bex .` (shorthand) / `bex edit .`.
- **Windows is out of scope.** The installer still puts `bin\bex.exe` (and `bin\zed.exe`) on `PATH`, and `auto_update_helper` swaps those exact file names. The platform CLI has no Windows installer today; renaming the Windows shim needs a coordinated installer + updater change and its own migration.
- Snap/Flatpak manifests keep their internal `bex` names; they are not published and are sandbox-scoped.

## Alternatives considered

- **Keep `bex` for the desktop, rename the platform CLI** — rejected: the platform CLI is the product's primary terminal surface (Render CLI parity); the editor launcher is secondary.
- **Install both under `bex` with a dispatcher** — rejected: two independently versioned projects would fight over one file.
- **Keep a `bex` compatibility alias for the desktop** — rejected: it is exactly the collision being removed.
