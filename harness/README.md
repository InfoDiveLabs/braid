# The development harness: what Sonarr and Radarr actually require

Every qBittorrent-compatible endpoint before this harness was written against
qBittorrent's own published API documentation and one engineer's reading of
it. That is belief. This directory exists to replace it with evidence: a real
Sonarr, a real Radarr, and `braid-server` built from the working tree, with a
logging reverse proxy sitting between the two `*arr` applications and Braid so
every request and response can be written to disk and inspected afterwards.

This was run for real. Nothing in this document is a guess at what these
clients would probably do.

**Versions used:** Sonarr `4.0.20.3014-ls325` (`lscr.io/linuxserver/sonarr`),
Radarr `6.4.4.10685-ls318` (`lscr.io/linuxserver/radarr`), both `linuxserver.io`
images pulled at the time this was written. `braid-server` built from
`docker/Dockerfile` against the working tree, unmodified apart from the two
fixes this run found and that are already committed alongside this document
(see "What broke, and what was fixed" below).

## Running it yourself

```
harness/record.sh up         # builds braid, starts Sonarr, Radarr and the proxy, waits for all three
harness/record.sh register   # reads each app's own API key off its config.xml and registers Braid
                              # as its qBittorrent download client, using that app's own
                              # downloadclient/schema endpoint rather than a guessed field list
harness/record.sh push <magnet>   # pushes a manual release into Sonarr so it grabs it for real
harness/record.sh down [--clean]  # stops the stack; --clean also removes harness/data and harness/recordings
```

Every exchange either app makes with Braid, once `register` has pointed them
at `braid-proxy` instead of `braid` directly, is written to
`harness/recordings/` as one JSON file per request/response pair. That
directory is not committed: it is a raw capture from one specific run, full of
container hostnames, session ids and a password that mean nothing outside
that run. What is committed is a curated, sanitised selection of it, under
`crates/dl-server/tests/fixtures/`, with a Rust test beside each one asserting
our own handler reproduces the recorded response for the recorded request.
Not run in CI.

Docker images for Sonarr and Radarr are large (roughly 300MB each) and their
first start is slow. `record.sh up` waits on `config.xml` appearing and on
`/ping` answering rather than guessing at a fixed delay.

## Registering a download client without touching a web UI

Sonarr and Radarr generate an API key into `/config/config.xml` on first
start, and reading it there and driving `POST /api/v3/downloadclient` with
that key is exactly what `record.sh register` does. The one part that is not
obvious from the outside: **the field names on that request are not the same
for the two applications**, and neither is documented anywhere public. Sonarr's
own `GET /api/v3/downloadclient/schema` names the category field `tvCategory`;
Radarr's names the same field `movieCategory`. Guessing `category` for either
one fails outright:

```
{"propertyName": "Category", "errorMessage": "...", ...}
```

`record.sh` never guesses this: it fetches each app's own schema, fills in
`host`, `port`, `username`, `password` and the correct category field name,
and posts that back. This is what "use Sonarr's own REST API" has to mean in
practice, not just in principle.

That local field name is purely each app's own settings-screen label, though.
On the wire, once either app is actually talking qBittorrent's protocol, both
send the plain field `category`, not `tvCategory` or `movieCategory`: see
`sonarr_creates_a_category_with_no_save_path.json`. The distinct names exist
one layer up, in each app's own configuration schema, and never reach Braid.

## What the recorded traffic actually contains

95 `app/webapiVersion` calls, 63 `app/preferences`, 47 `torrents/info`, 20
`torrents/categories`, 6 `auth/login`, 2 `torrents/add`, 2
`torrents/createCategory`, across the whole session (registering both
clients, Sonarr and Radarr each testing the client once per registration
attempt, roughly ninety seconds of their own periodic health-check polling
once registered, and one real add-to-completion cycle). That list is also the
complete list: in this entire session, covering registration, the client
"test" both apps run before saving, their own background polling, and a real
grab, **neither app ever called** `app/version`, `sync/maindata`,
`transfer/info`, `torrents/properties`, `torrents/files`, `torrents/pause`,
`torrents/resume`, `torrents/delete`, `torrents/setShareLimits`, or
`torrents/topPriority`. Braid implements all of them anyway, for the UI's own
use and because a client requesting one of the others is not something to
find out about in production, but the endpoints an add-and-monitor cycle
actually needs are a strict subset of what qBittorrent's documentation
describes, and this is what that subset is.

### The un-cached version check

Both apps call `app/webapiVersion` far more than once. A single client "test"
cycle called it seven to nine times, not once, and `app/version` was never
called at all, in six separate registration/test attempts across both apps.
Whatever gates Sonarr's schema fields on a qBittorrent version (`sequentialOrder`
needs 4.1.0+, `contentLayout` needs 4.3.2+, per the schema's own help text)
checks it live, apparently once per gated field, rather than caching one
answer for the whole request. Braid's `webapiVersion` handler is a constant
string lookup, so the repetition costs nothing, but a slower implementation
would need to know this is not a rare call.

