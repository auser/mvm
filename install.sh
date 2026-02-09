#!/usr/bin/env bash
set -euo pipefail

REPO="auser/mvm"
BINARY="mvm"
INSTALL_DIR="${MVM_INSTALL_DIR:-/usr/local/bin}"

# Colors (disabled if not a terminal)
if [ -t 1 ]; then
    BOLD='\033[1m'
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    RESET='\033[0m'
else
    BOLD='' GREEN='' RED='' RESET=''
fi

info()  { echo -e "${BOLD}${GREEN}==>${RESET} ${BOLD}$1${RESET}"; }
error() { echo -e "${RED}error:${RESET} $1" >&2; }
die()   { error "$1"; exit 1; }

detect_platform() {
    local os arch

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os="apple-darwin" ;;
        Linux)  os="unknown-linux-gnu" ;;
        *)      die "Unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64)         arch="x86_64" ;;
        aarch64|arm64)  arch="aarch64" ;;
        *)              die "Unsupported architecture: $arch" ;;
    esac

    echo "${arch}-${os}"
}

get_latest_version() {
    local url="https://api.github.com/repos/${REPO}/releases/latest"

    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//'
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$url" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//'
    else
        die "curl or wget is required"
    fi
}

download() {
    local url="$1" dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$dest" "$url"
    fi
}

main() {
    local platform version archive_name download_url tmpdir

    platform="$(detect_platform)"
    info "Detected platform: ${platform}"

    # Allow pinning a version: MVM_VERSION=v0.2.0 ./install.sh
    if [ -n "${MVM_VERSION:-}" ]; then
        version="$MVM_VERSION"
        info "Using specified version: ${version}"
    else
        info "Fetching latest release..."
        version="$(get_latest_version)"
        if [ -z "$version" ]; then
            die "Could not determine latest version. Set MVM_VERSION=vX.Y.Z and retry."
        fi
        info "Latest version: ${version}"
    fi

    archive_name="${BINARY}-${platform}.tar.gz"
    download_url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    info "Downloading ${download_url}..."
    download "$download_url" "${tmpdir}/${archive_name}" \
        || die "Download failed. Check that ${version} has a release for ${platform}."

    info "Extracting..."
    tar xzf "${tmpdir}/${archive_name}" -C "$tmpdir"

    # The archive contains mvm-<target>/mvm
    local extracted_dir="${tmpdir}/${BINARY}-${platform}"
    if [ ! -f "${extracted_dir}/${BINARY}" ]; then
        die "Binary not found in archive. Expected ${BINARY}-${platform}/${BINARY}"
    fi

    info "Installing to ${INSTALL_DIR}/${BINARY}..."
    if [ -w "$INSTALL_DIR" ]; then
        mv "${extracted_dir}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    else
        echo "  (requires sudo)"
        sudo mv "${extracted_dir}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    fi
    chmod +x "${INSTALL_DIR}/${BINARY}"

    info "Installed ${BINARY} ${version} to ${INSTALL_DIR}/${BINARY}"

    # Verify
    if command -v "$BINARY" >/dev/null 2>&1; then
        echo ""
        info "Run 'mvm bootstrap' to get started."
    else
        echo ""
        echo "  ${INSTALL_DIR} is not in your PATH."
        echo "  Add it:  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi
}

main
