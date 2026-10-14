#!/usr/bin/env bash
# Scaffolds a new deckd / Stream Deck plugin from the bundled Node.js template.
#
#   new-plugin.sh <uuid> "<Display Name>" [parent-dir] [author]
#
# Example: new-plugin.sh com.amir.weather "Weather" ~/dev/plugins
# Creates <parent-dir>/<uuid>.sdPlugin with the UUID, name and author filled in.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
fi

uuid=$1
name=$2
parent=${3:-.}
author=${4:-$(git config user.name 2>/dev/null || whoami)}

# Elgato UUIDs are reverse-DNS: lowercase letters, digits, dots and hyphens.
if [[ ! $uuid =~ ^[a-z0-9-]+(\.[a-z0-9-]+)+$ ]]; then
  echo "error: uuid must be reverse-DNS like com.yourname.plugin (lowercase, dots)" >&2
  exit 1
fi

template="$(cd "$(dirname "$0")/../assets/template-node" && pwd)"
dest="$parent/$uuid.sdPlugin"
if [[ -e $dest ]]; then
  echo "error: $dest already exists" >&2
  exit 1
fi

mkdir -p "$parent"
cp -r "$template" "$dest"

# Escape characters that are special in sed replacements.
esc() { printf '%s' "$1" | sed -e 's/[\/&|]/\\&/g'; }
for f in "$dest/manifest.json" "$dest/plugin.js"; do
  sed -i -e "s|__UUID__|$(esc "$uuid")|g" -e "s|__NAME__|$(esc "$name")|g" -e "s|__AUTHOR__|$(esc "$author")|g" "$f"
done

cat <<MSG
Created $dest

Next:
  ln -s "$(realpath "$dest")" ~/.config/deckd/plugins/
  systemctl --user restart deckd
  open http://127.0.0.1:28196 and drag "$name → Counter" onto a key or dial
Logs: tail -f ~/.config/deckd/logs/$uuid.log
MSG
