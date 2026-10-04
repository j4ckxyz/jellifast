#!/usr/bin/env bash
# Usage: bash packaging/test-install.sh ubuntu:24.04 dist/native-packages
# Run on the matching architecture, with a C compiler and Docker available.
set -euo pipefail

image=${1:?Supply a Debian, Ubuntu or Fedora container image}
packages=$(realpath "${2:?Supply a native-packages output directory}")
case "$(uname -m)" in
  x86_64) target=linux-amd64 ;;
  aarch64) target=linux-arm64 ;;
  *) echo 'Unsupported test architecture' >&2; exit 1 ;;
esac
case "$image" in
  ubuntu:*|debian:*) format=deb ;;
  fedora:*) format=rpm ;;
  *) echo 'Unsupported test distribution' >&2; exit 1 ;;
esac
package_dir="$packages/packages/$target/$format"
test -d "$package_dir"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
checks=$(mktemp -d)
trap 'rm -rf -- "$checks"' EXIT
cc -std=c99 -Wall -Wextra -Werror "$script_dir/check-runtime-libs.c" -ldl -o "$checks/check-runtime-libs"

docker run --rm \
  --volume "$package_dir:/packages:ro" \
  --volume "$checks:/checks:ro" \
  --env "FORMAT=$format" \
  "$image" sh -ec '
    set -- /packages/*."$FORMAT"
    test "$#" -eq 1
    test -f "$1"
    mkdir -p /root/.config/jellifast
    printf "%s\n" "preserve-existing-settings" > /root/.config/jellifast/settings-fixture
    if [ "$FORMAT" = deb ]; then
      apt-get update
      DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$1"
      dpkg-query -W jellifast
    else
      dnf install -y --setopt=install_weak_deps=False "$1"
      rpm -q jellifast
    fi
    # --version exercises linked libraries; the probe checks dlopen libraries
    # without installing a desktop, compiler, interpreter or test dependencies.
    # Trace the isolated fixture assertions so a failed check identifies itself.
    set -x
    jellifast --version
    test -f /usr/bin/jellifast
    test ! -L /usr/bin/jellifast
    test -f /usr/share/licenses/jellifast/LICENSE
    if [ "$FORMAT" = deb ]; then
      # Slim Debian/Ubuntu images exclude /usr/share/doc at installation time.
      # Verify the regular file in the package, not the intentionally stripped root.
      dpkg-deb --contents "$1" | grep -E "^-.* ./usr/share/doc/jellifast/README.md$"
    else
      test -f /usr/share/doc/jellifast/README.md
    fi
    /checks/check-runtime-libs
    test -s /usr/share/applications/jellifast.desktop
    test -s /usr/share/icons/hicolor/scalable/apps/jellifast.svg
    grep -qx "Icon=jellifast" /usr/share/applications/jellifast.desktop
    grep -qx "StartupWMClass=jellifast" /usr/share/applications/jellifast.desktop
    test -s /usr/share/jellifast/omarchy/jellifast.json.tpl
    test -x /usr/share/jellifast/omarchy/jellifast-theme
    test "$(cat /root/.config/jellifast/settings-fixture)" = preserve-existing-settings
    if [ "$FORMAT" = deb ]; then
      apt-get remove -y jellifast
    else
      dnf remove -y jellifast
    fi
    test ! -e /usr/bin/jellifast
    test ! -e /usr/share/applications/jellifast.desktop
    test ! -e /usr/share/icons/hicolor/scalable/apps/jellifast.svg
    test ! -e /usr/share/jellifast/omarchy/jellifast.json.tpl
    test ! -e /usr/share/jellifast/omarchy/jellifast-theme
    test "$(cat /root/.config/jellifast/settings-fixture)" = preserve-existing-settings
  '
