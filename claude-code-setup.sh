#!/usr/bin/env bash
# ============================================================
# claude-code-setup.sh
# Install (if needed) and launch Claude Code on a Linux server
# Usage:  chmod +x claude-code-setup.sh && ./claude-code-setup.sh
# ============================================================

set -euo pipefail

# ── Colours ──────────────────────────────────────────────────
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Colour

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERR]${NC}   $*"; }

# ── Pre-flight checks ───────────────────────────────────────
if [[ "$OSTYPE" != "linux-gnu"* ]]; then
    error "This script is intended for Linux servers."
    exit 1
fi

RAM_KB=$(grep MemTotal /proc/meminfo | awk '{print $2}')
RAM_MB=$((RAM_KB / 1024))
if (( RAM_MB < 4000 )); then
    warn "Only ${RAM_MB} MB RAM detected. Claude Code recommends 4 GB+."
fi

# ── Install dependencies ────────────────────────────────────
install_deps() {
    info "Updating package list and installing dependencies..."
    sudo apt-get update -qq
    sudo apt-get install -y -qq curl tmux git ripgrep > /dev/null
    info "Dependencies installed."
}

# ── Install Claude Code (native installer – no Node.js) ────
install_claude_code() {
    info "Installing Claude Code via native installer..."
    curl -fsSL https://claude.ai/install.sh | bash
    # Make sure the binary is on PATH for this session
    export PATH="$HOME/.claude/bin:$HOME/.local/bin:$PATH"
    info "Claude Code installed: $(claude --version 2>/dev/null || echo 'check PATH')"
}

# ── API key setup ────────────────────────────────────────────
setup_api_key() {
    if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
        warn "ANTHROPIC_API_KEY is not set."
        read -rp "Paste your Anthropic API key (or press Enter to skip and use OAuth): " key
        if [[ -n "$key" ]]; then
            export ANTHROPIC_API_KEY="$key"
            # Persist for future sessions
            if ! grep -q "ANTHROPIC_API_KEY" "$HOME/.bashrc" 2>/dev/null; then
                echo "export ANTHROPIC_API_KEY=\"$key\"" >> "$HOME/.bashrc"
                info "API key saved to ~/.bashrc"
            fi
        else
            info "Skipping API key — you can authenticate interactively via OAuth."
        fi
    else
        info "ANTHROPIC_API_KEY is already set."
    fi
}

# ── Main ─────────────────────────────────────────────────────
main() {
    echo ""
    echo "╔══════════════════════════════════════════╗"
    echo "║     Claude Code – Remote Server Setup    ║"
    echo "╚══════════════════════════════════════════╝"
    echo ""

    # 1. Dependencies
    if ! command -v curl &>/dev/null || ! command -v tmux &>/dev/null; then
        install_deps
    else
        info "Core dependencies already present."
    fi

    # 2. Claude Code
    if command -v claude &>/dev/null; then
        info "Claude Code is already installed: $(claude --version 2>/dev/null)"
        # Check for updates
        info "Checking for updates..."
        claude update 2>/dev/null || true
    else
        install_claude_code
    fi

    # 3. API key
    setup_api_key

    # 4. Launch
    echo ""
    info "Ready! Launching Claude Code inside tmux..."
    info "Tip: Detach with Ctrl+B then D. Reattach with: tmux attach -t claude"
    echo ""


    echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
}

main "$@"