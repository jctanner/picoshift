#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEPS_DIR="${PROJECT_DIR}/deps"

declare -A REPOS=(
    [kind]="git@github.com:jctanner/kind|OCP_SHIM"
    [opendatahub-operator]="https://github.com/opendatahub-io/opendatahub-operator|main"
    [entra-id-emulator]="https://github.com/jctanner/entra-id-emulator|main"
)

mkdir -p "$DEPS_DIR"

for name in "${!REPOS[@]}"; do
    IFS='|' read -r url branch <<< "${REPOS[$name]}"
    dir="${DEPS_DIR}/${name}"

    if [ -d "$dir" ]; then
        echo "=== ${name}: already cloned, checking branch ==="
        cd "$dir"
        current=$(git branch --show-current 2>/dev/null || echo "")
        if [ "$current" != "$branch" ]; then
            echo "    switching from '${current}' to '${branch}'"
            git checkout "$branch"
        fi
        echo "    pulling latest..."
        git pull --ff-only || echo "    (pull skipped — may have local changes)"
        cd "$PROJECT_DIR"
    else
        echo "=== ${name}: cloning ${url} (branch: ${branch}) ==="
        git clone --branch "$branch" "$url" "$dir"
    fi
done

echo ""
echo "=== deps/ ready ==="
for name in "${!REPOS[@]}"; do
    IFS='|' read -r _ branch <<< "${REPOS[$name]}"
    echo "  ${name}/ (${branch})"
done
