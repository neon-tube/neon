#!/bin/bash
# Install or update the Neon toolchain.
#
#   ./install.sh                 install to $HOME/.neon (or $NEON_PREFIX_PATH, or $1)
#   ./install.sh update          update an existing install
#   NEON_BUILD_FROM_SOURCE=1 ...  build from this checkout instead of downloading
#
# The installed layout is exactly the layout `cargo build` stages next to the binary
# (see cli/build.rs), which is exactly the layout Sysroot::find resolves:
#
#     prefix/bin/neon             prefix/lib/gcc/libneon_rt{,_debug,_san}.a
#                                 prefix/lib/clang/libneon_rt{,_debug,_san}.a
#     prefix/include/             prefix/stdlib/
#
# The language server (`neon-lsp`) and the editor plugins live in their own repos under
# github.com/neon-tube now, so this repo's build no longer produces `neon-lsp`. The
# guarded copies below still pick one up if a release asset happens to carry it.
#
# lib/ carries one archive set per compiler family present on the build machine; the
# compiler picks the flavor matching its `cc` at link time and `neon doctor` reports
# what an install ended up with.
set -e

# Color definitions (only if output is a TTY and NO_COLOR is not present)
if [ -t 1 ] && [ -z "${NO_COLOR+x}" ]; then
    BOLD='\033[1m'
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    NC='\033[0m'
else
    BOLD='' RED='' GREEN='' YELLOW='' BLUE='' CYAN='' NC=''
fi

info()    { echo -e "${BLUE}==>${NC} ${BOLD}$*${NC}"; }
warn()    { echo -e "${YELLOW}warning:${NC} $*"; }
error()   { echo -e "${RED}error:${NC} $*" >&2; }
success() { echo -e "${GREEN}success:${NC} ${BOLD}$*${NC}"; }

# Target prefix: NEON_PREFIX_PATH -> $1 -> $HOME/.neon. When invoked as a symlink
# inside PREFIX/bin (the neon-update case), the prefix is wherever we already live.
SCRIPT_DIR=$(dirname "$0")
SCRIPT_DIR_ABS=$(cd "$SCRIPT_DIR" && pwd)
if [ "$(basename "$SCRIPT_DIR_ABS")" = "bin" ]; then
    AUTO_PREFIX=$(dirname "$SCRIPT_DIR_ABS")
fi
PREFIX="${NEON_PREFIX_PATH:-${AUTO_PREFIX:-${1:-$HOME/.neon}}}"

ACTION="install"
INVOCATION_NAME=$(basename "$0")
if [ "$INVOCATION_NAME" = "neon-update" ] || [ "$1" = "update" ]; then
    ACTION="update"
fi

info "Neon toolchain action: ${CYAN}$ACTION${NC}"
info "Target prefix: ${CYAN}$PREFIX${NC}"

mkdir -p "$PREFIX/bin" "$PREFIX/lib" "$PREFIX/include" "$PREFIX/stdlib"

if [ -n "$NEON_BUILD_FROM_SOURCE" ] && [ "$NEON_BUILD_FROM_SOURCE" != "0" ] && [ "$NEON_BUILD_FROM_SOURCE" != "false" ]; then
    BUILD_FROM_SOURCE=true
else
    BUILD_FROM_SOURCE=false
fi

# Copy the staged sysroot pieces from a build tree or an unpacked release into PREFIX.
# One function for both paths, because the layouts are deliberately identical.
install_tree() {
    SRC="$1"

    if [ -f "$SRC/bin/neon" ]; then
        cp "$SRC/bin/neon" "$PREFIX/bin/neon"
    elif [ -f "$SRC/neon" ]; then
        cp "$SRC/neon" "$PREFIX/bin/neon"
    else
        error "no neon binary under $SRC"
        exit 1
    fi
    chmod +x "$PREFIX/bin/neon"
    for lsp in "$SRC/bin/neon-lsp" "$SRC/neon-lsp"; do
        if [ -f "$lsp" ]; then
            cp "$lsp" "$PREFIX/bin/neon-lsp"
            chmod +x "$PREFIX/bin/neon-lsp"
            break
        fi
    done

    # The runtime archives, per flavor. Cleared first so a flavor removed upstream
    # cannot linger from a previous install and silently keep being linked.
    if [ -d "$SRC/lib" ]; then
        rm -rf "$PREFIX/lib"
        mkdir -p "$PREFIX/lib"
        cp -r "$SRC/lib/." "$PREFIX/lib/"
    else
        error "no lib/ under $SRC — the runtime archives are missing"
        exit 1
    fi
    FLAVORS=""
    for flavor in gcc clang; do
        [ -f "$PREFIX/lib/$flavor/libneon_rt.a" ] && FLAVORS="$FLAVORS $flavor"
    done
    if [ -z "$FLAVORS" ]; then
        error "lib/ carries no runtime flavor (expected lib/gcc/ or lib/clang/)"
        exit 1
    fi
    info "Runtime flavors installed:${CYAN}$FLAVORS${NC}"

    rm -rf "$PREFIX/include" "$PREFIX/stdlib"
    cp -r "$SRC/include" "$PREFIX/include"
    cp -r "$SRC/stdlib" "$PREFIX/stdlib"
}

