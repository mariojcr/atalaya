# Releasing

Versions follow [Semantic Versioning](https://semver.org/) and are driven by
[Conventional Commits](https://www.conventionalcommits.org/) through
[Commitizen](https://commitizen-tools.github.io/commitizen/) (`cz`).

## Commit messages

```
feat(collector): read battery health          → minor version
fix(promql): reject ambiguous vector matches  → patch version
feat!: rename the overview.yaml keys          → major version (breaking change)
docs: … · refactor: … · test: … · ci: … · chore: …   → no release on their own
```

The scope (`collector`, `ui`, `promql`, `alerts`, `docs`…) is optional. While
the version is `0.x`, breaking changes bump the minor version.

## Cutting a release

The server and the collector share one version: a release tags both images.

```sh
pipx install commitizen          # once
cz bump --dry-run                # shows the next version and changelog
cz bump                          # bumps Cargo.toml/Cargo.lock, updates CHANGELOG.md, commits and tags vX.Y.Z
git push --follow-tags
```

The tag runs `.github/workflows/build.yml`, which:

1. checks that the tag is on `main` and matches the version in `Cargo.toml`;
2. checks formatting, lints and tests;
3. builds both images, checks that the binaries start, and pushes
   `ghcr.io/mariojcr/atalaya-server` and `ghcr.io/mariojcr/atalaya-collector`
   (linux/amd64);
4. creates the GitHub release with the notes of that version from
   `CHANGELOG.md`.

Every push to `main` also builds and pushes the images, tagged `main` and
`sha-<commit>`; pull requests run the checks only.

## Image tags

| Tag | Points to |
|---|---|
| `X.Y.Z` | that release |
| `X.Y` | the latest patch of that minor version |
| `X` | the latest release of that major version (from 1.0 on) |
| `latest` | the latest release (not pre-releases such as `vX.Y.Z-rc.1`, which get only their own tag) |
| `main`, `sha-<commit>` | builds of `main`, between releases |
