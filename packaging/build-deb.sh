#!/usr/bin/env bash
# Build a .deb for Ubuntu (24.04 / 26.04+). Run from the project root.
# Needs: cargo, libgtk-3-dev, dpkg-dev
set -euo pipefail
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(dpkg --print-architecture)
PKG=target/deb/chop-chop_${VERSION}_${ARCH}
cargo build --release
rm -rf "$PKG"; mkdir -p "$PKG/DEBIAN" "$PKG/usr/bin" "$PKG/usr/share/applications" "$PKG/usr/share/doc/chop-chop"
install -m755 target/release/chop-chop "$PKG/usr/bin/"
install -m644 packaging/io.github.ChopChop.desktop "$PKG/usr/share/applications/"
for size in 16 24 32 48 64 128 256 512; do
  install -Dm644 "data/appicon/io.github.ChopChop-$size.png" "$PKG/usr/share/icons/hicolor/${size}x${size}/apps/io.github.ChopChop.png"
done
install -Dm644 data/appicon/io.github.ChopChop.svg "$PKG/usr/share/icons/hicolor/scalable/apps/io.github.ChopChop.svg"
cat > "$PKG/usr/share/doc/chop-chop/copyright" <<COPY
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: chop-chop

Files: *
License: MIT

Files: data/icons/*
Copyright: Ubuntu Yaru theme contributors (https://github.com/ubuntu/yaru)
License: CC-BY-SA-4.0
COPY
SIZE=$(du -sk "$PKG/usr" | cut -f1)
cat > "$PKG/DEBIAN/control" <<CTRL
Package: chop-chop
Replaces: video-splitter, chopchop
Conflicts: video-splitter, chopchop
Provides: video-splitter, chopchop
Version: ${VERSION}
Section: video
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${SIZE}
Depends: libgtk-3-0t64 (>= 3.24) | libgtk-3-0 (>= 3.24), libglib2.0-0t64 | libglib2.0-0, libc6 (>= 2.39), ffmpeg
Maintainer: Carlos <mega.watt2@gmail.com>
Homepage: https://github.com/NullMagic2/chop-chop
Description: Chop Chop Splitter - cut and split videos fast (GTK 3, multicore)
 Cut a clip from a start time to an end time, or batch-split a whole
 video into parts, with a live thumbnail preview. Chunks are encoded in parallel across all CPU cores
 with FFmpeg for frame-accurate cuts. The segment can also be exported
 as WAV, MP3, OGG, FLAC, M4A or OPUS audio files.
 Uses Ubuntu's Yaru icon theme.
CTRL
cat > "$PKG/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
POST
chmod 755 "$PKG/DEBIAN/postinst"
dpkg-deb --build --root-owner-group "$PKG" >/dev/null
echo "Built ${PKG}.deb"
