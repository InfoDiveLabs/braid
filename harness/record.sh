#!/usr/bin/env bash
# Bring up braid-server built from the working tree next to a real Sonarr and
# a real Radarr, register braid as each one's download client through their
# own REST APIs, and record what they actually send it.
#
# Not run in CI. Docker images for Sonarr and Radarr are large and their
# first start is slow: this script waits rather than guessing at a timeout
# that would be wrong on a slower link.
#
# Usage: harness/record.sh [up|register|push|down]
#   up        build and start the stack, wait for both apps to be ready.
#   register  read each app's generated API key and register braid as its
#             download client, using that app's own schema endpoint rather
#             than a guessed field list.
#   push      push a manual release into Sonarr so it grabs it through the
#             registered client, exercising the add path for real. Takes a
#             magnet URI and an optional release title (see cmd_push below
#             for why the title has to name a series Sonarr already knows).
#   down      stop the stack. Volumes under harness/data are left in place
#             so a re-run does not have to answer the setup wizard again;
#             pass --clean to remove them too.
#
# With no argument, runs up then register.

set -euo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f compose.yml"

wait_for() {
    local description="$1"
    local check="$2"
    local attempts="${3:-120}"
    local delay="${4:-5}"
    for ((i = 0; i < attempts; i++)); do
        if eval "$check"; then
            return 0
        fi
        sleep "$delay"
    done
    echo "timed out waiting for: $description" >&2
    return 1
}

braid_password() {
    # Printed once, on its own line, between blank lines. See
    # auth::announce_generated_password. Base32, RFC 4648 alphabet, 26
    # characters for 16 random bytes: distinctive enough to grep for without
    # matching anything else this log line touches.
    $COMPOSE logs braid 2>/dev/null | grep -oE '[A-Z2-7]{26}' | tail -1
}

api_key_from() {
    local config_file="$1"
    grep -oE '<ApiKey>[^<]+' "$config_file" | sed 's/<ApiKey>//'
}

cmd_up() {
    mkdir -p data/braid/config data/braid/downloads
    mkdir -p data/sonarr/config data/sonarr/downloads
    mkdir -p data/radarr/config data/radarr/downloads
    mkdir -p recordings

    echo "building and starting the stack..."
    $COMPOSE up -d --build

    echo "waiting for braid's generated admin password..."
    wait_for "braid's admin password in the logs" '[ -n "$(braid_password)" ]' 60 2
    echo "braid password: $(braid_password)"

    echo "waiting for Sonarr to write config.xml..."
    wait_for "data/sonarr/config/config.xml to exist" '[ -f data/sonarr/config/config.xml ]' 60 3
    echo "waiting for Radarr to write config.xml..."
    wait_for "data/radarr/config/config.xml to exist" '[ -f data/radarr/config/config.xml ]' 60 3

    echo "waiting for Sonarr's API to answer..."
    wait_for "Sonarr HTTP" 'curl -sf http://localhost:8989/ping >/dev/null'
    echo "waiting for Radarr's API to answer..."
    wait_for "Radarr HTTP" 'curl -sf http://localhost:7878/ping >/dev/null'

    echo "stack is up."
}

# Fetch the qBittorrent schema from an *arr app's own downloadclient/schema
# endpoint, fill in the fields this harness needs, and hand back the JSON
# body ready to POST. Reading the schema rather than hand-writing the field
# list is the whole point: it is what makes this "Sonarr's own API" instead
# of one more guess at what Sonarr's API wants.
build_client_body() {
    local base_url="$1" api_key="$2" category_field="$3" category="$4" password="$5"
    local schema
    schema=$(curl -sf "$base_url/api/v3/downloadclient/schema" -H "X-Api-Key: $api_key")
    python3 - "$schema" "$category_field" "$category" "$password" <<'PY'
import json
import sys

schema, category_field, category, password = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
entries = json.loads(schema)
entry = next(e for e in entries if e["implementation"] == "QBittorrent")
entry["name"] = "Braid"
entry["enable"] = True

def set_field(name, value):
    for f in entry["fields"]:
        if f["name"] == name:
            f["value"] = value
            return
    raise SystemExit(f"field {name!r} not found in this app's own schema")

set_field("host", "braid-proxy")
set_field("port", 8080)
set_field("useSsl", False)
set_field("username", "admin")
set_field("password", password)
# Sonarr's own qBittorrent schema calls this field tvCategory and Radarr's
# calls it movieCategory: this was not documented anywhere and only came out
# of the app's own schema response, which is why record.sh reads the schema
# live rather than posting a hand-written body.
set_field(category_field, category)

print(json.dumps(entry))
PY
}

