# homebrew-core Submission

This directory holds the formula draft that will land in
[`Homebrew/homebrew-core`](https://github.com/Homebrew/homebrew-core) — *not*
the tap formula at `mercurialsolo/homebrew-tap`, which keeps shipping
prebuilt-binary tarballs as the fast install path.

The core formula is source-built, runs through Homebrew's CI bottle pipeline,
and is what `brew install claudectl` resolves to once accepted.

## Submitting

1. Bump `url` and `sha256` in `claudectl.rb` to the release you want to ship.
   ```sh
   curl -fsSL https://github.com/mercurialsolo/claudectl/archive/refs/tags/vX.Y.Z.tar.gz \
     | shasum -a 256
   ```
2. Locally:
   ```sh
   brew install --build-from-source ./packaging/homebrew-core/claudectl.rb
   brew test claudectl
   brew audit --strict --new --online claudectl
   ```
   All three must pass cleanly before opening the PR.
3. Fork `Homebrew/homebrew-core`, drop the file at `Formula/c/claudectl.rb`,
   and open a PR following the
   [Adding Software to Homebrew](https://docs.brew.sh/Adding-Software-to-Homebrew)
   checklist. Mention this repo and a recent release in the description.

## What this formula does differently from the tap

| Aspect                | Tap (`homebrew-tap`)             | Core (`homebrew-core`)                 |
| --------------------- | -------------------------------- | -------------------------------------- |
| Source                | Prebuilt release tarballs        | GitHub source tarball, built via Cargo |
| Bottles               | None                             | Built by Homebrew CI                   |
| Test                  | `--version` smoke test           | Completions + man + sandboxed `--list` |
| Auto-version tracking | Manual via `release.yml`         | `livecheck` block (`:github_latest`)   |
| Completions / man     | Not installed                    | Installed for bash/zsh/fish + `man1`   |

## When updating

After the formula is accepted into core, Homebrew's auto-bumper handles version
updates on each tag via the `livecheck` block. We only touch the formula here
if the install layout or test surface changes.
