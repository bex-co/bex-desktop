---
name: merge-upstream
description: Merge the latest zed-industries/zed upstream changes into the bex-desktop fork, resolving conflicts so that bex customizations are preserved on top of upstream's intent. Use when the user asks to merge, sync, or pull in upstream Zed changes.
---

# Merge Zed Upstream

bex-desktop is a fork of Zed with a small set of bex-specific commits carried on top. This skill brings in the latest upstream Zed changes via a true merge (no history rewrite), keeping the bex customizations intact.

## Remote layout — read this first

The remote names are the opposite of what you might assume:

- `origin` → `zed-industries/zed` — **upstream Zed. Never push here.**
- `bex-desktop` → `bex-co/bex-desktop` — the fork. All pushes go here.

Verify with `git remote -v` before doing anything; do not assume this mapping holds forever.

The local `main` branch mirrors upstream (`origin/main`) and carries no bex work. The bex commits live on the fork's `main` (`bex-desktop/main`) and on local working branches cut from it. The merge target is the branch that carries the bex commits — by default `bex-desktop/main` — **not** the local upstream-mirror `main`.

## What is bex-specific

Discover the current bex delta rather than trusting this list (it grows over time):

```
git log --oneline origin/main..bex-desktop/main
git diff --name-only origin/main...bex-desktop/main
```

As of this writing the delta is bex OIDC sign-in replacing Zed Cloud auth, plus the Muse Code agent:

- `crates/bex_auth/` — bex-only crate, does not exist upstream
- `crates/client/src/client.rs`, `crates/client/src/user.rs` — upstream files with bex auth modifications (the main conflict hotspots)
- `crates/oauth_callback_server/src/oauth_callback_server.rs` — modified for the loopback PKCE flow
- `Cargo.toml` / `Cargo.lock` — workspace member + deps for `bex_auth`
- `assets/settings/default.json` — bex default settings
- `README.md` — carries the `> [!IMPORTANT]` review-marker lines at the top

## Procedure

### 1. Preflight

Require a clean worktree (`git status`). Fetch only what you need — this repo is large:

```
git fetch origin main
git fetch bex-desktop main
```

Note the range you are about to merge:

```
git rev-list --count bex-desktop/main..origin/main
git log --oneline bex-desktop/main..origin/main | head -20
```

### 2. Create the merge branch and merge

```
git checkout -B merge-upstream-$(date +%Y-%m-%d) bex-desktop/main
git merge origin/main --no-edit
```

Use a merge, not a rebase: the fork's `main` is shared, and rewriting it would break everyone tracking it.

If the merge is clean, skip to Step 5.

### 3. Resolve conflicts

General principle: **adopt upstream's new structure and behavior, then re-apply the bex divergence on top of it.** Read the bex commits first (`git log origin/main..bex-desktop/main` and `git show` each) so you understand what the fork intends before picking sides.

Per-file guidance:

- **`crates/bex_auth/`** — bex-only; upstream never touches it. Conflicts here mean something is wrong; keep ours.
- **`crates/client/src/client.rs`, `user.rs`, `oauth_callback_server.rs`** — take upstream's refactors and re-apply the bex changes (bex OIDC endpoints and userinfo instead of Zed Cloud) inside the new structure. Do not discard upstream renames/moves to keep the old bex patch shape.
- **`Cargo.lock`** — never hand-merge lockfile hunks. Resolve `Cargo.toml` first (keep the `bex_auth` workspace member and its deps plus upstream's changes), then take upstream's lockfile and let cargo regenerate the bex entries:
  ```
  git checkout --theirs Cargo.lock
  cargo metadata --format-version 1 > /dev/null
  git add Cargo.lock
  ```
- **`README.md`** — keep the two `> [!IMPORTANT]` marker lines at the very top (fork convention, required by `CLAUDE.md`); take upstream's changes below them.
- **`assets/settings/default.json`** — keep bex's divergent defaults, adopt upstream's new keys.

### 4. Validate

At minimum, check the crates that had conflicts plus the bex crate:

```
cargo check -p bex_auth -p client -p oauth_callback_server
```

Run `cargo test -p <crate>` for conflicted crates when reasonable, and `./script/clippy` (not `cargo clippy`) if the merge touched many crates. Do not conclude the merge with a broken build — fix the resolution or `git merge --abort` and report.

### 5. Push and open the PR

Push to the **fork**, never to `origin`:

```
git push bex-desktop merge-upstream-<date>
```

`gh` may default to `zed-industries/zed` because that is `origin` — always pass the repo explicitly:

```
gh pr create --repo bex-co/bex-desktop --base main \
  --title "Merge zed upstream (<upstream short sha>)" \
  --body-file /tmp/merge-body.md
```

The body should list the upstream range merged (`old..new` shas and commit count), each conflicted file with a one-line resolution summary, and the validation commands run. End with a `Release Notes:` section (`- N/A` unless upstream brings user-facing changes worth calling out).

If the user asked to push directly instead of opening a PR, fast-forward the fork's main:

```
git push bex-desktop merge-upstream-<date>:main
```

## Final report to the user

- The upstream range merged (commit count, old and new upstream shas).
- Every conflicted file and how it was resolved.
- Validation commands and results.
- The PR URL (or the pushed branch), and which local branch you left checked out.

## Gotchas

- **`origin` is upstream, not the fork.** `git push origin` / bare `gh pr create` target `zed-industries/zed`. Every push and every `gh` call must name `bex-desktop` / `--repo bex-co/bex-desktop`.
- **Local `main` is an upstream mirror.** Do not commit bex work (including this merge) to it.
- **Fetch narrowly.** `git fetch origin` without a refspec pulls hundreds of upstream branches.
- **Never hand-merge `Cargo.lock`** — resolve `Cargo.toml`, then regenerate (Step 3).
- **Non-interactive git**: use `--no-edit` on merge and `GIT_EDITOR=true` on any command that would open an editor.
- **Keep the README marker.** Upstream README edits will conflict with the fork's `> [!IMPORTANT]` lines; the marker always stays on top.
