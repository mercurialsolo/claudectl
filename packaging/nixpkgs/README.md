# nixpkgs Inclusion

This directory keeps the repo-side handoff for the eventual `nixpkgs` PR.

Unlike Homebrew and the AUR, the actual package definition must land in the
`nixpkgs` repository, typically at:

```text
pkgs/by-name/cl/claudectl/package.nix
```

## Suggested package expression

Use the same metadata as `flake.nix`, but switch the source to the tagged GitHub
release in the `nixpkgs` PR:

```nix
{ lib, rustPlatform, fetchFromGitHub }:

rustPlatform.buildRustPackage rec {
  pname = "claudectl";
  version = "0.16.0";

  src = fetchFromGitHub {
    owner = "mercurialsolo";
    repo = "claudectl";
    rev = "v${version}";
    hash = lib.fakeHash;
  };

  cargoHash = lib.fakeHash;

  meta = {
    description = "Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you.";
    homepage = "https://github.com/mercurialsolo/claudectl";
    license = lib.licenses.mit;
    mainProgram = "claudectl";
    platforms = lib.platforms.unix;
  };
}
```

## Submission flow

1. Copy the package expression into a `nixpkgs` checkout.
2. Build once with `lib.fakeHash` values to get the real `src.hash` and
   `cargoHash` suggestions from Nix.
3. Replace the fake hashes, rebuild, and confirm `claudectl --help` runs.
4. Run the normal `nixpkgs` validation tools for the new package.
5. Open the upstream `nixpkgs` PR and link it from issue `#82`.

## Repo-side status

- `flake.nix` is already aligned to `0.16.0`.
- Metadata matches the crate and release assets.
- Remaining work is the upstream `nixpkgs` submission itself.
