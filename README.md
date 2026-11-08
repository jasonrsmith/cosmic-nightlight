# cosmic-nightlight

A night light (blue-light filter) for the [COSMIC](https://system76.com/cosmic)
desktop on Wayland, done the way GNOME, KDE and macOS do it: by reprogramming
the display's **hardware gamma table** (`GAMMA_LUT`), not by drawing a tinted
layer on top of the screen. Blacks stay black, nothing gets hazy, it costs
nothing per frame, and it doesn't show up in screenshots.

> **Status: experimental.** The compositor patch builds against cosmic-comp
> `ec674cd` (Pop!_OS 24.04 packages, November 2026). The Wayland side has been
> tested in a nested compositor; the final hardware write has so far only been
> checked by reading the GPU's properties (Intel UHD 630, `GAMMA_LUT_SIZE` 1024).
> Keep the restore command below handy the first time you try it.

## Why a compositor patch?

Redshift only works on X11. On Wayland, tools like gammastep and wlsunset
need the compositor to support the `wlr-gamma-control-unstable-v1` protocol,
and stock cosmic-comp doesn't (yet). COSMIC Settings has strings for a
built-in night light, but it hasn't shipped.

This repo has two parts:

1. **`compositor/`**: a patch that adds `wlr-gamma-control-unstable-v1` to
   cosmic-comp, plus scripts to build it for the exact version you have
   installed and swap it in. Once it's in, gammastep and wlsunset also work.
2. **`cosmic-nightlight`**: a small daemon that follows sunset and sunrise for
   your location and sets the gamma ramps over that protocol.

## Colour maths

- The white point for a colour temperature comes from the CIE Planckian locus
  (Kim et al. 2002 cubic-spline approximation): xyY → XYZ → linear sRGB,
  normalised so `temp_day` (6500 K) is exactly neutral.
- Channel scaling is applied in **linear light** (decode sRGB → scale →
  re-encode), so shadows keep their hue instead of going muddy.
- Optional dimming (`brightness_night`) is also applied in linear light.
- Changes fade smoothly and are paced in mireds (1/K), so every step of a
  fade looks equally large.
- The transition follows the sun's elevation (full day above +3°, full night
  below −6°), like Redshift.

## Requirements

- COSMIC from the Pop!_OS / System76 apt packages (the scripts read the
  installed `cosmic-comp` package version to know which commit to build).
- Rust (`cargo`), `git`, and `sudo` access for the install step. The
  compositor's build headers are downloaded with `apt-get download` and
  unpacked privately; nothing gets installed system-wide except the final
  binary.

## Install

```sh
git clone https://github.com/jasonrsmith/cosmic-nightlight
cd cosmic-nightlight

# 1. The daemon
cargo build --release
install -m755 target/release/cosmic-nightlight ~/.local/bin/
cosmic-nightlight status            # creates ~/.config/cosmic-nightlight/config
$EDITOR ~/.config/cosmic-nightlight/config   # set lat/lon to your location!
cosmic-nightlight install-autostart # start at login

# 2. The compositor (builds in ~/src/cosmic-comp; takes 10-20 minutes)
./compositor/rebuild.sh
sudo ./compositor/install.sh        # backs up the original, then log out and in
```

Optional keyboard shortcut: in COSMIC Settings → Keyboard → Keyboard
Shortcuts → Custom, add `~/.local/bin/cosmic-nightlight toggle` (use the full
path), for example on Super+Ctrl+N.

### After COSMIC updates

A COSMIC update puts the stock compositor back. Nothing breaks; the night
light just stops working and the daemon shows a notification at login. Run
`./compositor/rebuild.sh` and `sudo ./compositor/install.sh` again.

### Going back / recovery

```sh
sudo ./compositor/install.sh --restore   # then log out and in
```

If the desktop ever fails to start after installing the patched compositor,
press Ctrl+Alt+F3, log in on the text console, run the command above, and
go back with Ctrl+Alt+F1 (or reboot). The original binary is kept at
`/usr/bin/cosmic-comp.stock`.

## Usage

```
cosmic-nightlight                    run, following sunset/sunrise
cosmic-nightlight --temp 3400        run with a fixed temperature (preview)
cosmic-nightlight toggle             pause/resume with a fade (starts it if needed)
cosmic-nightlight reload             re-read the config
cosmic-nightlight stop               fade out and stop
cosmic-nightlight restart            stop and start again
cosmic-nightlight status             sun elevation, scheduled temperature, channel gains
cosmic-nightlight install-autostart  start at login
```

Config (`~/.config/cosmic-nightlight/config`):

```ini
lat = 51.48              # your latitude
lon = 0.0                # your longitude (west is negative)
temp_day = 6500          # Kelvin; 6500 = neutral
temp_night = 3700
brightness_day = 1.0     # 0.1-1.0, applied in linear light
brightness_night = 1.0
elevation_day = 3        # sun elevation (degrees) where full day starts
elevation_night = -6     # ... and where full night starts
fade_seconds = 1.5
```

## How the compositor patch works

- A new `zwlr_gamma_control_manager_v1` global, visible only to
  non-sandboxed clients (same policy as cosmic-comp's other privileged
  protocols).
- One client at a time may control each output. Destroying the control (or
  the client exiting or crashing) restores the default gamma.
- The ramp is handed to each output's render thread, which sets the CRTC's
  `GAMMA_LUT` property between frames. Bursts of updates (fades) are merged so
  only the newest ramp is written. The ramp is re-applied after VT switches,
  and resampled if a mirrored output's LUT size differs.
- Also includes upstream commit `cb86b9e` ("kms/device: Reset CRTC GAMMA_LUT
  during init"), cherry-picked if your version doesn't have it yet.

## License

GPL-3.0-only (the compositor patch is a derivative of cosmic-comp, which is
GPL-3.0-only). See [LICENSE](LICENSE).
