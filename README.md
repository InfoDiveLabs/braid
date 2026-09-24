<p align="center">
  <img src="assets/hero.jpg" alt="Braid" width="100%">
</p>

<p align="center">
  <b>A download manager that splits one file across every network path you have.</b><br>
  Wi-Fi, Ethernet and a tethered phone, pulling the same file at once, reassembled exactly.
</p>

<p align="center">
  <a href="#install">Install</a> &middot;
  <a href="#what-it-does">What it does</a> &middot;
  <a href="#ways-to-connect">Ways to connect</a> &middot;
  <a href="#how-it-works">How it works</a> &middot;
  <a href="DEVELOPMENT.md">Development</a>
</p>

<p align="center">
  <img alt="Platforms" src="https://img.shields.io/badge/macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-14110E?style=flat-square">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-D07A40?style=flat-square&logo=rust&logoColor=white">
  <img alt="Licence" src="https://img.shields.io/badge/GPL--3.0-3FA292?style=flat-square">
  <img alt="Binary size" src="https://img.shields.io/badge/15.5%20to%2023.4%20MB-14110E?style=flat-square">
</p>

---

<p align="center">
  <img src="assets/screenshots/transfers.png" alt="Braid on macOS" width="860"><br>
  <sub><i>A scripted demonstration, not a live desktop.
  <a href="#screens">What is simulated in these images</a>.</i></sub>
</p>

## What it does

Most download managers open several connections to the same server. Braid opens them
over **different networks**. A file arrives over your Wi-Fi, your Ethernet and your
phone's tether at the same time, and the sidebar shows exactly which link carried what.

Three things it takes seriously.

### It uses every path you have

Each connection is bound to a specific network interface, not left to the operating
system's routing table. Slow links get fewer chunks, a link that dies gets its work
migrated, and an interface you cap is a ceiling rather than a suggestion.

The bar under each transfer is split by interface, so aggregation is something you can
see rather than something the marketing claims.

### It can borrow your phone's connection

A laptop with one Wi-Fi card has one path. The phone next to it has another, over
mobile data, and Braid can use it as an extra lane: the phone forwards, the desktop
downloads, and both paths pull at once.

Pairing is a code on screen. Open the phone sheet from the sidebar, press **Show code**,
and point the phone at it. No address typed, no discovery to fail, and it works on a
network with no IPv4 on it at all. Each of the phone's networks becomes its own lane
with its own speed in the sidebar, so a lane that is quietly contributing nothing is
visibly contributing nothing.

**The phone decides what it lends.** Which of its networks it offers, and how much data
it will spend, are the phone's to set. Braid displays what it is told and enforces
nothing. A phone that goes out of range, sleeps, or hits its own limit simply stops
serving, and its work moves to the paths that remain.

Braid also notices when a phone's lane is the connection you already have, which is the
usual case when both are on the same Wi-Fi, and leaves it switched off with a note
saying so rather than offering you bandwidth that does not exist.

### It does not corrupt files

Every chunk is hashed as it lands and the hash is journalled before the chunk is
claimed. A crash costs the bytes in flight, never the file. Corruption is localised to
one chunk and re-fetched rather than restarting a 6 GB download.

It also refuses the failures that quietly produce a wrong file: a server that advertises
`Range` support and ignores it, a `200` where a `206` was asked for, a login page served
with a correct `Content-Length`, a response compressed after a ranged request.

### It speaks BitTorrent too

Magnet links and `.torrent` files, in the same list as everything else. A
torrent expands to show its files with per-file progress, reports its peers and
seeds when it is done, and obeys the same bandwidth limits and schedule as an
HTTP transfer. Braid registers as the system handler for `magnet:` links and
`.torrent` files, so clicking one in a browser opens it here.

