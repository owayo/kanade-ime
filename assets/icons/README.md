# Icon assets

`app.png` is the application and settings artwork. The settings header uses it at 44 points.

`input-template.png` is the user-selected rounded-square Mincho mark from [concept 01](designs/rounded-mincho-v1/variant-01.png), exported with the built-in image generation tool. Its black silhouette has a transparent background and a transparent character-shaped cutout; it is an alpha mask, not a black background. No font is loaded to render the mark. The [export prompt](designs/rounded-mincho-v1/export-prompt.md) records the production request.

macOS packaging crops the master to a 1,060-pixel square at offset (97, 112), removing its outer transparent padding while preserving the selected artwork. Every input-source representation is resampled directly from this crop. `KanadeInputMenu.tiff` contains 16-pixel and 32-pixel representations at 72 and 144 DPI, both representing 16 points. The visible mark fills that footprint, matching the bundled macOS Japanese IME icons. `KanadeInputMenu.icns` uses the same crop. The parent source and Japanese mode both declare template rendering so the system can choose the foreground color for its background. Application and settings artwork retain their existing scale.

The earlier `tray-*.png` koto artwork is retained as source history; current macOS input-source packaging uses the selected Mincho mark.

Windows packaging resamples `app.png` into the icon of the Start menu shortcut with `packaging/windows/build-icon.ps1`: bitmaps of 16 to 64 pixels and a 256-pixel PNG. The あ and A of the Windows taskbar are not artwork. The text service draws them when the taskbar asks, in the colour of the taskbar text.

## Windows brand icon

Windows shows an input method's brand icon in the input switcher (Win+Space) and in the taskbar's input indicator. Microsoft's [IME requirements](https://learn.microsoft.com/en-us/windows/apps/develop/input/input-method-editor-requirements) define it as a white box holding a black glyph, with an outer stroke of one pixel in black at half opacity. They ask for 16, 20, 24, 32, 40 and 48 pixels, each 32-bit with an alpha channel, stored in the DLL instead of a standalone `.ico` file.

`input-template.png` is the source for this icon too. `crates/kanade-winres` converts it while the input DLL is built. The silhouette becomes the white box and the cutout becomes the black glyph. The stroke is added around the silhouette, so the glyph keeps the margins it has in the artwork, and the stroke follows the rounded corners. The box fills each size except for that stroke. Every size is sampled by area directly from the master. Clear and solid areas of the master are treated as exact: its faint noise and its alpha of 253–254 do not tint the result.

`crates/kanade-tip/build.rs` links the six bitmaps into `kanade_tip.dll` as the DLL's only icon group, for the 64-bit and the 32-bit build alike. No resource compiler runs and no generated file is tracked. Registration names the icon by position zero.

To look at the result, run:

```sh
cargo run -p kanade-winres --example brand-icon-preview -- <output directory>
```

It writes `kanade-brand.ico`, with the same bitmaps as the DLL, and `kanade-brand-preview.png`, which shows every size on the light and the dark taskbar colour.

The horizontal strokes of the Mincho glyph are about 3% of the box, which is less than a pixel up to 24 pixels. Sampled as they are, they turn into grey bands two pixels high. Two adjustments keep them dark. A size that would draw the thin strokes narrower than three quarters of a pixel widens them to that. Every size also moves the glyph by less than half a pixel, to where its strokes fall on whole pixels. Both are derived from the template, not tuned by hand: the thin stroke is the most common run of glyph pixels, and the move is the one that leaves the fewest half-covered pixels. At 16 pixels the character is still dense. A cleaner small size would need separately drawn artwork.
