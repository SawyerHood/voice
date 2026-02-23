#!/bin/bash
set -e

# Read version from tauri.conf.json
VERSION=$(node -e "console.log(JSON.parse(require('fs').readFileSync('src-tauri/tauri.conf.json', 'utf8')).version)")
TAG="v$VERSION"

echo "📦 Creating GitHub Release $TAG..."

# Check if gh CLI is available
if ! command -v gh &> /dev/null; then
  echo "❌ GitHub CLI (gh) is required. Install with: brew install gh"
  exit 1
fi

# Find the build artifacts
DMG=$(ls src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1)
UPDATE_JSON=$(ls src-tauri/target/release/bundle/macos/*.json 2>/dev/null | head -1)
UPDATE_SIG=$(ls src-tauri/target/release/bundle/macos/*.sig 2>/dev/null | head -1)

if [ -z "$DMG" ]; then
  echo "❌ No DMG found. Run 'pnpm release' first."
  exit 1
fi

# Commit version bump if there are changes
if ! git diff --quiet; then
  git add -A
  git commit -m "chore: bump version to $VERSION"
fi

# Create and push tag
git tag -a "$TAG" -m "Buzz $TAG"
git push origin main
git push origin "$TAG"

# Build upload args
UPLOAD_ARGS="$DMG"
[ -n "$UPDATE_JSON" ] && UPLOAD_ARGS="$UPLOAD_ARGS $UPDATE_JSON"
[ -n "$UPDATE_SIG" ] && UPLOAD_ARGS="$UPLOAD_ARGS $UPDATE_SIG"

# Also upload latest.json for the auto-updater endpoint
# Tauri v2 generates update info — look for it
LATEST_JSON="src-tauri/target/release/bundle/macos/latest.json"
if [ -f "$LATEST_JSON" ]; then
  UPLOAD_ARGS="$UPLOAD_ARGS $LATEST_JSON"
fi

# Create GitHub Release
gh release create "$TAG" $UPLOAD_ARGS \
  --title "Buzz $TAG" \
  --notes "Buzz $TAG release 🐝" \
  --latest

echo "✅ GitHub Release $TAG created and artifacts uploaded!"
echo "🔗 https://github.com/SawyerHood/voice/releases/tag/$TAG"