if [ "$BUILD_FROM_SOURCE" = true ]; then
    info "Mode: ${CYAN}building from source${NC}"

    if [ "$ACTION" = "install" ]; then
        if git -C "$SCRIPT_DIR_ABS" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
            REPO_URL="$(git -C "$SCRIPT_DIR_ABS" rev-parse --show-toplevel)"
        else
            REPO_URL=$(git remote get-url origin 2>/dev/null || echo "https://github.com/neon-tube/neon.git")
        fi
        info "Cloning Neon repository into ${CYAN}$PREFIX/release${NC}..."
        if [ -d "$PREFIX/release" ]; then
            warn "Release directory already exists. Cleaning it first..."
            rm -rf "$PREFIX/release"
        fi
        git clone "$REPO_URL" "$PREFIX/release"
    else
        info "Updating Neon repository in ${CYAN}$PREFIX/release${NC}..."
        if [ ! -d "$PREFIX/release" ]; then
            warn "Release directory $PREFIX/release does not exist. Cloning it..."
            git clone "https://github.com/neon-tube/neon.git" "$PREFIX/release"
        else
            git -C "$PREFIX/release" pull
        fi
    fi

    cd "$PREFIX/release"

    # Archives for both compiler families are staged when both compilers exist here;
    # say up front which this machine will get.
    HAVE=""
    command -v gcc   >/dev/null 2>&1 && gcc --version 2>/dev/null | head -1 | grep -qiv clang && HAVE="$HAVE gcc"
    command -v clang >/dev/null 2>&1 && HAVE="$HAVE clang"
    if [ -z "$HAVE" ]; then
        error "neither gcc nor clang is installed; the runtime cannot be built"
        exit 1
    fi
    info "C compilers found:${CYAN}$HAVE${NC} (one runtime archive set per family)"

    info "Building the Neon toolchain (cargo build --release)..."
    cargo build --release

    # cli/build.rs staged the complete sysroot next to the binary; install is a copy of
    # that staging, nothing scavenged out of cargo's build directories. (`install_tree`
    # finds the binary at the tree's root there, where a release asset has it in bin/.)
    install_tree "target/release"

    if [ -d extra ]; then
        info "Copying editor extensions..."
        cp -r extra/. "$PREFIX/extra/" || true
    fi
