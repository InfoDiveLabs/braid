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

## Second run: does Sonarr actually import it now

The content_path fix above (every torrent gets a folder of its own) shipped
without ever being re-run against a real Sonarr. It has now been, end to end,
with the same flat, no-top-level-folder torrent the first run used.

**Sonarr does import it.** A magnet pushed the same way as before, grabbed
for real through the registered client, downloaded to completion, and this
time Sonarr's queue accepted `content_path` as distinct from `save_path` and
moved on: no more "Path matches client base download directory". It reached a
new, later failure instead: "Unable to parse file", because the file Sonarr
found is really *Big Buck Bunny*, named exactly that, and nothing about that
filename says `Pioneer.One.S01E03` to Sonarr's parser. That is a property of
using a real, unrelated, permanently-seeded public torrent for actual bytes,
the same limitation the first run's own README already named, not a Braid
defect. Manually importing the same file (`POST /api/v3/command` with
`ManualImport`, naming the series and episode explicitly, exactly the
operation Sonarr's own "Manual Import" screen drives) succeeded outright: a
real `downloadFolderImported` history event, and the file moved into
`/downloads/tv/Pioneer One/Pioneer.One.S01E03.720p.WEB.x264-GROUP.mp4`. That
is the proof this run set out to get: given a name Sonarr can actually match,
the file Braid produced imports cleanly, and `content_path`/`save_path`
are no longer the obstacle.

Radarr got the same treatment with a title that genuinely matches its own
metadata (*Big Buck Bunny* is a real, small, public-domain film with its own
TMDB entry): Radarr's own `GET /api/v3/manualimport` found the file, read its
size correctly, and matched it to the right movie by title and year on its
own, with no help from a folder name. It stopped one step short of a full
import in this session over "Unable to parse file" (a quality-parsing
rejection) and a JSON schema quirk in Radarr 6.4.4's own `POST
/api/v3/command` payload for `ManualImport` that this session did not resolve
in time; that is a gap in this write-up, not a claim that Radarr's own
automatic import was seen to work end to end the way Sonarr's manual import
was. What is proven for Radarr is the same thing the file listing already
showed: the file is real, complete, and at a path Radarr can read and
correctly identify.

### A harness bug that would have hidden all of this: three separate `/downloads`

The compose file this session inherited mounted `./data/braid/downloads`,
`./data/sonarr/downloads` and `./data/radarr/downloads` as `/downloads` in
three different containers. Braid's API could report a torrent finished at
`/downloads/tv-sonarr/Big Buck Bunny` all day and it would never matter,
because Sonarr's own `/downloads` was a completely different directory on the
host with nothing in it. This is exactly why the first run's "Path matches
client base download directory" failure was the *only* failure it ever saw:
that check is a pure string comparison inside Sonarr, done before it ever
touches a filesystem, so it fired and stopped the story before the missing
shared volume could matter. The moment that string check was fixed, this
would have surfaced instead, as "no files found are eligible for import",
and would have looked like a Braid bug. It is not one. Real
qBittorrent-plus-Sonarr deployments always bind-mount one download directory
into both containers at a matching path for exactly this reason. Fixed here:
`harness/compose.yml` now mounts a single `./data/downloads` into `braid`,
`sonarr` and `radarr` alike, and `record.sh up` creates that one directory
instead of three.

### A real crash: `torrents/properties` and `torrents/files` before a torrent's status is known

`braid-server` panicked and exited mid-session, twice, the first time a real
Sonarr called `torrents/properties` on a torrent it had only just added:

```
thread 'tokio-rt-worker' panicked at crates/dl-server/src/qbit/torrents.rs:297:45:
find_by_hash only matches a torrent
```

`find_by_hash` matches on `torrent_hash`, which (by design, see its own doc
comment) reads a magnet's hash straight out of the URL the instant it is
added, before the backend has joined a swarm and produced a `TorrentStatus`
at all. A hash matching there is not a promise that `snapshot.torrent` is
populated, and both `torrent_properties` and `torrent_files` `.expect()`-ed
that it was. A real client asking about a torrent it had just added, which is
an entirely ordinary thing to do, crashed the whole process. Fixed in
`crates/dl-server/src/qbit/torrents.rs`: both handlers now treat an absent
`TorrentStatus` the same way `torrents/info` already does, as "not reported
yet" rather than a contradiction. Regression coverage:
`properties_and_files_do_not_panic_before_the_torrent_is_known` in
`qbit/torrents.rs`, which reproduces the exact sequence (add a magnet, ask
about it before the backend has replied) without needing a real crash to
prove it stopped happening.

### A second real bug, found by testing "more than one category at once": a duplicate info hash silently lies

Pushing the same magnet into Sonarr's category and then into Radarr's
category (the brief's own "more than one category at once" case) produced a
`torrents/info` entry that claimed a finished download at
`/downloads/movies-radarr/Big Buck Bunny`, a directory that had nothing in
it. `dl_torrent`'s own log line gives it away:

```
INFO dl_torrent: download complete; seeding torrent=Some("Big Buck Bunny")
```

logged under one second after the add, with no "added torrent" or "Doing
initial checksum validation" line in between. librqbit's session is one per
info hash; a second `add` for a hash already open silently attaches to
whatever session already holds it and reports it complete immediately, at
the *first* add's destination, while the destination the second call was
just given is never written to at all. Real qBittorrent's own answer to
re-adding a hash it already has is to leave the existing torrent alone,
not to fabricate a second, phantom one. Fixed in `add_one`
(`crates/dl-server/src/qbit/torrents.rs`): a magnet naming a hash Braid
already has open is now a no-op, checked before a destination for the new
request is even computed. Regression coverage:
`re_adding_a_known_hash_under_a_different_category_does_not_create_a_second_torrent`.

### `torrents/topPrio`, not `torrents/topPriority`

Braid's route was named `topPriority`. Real qBittorrent's endpoint, and what
a real Sonarr with its "Recent Priority" set to "First" actually calls
immediately after every `torrents/add`, is `topPrio`:

```
Warn HttpClient: HTTP Error - Res: HTTP/1.1 [POST] http://braid-proxy:8080/api/v2/torrents/topPrio: 404.NotFound
Warn QBittorrent: Failed to set the torrent priority for DD8255ECDC7CA55FB0BBF81323D87062DB1F6D1C.
```

Sonarr logs a warning and moves on rather than failing the grab, which is
exactly why this was easy to miss: nothing about the download itself looked
wrong. Fixed by renaming the route. Fixture:
`sonarr_sets_top_priority_after_adding_a_magnet.json`.

### `torrents/setForceStart` was missing outright, and a real Sonarr calls it

With "Initial State" set to "Force Started", Sonarr's own `AddFromMagnetLink`
calls `torrents/setForceStart` right after every add, and got a 404 for an
endpoint that did not exist:

```
Warn HttpClient: HTTP Error - Res: HTTP/1.1 [POST] http://braid-proxy:8080/api/v2/torrents/setForceStart: 404.NotFound
Warn QBittorrent: Failed to set ForceStart for DD8255ECDC7CA55FB0BBF81323D87062DB1F6D1C.
```

This is one of the brief's own "strong candidates", now confirmed rather than
guessed at. Implemented: `value=true` calls `Engine::resume`, which is a real
action this engine can take (starting a torrent regardless of its queue
position is what "force start" means for something paused or queued);
`value=false` is accepted and left alone, the same honesty `topPrio` already
applies to a request (returning a torrent to ordinary queueing) this engine
has no per-torrent flag to represent. Fixtures:
`sonarr_sets_force_start_after_adding_a_magnet.json` for the wire exchange,
plus a direct unit test (`set_force_start_true_resumes_a_paused_torrent`)
proving `value=true` actually resumes a paused transfer rather than only
acknowledging the request.

### Tags on the download client never reach Braid at all, and can silently stop every grab

Configuring the Sonarr/Radarr *tags* field on the Braid download client
(distinct from a torrent's own tags) without also tagging the series or
movie made every single grab sit at `downloadClientUnavailable` forever:
Sonarr never called `torrents/add` at all, because it does its own routing
decision, client-side, before ever talking to the download client, and a
download client with tags set is only used for items carrying a matching
tag. Nothing about this reaches the wire, and no fixture models it, because
there is nothing for Braid to answer: the request is never sent. Worth
recording plainly because it looks, from the queue, exactly like a broken
download client.

### Seed ratio and seeding time limits still cannot be exercised, and now we know why

Neither Sonarr 4.0.20's nor Radarr 6.4.4's own qBittorrent client schema
(`GET /api/v3/downloadclient/schema`) has a ratio or seeding-time field
anywhere in it: `recentTvPriority`/`olderTvPriority` (and their Radarr
equivalents), `initialState`, `sequentialOrder`, `firstAndLast` and
`contentLayout` are the whole list. `torrents/setShareLimits` was not called
in this run either, and now there is a reason beyond "it did not come up":
these versions have nothing in their own settings screen that would ever
send it. This is not a gap in Braid to close.

### What else was exercised, and stayed clean

- **More than one category at once**: `tv-sonarr` and `movies-radarr` ran
  side by side without cross-talk, once the duplicate-hash bug above (found
  by exactly this test) was fixed.
- **`app/webapiVersion`'s repeated polling**, **Basic auth being sent and
  ignored**, and **recovery from a `403` after a restart**: all matched the
  first run's findings again, unchanged.
- **`torrents/setShareLimits`, `torrents/trackers`, `torrents/peers`,
  `app/buildInfo`, and `transfer/setDownloadLimit` / `setUploadLimit` /
  `downloadLimit` / `uploadLimit`**: named as candidates worth watching for.
  None appeared anywhere in this session's recordings, across both apps,
  registration, testing, polling, and multiple real grabs with tags,
  priorities and force-start all configured. Not implemented, on the
  brief's own terms: a real engine-backed implementation of an endpoint
  nothing asked for is still speculation, just speculation with working
  code behind it.

### What this run did not settle

- **Removing a completed download from the client after import**
  (`removeCompletedDownloads`, on by default in both apps' schemas) was
  configured, but not cleanly observed. Repeated test churn in this
  session, pushing the same magnet under three different fake episode
  numbers so each one would dodge Sonarr's own "already meets cutoff"
  dedup, left three queue entries all pointing at the same info hash, and
  Sonarr's own per-episode tracking visibly could not tell them apart. A
  single clean push-download-manual-import cycle is needed to see this for
  real; this run does not claim to have seen it either work or fail.
- **Pausing and resuming from the app** was not exercised. Neither app's
  REST queue API in these versions exposes a direct pause/resume action;
  doing this for real means driving the web UI itself, which this session
  did not attempt.
- **Radarr's own automatic import completing end to end** was not seen.
  Radarr's `GET /api/v3/manualimport` correctly found and identified the
  file by title and year with no help from the folder name, which is
  itself real evidence the underlying file placement is fine; the `POST
  /api/v3/command` call to actually perform that import hit a JSON schema
  quirk in Radarr 6.4.4's own API that this session did not resolve in
  time. Sonarr's own equivalent command did work, end to end, and both
  applications drive the same handlers in `qbit/torrents.rs`, so this is
  recorded as unresolved rather than as evidence of anything wrong.
