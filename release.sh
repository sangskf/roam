#!/bin/bash

# Exit on error
set -e

if [ -z "$1" ]; then
  echo "Usage: ./release.sh <version>"
  echo "Example: ./release.sh 0.1.1"
  exit 1
fi

VERSION=$1

# Check if git is clean
if [ -n "$(git status --porcelain)" ]; then
  echo "Error: Git working directory is not clean. Please commit or stash changes first."
  exit 1
fi

echo "🚀 Bumping version to $VERSION..."

# Function to update version using perl (works on both macOS and Linux)
update_version() {
  local file=$1
  if [ -f "$file" ]; then
    echo "Updating $file..."
    # Match 'version = "..."' at the start of a line (standard Cargo.toml format)
    perl -pi -e 's/^version = ".*"/version = "'$VERSION'"/' "$file"
  else
    echo "Warning: $file not found, skipping."
  fi
}

# Update Cargo.toml files
update_version "server/Cargo.toml"
update_version "client/Cargo.toml"
update_version "common/Cargo.toml"

# Update Cargo.lock by running check
echo "📦 Updating Cargo.lock..."
cargo check

# Git commit and tag
echo "Commiting and tagging..."
git add .
git commit -m "Bump version to v$VERSION"
git tag "v$VERSION"

echo "✅ Done! Version bumped to v$VERSION."
echo "👉 Run the following command to push changes and trigger the release workflow:"
echo ""
echo "    git push && git push --tags"
echo ""
