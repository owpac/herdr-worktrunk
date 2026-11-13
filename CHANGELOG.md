# Changelog

The changes of each release of herdr-worktrunk. The workflow release-pr writes this file.

## [2026.11.36](https://github.com/owpac/herdr-worktrunk/releases/tag/2026.11.36) - 2026-11-13

### Bug fixes

- the observer runs the binary of a new version at once after an install ([#40](https://github.com/owpac/herdr-worktrunk/pull/40))
- the observer follows the active version that herdr gives, in place of a hand-over at the build ([#42](https://github.com/owpac/herdr-worktrunk/pull/42))

## [2026.11.35](https://github.com/owpac/herdr-worktrunk/releases/tag/2026.11.35) - 2026-11-13

### Bug fixes

- install.sh downloads the release binary when the checkout is the commit of the tag ([#38](https://github.com/owpac/herdr-worktrunk/pull/38))

## [2026.11.34](https://github.com/owpac/herdr-worktrunk/releases/tag/2026.11.34) - 2026-11-12

### Features

- the popups take the colors of the herdr theme ([#36](https://github.com/owpac/herdr-worktrunk/pull/36))

## [2026.11.33](https://github.com/owpac/herdr-worktrunk/releases/tag/2026.11.33) - 2026-11-12

### Features

- the command remove --path removes a worktree for another plugin ([#34](https://github.com/owpac/herdr-worktrunk/pull/34))

### Bug fixes

- the observer keeps the workspace of a worktree when the agent leaves it ([#35](https://github.com/owpac/herdr-worktrunk/pull/35))

### Documentation

- remove the origin of the build and release method ([#32](https://github.com/owpac/herdr-worktrunk/pull/32))

## [2026.11.32](https://github.com/owpac/herdr-worktrunk/releases/tag/2026.11.32) - 2026-11-11

### Build and CI

- a release pull request writes the version and the changelog, like mise ([#30](https://github.com/owpac/herdr-worktrunk/pull/30))
