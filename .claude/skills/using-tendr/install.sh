#!/bin/bash
# Install the using-tendr skill into ~/.claude/skills/
#
# The skill lives inside this Tendr repo (.claude/skills/using-tendr/) so
# its content is versioned with Tendr itself. install.sh creates a symlink
# from ~/.claude/skills/using-tendr -> this directory so Claude Code's skill
# loader picks it up.
#
# Usage:
#   ./install.sh          # Create symlink
#   ./install.sh --check  # Verify installation
#   ./install.sh --remove # Remove symlink

set -e

SKILL_DIR="$(cd "$(dirname "$0")" && pwd)"
SKILL_NAME="$(basename "$SKILL_DIR")"
TARGET_DIR="$HOME/.claude/skills"
TARGET="$TARGET_DIR/$SKILL_NAME"

install_skill() {
    mkdir -p "$TARGET_DIR"
    if [[ -e "$TARGET" && ! -L "$TARGET" ]]; then
        echo "❌ $TARGET exists and is not a symlink — refusing to clobber"
        echo "   Remove it manually if you want install.sh to take over."
        exit 1
    fi
    ln -sf "$SKILL_DIR" "$TARGET"
    echo "✅ Linked: $TARGET → $SKILL_DIR"
    echo ""
    echo "Skill loaded on next Claude Code session start."
}

remove_skill() {
    if [[ -L "$TARGET" ]]; then
        unlink "$TARGET"
        echo "✅ Removed symlink: $TARGET"
    else
        echo "⚠️  No symlink at $TARGET"
    fi
}

check_installation() {
    echo "Checking using-tendr installation..."
    echo ""
    if [[ -L "$TARGET" ]]; then
        link_target="$(readlink "$TARGET")"
        echo "✅ Symlink: $TARGET → $link_target"
        if [[ "$link_target" = "$SKILL_DIR" ]]; then
            echo "✅ Target matches this skill directory"
        else
            echo "⚠️  Target differs from this skill directory ($SKILL_DIR)"
        fi
    else
        echo "❌ No symlink at $TARGET"
    fi
    echo ""
    if command -v tendr >/dev/null 2>&1; then
        echo "✅ tendr CLI on PATH: $(command -v tendr)"
    else
        echo "⚠️  tendr CLI not on PATH — this skill's advice won't be actionable"
        echo "   Build: cd \"$(cd "$SKILL_DIR/../../.." && pwd)\" && cargo build --release"
    fi
}

case "${1:-}" in
    --check|-c)  check_installation ;;
    --remove|-r) remove_skill ;;
    *)           install_skill ;;
esac