else
    info "Mode: ${CYAN}installing latest prebuilt release from GitHub${NC}"

    if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        REPO_URL="$(git remote get-url origin 2>/dev/null || echo "https://github.com/neon-tube/neon.git")"
    else
        REPO_URL="https://github.com/neon-tube/neon.git"
    fi
    REPO_NAME=$(echo "$REPO_URL" | sed -E 's/.*github\.com[:\/]([^\/]+\/[^\/\.]+).*/\1/')
    if [ -z "$REPO_NAME" ] || [[ "$REPO_NAME" == *.* ]] || [[ "$REPO_NAME" == *:* ]]; then
        REPO_NAME="neon-tube/neon"
    fi

    API_URL="https://api.github.com/repos/$REPO_NAME/releases/latest"
    info "Fetching latest release information from ${CYAN}$API_URL${NC}..."
    RELEASE_JSON=$(curl -sSL "$API_URL")
    if [ -z "$RELEASE_JSON" ] || echo "$RELEASE_JSON" | grep -q "message.*Not Found"; then
        error "Latest release not found for repository $REPO_NAME."
        error "You can build from source by setting: export NEON_BUILD_FROM_SOURCE=1"
        exit 1
    fi

    TAG_NAME=$(echo "$RELEASE_JSON" | grep -o '"tag_name": *"[^"]*"' | head -n 1 | cut -d'"' -f4)
    if [ -z "$TAG_NAME" ]; then
        error "Could not determine latest release tag name."
        exit 1
    fi
    info "Latest release tag: ${GREEN}$TAG_NAME${NC}"

    OS="$(uname -s)"
    ARCH="$(uname -m)"
    case "$OS" in
        Darwin)
            case "$ARCH" in
                arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
                *)             TARGET="x86_64-apple-darwin" ;;
            esac
            ;;
        Linux)
            case "$ARCH" in
                aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
                *)       TARGET="x86_64-unknown-linux-gnu" ;;
            esac
            ;;
        *)
            error "Unsupported OS: $OS"
            exit 1
            ;;
    esac
    info "Detected target: ${CYAN}$TARGET${NC}"

    DOWNLOAD_URL=$(echo "$RELEASE_JSON" | grep -o '"browser_download_url": *"[^"]*"' | cut -d'"' -f4 | grep "$TARGET" | head -n 1)
    if [ -z "$DOWNLOAD_URL" ]; then
        case "$OS" in
            Darwin) SEARCH_TERM="darwin\|macos\|osx\|mac" ;;
            Linux)  SEARCH_TERM="linux" ;;
        esac
        DOWNLOAD_URL=$(echo "$RELEASE_JSON" | grep -o '"browser_download_url": *"[^"]*"' | cut -d'"' -f4 | grep -i "$SEARCH_TERM" | head -n 1)
    fi
    if [ -z "$DOWNLOAD_URL" ]; then
        error "No matching release asset found for target $TARGET in release $TAG_NAME."
        error "Available assets:"
        error "$(echo "$RELEASE_JSON" | grep -o '"name": *"[^"]*"' | cut -d'"' -f4)"
        error ""
        error "You can build from source by setting: export NEON_BUILD_FROM_SOURCE=1"
        exit 1
    fi

    ASSET_NAME=$(basename "$DOWNLOAD_URL")
    info "Downloading ${CYAN}$ASSET_NAME${NC}..."
    TEMP_DIR=$(mktemp -d)
    trap 'rm -rf "$TEMP_DIR"' EXIT
    curl -sSL -o "$TEMP_DIR/$ASSET_NAME" "$DOWNLOAD_URL"

    info "Extracting release asset..."
    case "$ASSET_NAME" in
        *.tar.gz|*.tgz) tar -xzf "$TEMP_DIR/$ASSET_NAME" -C "$TEMP_DIR" ;;
        *.zip)          unzip -q "$TEMP_DIR/$ASSET_NAME" -d "$TEMP_DIR" ;;
        *)
            error "Unknown asset format for $ASSET_NAME. Expected .tar.gz or .zip"
            exit 1
            ;;
    esac

    # The asset root: wherever bin/ (or the bare binary) landed after extraction.
    BIN_DIR_PATH=$(find "$TEMP_DIR" -type d -name "bin" | head -n 1)
    if [ -n "$BIN_DIR_PATH" ]; then
        SRC_DIR=$(dirname "$BIN_DIR_PATH")
    else
        NEON_BIN_PATH=$(find "$TEMP_DIR" -type f -name "neon" | head -n 1)
        if [ -n "$NEON_BIN_PATH" ]; then
            SRC_DIR=$(dirname "$NEON_BIN_PATH")
        else
            SRC_DIR="$TEMP_DIR"
        fi
    fi

    info "Installing prebuilt components..."
    install_tree "$SRC_DIR"
    if [ -d "$SRC_DIR/extra" ]; then
        cp -r "$SRC_DIR/extra/." "$PREFIX/extra/"
    fi
fi

info "Creating neon-update..."
if [ "$BUILD_FROM_SOURCE" = true ]; then
    ln -sf "$PREFIX/release/install.sh" "$PREFIX/bin/neon-update"
else
    # Always download the script — $0 is "bash" when run via curl|bash.
    curl -sSL "https://raw.githubusercontent.com/$REPO_NAME/main/install.sh" -o "$PREFIX/bin/neon-update"
    chmod +x "$PREFIX/bin/neon-update"
fi

# The installed compiler can check itself out.
if "$PREFIX/bin/neon" doctor >/dev/null 2>&1; then
    info "Post-install check: ${GREEN}neon doctor is happy${NC}"
else
    warn "Post-install check: 'neon doctor' reported problems. Run ${CYAN}$PREFIX/bin/neon doctor${NC} to see them."
fi

echo ""
if [ "$ACTION" = "install" ]; then
    success "Neon has been successfully installed!"
    echo -e "Installation location: ${GREEN}$PREFIX${NC}"
    echo ""
    echo -e "${BOLD}To finish setup, add the Neon bin directory to your PATH.${NC}"
    echo ""
    echo -e "${BOLD}For Bash:${NC}"
    echo -e "  echo 'export PATH=\"\$PATH:$PREFIX/bin\"' >> ~/.bashrc"
    echo -e "  source ~/.bashrc"
    echo ""
    echo -e "${BOLD}For Zsh:${NC}"
    echo -e "  echo 'export PATH=\"\$PATH:$PREFIX/bin\"' >> ~/.zshrc"
    echo -e "  source ~/.zshrc"
    echo ""
    echo -e "${BOLD}For Fish:${NC}"
    echo -e "  fish_add_path $PREFIX/bin"
    echo ""
else
    success "Neon toolchain successfully updated!"
fi