### Basic auth is sent on every request and is not needed

Sonarr attaches an HTTP `Authorization: Basic` header, base64 of
`admin:<password>`, to every single request, including the very first
`webapiVersion` probe sent before any login attempt. Braid's session
middleware never looks at this header at all; the cookie-based login flow
(`POST auth/login` sets `SID`, and every following handler checks `SID`
alone) is sufficient and matches what real qBittorrent's own clients expect.
This was worth confirming rather than assuming: an implementation that used
Basic auth as an alternative or a shortcut would be answering a header these
clients do not actually need honoured, and dropping it entirely (as Braid
already does) breaks nothing.

### A 403 mid-session is recovered from automatically

`braid-server` was restarted twice during this run (see "What broke, and what
was fixed"), and Braid's session store is in memory: a restart forgets every
`SID` a client is still holding. Both apps' very next poll after each restart
hit `app/webapiVersion` and got `403`, and both immediately, automatically,
without any queued failure or manual re-add, issued a fresh `POST
auth/login` and resumed polling in the same tick. No special handling is
needed in Braid for this case, and this was worth checking rather than
assuming, since a client that gave up and needed the download client
re-enabled by hand after a restart would need very different advice in
`docker/README.md` than one that recovers on its own.

## What broke, and what was fixed

### librqbit's DHT cache tries to write to `/root/.cache`

The very first torrent add in this harness failed outright:

```
torrent transfer failed: could not start the torrent session: error
initializing persistent DHT: error creating dir "/root/.cache/com.rqbit.dht":
Permission denied (os error 13)
```

`docker/entrypoint.sh` drops from root to `PUID`/`PGID` with `setpriv` before
running `braid-server`, which is the whole point of `PUID`/`PGID` existing.
`setpriv` changes the process's uid and gid; it does not change `HOME`, which
stays whatever the base image set it to, `/root`. librqbit's DHT persistence
resolves its cache location from that unchanged `HOME`, and a process running
as an arbitrary numeric uid has no permission to write under `/root` at all.
**This is not specific to this harness.** It reproduces with the plain
`docker/compose.example.yml`, for any `PUID` other than `0`, which is to say
for every container that follows the advice `docker/README.md` already gives.
Every torrent add in the shipped image fails this way today.

This harness works around it by setting `HOME=/config` on the `braid`
service in `compose.yml`, which is writable by the configured `PUID` for the
same reason `/config/credentials.conf` already is. That is a harness-only
patch, not a fix: the real fix belongs in `docker/entrypoint.sh` (export a
sane `HOME` before the final `exec`) or in `dl-torrent` (pass librqbit an
explicit persistence path derived from `BRAID_CONFIG_DIR` rather than letting
it resolve one from the environment), and is not part of this change. Filing
it as a follow-up matters more than this paragraph does.

### An empty category save path was taken literally

Sonarr and Radarr both call `torrents/createCategory` with no `savePath`
field at all (see `sonarr_creates_a_category_with_no_save_path.json`); both
rely entirely on the category existing, with an empty save path recorded
against it, to mean "put it under the default download directory, in a
folder named for the category." Braid's `destination_for` used to take that
recorded empty string literally: `PathBuf::from("")` resolves to the
process's own working directory, which is neither `/downloads` nor writable
by the container's configured user. **Every real add through either
application failed** with a permission error, the same shape as the DHT bug
above but in ordinary application code rather than in a container detail:

```
the torrent could not be read: error opening "Big Buck Bunny.en.srt" in
read/write mode: Permission denied (os error 13)
```

This one is fixed in `crates/dl-server/src/qbit/torrents.rs`:
`destination_for` now resolves an empty category save path to
`<download_dir>/<category name>`, matching real qBittorrent's own documented
behaviour for a category with no save path set. `crates/dl-server/tests/
fixtures/sonarr_adds_a_magnet_with_a_category_and_no_save_path.json` and its
test are the regression coverage; `a_category_created_with_no_save_path_
still_lands_somewhere_writable` in `qbit/torrents.rs` is a second, narrower
one for the same bug written directly against `destination_for`'s own tests.

With that fix and the `HOME` workaround both in place, a real add-to-completion
cycle was driven end to end: a magnet pushed into Sonarr via
`POST /api/v3/release/push` (see "Driving a real download" below) was grabbed,
added to Braid, downloaded from 32 real peers on the public BitTorrent network
at roughly 20 to 30 MB/s, and finished in under a minute. `torrents_info_
while_downloading.json` and `torrents_info_after_completion.json` are that
exact run.

### A completed download with no top-level folder cannot be told apart from its category directory

Once the fix above landed and a torrent actually finished, Sonarr's own queue
correctly saw it as fully fetched (`sizeleft: 0`) but refused to import it:

```
Unable to Import. Path matches client base download directory, it's possible
'Keep top-level folder' is disabled for this torrent or 'Torrent Content
Layout' is NOT set to 'Original' or 'Create Subfolder'?
```

The test torrent used here (see below) has three files at its root with no
containing folder, and Braid's `content_path` only creates a distinct path
for a torrent with exactly one file; for anything else it reports the
category's own directory unchanged. When two different torrents can share
one category folder and neither gets a folder of its own, an importer has no
way to know which files in that directory belong to which download, and
Sonarr's own safety check refuses to guess. Real qBittorrent has a
`contentLayout` setting for exactly this (`Original` / `Create Subfolder` /
`Subfolder for Multi-file Torrents`, which is also a field in Sonarr and
Radarr's own client schema, sent as part of `torrents/add` when not left at
the default), and Braid does not implement or read that field at all.

**This is not fixed here.** Deciding how Braid should place a multi-file
torrent's contents, and whether to read `contentLayout` off `torrents/add` at
all, is a design question for the API surface, not a one-line correction like
the two bugs above, and the line drawn for this plan is the surface Sonarr
and Radarr need, not re-implementing qBittorrent's own content-layout
options. It is recorded here, with a real fixture
(`torrents_info_after_completion.json`) and a real Sonarr queue message,
specifically so the next person deciding whether to build that does not have
to take it on faith that it is needed.

### `paused` arrives as `False`, capital F, and it does not matter

Sonarr's `torrents/add` sends `paused=False` on every ordinary add (see
`sonarr_adds_a_magnet_with_a_category_and_no_save_path.json`), not
`paused=false`. Braid's handler only treats the exact literal `true` as a
request to add paused, so this is read correctly as "not paused" purely by
accident of what it checks for rather than what it does not. Worth recording
so nobody "fixes" the case sensitivity later and breaks the one case that
currently works by not caring.

