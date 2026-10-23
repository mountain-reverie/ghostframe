#!/usr/bin/env bash
# Installs the apt packages named in packages-build.txt (and, when
# E2E_RUNTIME=true, packages-e2e-runtime.txt), preferring a restored cache so a
# typical run makes no request to the Ubuntu mirror at all.
#
# Environment:
#   E2E_RUNTIME  'true' to also install packages-e2e-runtime.txt
#   CACHE_HIT    'true' when actions/cache restored $STAGE for this key
#   STAGE        user-owned directory the cache action restores/saves. Layout:
#                  $STAGE/archives/*.deb   downloaded packages
#                  $STAGE/lists/           a copy of /var/lib/apt/lists
#   GITHUB_OUTPUT  receives `stage-ready=true` once $STAGE holds a savable cache
#
# Why the cache holds the apt lists as well as the .debs: on a hit we install
# with `--no-download`, which needs apt to resolve the exact versions that were
# downloaded. The runner's own lists (from the image build, up to a week stale)
# would resolve different versions and then fail for want of their .debs. The
# lists and debs are saved together, from one `apt-get update`, so they agree
# with each other by construction.
#
# Why the network is bounded: on 2026-10-23 the mirror served a run's
# `apt-get update` + install in 41 minutes (normally ~30 s), which ate the
# e2e job's whole 60-minute budget. Every network apt call below therefore runs
# under `timeout`, is retried (resuming partial downloads, which live in
# archives/partial), and the retry swaps which mirror is tried first. Composite
# steps have no timeout-minutes of their own, hence the wrapper.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
: "${STAGE:?STAGE must name the cache directory}"
E2E_RUNTIME=${E2E_RUNTIME:-false}
CACHE_HIT=${CACHE_HIT:-false}
LISTS=${APT_LISTS_DIR:-/var/lib/apt/lists}
ARCHIVES=${APT_ARCHIVES_DIR:-/var/cache/apt/archives}
MIRRORS=${APT_MIRRORS_FILE:-/etc/apt/apt-mirrors.txt}

# Package sets, installed one apt transaction each, in this order.
pkg_files=("$here/packages-build.txt")
if [[ $E2E_RUNTIME == true ]]; then
  pkg_files+=("$here/packages-e2e-runtime.txt")
fi

read_pkgs() {
  sed -e 's/#.*//' -e 's/[[:space:]]//g' "$1" | grep -v '^$'
}

# The hosted runners resolve archive URLs through
# mirror+file:/etc/apt/apt-mirrors.txt (azure.archive.ubuntu.com first, then
# archive.ubuntu.com, then security.ubuntu.com -- all three appear in the
# 2026-10-23 install logs). apt only moves to the next mirror on a failure, never
# on slowness, so after a timed-out attempt swap the first two entries.
swap_first_two_mirrors() {
  [[ -f $MIRRORS ]] || return 0
  local m
  mapfile -t m < <(grep -E '^https?://' "$MIRRORS" | awk '{print $1}')
  ((${#m[@]} >= 2)) || return 0
  { printf '%s\n' "${m[1]}" "${m[0]}"; printf '%s\n' "${m[@]:2}"; } \
    | sudo tee "$MIRRORS" >/dev/null
  echo "apt mirror order is now: $(tr '\n' ' ' <"$MIRRORS")"
}

# net_apt SECONDS ATTEMPTS APT-GET-ARGS...
net_apt() {
  local secs=$1 tries=$2 n rc
  shift 2
  for ((n = 1; n <= tries; n++)); do
    rc=0
    sudo timeout --kill-after=10 "$secs" apt-get \
      -o Acquire::Retries=3 -o Acquire::http::Timeout=20 \
      -o Acquire::https::Timeout=20 "$@" || rc=$?
    if ((rc == 0)); then
      return 0
    fi
    echo "::warning::apt-get $1 attempt $n/$tries failed (exit $rc$( ((rc == 124)) && echo ', timed out after '"$secs"'s'))"
    if ((n < tries)); then
      swap_first_two_mirrors
    fi
  done
  return "$rc"
}

offline_install() {
  local f=$1
  mapfile -t pkgs < <(read_pkgs "$f")
  sudo apt-get install -y --no-install-recommends --no-download "${pkgs[@]}"
}

install_from_cache() {
  echo "Restoring apt lists and debs from $STAGE"
  sudo cp -a "$STAGE/lists/." "$LISTS/"
  sudo cp -a "$STAGE/archives/." "$ARCHIVES/"
  local f
  for f in "${pkg_files[@]}"; do
    offline_install "$f" || return 1
  done
}

install_from_network() {
  # apt stamps each .deb with the server's Last-Modified time, so "newer than
  # a marker" cannot tell what this run fetched; compare directory listings.
  local before
  before=$(mktemp)
  sudo find "$ARCHIVES" -maxdepth 1 -name '*.deb' -printf '%f\n' | sort >"$before"

  # Retries cover transient proxy and network hiccups.
  #
  # The fallback covers a third-party repo serving a broken index. The
  # runner image ships Google's Chrome apt source, which nothing here
  # installs from, and on 2026-09-09 it served a "Hash Sum mismatch"
  # that failed clippy, unit, build-release and cbindgen-check on
  # master before a single package was installed or a test ran.
  #
  # Continuing is safe because every package this action installs comes
  # from the Ubuntu archive, and `apt-get install` below is the real
  # gate: if an index we actually need is missing, it fails loudly and
  # names the package. Swallowing update errors cannot turn a missing
  # dependency into a silent pass.
  net_apt "${APT_UPDATE_TIMEOUT:-180}" 3 update -qq ||
    echo "::warning::apt-get update reported errors; continuing (apt-get install will fail if a needed index is missing)"

  local f
  for f in "${pkg_files[@]}"; do
    mapfile -t pkgs < <(read_pkgs "$f")
    # Download first, under the timeout; the install proper is then local
    # (dpkg only) and needs no bound.
    net_apt "${APT_DOWNLOAD_TIMEOUT:-300}" 3 install -y --no-install-recommends --download-only "${pkgs[@]}"
    offline_install "$f"
  done

  # Save only what this run fetched (the image may already hold debs), plus
  # the lists those debs were resolved from.
  mkdir -p "$STAGE/archives" "$STAGE/lists"
  local n=0 deb
  while IFS= read -r deb; do
    sudo cp -a "$ARCHIVES/$deb" "$STAGE/archives/"
    n=$((n + 1))
  done < <(sudo find "$ARCHIVES" -maxdepth 1 -name '*.deb' -printf '%f\n' | sort | comm -13 "$before" -)
  sudo find "$LISTS" -maxdepth 1 -type f ! -name lock -exec cp -a {} "$STAGE/lists/" \;
  sudo chown -R "$(id -u):$(id -g)" "$STAGE"
  rm -f "$before"
  echo "Staged $n .deb files ($(du -sh "$STAGE" | cut -f1)) for the apt cache"
  if ((n > 0)) && [[ -n ${GITHUB_OUTPUT:-} ]]; then
    echo "stage-ready=true" >>"$GITHUB_OUTPUT"
  fi
}

if [[ $CACHE_HIT == true ]]; then
  if install_from_cache; then
    echo "apt cache hit: installed offline, no mirror request made"
    exit 0
  fi
  echo "::warning::apt cache hit but the offline install failed; falling back to the network (the cache entry is stale or incomplete)"
  # The key already exists, so nothing is saved after this.
  GITHUB_OUTPUT=
  install_from_network
else
  install_from_network
fi
