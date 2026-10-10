#!/usr/bin/env bash
set -euo pipefail

echo "==> Checking Rust build environment..."
NIGHTLY_TOOLCHAIN="${AUX_RUST_TOOLCHAIN}"
CARGO_SHEAR_VERSION="1.13.2"
LICENSURE_VERSION="0.8.1"

retry() {
    local attempts="$1"
    shift
    local i
    for i in $(seq 1 "${attempts}"); do
        "$@" && return 0
        if [ "${i}" = "${attempts}" ]; then
            return 1
        fi
        echo "Command failed, retrying (${i}/${attempts}): $*" >&2
        sleep 5
    done
}

eval "$(
python3 <<'PY_TOOLCHAIN'
import shlex
import tomllib

with open("rust-toolchain.toml", "rb") as f:
    toolchain = tomllib.load(f)["toolchain"]

def array(name, values):
    print(f"{name}=(" + " ".join(shlex.quote(v) for v in values) + ")")

print("XKERNEL_TOOLCHAIN=" + shlex.quote(toolchain["channel"]))
array("XKERNEL_COMPONENTS", toolchain.get("components", []))
array("XKERNEL_TARGETS", toolchain.get("targets", []))
PY_TOOLCHAIN
)"

DEFAULT_EXTRA_TARGETS=(
    x86_64-unknown-uefi
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    riscv64gc-unknown-linux-musl
)
NIGHTLY_TARGETS=(
    x86_64-unknown-linux-musl
    aarch64-unknown-linux-musl
    riscv64gc-unknown-linux-musl
)

dedup_words() {
    printf '%s\n' "$@" | awk 'NF && !seen[$0]++'
}

mapfile -t DEFAULT_TARGETS < <(dedup_words "${XKERNEL_TARGETS[@]}" "${DEFAULT_EXTRA_TARGETS[@]}")

default_install_args=("${XKERNEL_TOOLCHAIN}" --profile minimal --no-self-update)
for component in "${XKERNEL_COMPONENTS[@]}"; do
    default_install_args+=(--component "${component}")
done
for target in "${DEFAULT_TARGETS[@]}"; do
    default_install_args+=(--target "${target}")
done

nightly_install_args=("${NIGHTLY_TOOLCHAIN}" --profile minimal --component rustfmt --no-self-update)
for target in "${NIGHTLY_TARGETS[@]}"; do
    nightly_install_args+=(--target "${target}")
done

echo "==> Installing x-kernel toolchain: ${XKERNEL_TOOLCHAIN}"
retry 3 rustup toolchain install "${default_install_args[@]}"

echo "==> Installing auxiliary nightly toolchain: ${NIGHTLY_TOOLCHAIN}"
retry 3 rustup toolchain install "${nightly_install_args[@]}"

echo "==> Active default toolchain"
cargo --version
rustc --version
rustup show active-toolchain

# Builder image ships tools under /usr/local/cargo/bin while CARGO_HOME is a
# separate cache volume. cargo-shear 1.13.2 may report `Version: dev`.
# Do not rely on `cargo install --list` alone: it needs a usable rustup
# toolchain and can fail even when /usr/local/cargo/.crates.toml is present.
# Prefer, in order: exact --version, .crates.toml / install-list metadata,
# then the image binary embedding `{package}-{version}`.
IMAGE_CARGO_BIN="/usr/local/cargo/bin"

tool_version_token() {
    local program="$1"
    "$program" --version 2>/dev/null | awk '{print $NF}'
}

cargo_install_root_for() {
    local program="$1"
    local executable install_root
    executable="$(command -v "${program}" 2>/dev/null || true)"
    if [ -z "${executable}" ]; then
        return 1
    fi
    install_root="$(dirname "$(dirname "${executable}")")"
    if [ "$(basename "$(dirname "${executable}")")" != "bin" ]; then
        return 1
    fi
    printf '%s\n' "${install_root}"
}

# .crates.toml lines look like:
#   "cargo-shear 1.13.2 (registry+https://github.com/rust-lang/crates.io-index)" = ["cargo-shear"]
cargo_crates_toml_has() {
    local package="$1"
    local version="$2"
    local root="$3"
    local crates_toml="${root}/.crates.toml"
    if [ ! -f "${crates_toml}" ]; then
        return 1
    fi
    grep -E "^\"${package} ${version} " "${crates_toml}" >/dev/null 2>&1
}

cargo_install_list_has() {
    local package="$1"
    local version="$2"
    local root="$3"
    cargo install --list --root "${root}" 2>/dev/null \
        | awk -v expect="${package} v${version}:" '
            { gsub(/\r/, ""); if ($0 == expect) found=1 }
            END { exit found ? 0 : 1 }
        '
}

cargo_root_has_package() {
    local package="$1"
    local version="$2"
    local root="$3"
    cargo_crates_toml_has "${package}" "${version}" "${root}" \
        || cargo_install_list_has "${package}" "${version}" "${root}"
}

# cargo install embeds registry paths like .../cargo-shear-1.13.2/... in the bin.
image_bin_embeds_version() {
    local bin="$1"
    local package="$2"
    local version="$3"
    grep -a -F "${package}-${version}" "${bin}" >/dev/null 2>&1
}

cargo_tool_ready() {
    local program="$1"
    local package="$2"
    local version="$3"
    local reported root
    local image_bin="${IMAGE_CARGO_BIN}/${program}"

    reported="$(tool_version_token "${program}" || true)"
    if [ "${reported}" = "${version}" ]; then
        return 0
    fi

    # Image/build quirk: clap reports "dev" while install metadata is correct.
    if [ "${reported}" = "dev" ]; then
        root="$(cargo_install_root_for "${program}" || true)"
        if [ -n "${root}" ] && cargo_root_has_package "${package}" "${version}" "${root}"; then
            return 0
        fi
        if cargo_root_has_package "${package}" "${version}" /usr/local/cargo; then
            return 0
        fi
        if [ -x "${image_bin}" ] && image_bin_embeds_version "${image_bin}" "${package}" "${version}"; then
            return 0
        fi
    fi

    # PATH may miss /usr/local/cargo/bin; still accept a pinned image binary.
    if [ -x "${image_bin}" ]; then
        reported="$("${image_bin}" --version 2>/dev/null | awk '{print $NF}' || true)"
        if [ "${reported}" = "${version}" ]; then
            return 0
        fi
        if [ "${reported}" = "dev" ] && {
            cargo_root_has_package "${package}" "${version}" /usr/local/cargo \
                || image_bin_embeds_version "${image_bin}" "${package}" "${version}"
        }; then
            return 0
        fi
    fi

    return 1
}

ensure_cargo_tool() {
    local program="$1"
    local package="$2"
    local version="$3"

    if cargo_tool_ready "${program}" "${package}" "${version}"; then
        return 0
    fi

    echo "==> Installing ${package} ${version}"
    retry 3 cargo install "${package}" \
        --version "${version}" --locked --force
}

ensure_cargo_tool cargo-shear cargo-shear "${CARGO_SHEAR_VERSION}"

echo "==> Dependency analyzer"
cargo-shear --version

ensure_cargo_tool licensure licensure "${LICENSURE_VERSION}"

echo "==> License header analyzer"
licensure --version

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

echo "==> Installed default targets"
rustup target list --installed

echo "==> Installed nightly targets"
rustup +"${NIGHTLY_TOOLCHAIN}" target list --installed
