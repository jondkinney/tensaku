# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.29.0](https://github.com/jondkinney/tensaku/compare/v0.28.0...v0.29.0) - 2026-09-05

### Added

- Add an app-wide **Keep canvas size fixed** preference to clip annotations at the canvas boundary instead of expanding it. Automatic expansion remains the default. (#50)
- Open a file picker when Tensaku starts without arguments, and accept `tensaku photo.jpg` or `tensaku -` for stdin input. Clipboard annotation is documented with `wl-paste --type image/png | tensaku -`. (#56)
- Choose an output format in Save As, or convert by changing the filename extension. PNG, JPEG, WebP, AVIF, TIFF, and BMP are available when supported by the installed encoders. Conversion also works with `--output-filename`. (#54)

### Changed

- Expand the canvas onto a neutral background with a subtle shadow around the original screenshot, preserving its crisp edges.

### Fixed

- Keep the first-run welcome dialog above its editor so it remains visible and usable. (#52)
- Exclude selection handles, selection glow, text carets, and other editing decorations from saved images and clipboard copies. (#53)
- Preserve the input image format in Save As by default and add the correct extension when it is omitted. Unsupported source formats fall back to PNG. (#54)
- Preserve text wrapping after editing, allow committed text to be dragged beyond the canvas, and include the full text height when expanding the background.
- Remove seams along the expanded canvas at fractional zoom levels.
- Keep the editor open after a failed save and preserve the entered filename and format when retrying Save As.
- Remove routine startup diagnostics and the message for an absent optional CSS overrides file.

### Compatibility

- `tensaku_cli::CommandLine` adds an `input` field for positional filenames. Rust callers constructing this struct directly must supply the new field.

## [0.28.0](https://github.com/jondkinney/tensaku/compare/v0.27.0...v0.28.0) - 2026-08-21

### Added

- *(scroll-capture)* hands-off pointer — park once, pause on leave
- *(pin)* close the column gap behind a dragged-away pin
- *(pin)* right-click names a pin
- *(pin)* survivors close the gap when a pin closes
- *(omarchy)* --wire-capture-key binds a key to Tensaku's own capture

### Fixed

- *(scroll-capture)* park the pointer using the output's fractional scale
- *(pin)* delete the drag snapshot when the pin closes, not on destroy
- *(capture)* keep overlay chrome from taking the pointer's picks

## [0.27.0](https://github.com/jondkinney/tensaku/compare/v0.26.7...v0.27.0) - 2026-08-21

### Added

- *(pin)* place and stack pins on sway too
- *(pin)* degrade gracefully off Hyprland
- *(pin)* shape the pin like the display
- *(pin,capture)* square stacked pins, click-to-edit, untitled names
- *(capture)* the size pill takes the shot, and the puck matches
- *(capture)* a shutter button beside the restored region's puck
- *(capture)* the cursor says what a restored region will do
- *(capture)* grips and a move puck on a restored region
- *(capture)* adjust a restored region, and hide its measurement
- *(capture)* R restores the last region
- *(capture)* show the selection's pixel size while dragging
- *(scroll-capture)* the same pointer guides as the area capture
- *(capture)* A switches scrolling capture back to a normal one
- *(capture)* crosshair guides before the drag starts
- *(capture)* own the region selection, with window and mode switching
- *(pin)* tooltips, copy confirmations, and drag-out
- *(pin)* copy path saves the shot when it has none
- *(canvas)* the plain wheel walks whichever axis overflows
- *(pin)* pin the finished shot to the desktop
- *(spotlight)* Alt+wheel adjusts the loupe
- *(spotlight)* a magnification slider turns a spotlight into a loupe
- *(text)* an outlined style that reads over anything
- *(blur)* a Secure checkbox states what each mode can undo
- *(shapes)* f toggles fill on any fillable selection
- *(shapes)* a single r / e toggles a selected shape's fill
- *(canvas)* right-click an annotation to restack it
- *(stacking)* text lands above artwork, counters above text
- *(pointer)* plain wheel zooms when the Pointer is armed
- *(prefs)* remove the invert-scrolling preference
- *(canvas)* plain wheel sizes the next annotation
- *(counter)* preview the badge on the canvas, not in the cursor
- *(counter)* the cursor previews the badge it will stamp
- *(counter)* stamp the counter over text instead of grabbing it
- *(shapes)* shape tool buttons show their own fill, and widen the double-tap
- *(shapes)* double-tap a shape's key to toggle its fill, replacing F
- *(selection)* grab large annotations by their border, draw in their interior

### Fixed

- *(pin)* crop the preview to the capture's top, not its middle
- *(pin)* drop the mat and the rounded corner
- *(pin)* float, pin and place through the Lua dispatch API
- *(pin)* tooltips use the app's own, and the drag polls twice a frame
- *(pin)* the drag follows the pointer itself
- *(pin)* drag follows the pointer, and tooltips get out of the way
- *(pin)* slots follow where pins are, and moving one keeps up
- *(capture)* the shutter pill and puck carry their own cursors
- *(pin)* copy works, preview shows the shot, pins stack
- *(capture)* puck back to centre, size and shutter parked beneath it
- *(capture)* size readouts agree with the editor
- *(scroll-capture)* stop spawning hyprctl on every drag event
- *(capture)* hints along the bottom, not across the middle
- *(capture)* don't capture the overlay you just switched away from
- *(scroll-capture)* dim to the same weight as the area capture
- *(capture)* the area overlay wears the scroll capture's pill
- *(capture)* centre the area overlay's hint like the scroll one
- *(text)* the outline is always white
- *(capture)* crosshair pointer while aiming a region
- *(capture)* cache the backdrop; stop toasting the saved text style
- *(capture)* take the picture before the overlay exists
- *(capture)* let the overlay leave the screen before capturing it
- *(capture)* merge the --capture flag into the configuration
- *(pin)* back every pin with a file so the drag carries a path
- *(pin)* drag a thumbnail, not the whole capture
- *(pin)* ask where to save when nothing is configured
- *(pin)* render the pixels the pin needs
- *(text)* Ctrl+Shift+wheel reaches the outlined style
- *(spotlight)* make magnification global, like darkness
- *(spotlight)* render the loupe in the pass that renders spotlights
- *(blur)* stop the secure blur seeding itself from its own glow
- *(blur)* put Secure beside the picker, not inside it
- *(ellipse)* grab the curve, not the box around it
- *(picking)* size the border-grab band in screen pixels
- *(shapes)* apply the r / e fill toggle instead of dropping it
- *(canvas)* stop inverting the compositor's scroll delta
- *(counter)* tighten the badge's lead on the pointer
- *(counter)* system cursor, offset badge
- *(counter)* move the pointer off the number it previews
- *(counter)* the cursor over text matches what the click does
- *(shapes)* keep each shape's fill toggle to that shape
- *(toolbar)* bundle the shape buttons' filled glyphs
- *(selection)* dismiss a selection before drawing inside a large annotation
- *(text)* clicking away from an in-progress text only ends it
- *(text)* grab an existing text box rather than stacking a new one on it
- *(input)* ignore modifiers left over from the launch chord
- *(arrow)* thicken only the tail when zoomed out
- *(canvas)* stop the expanded background showing a line per expansion
- *(omarchy)* opt out of default window opacity, wiring through Lua
- *(selection)* shrink the selection halo with the artwork when zoomed out
- *(canvas)* settle the edge extension away from the boundary
- *(canvas)* extend the image edge per line instead of one flat colour
- *(text)* wrap auto-fit text at the image edge while typing
- *(stitch)* keep a fixed edge's drop shadow out of every seam
- *(stitch)* crop viewport-fixed bands out of the motion search
- *(scroll-capture)* let the page inside a selected region take wheel and keys
- *(scroll-capture)* target the overlay's monitor for capture and pointer warps
- *(render)* tile the spotlight overlay past GL texture limits

### Other

- *(region-capture)* run cargo fmt
- *(pin)* let the compositor move the pin
- *(pin)* lead the pointer by the latency instead of chasing it
- *(pin)* read the pointer off-thread, move on the frame clock
- *(capture)* composite the selection instead of painting it
- Revert "feat(pointer): plain wheel zooms when the Pointer is armed"
- drop the two removed preferences from the README
- *(canvas)* add probes for tile seams and flat-render uniformity
- *(canvas)* grow the raster by re-viewing it, not by copying it
- *(canvas)* add a grow-raster digest harness
- *(canvas)* reuse background textures across an auto-grow
- *(blur)* read back only the blurred region, and stop leaking textures
- *(canvas)* stop copying the raster twice on every re-upload
- *(canvas)* add an env-gated frame profiler
- update star history chart
- update star history chart
- update star history chart
- update star history chart
- *(stitch)* use is_multiple_of in the sparse-doc fixture
- *(stitch)* replay harness takes TENSAKU_STITCH_REPLAY_AXIS
- update star history chart

## [0.26.7](https://github.com/jondkinney/tensaku/compare/v0.26.6...v0.26.7) - 2026-08-15

### Added

- *(scroll-capture)* persist the restorable region on every shape commit
- *(config)* config.toml as the canonical store for GUI preferences
- *(scroll-capture)* manual scrolling, faster auto capture, robust stitching
- *(capture)* force opaque screencopy output with format regression tests
- add version label to preferences dialog

### Fixed

- *(scroll-capture)* hold an exclusive keyboard grab in every interactive state
- *(scroll-capture)* deliver Esc and the restore key before a region is dragged
- *(render)* tile textures past GL limits; cancel spring-back timer on unrealize

### Other

- *(readme)* document scroll capture, unified preferences, and new config keys
- update star history chart
- self-host star history chart

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
