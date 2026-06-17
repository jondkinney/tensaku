# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.26.6](https://github.com/jondkinney/tensaku/compare/v0.26.5...v0.26.6) - 2026-06-17

### Added

- *(window)* also try the legacy Hyprland dispatcher for older versions
- *(window)* floor crop-resize width to keep the top bar one row
- *(window)* hold crop/grow window size across moves on Hyprland
- *(a11y)* arrow-key toolbar nav + Esc cancels crop from any control
- *(crop+ux)* aspect-locked arrow resize, sticky control focus, smarter top-bar wrap
- *(crop+ux)* view-window crop model, non-destructive transforms, full keyboard nav
- *(ux)* session batch — glyph tooltips, Pen/Counter, slider+cursor fixes, group move

### Fixed

- *(crop)* match the resize popover's units dropdown padding to the toolbar
- *(window)* drop the single-row floor's +8px buffer to close the tool gap
- *(crop)* scrollbars track the crop region, not the full image
- *(crop)* plain wheel adjusts the crop; mouse only touches crop handles
- *(window)* tighten the single-row floor to the packed toolbar width
- *(selection)* don't show handles for a hidden layer
- *(arrow)* curve to the top by default
- *(crop)* un-invert Shift+arrow resize under a locked aspect ratio

### Other

- *(crop)* renderer core for materialized-crop model (not yet wired)

## [0.26.5](https://github.com/jondkinney/tensaku/compare/v0.26.4...v0.26.5) - 2026-06-12

### Fixed

- *(toolbars)* dismiss swatch tooltips when the color picker closes

## [0.26.4](https://github.com/jondkinney/tensaku/compare/v0.26.3...v0.26.4) - 2026-06-12

### Fixed

- *(welcome)* force SpinButton repaint so detected scale pre-fills

## [0.26.3](https://github.com/jondkinney/tensaku/compare/v0.26.2...v0.26.3) - 2026-05-30

### Fixed

- *(doctor)* report envs.conf wiring, not just the live env

## [0.26.2](https://github.com/jondkinney/tensaku/compare/v0.26.1...v0.26.2) - 2026-05-30

### Fixed

- *(doctor)* accurate Omarchy reporting

## [0.26.1](https://github.com/jondkinney/tensaku/compare/v0.26.0...v0.26.1) - 2026-05-30

### Added

- *(omarchy)* --wire-omarchy also floats + centers the Tensaku window

## [0.26.0](https://github.com/jondkinney/tensaku/compare/v0.25.2...v0.26.0) - 2026-05-30

### Added

- auto-install and wire the Omarchy screenshot wrapper

## [0.25.2](https://github.com/jondkinney/tensaku/compare/v0.25.1...v0.25.2) - 2026-05-22

### Fixed

- *(scaling)* correct screenshot sizing & sharpness on fractional-scale monitors

## [0.25.1](https://github.com/jondkinney/tensaku/compare/v0.25.0...v0.25.1) - 2026-05-21

### Other

- add crate-level doc comment to main.rs
