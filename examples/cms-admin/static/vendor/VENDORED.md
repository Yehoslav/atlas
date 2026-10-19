# Vendored assets

These files are vendored so the dashboard works offline and has no frontend
build step. Update them deliberately, and keep the upstream license beside the
file when upgrading.

| File | Upstream | Version | License |
|---|---|---:|---|
| `htmx.min.js` | [bigskysoftware/htmx](https://github.com/bigskysoftware/htmx) | 2.0.10 | Zero-Clause BSD, see `HTMX-LICENSE.txt` |
| `catppuccin.css` | [catppuccin/palette](https://github.com/catppuccin/palette) | 1.8.0 | MIT, see `CATPPUCCIN-LICENSE.txt` |

Only the Mocha variables are used. The upstream palette file contains the four
official flavours; keeping the canonical file avoids copying or subtly changing
the palette, while this dashboard deliberately has no theme switch.
