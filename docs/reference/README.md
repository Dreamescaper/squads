# Reference: live end-to-end probes

These shell scripts were inherited from the vendored protocol core
(`crates/squads-protocol`, originally `better-teams`/`ost`). They are **not** run by CI
and require a real Microsoft sign-in, so they exist purely as **manual verification**
probes for a developer with a live account.

| Script | Checks |
|---|---|
| `e2e_chats.sh` | Lists chats and reads a message page. |
| `e2e_read.sh` | Reads messages from a known conversation. |
| `e2e_teams.sh` | Lists joined teams and channels. |
| `e2e_trouter.sh` | Connects to Trouter and observes push events. |
| `e2e_echo123.sh` | Calls the Teams echo bot (signalling + media loopback). |

Typical usage (after `cargo build`):

```sh
export TEAMS_EMAIL=you@example.com
./docs/reference/e2e_chats.sh
```

See `crates/squads-protocol/README.md` and `AGENTS.md` for the environment variables the
original project used (`TEAMS_*`, region overrides, `TEAMS_MANUAL_CALLS`, …).
