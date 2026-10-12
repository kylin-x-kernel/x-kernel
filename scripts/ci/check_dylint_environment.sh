#!/usr/bin/env bash
# Verify the Dylint tooling the CI builder image provides for the dylint
# lint stages. Run from the repository root; other stages do not need it.
set -euo pipefail

# shellcheck source=scripts/ci/cargo_tool_receipts.sh
source "$(dirname "$0")/cargo_tool_receipts.sh"

# The CI image provides the Dylint tools, the lint toolchain and a driver
# prebuilt for that toolchain. Installing any of them here would make every
# node build its own driver, so a mismatch means the pins changed without a
# new image. The pins live in xtask/xkmake/src/hygiene.rs (DYLINT_VERSION) and
# tools/lints/rust-toolchain.toml.
DYLINT_VERSION="$(sed -n 's/^\(pub(crate) \)\{0,1\}const DYLINT_VERSION: &str = "\(.*\)";$/\2/p' xtask/xkmake/src/hygiene.rs)"
eval "$(
python3 <<'PY_LINT_TOOLCHAIN'
import shlex
import tomllib

with open("tools/lints/rust-toolchain.toml", "rb") as f:
    toolchain = tomllib.load(f)["toolchain"]

print("LINT_TOOLCHAIN=" + shlex.quote(toolchain["channel"]))
print("LINT_COMPONENTS=(" + " ".join(shlex.quote(c) for c in toolchain.get("components", [])) + ")")
PY_LINT_TOOLCHAIN
)"
# Kernel targets of the platforms scanned by the Jenkins dylint stages.
LINT_TARGETS=(
    aarch64-unknown-none-softfloat
    x86_64-unknown-none
)

dylint_image_error() {
    echo "error: $*" >&2
    echo "error: the Dylint tools, the lint toolchain and its driver come from the CI builder image." >&2
    echo "error: Publish a new image version for cargo-dylint ${DYLINT_VERSION:-?} and ${LINT_TOOLCHAIN:-?}," >&2
    echo "error: then point Jenkinsfile at it and bump the Rustup volume suffix." >&2
    exit 1
}

[ -n "${DYLINT_VERSION}" ] || dylint_image_error "cannot read DYLINT_VERSION from xtask/xkmake/src/hygiene.rs"

# cargo-dylint needs its subcommand to report a version; dylint-link forwards
# its arguments to the linker, so only its installation receipt is checked.
reported="$(cargo-dylint dylint --version 2>/dev/null || true)"
[ "${reported}" = "cargo-dylint ${DYLINT_VERSION}" ] \
    || dylint_image_error "expected cargo-dylint ${DYLINT_VERSION}, found '${reported:-none}'"
root="$(cargo_install_root_for dylint-link || true)"
{ [ -n "${root}" ] && cargo_root_has_package dylint-link "${DYLINT_VERSION}" "${root}"; } \
    || dylint_image_error "dylint-link ${DYLINT_VERSION} is not installed"

grep -q "^${LINT_TOOLCHAIN}-" <<<"$(rustup toolchain list)" \
    || dylint_image_error "lint toolchain ${LINT_TOOLCHAIN} is not installed"
installed_components="$(rustup +"${LINT_TOOLCHAIN}" component list --installed)"
for component in "${LINT_COMPONENTS[@]}"; do
    grep -q "^${component}\(-\|\$\)" <<<"${installed_components}" \
        || dylint_image_error "lint toolchain ${LINT_TOOLCHAIN} lacks component ${component}"
done
installed_targets="$(rustup +"${LINT_TOOLCHAIN}" target list --installed)"
for target in "${LINT_TARGETS[@]}"; do
    grep -qx "${target}" <<<"${installed_targets}" \
        || dylint_image_error "lint toolchain ${LINT_TOOLCHAIN} lacks target ${target}"
done

lint_host="$(rustc +"${LINT_TOOLCHAIN}" -vV | sed -n 's/^host: //p')"
lint_driver="${DYLINT_DRIVER_PATH:-}/${LINT_TOOLCHAIN}-${lint_host}/dylint-driver"
{ [ -n "${DYLINT_DRIVER_PATH:-}" ] && [ -x "${lint_driver}" ]; } \
    || dylint_image_error "no prebuilt Dylint driver at ${lint_driver}"

echo "==> Dylint tools"
echo "${reported}; dylint-link ${DYLINT_VERSION} (installation receipt)"
echo "lint toolchain ${LINT_TOOLCHAIN}; driver ${lint_driver}"