### `savepath` is never sent

Neither application ever sends the `savepath` field on `torrents/add`, only
`category`. Every destination in this harness's real traffic was resolved
from a category. `destination_for`'s explicit-`savepath` branch exists for
Braid's own web UI and for other, non-`*arr` qBittorrent clients that do send
it; this recorded session never exercised it, and no fixture claims it did.

## Driving a real download

There is no indexer in this harness, so there is nothing that would hand
either app a genuine release on its own. `POST /api/v3/release/push` (used by
Sonarr's own "Interactive Search: push a result by hand" feature) accepts a
manually described release and, if it matches a series and episode Sonarr
already knows about, grabs it through whichever download client is
configured, exactly as if an indexer had returned it. That match is
title-based, not a hash or an id, so a series has to actually be added first
(`Pioneer One`, added via `POST /api/v3/series` after a `GET
/api/v3/series/lookup?term=Pioneer+One`, since Sonarr's metadata lookup needs
a real series to attach an episode to) before a pushed release naming it will
be accepted rather than rejected as `Unknown Series`.

The magnet used for the actual bytes has nothing to do with either
application: it is WebTorrent's own public, permanently-seeded demo torrent
for the Creative Commons short film *Big Buck Bunny*
(`dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c`), chosen only because it has real
peers on the open internet at all times and is legal to redistribute. The
release pushed into Sonarr was labelled `Pioneer.One.S01E02.720p.WEB.x264-
GROUP` so Sonarr's parser would match it against a real, already-added
series; the file that actually downloaded is the *Big Buck Bunny* short, not
an episode of *Pioneer One*. `record.sh push <magnet>` automates the push
itself; adding the series first is one-time setup done by hand while building
this harness and is not scripted, since a fixture only needed to observe one
add cycle, not repeat it indefinitely.

## What was not verified

- **Radarr's own add-to-completion cycle** was not separately driven end to
  end the way Sonarr's was: adding a real movie needs the same
  lookup-then-add dance Sonarr's series did, against Radarr's own metadata
  service, which was not done here for time. What *is* directly recorded for
  Radarr is registration, its client "test", its own periodic polling, and
  that its `torrents/createCategory` call is byte-for-byte the same shape as
  Sonarr's (`category=movies-radarr`, no `savePath`), which is what the
  `destination_for` bug and fix actually turn on; both applications drive the
  exact same handlers in `qbit/torrents.rs`; there is no separate code path
  for Radarr's add to have its own version of that bug or its own version of
  the fix.
- **Prowlarr** is named in several comments in `qbit/app.rs` and
  `qbit/torrents.rs` (the reason `torrents/add` accepts a multipart upload at
  all) but was not part of this harness. Nothing here confirms or challenges
  those comments; they came from the plan this harness implements, not from
  a Prowlarr instance.
- **A version minimum higher than `v4.6.0` / webapi `2.9.2`** was not found:
  every schema field's help text names a maximum version of `4.3.2`
  (`contentLayout`), well under what Braid reports, and neither app ever
  refused to register or test the client over its version.
- **`num_seeds` and `num_leechs` being hardcoded to `0`** was not flagged by
  either application in the traffic recorded here: neither client's queue or
  UI (so far as this harness's use of each app's REST API can tell) surfaced
  a warning or a different behaviour tied to those two fields, in either the
  in-progress or the completed state.