register_client() {
    local label="$1" base_url="$2" config_file="$3" category_field="$4" category="$5" password="$6"
    local api_key
    api_key=$(api_key_from "$config_file")
    echo "$label API key: $api_key"

    local body
    body=$(build_client_body "$base_url" "$api_key" "$category_field" "$category" "$password")

    echo "$label: testing the client before saving it..."
    local test_status
    test_status=$(curl -s -o /tmp/harness-test-response.json -w '%{http_code}' \
        -X POST "$base_url/api/v3/downloadclient/test" \
        -H "X-Api-Key: $api_key" -H "Content-Type: application/json" \
        -d "$body")
    echo "$label: test responded $test_status"
    cat /tmp/harness-test-response.json
    echo

    echo "$label: saving the client..."
    curl -sf -X POST "$base_url/api/v3/downloadclient" \
        -H "X-Api-Key: $api_key" -H "Content-Type: application/json" \
        -d "$body" | python3 -m json.tool
}

cmd_register() {
    local password
    password=$(braid_password)
    if [ -z "$password" ]; then
        echo "no braid password found, run '$0 up' first" >&2
        exit 1
    fi

    register_client "Sonarr" "http://localhost:8989" "data/sonarr/config/config.xml" \
        "tvCategory" "tv-sonarr" "$password"
    register_client "Radarr" "http://localhost:7878" "data/radarr/config/config.xml" \
        "movieCategory" "movies-radarr" "$password"
}

# Push a manually crafted release straight at Sonarr's queue, the same
# endpoint Sonarr's own "Interactive Search: push a result by hand" feature
# uses. It bypasses needing a real indexer, and it is the only way in this
# harness to make Sonarr actually call torrents/add for real: there is no
# indexer here that would ever hand it a genuine release otherwise.
#
# The title has to name a series (and, for a season pack or single episode,
# an SxxEyy Sonarr can parse) that Sonarr already knows about, or the push is
# accepted but rejected as "Unknown Series" without ever reaching the
# download client. Adding that series first is not something this script
# does: see harness/README.md for how it was done by hand while recording the
# fixtures under crates/dl-server/tests/fixtures/.
cmd_push() {
    local api_key
    api_key=$(api_key_from "data/sonarr/config/config.xml")
    local magnet="${1:?usage: record.sh push <magnet-uri> [release-title]}"
    local title="${2:-Pioneer.One.S01E01.720p.WEB.x264-GROUP}"
    curl -sf -X POST "http://localhost:8989/api/v3/release/push" \
        -H "X-Api-Key: $api_key" -H "Content-Type: application/json" \
        -d "$(python3 - "$magnet" "$title" <<'PY'
import json, sys
magnet, title = sys.argv[1], sys.argv[2]
print(json.dumps({
    "title": title,
    "downloadUrl": magnet,
    "protocol": "torrent",
    "publishDate": "2026-01-01T00:00:00Z",
    "size": 0,
    "indexerId": 0,
    "indexer": "manual",
    "guid": f"harness-manual-push-{title}",
}))
PY
)" | python3 -m json.tool
}

cmd_down() {
    $COMPOSE down
    if [ "${1:-}" = "--clean" ]; then
        rm -rf data recordings
    fi
}

case "${1:-}" in
    up) cmd_up ;;
    register) cmd_register ;;
    push) shift; cmd_push "$@" ;;
    down) shift; cmd_down "$@" ;;
    "") cmd_up && cmd_register ;;
    *) echo "usage: $0 [up|register|push <magnet>|down [--clean]]" >&2; exit 1 ;;
esac
