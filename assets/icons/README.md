# Icon assets

`app.png` is the application and settings artwork. The settings header uses it at 44 points.

`input-template.png` is the user-selected rounded-square Mincho mark from [concept 01](designs/rounded-mincho-v1/variant-01.png), exported with the built-in image generation tool. Its black silhouette has a transparent background and a transparent character-shaped cutout; it is an alpha mask, not a black background. No font is loaded to render the mark. The [export prompt](designs/rounded-mincho-v1/export-prompt.md) records the production request.

macOS packaging crops the master to a 1,060-pixel square at offset (97, 112), removing its outer transparent padding while preserving the selected artwork. Every input-source representation is resampled directly from this crop. `KanadeInputMenu.tiff` contains 16-pixel and 32-pixel representations at 72 and 144 DPI, both representing 16 points. The visible mark fills that footprint, matching the bundled macOS Japanese IME icons. `KanadeInputMenu.icns` uses the same crop. The parent source and Japanese mode both declare template rendering so the system can choose the foreground color for its background. Application and settings artwork retain their existing scale.

The earlier `tray-*.png` koto artwork is retained as source history; current macOS input-source packaging uses the selected Mincho mark.