It is a client, not an index: no search, no bundled tracker list. See
[Torrents, and the law](#torrents-and-the-law).

### It survives expiring links

Pre-signed URLs are re-resolved before they lapse rather than after they fail. Sixteen
connections hitting a dead link produce exactly one refresh, not sixteen. Signatures
bound to a source address are resolved once per interface, because each path leaves
from a different one.

---

## Ways to connect

<p align="center">
  <img src="assets/ways-to-connect.jpg" alt="Three ways paths combine into one file" width="860">
</p>

Every combination below ends the same way: several paths pulling one file at once,
each carrying the share it earns, and any of them able to disappear without costing
the download.

| Setup | What carries the file | Needs the companion |
|---|---|---|
| Computer alone | Every interface you select: Wi-Fi, Ethernet, a tethered phone appearing as a network card | no |
| Phone on the same network | Your own connection, plus the phone's mobile data | yes |
| Phone on a cable | Your own connection, plus the phone's mobile data, plus the phone's Wi-Fi as a separate path | yes |
| Plain USB tethering | The phone appears as one network card and is used like any other | no |
| Several phones | Each phone's networks are separate paths, weighted independently | yes |

Two things are worth knowing because they surprise people.

**A phone on your Wi-Fi is usually not a second path.** If the phone reaches the
internet through the same router your computer does, it is your own connection wearing
a second name. Braid detects this by comparing the address each path leaves from, and
marks that lane "Not used" rather than pretending. Its mobile data is a real second
path; its Wi-Fi generally is not.

**A phone joins the next transfer, not one already running.** A transfer's paths are
fixed when it starts, the same as your interface selection. Pair first, then download.

## The Android companion

The companion turns a phone into one of those paths. It forwards, the desktop
downloads, and the phone never stores or verifies anything: every hard problem stays on
the desktop, which already solves them.

**Pairing is a code on screen.** Press **Add phone** in the sidebar, then **Show code**,
and point the phone at it. Nothing is typed. The code carries this computer's address
on the network the phone is actually on, and a token good for two minutes and one use.
Discovery over mDNS and a typed address both still work, because multicast is dropped
by plenty of networks and gated by some operating systems.

**The phone owns its own policy.** Which networks it lends, and how much data it will
spend, are set on the phone. Switch mobile sharing on there and the path appears on the
desktop by itself: there is no second switch to agree with yourself. When a limit is
reached, or the phone sleeps, or it walks out of range, that path stops and its work
moves to the ones that remain.

**It is not released yet.** The desktop half is built and tested, and the companion is
written and running on hardware: cellular binding proven on a real carrier, per-path
data limits, survival through screen lock and Doze, and QR pairing verified end to end.
It is not yet published anywhere you can install it from.

## Screens

Every image here is the running application: the real interface, the real
engine, real transfers with real chunking and a real journal behind them. They
are captured from a scripted demonstration rather than from someone's desktop,
and two things in them are staged. Both are named here rather than left for
you to discover.

**The three network interfaces are one.** The machine these were captured on
has a single routable interface. The Wi-Fi, Ethernet and USB Tether in the
sidebar are three connections over loopback wearing those labels, so that the
part of the interface built to display aggregation has something to display.
The mechanism is real and tested; the three separate paths in the picture are
not.

**The torrent's swarm is simulated.** Its peer count, throughput and file list
come from a stand-in backend rather than from a real swarm.

The pairing screen is the exception to both: it is the real application, driven to
that screen and captured there, showing a genuine code for the machine it was taken
on.

Everything else is what it appears to be. The transfers are genuine downloads
performed by the engine against a local test server, which is why the inspector
reports `127.0.0.1`, and the piece grid is the journal's own bitmap rather than
a drawing of one.

<table>
<tr>
<td width="50%">
<img src="assets/screenshots/inspector-pieces.png" alt="Piece map"><br>
<b>Every chunk, as it lands.</b> The grid is the journal's own bitmap: proof that a
resumed transfer kept what it had, and the one place a failed hash is visible as a cell
going from Have back to Missing.
</td>
<td width="50%">
<img src="assets/screenshots/settings-network.png" alt="Network settings"><br>
<b>Your interfaces, and how they are bound.</b> The Binding column shows the mechanism
actually in force, so a silent fall back to a plain source-address bind is visible
rather than assumed.
</td>
</tr>
<tr>
<td width="50%">
<img src="assets/screenshots/settings-bandwidth.png" alt="Bandwidth schedule"><br>
<b>Limits that know what time it is.</b> Paint the hours you want capped. Outside them
Braid runs unlimited, and the schedule reaches a running transfer rather than the next
one.
</td>
<td width="50%">
<img src="assets/screenshots/pair-phone.png" alt="Pairing a phone"><br>
<b>Point a phone at the screen.</b> The code carries this computer's address on the
network the phone is actually on, and a token good for two minutes and one use. The
address is printed underneath, because a camera that will not focus should not be the
end of it.
</td>
</tr>
<tr>
<td width="50%">
<img src="assets/screenshots/add-transfer.png" alt="Add transfer"><br>
<b>A URL, a magnet link or a .torrent.</b> For HTTP it asks the server first: size,
whether ranges really work, whether there is a validator to resume against. Checksum
verification is offered up front rather than discovered afterwards.
</td>
</tr>
</table>

<p align="center">
  <img src="assets/screenshots/transfers-linux.png" alt="Braid on Linux and Windows" width="760"><br>
  <i>One application, native to each desktop: the same Slint UI with per-platform
  metrics, selection styling and naming.</i>
</p>

---

## Install

Download the installer for your platform from
[Releases](https://github.com/InfoDiveLabs/braid/releases).

| Platform | File | Notes |
|---|---|---|
| macOS 11+ | `Braid-x.y.z-macos.dmg` | Drag to Applications. Unsigned, so the first launch needs right-click then Open. |
| Debian, Ubuntu | `braid_x.y.z_amd64.deb` | `sudo apt install ./braid_*.deb` |
| Fedora, RHEL | `braid-x.y.z.x86_64.rpm` | `sudo dnf install ./braid-*.rpm` |
| Windows 10+ | `braid-x.y.z.msi` | See the note on Windows below. |

Both a desktop application (`braid`) and a command-line tool (`dl`) are installed.

```console
$ dl add https://releases.ubuntu.com/24.04/ubuntu-24.04-desktop-amd64.iso \
     --connections 8 --verify sha256:9d3f...
```

## How it works

<p align="center">
  <img src="assets/how-it-works.jpg" alt="Three network paths braided into one verified file" width="860">
</p>

A transfer is split into chunks and handed to whichever interface is fastest right now,
measured rather than assumed. Chunks are written into a single preallocated file by one
writer, and the journal records a chunk as durable only after its bytes are.

That ordering is the whole design:

> Chunk data is made durable **before** the journal claims the chunk.

Break it and resume trusts bytes that were never written, producing a corrupt file that
looks complete. Every durability mode keeps it. What they change is how often the pair
happens, so a crash costs re-downloading rather than correctness.

| Mode | Flushes | Journal |
|---|---|---|
| Safe | every chunk | waits for the drive |
| Balanced | 5 s or 64 MB | waits for the drive |
| Fast | 30 s or 512 MB | leaves it to the OS |

---

## Building it

```console
$ cargo build --release            # both binaries
$ cargo test --workspace           # 411 tests
$ cargo run -p xtask -- package    # the host platform's installer
```

Rust 1.92 or newer. On Linux you also need `libxkbcommon-dev` and
`libfontconfig-dev`.

[DEVELOPMENT.md](DEVELOPMENT.md) covers the layout, the invariants worth
knowing before changing the storage layer, how the UI is built for three
desktops from one source, and what is not verified.
[CONTRIBUTING.md](CONTRIBUTING.md) covers the bar for a change.

## Torrents, and the law

Braid is a BitTorrent client, not an index. There is no search, no bundled tracker list
and no content directory of any kind, and there will not be. That is a deliberate line:
clients are lawful, and the cases that were not involved supplying somewhere to find
infringing material.

Seeding is publication. Joining a swarm announces your address to every peer and tracker
in it. That is how the protocol works, and binding a transfer to a particular interface
is **not** a privacy control.

## Licence

Copyright (C) 2026 [InfoDive Labs Pvt Ltd.](https://www.infodivelabs.com)
Written by Suraj Tiwari.

Braid is free software: you can redistribute it and modify it under the terms
of the GNU General Public License version 3, as published by the Free Software
Foundation. It is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY, without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See [LICENSE](LICENSE) for the full terms.
