#!/usr/bin/env sh
# Downloads a btop-gpui tarball from GitHub Releases and unpacks it into
# ~/.local, then wires up the launcher entry and the icon.
#
#   curl -fsSL https://raw.githubusercontent.com/Michael-Obele/btop-gpui/main/install.sh | sh
#
# Everything lands under $HOME — no sudo, no system directories, and
# `rm -rf ~/.local/btop-gpui.app` is a complete uninstall.
#
# Environment:
#   BTOP_GPUI_VERSION   a release tag, or "latest" (the default)
#   BTOP_GPUI_CHANNEL   "stable" (default) or "nightly"; nightly installs
#                       alongside stable rather than over it
#   BTOP_GPUI_BUNDLE    path to a local tarball, to test the install without
#                       publishing a release
#   BTOP_GPUI_UNINSTALL set to 1 to remove an existing install
set -eu

REPO="${BTOP_GPUI_REPO:-Michael-Obele/btop-gpui}"

main() {
  platform="$(uname -s)"
  arch="$(uname -m)"
  channel="${BTOP_GPUI_CHANNEL:-stable}"
  BTOP_GPUI_VERSION="${BTOP_GPUI_VERSION:-latest}"

  if [ -n "${BTOP_GPUI_UNINSTALL:-}" ]; then
    uninstall "$channel"
    exit 0
  fi

  if [ "$platform" = "Linux" ]; then
    platform="linux"
  else
    echo "btop-gpui supports Linux only (X11 and Wayland)." >&2
    exit 1
  fi

  case "$arch" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *)
    echo "Unsupported architecture: $arch" >&2
    echo "btop-gpui ships x86_64 and aarch64 builds." >&2
    exit 1
    ;;
  esac

  if command -v curl >/dev/null 2>&1; then
    curl() {
      command curl -fL "$@"
    }
  elif command -v wget >/dev/null 2>&1; then
    curl() {
      wget -O- "$@"
    }
  else
    echo "Could not find 'curl' or 'wget' in your PATH" >&2
    exit 1
  fi

  if [ -n "${TMPDIR:-}" ] && [ -d "$TMPDIR" ]; then
    temp="$(mktemp -d "$TMPDIR/btop-gpui-XXXXXX")"
  else
    temp="$(mktemp -d "/tmp/btop-gpui-XXXXXX")"
  fi

  # Set before the download so a failed fetch still cleans up after itself.
  trap 'rm -rf "$temp"' EXIT INT TERM

  linux "$@"

  if [ "$(command -v btop-gpui 2>/dev/null || true)" = "$HOME/.local/bin/btop-gpui" ]; then
    echo "btop-gpui has been installed. Run with 'btop-gpui'"
  else
    echo "To run btop-gpui from your terminal, add ~/.local/bin to your PATH:"
    case "${SHELL:-}" in
    *zsh)
      echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
      ;;
    *fish)
      echo "   fish_add_path -U \$HOME/.local/bin"
      ;;
    *)
      echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
      ;;
    esac
    echo "To run it now: '$HOME/.local/bin/btop-gpui'"
  fi
}

# Remove an install. The binary, the icon, the launcher entry and the log
# directory all have known locations, so this is exhaustive.
uninstall() {
  suffix=""
  [ "$1" = "nightly" ] && suffix="-nightly"
  rm -rf "$HOME/.local/btop-gpui$suffix.app"
  rm -f "$HOME/.local/bin/btop-gpui"
  rm -f "$HOME/.local/share/applications/btop-gpui$suffix.desktop"
  rm -f "$HOME/.local/share/icons/hicolor/scalable/apps/btop-gpui.svg"
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
  fi
  echo "Removed btop-gpui."
  echo "Your settings, themes and logs were left in place:"
  echo "  ~/.config/btop-gpui     settings and custom themes"
  echo "  ~/.local/state/btop-gpui  the log file"
  echo "Delete those too if you want a clean slate."
}

