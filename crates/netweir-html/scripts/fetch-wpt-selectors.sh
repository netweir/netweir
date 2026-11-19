#!/bin/sh
# Fetches web-platform-tests' selector tests (css/selectors and the
# dom/nodes querySelector, matches and closest suites) at a pinned commit
# and turns the static parts into tests/wpt-selectors/cases.json with
# scripts/wpt-selectors-cases.js (needs node). Run it to update them:
#   scripts/fetch-wpt-selectors.sh COMMIT
set -eu
commit=${1:?usage: scripts/fetch-wpt-selectors.sh WPT_COMMIT}
dir=tests/wpt-selectors
raw="https://raw.githubusercontent.com/web-platform-tests/wpt/$commit"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {
  mkdir -p "$tmp/$(dirname "$1")"
  curl -fsS "$raw/$1" -o "$tmp/$1"
}
for f in dom/nodes/selectors.js dom/nodes/ParentNode-querySelector-All-content.html \
  dom/nodes/Element-closest.html LICENSE.md; do
  fetch "$f"
done
# Every file under css/selectors, through the tree API: the contents API
# lists one directory at a time.
tree=$(gh api "repos/web-platform-tests/wpt/git/trees/$commit:css/selectors?recursive=1" \
  --jq '.tree[] | select(.type == "blob") | .path')
echo "$tree" | grep -E '\.(html|xht|xhtml)$' | while read -r f; do
  echo "css/selectors/$f"
done | xargs -P 16 -n 1 sh -c 'mkdir -p "'"$tmp"'/$(dirname "$0")" && curl -fsS "'"$raw"'/$0" -o "'"$tmp"'/$0"'
echo "$commit" > "$tmp/COMMIT"

rm -rf "$dir" && mkdir -p "$dir"
node scripts/wpt-selectors-cases.js "$tmp" "$dir"
cp "$tmp/LICENSE.md" "$tmp/COMMIT" "$dir/"
