# Every pipeline run cuts its own version

`release.yml` runs on every push to `main` (and on manual dispatch) and
mints its own version per run: the greater of the workspace manifest and
the latest `v*` tag, patch-bumped when the manifest is not ahead. Ordinary
runs only ever move the patch; a manually bumped minor or major in the root
`Cargo.toml` ships as-is. `scripts/mint-version.sh` is the one tool that
computes (`--next`) and applies (manifest, lockfile member entries,
AppStream releases block) that version, so the rule is runnable locally,
not just readable in YAML.

The bump is never committed back. Branch protection takes the five checks
on every `main` commit, and a bot-pushed bump would have none -- so each
build job mints into its own checkout instead (a pure function of the
version, proved by the `--locked` build that follows). The tag goes on the
built tree at publish time, after the builds pass, so a failed run leaves
no tag behind. One release runs at a time (top-level `concurrency`), since
versions come from the latest tag and two runs computing at once would mint
the same one. Real releases cut from `main` only; anything else must stay
a dry run.

Since the release now runs on every `main` push, its reuse of `ci.yml`
would share a concurrency group with the direct CI run on that same push
and cancel it (or be cancelled by it) on every merge. The group's key
therefore carries the workflow name -- the caller's, inside a called
workflow -- so direct and release-called runs cancel only their own kind.

The push trigger is now limited to the files that can reach the image
(`crates/**`, `Cargo.toml`, `Cargo.lock`, `build-appimage.sh`, `data/**`,
`scripts/mint-version.sh`): a push touching none of those runs no release
and mints no version, while `ci.yml` still checks it. `workflow_dispatch`
is unaffected, so pipeline changes are verified with a dry run.
