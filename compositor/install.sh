#!/bin/sh
# Install the gamma-control-patched cosmic-comp over the packaged one.
# Usage: sudo ./compositor/install.sh             install the build made by rebuild.sh
#        sudo ./compositor/install.sh --restore   put the packaged cosmic-comp back
set -eu

STOCK=/usr/bin/cosmic-comp.stock
TARGET=/usr/bin/cosmic-comp
# rebuild.sh builds in ~/src/cosmic-comp of the user who ran it (not root).
USER_HOME=$(getent passwd "${SUDO_USER:-$(id -un)}" | cut -d: -f6)
SRC_DIR=${SRC_DIR:-$USER_HOME/src/cosmic-comp}
BUILT="$SRC_DIR/target/release/cosmic-comp"

if [ "$(id -u)" != 0 ]; then
    echo "Run with sudo." >&2
    exit 1
fi

if [ "${1:-}" = "--restore" ]; then
    [ -f "$STOCK" ] || { echo "No backup at $STOCK (already stock?)"; exit 1; }
    cp "$STOCK" "$TARGET.new" && mv -f "$TARGET.new" "$TARGET"
    echo "Restored the packaged cosmic-comp. Log out and back in."
    exit 0
fi

[ -x "$BUILT" ] || { echo "No build at $BUILT. Run compositor/rebuild.sh first." >&2; exit 1; }

# Only install a build made from the exact commit of the installed package.
pkg_commit=$(dpkg-query -W -f='${Version}' cosmic-comp | sed 's/.*~//')
built_commit=$(cat "$SRC_DIR/.nightlight-base" 2>/dev/null || echo unknown)
case "$built_commit" in
    "$pkg_commit"*) ;;
    *)
        echo "Build is from $built_commit but the installed cosmic-comp package is $pkg_commit." >&2
        echo "Run compositor/rebuild.sh (as your user) first." >&2
        exit 1
        ;;
esac

# Back up the packaged binary (refresh the backup if the package was updated).
if ! cmp -s "$TARGET" "$BUILT"; then
    if ! strings "$TARGET" | grep -q zwlr_gamma_control_manager_v1; then
        cp "$TARGET" "$STOCK"
    fi
fi

# Copy then rename: replacing a running executable in place fails.
cp "$BUILT" "$TARGET.new"
chmod 755 "$TARGET.new"
mv -f "$TARGET.new" "$TARGET"

echo "Installed patched cosmic-comp (backup of the original: $STOCK)."
echo "Log out and back in to start it."
echo
echo "If the desktop doesn't come back after logging in: press Ctrl+Alt+F3,"
echo "log in on the text console, and run:"
echo "    sudo $0 --restore"
echo "then press Ctrl+Alt+F1 (or reboot)."