linux() {
  suffix=""
  [ "$channel" != "stable" ] && suffix="-$channel"
  bundle="btop-gpui-linux-$arch"

  if [ -n "${BTOP_GPUI_BUNDLE:-}" ]; then
    echo "Using local bundle: $BTOP_GPUI_BUNDLE"
    cp "$BTOP_GPUI_BUNDLE" "$temp/$bundle.tar.gz"
  else
    echo "Downloading btop-gpui $BTOP_GPUI_VERSION ($channel, $arch)"
    url="https://github.com/$REPO/releases/latest/download/$bundle.tar.gz"
    if [ "$BTOP_GPUI_VERSION" != "latest" ]; then
      url="https://github.com/$REPO/releases/download/$BTOP_GPUI_VERSION/$bundle.tar.gz"
    fi
    curl "$url" >"$temp/$bundle.tar.gz"
  fi

  # --- unpack --------------------------------------------------------------
  rm -rf "$HOME/.local/btop-gpui$suffix.app"
  mkdir -p "$HOME/.local/btop-gpui$suffix.app"
  tar -xzf "$temp/$bundle.tar.gz" -C "$temp"

  app="$HOME/.local/btop-gpui$suffix.app"
  cp -r "$temp/$bundle/." "$app/"

  # --- check the runtime libraries ----------------------------------------
  # `ldd` catches the direct dependencies. It cannot catch `libvulkan.so.1` or
  # `libfontconfig.so.1`, which GPUI dlopens after start; those are checked
  # separately below, because a missing Vulkan driver is the single most likely
  # reason a freshly installed copy refuses to start.
  if command -v ldd >/dev/null 2>&1; then
    missing="$(ldd "$app/bin/btop-gpui" 2>/dev/null |
      sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
    if [ -n "$missing" ]; then
      echo "Warning: missing libraries:"
      echo "$missing" | sed 's/^/    /'
      echo "On Debian/Ubuntu: sudo apt install libxcb1 libxkbcommon-x11-0 libxau6"
    fi
  fi

  for lib in libvulkan.so.1 libfontconfig.so.1; do
    if ! ldconfig -p 2>/dev/null | grep -q "$lib" &&
      ! find /usr/lib /usr/lib64 /usr/lib/*-linux-gnu* -name "$lib" -print -quit 2>/dev/null | grep -q .; then
      case "$lib" in
      libvulkan.so.1)
        echo "Warning: libvulkan.so.1 not found. btop-gpui renders with Vulkan and"
        echo "         will not start without it."
        echo "         sudo apt install libvulkan1 mesa-vulkan-drivers   # or your GPU's driver"
        ;;
      libfontconfig.so.1)
        echo "Warning: libfontconfig.so.1 not found; text will not render."
        echo "         sudo apt install libfontconfig1 fonts-dejavu-core"
        ;;
      esac
    fi
  done

  # --- wire it up ----------------------------------------------------------
  mkdir -p "$HOME/.local/bin" \
    "$HOME/.local/share/applications" \
    "$HOME/.local/share/icons/hicolor/scalable/apps"

  # Symlink rather than copy: a version bump replaces one directory and the
  # link follows, so upgrading never leaves a stale binary on PATH.
  ln -sf "$app/bin/btop-gpui" "$HOME/.local/bin/btop-gpui"

  install -m 644 "$app/share/icons/hicolor/scalable/apps/btop-gpui.svg" \
    "$HOME/.local/share/icons/hicolor/scalable/apps/btop-gpui.svg"

  # StartupWMClass MUST equal the app_id set in `chrome::window_options`
  # ("btop-gpui") or the taskbar grows a second, differently-named entry.
  install -m 644 "$app/share/applications/btop-gpui.desktop" \
    "$HOME/.local/share/applications/btop-gpui$suffix.desktop"

  # Refresh the desktop database so the launcher picks up the entry without a
  # logout. Missing on minimal installs, which is fine.
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
  fi

  # GTK caches icons by mtime; without this a re-install can keep the old one.
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
  fi
}

main "$@"
