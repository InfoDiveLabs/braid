# Development

How the code is laid out, how to build it, and the invariants worth knowing
before changing anything.

## Building

```console
$ cargo build --release            # both binaries
$ cargo test --workspace           # 411 tests
$ cargo clippy --workspace --all-targets
$ cargo fmt --all
```

Rust 1.92 or newer. On Linux you also need `libxkbcommon-dev` and
`libfontconfig-dev`.

Two binaries come out of this workspace: `braid`, the desktop application, and
`dl`, the command-line tool.

`.cargo/config.toml` defines a `xtask` alias, so `cargo xtask <command>` and
`cargo run -p xtask -- <command>` are the same thing. The long form is used
below so the commands work even without it.

## Workspace

| Crate | Role |
|---|---|
| `dl-core` | The engine: scheduling, chunking, crash-safe storage, link refresh. No UI, no CLI, no sockets. |
| `dl-net` | The only crate that knows about sockets and NICs: enumeration, per-NIC binding, HTTP transport. |
| `dl-torrent` | BitTorrent over librqbit. The only crate that knows what a piece or a peer is. |
| `dl-testkit` | Mock origin, adversarial scenario catalogue, fault injection. |
| `dl-cli` | The `dl` binary. |
| `dl-gui` | The `braid` binary. Slint UI. |
| `xtask` | Screenshots, size gating, packaging. |

The boundaries are what make this testable. `dl-core` reaches the network only
through `ByteSource` and `LaneSet`, and the filesystem only through `Storage`,
so the engine runs against a lying server and a failing disk with no sockets
involved. `dl-gui` never depends on `dl-net`; it talks to `dl_core::Engine`.

## The invariant everything hangs on

> Chunk data is made durable **before** the journal claims the chunk.

Break it and a resume trusts bytes that were never written, producing a corrupt
file that looks complete. `Journal::flush` is the only place that ordering is
expressed, and `record_chunk` cannot express it the wrong way round.

The durability modes change how *often* that pair happens, never the order. So
a crash costs re-downloading, never correctness.

## Other rules that are not obvious

- **`Accept-Encoding: identity` on every ranged request.** `Range` applies to
  encoded bytes. If a server gzips, offsets address compressed bytes while we
  write decompressed ones, and the file is silently wrong. A 206 that comes
  back with a non-identity encoding aborts the chunk.
- **A `200` where a `206` was asked for means the resource changed.** So does a
  changed `Content-Range` total. Neither is retried; both invalidate the partial.
- **`Interface.name` is the identity.** It keys the socket option, the saved
  interface selection and the per-interface limits. The display name is
  presentation on top and must never become the key.
- **Selection in the transfer list is by id, not row index.** The list is
  filtered and re-sorted ten times a second.
- **One writer.** Windows `seek_write` moves the file cursor, unlike Unix
  `write_at`, so chunks go through a single writer task rather than N
  positional writers.

## UI

One Slint UI for all three desktops. `build.rs` swaps
`ui/platform/{macos,generic}.slint` behind the `@platform` import, so platform
differences are values rather than branches. A token added to one file must be
added to the other or that target stops compiling, which is deliberate: a
missing platform value should be a build error, not a silent fallback.

```console
$ cargo run -p xtask -- screenshots                     # drives the UI headlessly
$ DL_UI_PLATFORM=generic cargo run -p xtask -- screenshots --out artifacts/generic
$ cargo run -p xtask -- settings                        # every settings page
```

`DL_UI_PLATFORM` renders the Windows and Linux look from any host, so the two
token files cannot drift apart unnoticed.

## Testing

The test suite is the deliverable for the storage layer, not an afterthought.

- `dl-testkit` runs a real origin that misbehaves on purpose: servers that
  advertise `Range` and ignore it, ETags that change mid-download, bodies that
  truncate, rate limits with and without `Retry-After`, login pages served with
  a correct `Content-Length`.
- `FaultyFile` injects storage faults, including `DropFsync`, the lying-drive
  case that proves the journal ordering rather than assuming it.
