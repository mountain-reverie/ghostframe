#!/usr/bin/env bash
# Each crate reads the process environment in exactly one place, its
# config.rs, and passes the parsed values down. This script is the guard for
# that rule. CI's clippy job and `just ci-local` / `just ci-client` both run
# it, so a stray read fails locally rather than only after a push.
#
# Comment lines are excluded deliberately: the guard constrains code, not
# prose. Matching comments would push authors to stop naming `env::var` in
# documentation, which is the opposite of useful.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0
fail() {
    # `::error::` is what GitHub Actions annotates; it reads fine locally too.
    echo "::error::$1"
    status=1
}
# grep exits 1 on "no match", which is the passing case here; under
# `pipefail` that would abort the script, so every stage tolerates it.
scan() { grep -rn "$@" --include='*.rs' || true; }
scan_e() { grep -rnE "$@" --include='*.rs' || true; }
drop() { grep -v "$@" || true; }
drop_e() { grep -vE "$@" || true; }
code_only() { drop_e ':[0-9]+: *(//|///|//!)'; }

# ghostframe-lib. gpu_pipeline/frame.rs is exempt: its three cdf53-diag reads
# sit in the Vulkan dispatch path, where threading config through is a much
# larger change. They are feature-gated and race nothing; tracked as deferred
# work in docs/superpowers/plans/2026-09-08-lib-config.md.
hits=$(scan 'env::var' ghostframe-lib/src \
    | drop '^ghostframe-lib/src/config.rs' \
    | drop '^ghostframe-lib/src/capture/gpu_pipeline/frame.rs' \
    | code_only)
if [[ -n "$hits" ]]; then
    echo "$hits"
    fail "ghostframe-lib must read std::env only in config.rs"
fi

# Every ghostframe-client-* crate, same rule. ghostframe-client-gpu's
# config.rs is the one sanctioned reader (GHOSTFRAME_CLIENT_DUMP_*).
# ghostframe-client-native/src/lib.rs is exempt for its single TS_CONTROL_URL
# read, which is a tailscale variable passed through to tsnet rather than a
# ghostframe knob; tracked for a proper config module in that crate.
hits=$(scan 'env::var' ghostframe-client-*/src \
    | drop_e '^ghostframe-client-[a-z0-9]+/src/config\.rs' \
    | drop '^ghostframe-client-native/src/lib.rs' \
    | code_only)
if [[ -n "$hits" ]]; then
    echo "$hits"
    fail "ghostframe-client-* crates must read std::env only in config.rs"
fi

# ghostframe-xdaemon, same rule. tests/ is not scanned: integration tests
# legitimately read PATH. It must not mutate the environment either.
hits=$(scan 'env::var' ghostframe-xdaemon/src \
    | drop '^ghostframe-xdaemon/src/config.rs' \
    | code_only)
if [[ -n "$hits" ]]; then
    echo "$hits"
    fail "ghostframe-xdaemon must read std::env only in config.rs"
fi
hits=$(scan_e 'env::(set_var|remove_var)' ghostframe-xdaemon/src \
    | code_only)
if [[ -n "$hits" ]]; then
    echo "$hits"
    fail "ghostframe-xdaemon must not mutate the process environment"
fi

exit "$status"
