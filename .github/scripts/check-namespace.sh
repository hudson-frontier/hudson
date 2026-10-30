#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Ubuntu CI bootstrap only; operators manage their production host policy.
set -euo pipefail

if [[ "$(cat /sys/module/apparmor/parameters/enabled 2>/dev/null || true)" == Y ]]; then
  # Use Ubuntu's maintained profile for this exact system executable. It allows
  # namespace setup and denies capabilities to the executed sandbox children.
  sudo apparmor_parser --replace /usr/share/apparmor/extra-profiles/bwrap-userns-restrict
fi

exec /usr/bin/bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-uts \
  --unshare-net --disable-userns --cap-drop ALL --die-with-parent --new-session \
  --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
  --proc /proc --dev /dev --chdir / --remount-ro / -- /usr/bin/true
