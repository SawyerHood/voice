#!/bin/bash
set -e

if [ -z "$1" ]; then
  echo "Usage: pnpm version:bump <version>"
  echo "Example: pnpm version:bump 0.2.0"
  exit 1
fi

VERSION="$1"

# Update package.json
node -e "
const fs = require('fs');
const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
pkg.version = '$VERSION';
fs.writeFileSync('package.json', JSON.stringify(pkg, null, 2) + '\n');
"

# Update src-tauri/tauri.conf.json
node -e "
const fs = require('fs');
const conf = JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json', 'utf8'));
conf.version = '$VERSION';
fs.writeFileSync('src-tauri/tauri.conf.json', JSON.stringify(conf, null, 2) + '\n');
"

# Update src-tauri/Cargo.toml version
sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" src-tauri/Cargo.toml

echo "✅ Bumped version to $VERSION in package.json, tauri.conf.json, and Cargo.toml"
