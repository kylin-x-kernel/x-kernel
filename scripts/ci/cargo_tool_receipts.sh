# shellcheck shell=bash
# Cargo install receipts: locate the install root of a tool on PATH and check
# that a package version is recorded there. Sourced by the CI setup scripts.

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
