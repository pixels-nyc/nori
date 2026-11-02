# nori Development Guide

How to develop, build, deploy, and troubleshoot the nori compositor
(the PiX OS Smithay-based Niri fork). Written 2026-11-02 after a
full troubleshoot → fix → deploy cycle; captures the workflow and
the lessons so the next round doesn't rediscover them.

## Repository layout (read this first)

There is **one** nori checkout, visible at two paths:

- `/home/super/Projects/nori` — the real checkout (git repo, branches, work).
- `/home/super/pixos/desktop/nori` — a **symlink** to the above. Pixos tracks
  only the symlink, so `nori.nix` and in-tree references resolve to whatever
  `Projects/nori` currently has checked out. There is no sync step — and no
  record of which commit is deployed. Record deploys (see below).

Related checkouts:

- `/home/super/Work/niri` — the push target (`origin` for `Projects/nori`).
  Holds `rebase-trial` (live line) and the retired `main`
  (tag `archive/main-20261102`).
- Upstream is YaLTeR/niri (read-only reference; no direct interaction).

## Branch policy

- `rebase-trial` is the live dev line. All work lands here.
- `main` is retired (Nov 2026). Its only unique value — the Oct 13
  column-nest layout fix — was cherry-picked to the tip as `eea02bac`.
  Don't revive it; the tag preserves the history.
- Commit in-flight work early (WIP commits or a `node/<host>` lane).
  A 300-line headless fix once sat uncommitted for 10 days and nearly
  got lost. The tree should never hold the only copy of real work.

## Build

```bash
cd /home/super/Projects/nori
cargo check --lib          # fast typecheck; must be warning-free
cargo test cached_read     # targeted: trigger-cache + headless demand tests
cargo test layout          # targeted: layout incl. nest-restore tests
cargo test --lib           # full suite: baseline is 208 pass / 37 pre-existing
                           # stale-snapshot failures (recorded Oct 2026).
                           # New failures are regressions; stale ones are not
                           # yours to fix in a feature commit.
cargo build --release      # ~20 min; produces target/release/niri
./target/release/niri --version   # smoke: must show the new hash
```

Zero-warning policy: `cargo check` output must be clean. The two
`unused_mut` warnings that lingered in `niri.rs` (dead `mut` beside a
commented-out write) are the kind of rot that hides real warnings.

## Deploy

Headless slots and the headful session deploy differently:

| Target | Binary | Takes effect |
|---|---|---|
| Slots (`nori@1`, `nori@2`) | `~/.local/bin/nori` → `target/release/niri` (**symlink**) | On `systemctl --user restart nori@N.service` |
| Headful session | `~/.local/bin/niri.real` (**copy**) | On session restart |

Rules:

- Back up the running headful binary before staging:
  `cp niri.real niri.real.pre-YYYYMMDD`.
- Never `cp` over a running `niri.real` (`ETXTBSY: Text file busy`).
  Stage via rename: `cp target/release/niri niri.real.new && mv -f
  niri.real.new niri.real`. The running process keeps its inode.
- Expect `niri msg` to warn about CLI/compositor version skew after
  staging. Read-only queries still work; avoid `msg action` calls that
  depend on new IPC until the session restarts.
- After a slot restart, decks re-freeze by design (detached). Check
  `atop slot list` — the BUILD column shows exactly what each head runs.
- Record every deploy: commit hash + binary version + date in the
  active planning triage note (e.g. `pixos/planning/*triage*.md`).

## Operate

- `atop slot list` — per-head status **including BUILD** (drift detector).
- `pixos display doctor` (or `display.py doctor`) — runtime health:
  per-head builds + skew alarm, event-stream subscriber count
  (bloat past ~10), orphaned waybar helpers, slot deck-unit states.
- `atop surface ensure-deck --all` — respawns missing slot surfaces.
  Runs every 60s via `pixos-surface-ensure.timer`. Skips frozen units.

## Troubleshooting lessons (Nov 2026)

**"Compositor CPU is high" is usually clients, not the compositor.**
Freeze ALL clients with SIGSTOP (windows + layer shells + helpers)
and re-measure. A healthy compositor idles at ~0%. Our "29% floor"
was a contaminated test: the freeze pattern missed orphaned helpers.

**Orphaned event subscribers multiply compositor cost.** Every
`event-stream` subscriber gets every event serialized on the main
thread. 35 subscribers (33 leaked by a debug script's reconnect loop)
cost 30–45% CPU. Prevention: terminate+reap children in `finally`,
sweep stale children before reconnecting.

**Orphaned pollers survive restarts.** Waybar helpers whose stdout
pipe closed kept polling niri because `except Exception: pass`
swallowed `BrokenPipeError`. Any waybar `exec` helper must exit on
broken pipe (`os._exit`, unswallowable). `display doctor` now detects
leftovers by PPID.

**Systemd oneshots reap their children.** A spawner service with the
default `KillMode=control-group` SIGKILLs everything it started when
it exits. Spawner units need `KillMode=none`, and cgroup moves must
warn loudly on failure (a silent `False` hid a 60s spawn→SIGKILL loop
for days).

**Measure, don't snapshot.** Single `top` readings lie under churn.
Use `/proc/<pid>/stat` deltas over 8–10s windows for CPU, and
`/proc/<pid>/io` `syscr` deltas for syscall storms.

## Open threads

- 37 stale-snapshot lib test failures (pre-existing; need snapshot refresh).
- `main` branch still exists in `Work/niri` (checked out); delete after
  confirming nothing references it:
  `checkout rebase-trial && branch -D main`.
- Headful session still runs pre-Nov binaries until the next restart.
