#!/usr/bin/env bash
set -ueo pipefail
cd "$TICKBOX_TEMPDIR/work"
export CARGO_TARGET_DIR="$TICKBOX_CWD/target/${TICKBOX_BRANCH}.doc.normal"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings"
exec cargo doc --no-deps
