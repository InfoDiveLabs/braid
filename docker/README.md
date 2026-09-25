# Running Braid in a container

`braid-server` in one image: a torrent client and an HTTP download manager
where a Deluge or qBittorrent container sits today, instead of a torrent
client plus a shell script with curl in it.

## What works today

This image builds and runs `braid-server`, which currently answers
`GET /health` and nothing else. The qBittorrent-compatible API and the
embedded web UI described in the rest of this document are being built in
parallel and are not finished. Do not point Sonarr, Radarr, or anything else
at this container expecting the qBittorrent API to be there yet: it is not,
and claiming otherwise here would just move the surprise to whoever tries it
first. Once that work lands this file should be the first thing updated.

## Quick start

```
cp docker/compose.example.yml compose.yml
docker compose up -d
docker compose logs braid   # the generated admin password is in here, once
```

## Volumes

- `/config`: `server.conf` and anything else Braid persists between runs.
- `/downloads`: where files land. Point this at the same directory a
  Deluge or qBittorrent container already writes into and Braid slots in
  without moving anything.

## Environment variables

Read from `crates/dl-server/src/config.rs`. Each is `BRAID_` plus the field
name, upper-cased. A `server.conf` file in `/config` sets the same fields;
the environment always wins over the file, and the file always wins over the
default below.

| variable | default | meaning |
|---|---|---|
| `BRAID_WEB_PORT` | `8080` | the port serving the web UI and both APIs |
| `BRAID_DOWNLOAD_DIR` | `/downloads` | where files are written |
| `BRAID_CONFIG_DIR` | `/config` | where `server.conf` and persisted state live |
| `BRAID_TORRENT_PORT` | `6881` | the BitTorrent listen port, TCP and UDP |
| `BRAID_MAX_CONCURRENT` | `3` | active downloads at once |
| `BRAID_CONNECTIONS` | `8` | connections per download |
| `BRAID_DOWNLOAD_LIMIT` | unlimited | download rate cap, bytes per second |
| `BRAID_UPLOAD_LIMIT` | unlimited | upload rate cap, bytes per second (torrents) |
| `BRAID_AUTH_REQUIRED` | `true` | require login; leave this alone unless you know why you wouldn't |
| `BRAID_INTERFACES` | unset | comma separated interface names to spread a download across, e.g. `eth0,wlan0`; unset lets the OS route |

Plus the container-level variables the entrypoint reads before any of the
above: `PUID`, `PGID`, and optionally `UMASK` (default `022`). `PUID` has no
default and the container refuses to start without it, rather than writing
root-owned files into whatever `/downloads` is pointed at.

## Finding the admin password

The first time the server starts with no existing account, it generates one
and prints the password once, on its own line surrounded by blank lines, so
it is easy to spot in a scrolling log:

```
docker compose logs braid
```

It is not printed again on later starts. If you lose it before writing it
down, the current recovery path is whatever the auth work documents; check
there once it lands.

## Migrating from qBittorrent

Not yet possible in the way that phrase usually means: pointing the same
`*arr` app config at this container instead. The qBittorrent-compatible
`/api/v2` surface is still being built. What you can do today is run this
container alongside the one you have, pointed at the same `/downloads`, and
switch the `*arr` apps over once that API exists and this file says so.
