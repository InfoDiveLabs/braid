# Running Braid in a container

`braid-server` in one image: a torrent client and an HTTP download manager
where a Deluge or qBittorrent container sits today, instead of a torrent
client plus a shell script with curl in it.

## What works today

- The web UI, on the same port.
- Braid's own API at `/api/v1`: both kinds of transfer, live progress,
  checksums, per lane throughput.
- A qBittorrent-compatible API at `/api/v2`, enough for Sonarr, Radarr,
  Prowlarr and Lidarr to drive it as a download client.
- BitTorrent, and HTTP downloads with resume, per chunk hashing and link
  refreshing. That second half is the reason this exists: qBittorrent does
  not do it, so most stacks run a torrent client plus a shell script with
  curl in it, and the script has no resume and no integrity check.

Verified against a real Sonarr 4.0.20 and Radarr 6.4.4, from grab to library
import, with those exchanges replaying as tests. The API surface those clients
need is covered; `harness/README.md` lists what is not, and Limitations below
covers what will bite you.

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

It is not printed again on later starts. If you lose it, delete
`credentials.conf` from `/config` and restart: a new password is generated
and printed the same way. Nothing else in `/config` is touched, so the
transfer list and settings survive.

## Replacing a qBittorrent container

Braid answers the same API, on the same kind of port, with the same
username and password shape. The migration is a settings change in each
`*arr` app, not a re-import.

**1. Run it beside the container you have**, pointed at the same
`/downloads`. Nothing is moved and nothing is shared: both can be up while
you switch over, and you can switch back by reversing step 3.

```
cp docker/compose.example.yml compose.yml
# edit PUID/PGID to your user, and point ./downloads at the directory your
# existing client already writes into
docker compose up -d
docker compose logs braid    # the admin password is here, printed once
```

**2. Stop the old client from taking new work.** Pause its queue, or set
its `*arr` download client to disabled. Leave it running until whatever it
is mid-download has finished: Braid does not adopt another client's
in-flight torrents, and killing them loses that progress.

**3. Point each `*arr` app at Braid.** Settings, then Download Clients, then
the qBittorrent entry you already have. Change:

| field | value |
|---|---|
| Host | the container name, `braid`, or the host's address |
| Port | `8080` |
| Username | `admin` |
| Password | the one printed in the log at first start |
| Category | leave exactly as it is, for example `tv-sonarr` |
| Use SSL | off, unless you have put a reverse proxy in front |

Press Test. It logs in, reads the version, and asks for its category. Then
Save.

**Leave the category alone.** It is how each app finds its own downloads
again, and Braid stores it against the transfer and keeps it across
restarts. Changing it here means the app stops recognising anything it
asked for before the change.

**4. Check one download all the way through** before removing the old
container. Grab something small, watch it appear in Braid's web UI, and
confirm the `*arr` app imports it when it finishes.

## Limitations worth knowing before you switch

**Seed and leecher counts are always zero.** Everything else in the
listing is measured. If a client of yours gates on those numbers, this will
not suit you yet.

**Queue priority and share limits are accepted and ignored.** There is no
queue, so `torrents/topPrio` does nothing. `setForceStart` is the exception
and genuinely starts a torrent.

**Content layout is not configurable.** Every torrent gets a folder of its
own. qBittorrent's "original layout" and "don't create subfolder" modes have
no equivalent.

**Bandwidth limits cannot be set over the API.** Set them in Braid's own web
UI instead.

**Trackers, peers, per-file priority, recheck, rename and set-location are
not implemented.** Generic qBittorrent clients and mobile apps expect these;
the `*arr` apps do not use them.

**Nothing here is code signed or audited.** This is a beta.

Each of these is tracked under
[qBittorrent API compatibility](https://github.com/InfoDiveLabs/braid/issues/12).

## If something does not work

The most useful thing you can send is what the client actually asked for
and what it got back. Braid logs at `RUST_LOG=debug`, and the exchange with
an `*arr` app is small enough to read.
