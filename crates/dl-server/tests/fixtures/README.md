Recorded exchanges between a real Sonarr, a real Radarr, and braid-server,
captured through the logging proxy in `harness/`. Anything host-specific or
time-specific (container hostnames, the generated admin password, session
ids, `added_on`/`completion_on` timestamps) is written here as a
`<PLACEHOLDER>` rather than the literal value that was actually sent, since
neither is stable across a re-run of the harness.

See `harness/README.md` for what each fixture demonstrates and how it was
produced. See `compatible_api_fixtures.rs`, one directory up, for the tests
that replay these against our own handlers.
