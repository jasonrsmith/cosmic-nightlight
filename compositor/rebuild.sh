#!/bin/sh
# (Re)build cosmic-comp with wlr-gamma-control support, for the exact version
# of cosmic-comp that is installed. Run as your normal user (no sudo needed);
# afterwards run:  sudo ./compositor/install.sh
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
SRC_DIR=${SRC_DIR:-$HOME/src/cosmic-comp}
SYSROOT=${SYSROOT:-$HOME/src/cosmic-sysroot}
UPSTREAM_GAMMA_RESET=cb86b9ec6f18ca2d2ad3b39426a57ab850867070

pkg_commit=$(dpkg-query -W -f='${Version}' cosmic-comp | sed 's/.*~//')
echo "Installed cosmic-comp package is at commit $pkg_commit"

# 1. Headers for the build, unpacked privately (matching installed library versions).
rm -rf "$SYSROOT"
mkdir -p "$SYSROOT/debs"
(cd "$SYSROOT/debs" && apt-get download libinput-dev libseat-dev libgbm-dev \
    libxkbcommon-dev libdisplay-info-dev libpixman-1-dev)
for deb in "$SYSROOT"/debs/*.deb; do dpkg -x "$deb" "$SYSROOT"; done
MULTIARCH=$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || echo "$(uname -m)-linux-gnu")
LIB="$SYSROOT/usr/lib/$MULTIARCH"
for so in "$LIB"/*.so; do
    target=$(readlink "$so" || true)
    if [ -n "$target" ] && [ ! -e "$LIB/$target" ]; then
        ln -sf "/usr/lib/$MULTIARCH/$target" "$so"
    fi
done
sed -i "s|^prefix=/usr|prefix=$SYSROOT/usr|" "$LIB"/pkgconfig/*.pc
export PKG_CONFIG_PATH="$LIB/pkgconfig"

# 2. Source at the installed commit + patches.
[ -d "$SRC_DIR/.git" ] || git clone https://github.com/pop-os/cosmic-comp.git "$SRC_DIR"
cd "$SRC_DIR"
# This resets the checkout, so refuse to touch one with local work we did not make.
if [ ! -f .nightlight-base ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "$SRC_DIR has uncommitted changes; set SRC_DIR to another directory." >&2
    exit 1
fi
git fetch origin
git reset -q --hard
git clean -qfd -e target
branch="night-light-$pkg_commit"
git checkout -q -B "$branch" "$pkg_commit"
if ! git merge-base --is-ancestor "$UPSTREAM_GAMMA_RESET" HEAD; then
    git -c user.name=cosmic-nightlight -c user.email=nightlight@localhost \
        cherry-pick "$UPSTREAM_GAMMA_RESET"
fi
git apply --3way "$HERE/gamma-control.patch"
echo "$pkg_commit" > .nightlight-base

# 3. Build.
cargo build --release
echo
echo "Built $SRC_DIR/target/release/cosmic-comp"
echo "Now run:  sudo $HERE/install.sh   and log out/in."
