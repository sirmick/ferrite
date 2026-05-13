# tools/assets/

Vendored static assets used by the helper scripts in `tools/`.

## `bluemarble_2048.jpg` (2048 × 1024 equirectangular, ~260 KB)

NASA Blue Marble — land / ocean / ice composite, equirectangular
projection, 2048 × 1024. Used by `ft8_worldmap.py` as the basemap
under FT8 grid-square markers.

- **Source.** Wikimedia Commons, file
  [`Land_ocean_ice_2048.jpg`](https://commons.wikimedia.org/wiki/File:Land_ocean_ice_2048.jpg).
- **License.** Public domain. Image produced by NASA Earth
  Observatory (the "Blue Marble" Land / Ocean / Sea Ice mosaic);
  NASA imagery is generally in the public domain. The Wikimedia
  Commons mirror restates this.
- **Why vendored.** The map renderer needs a basemap and we don't
  want a first-run network fetch (operator may be offline, or in a
  sandbox that blocks egress). 260 KB is cheap.
