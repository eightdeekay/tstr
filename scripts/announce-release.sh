#!/usr/bin/env bash
#
# Announce a tstr release to Slack.
#
# Reads the version from Cargo.toml and the headline from the newest CHANGELOG
# entry, builds a chat.postMessage payload, and POSTs it with curl. Called as
# the last step of the /release flow; safe to run standalone.
#
#   scripts/announce-release.sh --dry-run     # print the payload, post nothing
#   scripts/announce-release.sh               # actually post
#
# Configuration comes from the environment so nothing workspace-specific is
# committed to this public repo:
#
#   SLACK_BOT_TOKEN             xoxb-… bot token with chat:write
#   TSTR_RELEASE_SLACK_CHANNEL  channel name (#foo) or ID (C…/G…)
#
# The token is only ever passed to curl as "$SLACK_BOT_TOKEN" — it is never
# echoed, and curl is deliberately never run with -v (which would print the
# Authorization header).

set -euo pipefail

cd "$(dirname "$0")/.."

DRY_RUN=0
CHANNEL="${TSTR_RELEASE_SLACK_CHANNEL:-}"
VERSION=""
HEADLINE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run)  DRY_RUN=1; shift ;;
    --channel)  CHANNEL="$2"; shift 2 ;;
    --version)  VERSION="$2"; shift 2 ;;
    --headline) HEADLINE="$2"; shift 2 ;;
    -h|--help)  sed -n '3,20p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# --- gather facts -----------------------------------------------------------

if [ -z "$VERSION" ]; then
  VERSION=$(python3 -c "
import re
src = open('Cargo.toml').read()
print(re.search(r'(?m)^version\s*=\s*\"([^\"]+)\"', src).group(1))
")
fi

REPO_URL=$(python3 -c "
import re
src = open('Cargo.toml').read()
m = re.search(r'(?m)^repository\s*=\s*\"([^\"]+)\"', src)
print(m.group(1).rstrip('/') if m else '')
")

# Headline = the bolded lead of the first bullet under the newest CHANGELOG
# entry. That bold text is written as a standalone sentence by convention, so
# it makes a serviceable one-line summary without any hand-editing.
if [ -z "$HEADLINE" ]; then
  HEADLINE=$(python3 -c "
import re, sys

lines = open('CHANGELOG.md').read().splitlines()
start = next((i for i, l in enumerate(lines) if l.startswith('## [')), None)
if start is None:
    sys.exit('no version heading found in CHANGELOG.md')

# Stop at the next version heading so we only read the newest entry.
end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith('## [')), len(lines))
entry = '\n'.join(lines[start + 1:end])

m = re.search(r'^- \*\*(.+?)\*\*', entry, re.S | re.M)
if not m:
    sys.exit('no bolded bullet lead found in the newest CHANGELOG entry')

# Collapse the hard-wrapped markdown back into one line.
print(' '.join(m.group(1).split()))
")
fi

if [ -z "$CHANNEL" ]; then
  echo "no channel: set TSTR_RELEASE_SLACK_CHANNEL or pass --channel" >&2
  exit 2
fi

if [ -z "${SLACK_BOT_TOKEN:-}" ]; then
  echo "SLACK_BOT_TOKEN is not set" >&2
  exit 2
fi

# --- resolve the channel ----------------------------------------------------

# chat.postMessage takes an ID reliably; a #name only sometimes. Resolve names
# to IDs up front so a rename or a scope gap fails here with a clear message
# rather than silently posting nowhere.
if printf '%s' "$CHANNEL" | grep -qE '^[CGD][A-Z0-9]{6,}$'; then
  CHANNEL_ID="$CHANNEL"
else
  WANTED="${CHANNEL#\#}"
  # A dry run should still show the payload even when the channel can't be
  # resolved yet (e.g. the bot hasn't been invited to a private channel), so
  # failure here is fatal only for a real post.
  set +e
  CHANNEL_ID=$(curl -s \
    "https://slack.com/api/conversations.list?limit=1000&exclude_archived=true&types=public_channel,private_channel" \
    -H "Authorization: Bearer $SLACK_BOT_TOKEN" \
    | WANTED="$WANTED" python3 -c "
import json, os, sys
d = json.load(sys.stdin)
if not d.get('ok'):
    sys.exit('conversations.list failed: ' + d.get('error', 'unknown'))
wanted = os.environ['WANTED']
for c in d.get('channels', []):
    if c['name'] == wanted:
        print(c['id']); break
else:
    sys.exit(f'channel #{wanted} not visible to this bot — invite it, or grant groups:read if private')
")
  RESOLVE_STATUS=$?
  set -e
  if [ $RESOLVE_STATUS -ne 0 ]; then
    if [ "$DRY_RUN" -eq 1 ]; then
      echo "(unresolved channel — showing payload anyway since this is a dry run)" >&2
      CHANNEL_ID="$CHANNEL"
    else
      exit $RESOLVE_STATUS
    fi
  fi
fi

# --- build the payload ------------------------------------------------------

PAYLOAD=$(mktemp -t tstr-slack-XXXXXX.json)
trap 'rm -f "$PAYLOAD"' EXIT

TAG_URL="$REPO_URL/releases/tag/v$VERSION"

CHANNEL_ID="$CHANNEL_ID" VERSION="$VERSION" HEADLINE="$HEADLINE" TAG_URL="$TAG_URL" \
python3 -c "
import json, os

version  = os.environ['VERSION']
headline = os.environ['HEADLINE']
tag_url  = os.environ['TAG_URL']

text = f':package: *tstr {version}* — {headline}'
if tag_url.startswith('http'):
    text += f' <{tag_url}|Release notes>'

payload = {
    'channel': os.environ['CHANNEL_ID'],
    'text': text,
    # Suppress link unfurling — the GitHub preview card is bigger than the
    # message and buries the one line anyone needs to read.
    'unfurl_links': False,
    'unfurl_media': False,
}
print(json.dumps(payload, indent=2))
" > "$PAYLOAD"

if [ "$DRY_RUN" -eq 1 ]; then
  echo "--- DRY RUN: would POST to chat.postMessage ---"
  cat "$PAYLOAD"
  echo "--- nothing sent ---"
  exit 0
fi

# --- post -------------------------------------------------------------------

curl -s -X POST https://slack.com/api/chat.postMessage \
  -H "Authorization: Bearer $SLACK_BOT_TOKEN" \
  -H "Content-Type: application/json; charset=utf-8" \
  --data @"$PAYLOAD" \
  | python3 -c "
import json, sys
d = json.load(sys.stdin)
if d.get('ok'):
    print(f\"posted to {d.get('channel')} at {d.get('ts')}\")
else:
    sys.exit('slack error: ' + d.get('error', 'unknown'))
"