- `crates/dl-cli/tests/kill_and_resume.rs` spawns the CLI, `SIGKILL`s it at a
  chosen byte offset and asserts the resumed run produces the right hash *and*
  re-downloaded only a bounded amount. A resume that silently restarts from
  zero would pass a correctness-only test.
- `crates/dl-torrent/tests/loopback_swarm.rs` runs a seeder and a leecher
  in-process with the DHT and trackers off.

`DL_KILL_CYCLES` raises the SIGKILL cycle count for CI.

## Packaging

```console
$ cargo run -p xtask -- package --out dist
```

Builds the host platform's installer and nothing else. There is no
cross-packaging: a release comes from a CI matrix with one runner per target.

| Platform | Output | Needs |
|---|---|---|
| macOS | `.dmg` containing `Braid.app` | `hdiutil`, `iconutil` (both stock) |
| Linux | `.deb`, `.rpm` | `cargo-deb`, `cargo-generate-rpm` |
| Windows | `.msi` | `cargo-wix` |

`cargo run -p xtask -- bundle` builds just the macOS `.app`, which is what
registers the `magnet:` and `.torrent` handlers.

## Binary size

Size is a product goal, not a vanity metric: renderer choice is most of it,
which is why this uses femtovg rather than skia.

```console
$ cargo run -p xtask -- size
release binaries for aarch64-apple-darwin
  dl            6191184 bytes (5.90 MB)
  braid        16258496 bytes (15.51 MB)
```

Reported per platform, not checked against a stored number: a macOS arm64
binary and a Linux x86_64 one differ by half their size, so one figure says
which machine measured it rather than how the binary is trending. The release
workflow prints it for each platform it builds.

A separate check proves `dl-testkit` and `i-slint-backend-testing` are absent
from the default dependency graph, so dev-only code cannot reach a shipped
binary. It is a `cargo tree` and needs no build, so it runs in the lint job on
every push.

```console
$ cargo run -p xtask -- verify-release
```

## Checking the other platforms from a Mac

Platform-specific breakage is cheap to catch here and expensive to catch in CI:
a `cfg`-gated function, a non-portable file API, a dependency that only
builds on Unix.

**Linux**, build and full test suite:

```console
$ docker run --rm -v "$PWD":/src:ro -v /tmp:/tmp rust:1.92-bookworm bash -c '
    apt-get update -qq && apt-get install -y -qq libxkbcommon-dev libfontconfig1-dev pkg-config
    cd /src && export RUSTFLAGS="-D warnings" CARGO_TARGET_DIR=/tmp/lt SLINT_BACKEND=headless
    cargo test --workspace --all-features'
```

**Windows**, type-check only. `cargo check` does not link, so it needs no MSVC
toolchain; `x86_64-pc-windows-gnu` with mingw is close enough to catch the
things that actually break, which are `cfg` mistakes and Unix-only APIs used
without a guard:

```console
$ docker run --rm -v "$PWD":/src:ro -v /tmp:/tmp rust:1.92-bookworm bash -c '
    apt-get update -qq && apt-get install -y -qq mingw-w64
    cd /src && rustup target add x86_64-pc-windows-gnu
    export CARGO_TARGET_DIR=/tmp/wt RUSTFLAGS="-D warnings"
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
    export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
    export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
    cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu'
```

`rustup target add` has to run from inside the project, or it installs the
standard library for the default toolchain rather than the pinned one.

Note `RUSTFLAGS="-D warnings"`, which is what CI uses. Without it an unused
import on one platform passes locally and fails there.

## What is not verified

Stated so nobody assumes otherwise.

- **Windows at runtime.** The workspace type-checks for Windows (see above) and
  CI builds it, but nobody has run it. `IP_UNICAST_IF` and the MSI are written
  from Microsoft's documentation, and the IPv6 byte-order question there is
  explicitly unresolved.
- **Linux desktop integration.** The `.desktop` file and `xdg-mime`
  registration are written but have not been run on a Linux desktop.
- **Per-interface binding on real multi-NIC hardware.** Covered by a privileged
  CI job using `dummy` and `feth` interfaces, which proves the socket option
  took effect via an echoed source address. It is not the same as a machine
  with two real uplinks.
