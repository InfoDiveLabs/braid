# Contributing

Thanks for looking. A few things that will save you time.

## Before you start

Open an issue first for anything beyond a bug fix. This project makes some
deliberate choices that look like omissions, and the answer to "why not just
use X" is usually written down somewhere; asking first is cheaper than finding
out in review.

Read [DEVELOPMENT.md](DEVELOPMENT.md) for the layout and the invariants. The
ordering rule in the storage layer is the one to understand before touching
anything under `dl-core/src/store`.

## The bar

Every change has to leave this green:

```console
$ cargo fmt --all
$ cargo clippy --workspace --all-targets     # silent, warnings included
$ cargo test --workspace
```

Beyond that:

**Tests describe the property, not the mechanism.** A name like
`a_run_to_midnight_does_not_wrap` says what breaks if it fails.
`test_schedule_2` does not. Where a test guards something non-obvious, say in a
comment what goes wrong without it.

**Comments explain why, not what.** The code says what it does. A comment earns
its place by naming the failure it prevents or the constraint that forced the
shape. Do not write a running commentary of how the code came to be this way.

**New behaviour needs a test that fails without it.** Especially in the storage
and network layers, where the failure modes are silent: a resume that quietly
restarts from zero passes a correctness-only test.

**Nothing in the UI may claim to do something it does not.** A control that
does nothing is worse than an absent one. If a setting cannot be honoured yet,
either leave it out or record it in `docs/ui-status.md` as design-only.

## Platform work

Windows and Linux support is real but unverified on hardware. If you have a
machine and can confirm or refute any of the items under "What is not verified"
in [DEVELOPMENT.md](DEVELOPMENT.md), that is genuinely one of the most useful
contributions available right now, and it needs no Rust.

Per-interface binding is the risky part. If you change anything under
`dl-net/src/bind`, say which platforms you ran it on and how you confirmed the
socket option actually took effect, rather than that the call returned `Ok`.

## Torrents

This is a client, not an index. Pull requests adding search, a tracker list, or
any form of content directory will be declined. That line is deliberate and is
explained in the README.

## Commits

Write the message for someone reading `git log` in a year: what changed and why
it needed to. Keep unrelated changes in separate commits.

## Licence

Contributions are accepted under GPL-3.0-only, the same terms as the project.
Copyright is held by [InfoDive Labs Pvt Ltd.](https://www.infodivelabs.com)
