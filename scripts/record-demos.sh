#!/usr/bin/env bash
# Record demo GIFs for subreddit showcase.
#
# IMPORTANT: Must be run in an interactive terminal (not from a script runner
# or CI). The TUI needs a real TTY to render.
#
# Usage:
#   ./scripts/record-demos.sh [target]
#
# Targets:
#   all        - Record all GIFs (default)
#   hero       - Full dashboard with all features (~50s, press q after one cycle)
#   health     - Health monitoring showcase (~20s)
#   brain      - Brain + rules showcase (~26s)
#   overview   - Quick dashboard overview (~12s)
#   skills     - Skills & Hive mode (~30s, requires CLAUDECTL_DEMO_SKILLS=1)
#
# Requirements:
#   - agg (cargo install agg, or: brew install agg)
#   - claudectl built (cargo build --release)
#
# Quick single recordings:
#   claudectl --demo --record demo-hero.gif     # Press q to stop
#   claudectl --demo --record demo-hero.cast    # Convert later: agg demo-hero.cast demo-hero.gif

set -euo pipefail
cd "$(dirname "$0")/.."

BINARY="./target/release/claudectl"
OUT_DIR="docs/assets"
mkdir -p "$OUT_DIR"

# Check for interactive terminal
if [ ! -t 0 ]; then
    echo "Error: This script must be run in an interactive terminal (needs a TTY)."
    echo ""
    echo "Quick alternative — run these directly in your terminal:"
    echo "  claudectl --demo --record docs/assets/demo-hero.gif"
    echo "  # Press q after ~30s to stop recording"
    exit 1
fi

# Check for agg
if ! command -v agg &>/dev/null; then
    echo "Error: agg not found. Install with: cargo install agg"
    exit 1
fi

# Build release binary if needed
if [ ! -f "$BINARY" ] || [ src/demo.rs -nt "$BINARY" ]; then
    echo "Building release binary..."
    cargo build --release
fi

record_gif() {
    local name="$1"
    local duration="$2"
    local desc="$3"
    local output="$OUT_DIR/${name}.gif"
    local cast="$OUT_DIR/${name}.cast"

    echo ""
    echo "Recording: $name ($desc)"
    echo "  Will auto-stop after ${duration}s (or press q to stop early)"
    echo "  Output: $output"
    echo ""

    # Use --duration for graceful auto-quit (flushes recording properly)
    "$BINARY" --demo --record "$cast" --duration "${duration}" 2>/dev/null || true

    if [ -f "$cast" ] && [ "$(wc -l < "$cast")" -gt 1 ]; then
        echo "  Converting to GIF..."
        # Use resvg renderer for proper Unicode block character support.
        # Don't override cols/rows — use the terminal dimensions from the cast file.
        agg --font-size 14 --speed 1.5 --renderer resvg --theme dracula "$cast" "$output" 2>/dev/null
        rm -f "$cast"
        local size
        size=$(du -h "$output" | cut -f1)
        echo "  Done: $output ($size)"
    else
        echo "  Warning: recording too short or failed. Try running manually:"
        echo "    $BINARY --demo --record $output"
        rm -f "$cast"
    fi
}

target="${1:-all}"

case "$target" in
    hero|all)
        # Full cycle: 24 ticks * 2s = 48s — captures all features
        record_gif "demo-hero" 50 "Full dashboard with health, brain, rules, routing"
        ;;&

    health|all)
        # Health icons visible from the start (cache, stall, context, cost spike, loops)
        record_gif "demo-health" 20 "Health monitoring — cache, context, cost, stalls, loops"
        ;;&

    brain|all)
        # Rules fire at ticks 3,5,8; brain at 10,13,15; routing at 18
        record_gif "demo-brain-rules" 40 "Brain auto-pilot and rules engine"
        ;;&

    overview|all)
        # Quick dashboard for r/commandline
        record_gif "demo-overview" 12 "Live dashboard — status, cost, context at a glance"
        ;;&

    skills|all)
        # Skills & Hive mode — auto-opens K view, cycles Skills → Hive → Skills.
        # 30s captures ~2 full cycles of the scripted tab rotation in refresh_demo.
        export CLAUDECTL_DEMO_SKILLS=1
        record_gif "claudectl-demo-skills" 30 "Skills & Hive — discover, share, invite, join"
        unset CLAUDECTL_DEMO_SKILLS
        ;;&

    social)
        # 30-second showcase for social media (README, Twitter, etc.)
        record_gif "demo-social" 30 "30s social media showcase — brain + health + orchestration"
        echo ""
        echo "Next steps for social sharing:"
        echo "  1. Compress: gifsicle -O3 --lossy=80 $OUT_DIR/demo-social.gif -o $OUT_DIR/demo-social-opt.gif"
        echo "  2. Add to README above the asciinema embed"
        echo "  3. Upload to GitHub release assets for hotlinking"
        ;;

    *)
        if [ "$target" != "all" ] && [ "$target" != "hero" ] && [ "$target" != "health" ] && [ "$target" != "brain" ] && [ "$target" != "overview" ] && [ "$target" != "social" ] && [ "$target" != "skills" ]; then
            echo "Unknown target: $target"
            echo "Usage: $0 [all|hero|health|brain|overview|social|skills]"
            exit 1
        fi
        ;;
esac

echo ""
echo "All recordings complete. Files in $OUT_DIR/"
ls -lh "$OUT_DIR"/demo-*.gif 2>/dev/null
echo ""
echo "Recommended sizes for Reddit:"
echo "  - Hero: < 5MB (compress with: gifsicle -O3 --lossy=80 in.gif -o out.gif)"
echo "  - Individual features: < 2MB"
