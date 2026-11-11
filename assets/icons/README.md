# STFN app icon

`stfn.svg` is the source. The raster sizes are generated with:

```bash
for s in 1024 512 256 128 64 48 32; do
  magick -background none stfn.svg -resize ${s}x${s} -depth 8 -strip stfn-${s}.png
done
```

Install to the hicolor icon theme (for niri overview / desktop entries):

```bash
for s in 32 48 64 128 256 512 1024; do
  install -Dm644 assets/icons/stfn-$s.png \
    ~/.local/share/icons/hicolor/${s}x${s}/apps/stfn.png
done
install -Dm644 assets/stfn.desktop ~/.local/share/applications/stfn.desktop
```
