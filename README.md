# hearth

[![CI](https://github.com/lexicone42/hearth/actions/workflows/ci.yml/badge.svg)](https://github.com/lexicone42/hearth/actions/workflows/ci.yml)

A small, self-hosted **home-automation hub in Rust**. It ingests data from
several **sources**, normalizes everything through one vendor-neutral domain
model, and pushes it to **sinks** — a [SmartThings](https://smartthings.com)
account (so readings appear as virtual devices and can drive Routines), and a
local HTTP sink that also serves hearth's own dashboard, which the Samsung
Family Hub fridge loads directly over the LAN.

It began as an [Ambient Weather](https://ambientweather.net) → SmartThings bridge
and grew into a general hub: it now runs **six source integrations** — Ambient
Weather, Dyson, EcoFlow, Schlage, Whisker (Litter-Robot 5) and SmartThings
read-back — publishes live to SmartThings, and serves its own dashboard on the
LAN.

## What it does today

| Source | Transport | Status |
|---|---|---|
| **Ambient Weather** station | REST poll | ✅ live — temp, humidity, wind, rain, UV/solar, PM2.5, remote sensors |
| **Dyson** purifier/fan | local **MQTT** (push) | ✅ live — PM2.5 / PM10 / VOC / NO₂, temp, humidity, filter life, fan speed |
| **Schlage** Wi-Fi lock | unofficial cloud REST (AWS Cognito SRP) | ✅ live — lock state + battery |
| **Whisker** Litter-Robot 5 | unofficial cloud REST + GraphQL (Cognito SRP) | ✅ live — per-cat weight, litter level, waste drawer, unit status, "needs service" alert |
| **SmartThings** read-back | REST poll of `GET /devices/{id}/status` | ✅ live — lock + battery (makes the sink a source too) |
| **EcoFlow** power station | HMAC-signed REST | ⚠️ code-complete, unverified live — the IoT Open API currently rejects our request signature (`code 8521 signature is wrong`); the source backs off to 30 min and never affects the rest of the hub |

| Sink | How |
|---|---|
| **SmartThings** | Virtual Devices API (outbound `POST .../events`), OAuth-refreshed |
| **Local HTTP API** | axum on the LAN: `GET /api/latest` (latest-value snapshot), `/api/history` (per-cat 10-day weight sparklines), `/api/visits` (per-visit detail), `/healthz`. An optional `[api].token` guards the `/api/*` routes — see [HTTP API](#http-api) |
| **Fridge dashboard** | `GET /` — a self-contained dark, portrait dashboard (Overview / Cats / Gallery / Weather / Air tabs, per-cat drill-down at `#/cat/<slug>`), with cat photos served from `/assets/cats/<file>`. Built for the Samsung Family Hub panel — see [The fridge dashboard](#the-fridge-dashboard) |

Every reading lands under a source-namespaced entity id — the one identifier the
config, the API and the dashboard all key on:

```
ambient_weather.outdoor.temperature   dyson.<serial>.pm25
schlage.<lock>.lock                   whisker.<cat>.weight
ecoflow.<sn>.battery                  smartthings.front_door.battery
```

## Architecture

`source → domain → sink`, decoupled by an internal event bus:

- Each **source** is an independent async task. Poll sources (Ambient, EcoFlow,
  Schlage, Whisker, SmartThings read-back) tick on `[poll].interval_secs`,
  backing off exponentially (capped at 30 min) on consecutive failures; push
  sources (Dyson) emit whenever the device sends an MQTT message. They produce
  canonical `Observation`s onto a `tokio::mpsc` bus — with one exception: the
  Whisker weight-history task bypasses the bus and appends straight to the local
  archive, since it is history, not current state.
- A single **router** drains the bus and fans each batch out to the **sink(s)** —
  the only place a sink is called.
- The **domain** is vendor-neutral: `DeviceClass` (temperature, humidity, PM2.5,
  battery, power, lock, weight, litter level, waste drawer, status, alert, …) is
  the pivot every sink maps from, so SmartThings is just the first of potentially
  many outputs. Classes with no sensible SmartThings write mapping (lock, weight,
  litter level, waste drawer, status) still flow to the local API sink and the
  dashboard; a binary `Alert` maps to a virtual `contactSensor` so an in-app
  Routine can push a notification.

Adding a sensor kind = one `DeviceClass` + a per-sink mapping. Adding a source =
a new module that emits `Observation`s onto the bus.

Every source is optional and independently failable: omit its config section and
no task is spawned; when one is misbehaving it logs, backs off and keeps the rest
of the hub running.

## Why an external bridge (not a hub app)

SmartThings retired its Groovy cloud platform (2022–2023), and its modern on-hub
runtime is Lua Edge drivers — neither runs Rust. So hearth is an **always-on
external daemon** (Raspberry Pi / NAS / small box) that talks to each device on
one side and the SmartThings **cloud API** on the other. SmartThings Personal
Access Tokens now expire after 24h, so it uses an OAuth `authorization_code`
flow and self-refreshes from a stored refresh token.

## Running

```sh
cp config.example.toml config.toml     # fill in your keys / devices
cargo run -- auth                      # one-time SmartThings OAuth (or set a 24h PAT)
cargo run -- provision                 # self-create the SmartThings virtual devices
cargo run                              # start the hub
```

With `[api]` enabled, the dashboard is then at `http://<host>:8091/` and the JSON
snapshot at `http://<host>:8091/api/latest`.

### CLI

The binary takes an optional subcommand as its first argument. With none it
starts the hub and runs until ctrl-c. Any other word exits `2` with a usage line.
**Every** invocation — subcommands included — loads the config first, so a valid
`config.toml` must exist before `auth` will run.

| Command | What it does |
|---|---|
| *(none)* | Start the hub: every configured source, the router, the SmartThings sink, and the local HTTP API + dashboard. Runs until ctrl-c. |
| `auth` | One-time interactive SmartThings OAuth. Prints an authorize URL, takes the redirect code, and writes refreshable tokens to `token_store.json` (chmod `0600`). Then exits. |
| `provision` | Self-create the SmartThings virtual devices declared in `[[smartthings.devices]]`; the resulting ids are saved to `device_store.json`. Rerun after adding a device block. Then exits. |
| `whisker-history-import <snapshot-dir>` | Bank a previously-saved 30-day Litter-Robot activity snapshot into the local archive. Then exits. |

```sh
hearth                                     # run the hub
hearth auth                                # one-time SmartThings OAuth
hearth provision                           # create the virtual devices
hearth whisker-history-import ~/lr5-snap   # bank a saved activity snapshot
```

`whisker-history-import` reads every `*.json` in `<snapshot-dir>` (sorted, for a
deterministic order) that parses as a JSON array of activity events, maps the
PET_VISITs, and appends the new ones — printing how many it added. It is
**idempotent** (deduped by `eventId`), so importing the same snapshot twice adds
nothing. A `*.json` that isn't an activity array is skipped with a warning, not
fatal. Cat names are resolved by a live pet lookup; if auth fails the import
still proceeds with `cat = null` — the weights are what matter. It requires
`[whisker]` in the config, and `<snapshot-dir>` may live outside the repo.
Omitting the directory exits `2`.

### Environment

| Variable | Effect |
|---|---|
| `HEARTH_CONFIG` | Path to the config file. Default `config.toml`, resolved relative to the working directory. |
| `RUST_LOG` | Standard `tracing` filter. Default `info,hearth=debug`. |

## Configuration

One TOML file. Copy `config.example.toml` to `config.toml` (gitignored — it holds
secrets) and fill it in; `config.example.toml` documents every key and its
default.

Only `[ambient]` is required. Every other section is optional: omit it to take
its defaults, or — for a source — to disable it entirely, in which case no task
is spawned and the hub never touches that vendor.

| Section | What it turns on |
|---|---|
| `[ambient]` | **Required.** The Ambient Weather station poll (application key, API key, station MAC). |
| `[poll]` | REST poll interval for every poll source. Default `interval_secs = 60`. |
| `unit_system` | `"imperial"` (default) or `"metric"` — how values are rendered and exported. |
| `[ecoflow]` | EcoFlow IoT Open API source (access key + secret key; serials optional, else discovered each poll). |
| `[schlage]` | Schlage lock source (Schlage Home email + password). |
| `[whisker]` | Litter-Robot 5 source (Whisker email + password), plus `serial`, `history_dir`, and the `drawer_full_pct` / `litter_low_pct` alert thresholds. |
| `[[dyson]]` | One block per Dyson device. `host` is the only required key; `ssid` + `wifi_password` off the setup sticker derive the MQTT credential locally (no cloud round-trip). |
| `[api]` | The local HTTP server: JSON API **and** the dashboard. `listen` defaults to `0.0.0.0:8091`; `token` is an optional bearer for the `/api/*` routes. |
| `[history]` | Long-term recording of every observation to redb. Omit to record nothing; see [Long-term history](#long-term-history). |
| `[smartthings]` | The SmartThings sink: auth (`[smartthings.oauth]` or a 24h PAT), `[[smartthings.devices]]` bindings, and `[[smartthings.read]]` for the read-back source. |

The unofficial-cloud sources (Schlage, Whisker) authenticate over AWS Cognito SRP
with your account credentials, which are held **in memory only** and never
persisted. They are reverse-engineered APIs with no public contract — they can
break whenever the vendor changes them, so every error there is logged and backed
off, never fatal.

## HTTP API

Enabled by the `[api]` section — omit it and no listener is started. Default bind
`0.0.0.0:8091` (all interfaces; hearth assumes a trusted LAN). One axum router,
in `src/api/server.rs`.

**Auth.** When `[api].token` is set, the three `/api/*` endpoints require
`Authorization: Bearer <token>`; a missing or wrong token gets `401`. With no
token configured everything is open. The page shell (`/`), the photos
(`/assets/cats/…`) and `/healthz` are *never* authenticated — `/` has the token
injected into it at serve time so its own same-origin polls authenticate without
a secret ever living in the repo. Note what that implies: anyone who can reach
the port can read the token out of the page source. This is **LAN-grade** auth —
a speed bump for casual clients and port scans, not an access control. If
anything you don't trust can reach the port, bind `127.0.0.1:8091` and put an
authenticating reverse proxy in front.

| Method | Path | Query | Auth | Response |
|---|---|---|---|---|
| `GET` | `/` | — | none | `text/html` — the fridge dashboard, one self-contained document |
| `GET` | `/api/latest` | — | bearer | JSON — latest-value snapshot |
| `GET` | `/api/history` | — | bearer | JSON — per-cat daily-median weight, last 10 days |
| `GET` | `/api/visits` | `cat=<slug>` (**required**), `days=<n>` (default 30, capped at 60) | bearer | JSON — one cat's visits, chronological |
| `GET` | `/assets/cats/{name}` | — | none | image bytes (`webp`/`png`/`jpeg`), `Cache-Control: public, max-age=86400` |
| `GET` | `/healthz` | — | none | `text/plain` — `ok` |

### `GET /api/latest`

The newest observation per entity, re-expressed in the hub's configured unit
system. The one endpoint a dashboard client needs for current state.

```json
{
  "generated_at": 1769500000000,
  "unit_system": "imperial",
  "entities": [
    {
      "entity": "ambient_weather.outdoor.temperature",
      "class": "Temperature",
      "value": 72.4,
      "unit": "°F",
      "display": "72.4 °F",
      "observed_at": 1769499960000,
      "received_at": 1769499998000
    }
  ]
}
```

- `entities` is sorted by `entity` id, for a stable payload.
- `value` is a number, a bool, or a string, depending on the class.
- `unit` is omitted for counts, flags, text, and unitless indices.
- `observed_at` is omitted when the source gave no timestamp; `received_at` (when
  the hub got it) is always present. Both are epoch ms UTC.

### `GET /api/history`

Per-cat daily-median weight for the sparklines, keyed by **cat slug** — the same
slug used in the `whisker.<slug>.weight` entity id, so a client can join the two.

```json
{
  "fixture_one": { "series": [9.4, 9.5, 9.4], "visits": 37 },
  "fixture_two": { "series": [7.0], "visits": 4 }
}
```

- `series` is one median (lb) per day that has data, oldest first, over at most
  the most recent **10 days** — a fixed window; there is no query parameter.
- Weights outside 3–30 lb are dropped as sensor noise.
- Returns `{}` (never an error) when `[whisker]` isn't configured or the archive
  is missing or unreadable — the dashboard silently falls back to its embedded
  series.

### `GET /api/visits?cat=<slug>&days=<n>`

Every plausible visit for one cat, chronological — the raw projection the per-cat
detail page slices into its weight / waste / box / time-in-box charts, so a new
visualization is a front-end change only.

```json
[
  {
    "ts": "2026-01-02T09:00:00Z",
    "box_name": "test room",
    "weight": 9.5,
    "waste": "Urine",
    "duration": 61,
    "waste_weight": 48.0
  }
]
```

- `cat` is the slug, and is **required** — omitting it is a `400`.
- `days` defaults to 30 and is capped at 60.
- `waste`, `duration` (seconds in the box) and `waste_weight` (grams) are `null`
  when the vendor feed didn't report them.
- Returns `[]` for an unknown slug, or when `[whisker]` isn't configured.

### `GET /assets/cats/{name}`

Serves a file from the gitignored `data/cats/` dir, same-origin, so the dashboard
stays self-contained (no external host; works even if the photos' source site is
down). The page requests `<cat-slug>.webp`.

Only a bare `[A-Za-z0-9._-]` filename is accepted — no `/`, no `..`. Anything
else is `400`; a missing file is `404`.

### `GET /healthz`

Liveness only, never authenticated: `200 ok` means the process is up and serving.

## The fridge dashboard

With `[api]` enabled, `GET /` serves a complete dashboard — one HTML document
compiled into the binary (`src/api/dashboard.html`), no build step and no external
assets, with the API token injected at serve time. Open it from any browser on
the LAN:

```
http://<hub-host>:8091/
```

It was built for, and empirically probed on, a **Samsung Family Hub fridge panel**
(Chromium 130, portrait 720×1040), so the layout is single-column portrait. To pin
it: open the panel's browser, go to that URL, and leave the tab open — it handles
its own refreshes.

Tabs appear only when their data is present, so a source that comes online shows
up on its own:

- **Overview** — one-glance verdict, litter boxes, headline numbers
- **Cats** — a card per cat: current weight, 10-day sparkline, visit count
- **Gallery** — photo grid from `data/cats/`, tap through to a cat
- **Weather** — every `ambient_weather.*` channel
- **Air** — every `dyson.*` channel

Tapping a cat opens `#/cat/<slug>`: weight trend with a daily min/max band, daily
pee/poop bars, time-in-box, and a box-preference split — all computed in the page
from `/api/visits`, with one shared day cursor scrubbing every chart together.

Behavior that matters on a fridge panel: it polls `/api/latest` (+ `/api/history`)
every 12s, paints instantly from `localStorage` on load, and **refetches on wake**
(`visibilitychange` / `focus` / `pageshow`) — the fix for the panel's screensaver
freeze — with a reload backstop every 20 minutes while hidden. If the API is
unreachable it keeps showing the last data it had and marks itself offline rather
than going blank.

## Deployment

In production hearth runs as a supervised **OpenRC** service; a `systemd` unit
would look similar. `packaging/hearth.openrc` is the service script and
`packaging/install.sh` installs, enables and starts it.

```sh
cargo build --release             # the service runs target/release/hearth
sudo sh packaging/install.sh      # install to /etc/init.d, enable at boot, start
```

**Edit `packaging/hearth.openrc` before installing** — the project paths and
`command_user` are hard-coded to the author's host.

The service:

- runs under `supervise-daemon` with `respawn_delay=5` and **no respawn cap** — a
  home hub should keep retrying indefinitely rather than give up after N tries;
- runs as the **owner of `config.toml` / `token_store.json`**, so OAuth refresh
  can rewrite them (get this wrong and tokens silently stop persisting);
- sets its working directory to the project root — the config path,
  `data/whisker/` and `data/cats/` all resolve relative to it;
- exports `RUST_LOG=info` and waits on the network (`need net`);
- writes stdout and stderr to `hearth.log` in the project root (gitignored).

Redeploy after a code change:

```sh
cargo build --release && sudo rc-service hearth restart
rc-service hearth status
tail -f hearth.log
```

Healthy looks like a `smartthings publish sent=N` line every poll interval.

## Local data (`data/`)

hearth writes persistent state under `data/` in its working directory. **It is
gitignored and holds personal data — never commit it.**

| Path | Written by | Contents |
|---|---|---|
| `data/whisker/visits.jsonl` | the Whisker history task, and `whisker-history-import` | One JSON object per line, one per Litter-Robot PET_VISIT: `event_id`, `ts`, box `serial` + `box_name`, `pet_id`, `cat`, `weight_lb`, `waste_type`, `waste_weight`, `duration_s`. |
| `data/cats/<slug>.webp` | **you**, not hearth | Cat photos for the dashboard gallery and avatars, served at `/assets/cats/<name>`. hearth only ever reads this directory. |

### The visit archive

Whisker's cloud only retains ~30 days of activity, so hearth keeps its own forever
record. The store is:

- **append-only** — a new visit is one appended line; nothing is rewritten;
- **idempotent** — deduplicated by the cloud's `eventId`, so re-importing a saved
  snapshot or re-scanning the overlapping live feed never double-counts;
- **owner-only** — chmod `0600` after every append, because it is personal
  pet/health data (same discipline as `token_store.json`);
- **crash-tolerant** — a malformed line (e.g. a partial write) is logged and
  skipped, never wedging the whole archive.

Defaults to `data/whisker`; override with `[whisker].history_dir`. It is also the
sole data source for `/api/history` and `/api/visits`: if it is missing or
unreadable those endpoints return empty and the dashboard falls back to its
embedded snapshot, with no error surfaced.

## Long-term history

Sources describe the **present**: the router hands each batch to the sinks and
`/api/latest` keeps the newest value per entity. Nothing there remembers
yesterday. Enable `[history]` and the router also writes every batch to an
embedded [redb](https://github.com/cberner/redb) database — pure Rust, ACID, one
file — giving any entity from any source a queryable time series with no
per-source code. A new device that emits observations gets history for free.

```toml
[history]
path = "data/history.redb"   # default
retain_days = 730            # 0 keeps everything
heartbeat_secs = 900         # see below
```

The table is keyed `(entity, epoch_ms)`. redb orders tuple keys by component, so
every point for one entity is contiguous in the B-tree and a time range is a
single sequential scan — no secondary index.

**Points are written only when a value changes.** Polling ~80 entities a minute
would be ~42M points a year, nearly all of them repeats. The `heartbeat` is what
keeps change-only honest: it forces a point when the last one is older than the
interval, so a gap longer than the heartbeat means *genuinely no data* rather
than "steady". Without it, a flat line and an outage are indistinguishable.

A value's timestamp never moves backwards, so a replayed or out-of-order
observation can't rewrite history, and an undecodable row is skipped with a
warning rather than failing a whole read.

Two shapes of data live side by side on purpose. An **observation** is one scalar
sample of one channel; a **litter-box visit** is a discrete event carrying
several *correlated* fields (which cat, what weight, what waste, how long, which
box). Splitting a visit into four independent observations would throw away the
correlation that makes it useful, so [the visit archive](#the-visit-archive)
stays its own store rather than being forced into this one.

> Note: unlike the visit archive, a redb file can't be safely copied with `cp`
> while it is being written — the backup story for it needs redb's own snapshot
> mechanism, which isn't wired up yet.

### Backing it up

The archive is the **only** copy of this history — the cloud has already forgotten
everything older than its ~30-day window, so a lost archive is lost for good. Set
`[whisker].backup_dir` and hearth keeps dated full copies of it:

```toml
[whisker]
backup_dir = "/mnt/other-disk/hearth-backups/whisker"
backup_keep = 14   # 0 keeps every backup forever
```

One `visits-YYYY-MM-DD.jsonl` per UTC day, written atomically (temp file →
`fsync` → rename, so a crash can't replace a good backup with a truncated one),
chmod `0600`, then read back and record-counted to catch silent corruption.
Oldest are pruned past `backup_keep`. Unset `backup_dir` means **no backups**.

Copies are full and uncompressed on purpose: restoring is `cp`, and any single
backup is independently readable in a text editor. At ~200 KB growing a few KB a
day, the redundancy costs nothing worth optimizing.

**Point it at a different physical disk** (or a mounted NAS). A backup on the
archive's own disk survives a fat-finger but not a disk failure; hearth compares
the two devices and logs a warning when they match, because that case otherwise
looks like protection while providing none. None of this defends against losing
the whole machine — for that, sync the backup dir somewhere off-site.

### Cat photos

Optional — a missing photo degrades to an initial-circle, so the dashboard works
without any. Name each file after the cat's **slug**: lowercase, runs of
non-alphanumerics collapsed to `_` ("Fixture One" → `fixture_one.webp`). This is
the same slug used in the `whisker.<slug>.weight` entity id and in the
`/api/history` keys. Downsize before caching — ~480px, 25–50KB each is plenty for
a fridge panel — and keep them `0600` like the rest of `data/`.

Unlike the archive, this path is **hard-coded** to `data/cats` relative to the
process's working directory, so the service must run with the project root as its
CWD.

## Contributing

CI (`.github/workflows/ci.yml`) runs on every push to `main` and every PR: four
gates in one job, plus a separate secret scan. All must pass. Run them locally
before pushing:

```sh
cargo fmt --all --check
cargo build --all-targets
cargo test
cargo clippy --all-targets -- -D warnings   # warnings are errors
```

### Secret scanning

Secrets live only in the gitignored `config.toml`, never in source. A
[gitleaks](https://github.com/gitleaks/gitleaks) pre-commit hook enforces that —
enable it once per clone:

```sh
git config core.hooksPath .githooks
```

It scans your staged changes and blocks any commit containing a secret (install
`gitleaks` for it to run locally; without it the hook warns and passes). CI runs
`gitleaks git --no-banner --redact` over the **full** history (`fetch-depth: 0`),
so a leak in any commit is caught server-side even if the local hook was skipped.

### Tests must use synthetic data

**This repo is public.** No real device serials, account emails, pet ids, MAC
addresses, or names in fixtures — use obvious placeholders (`LR5-TEST-000000`,
`PET-TEST-1`, `"Fixture One"`, `"test room"`).

A few tests verify behavior against the real local `data/` archive. They are
`#[ignore]`d and must stay that way — CI has no archive, and their output would
leak personal data into a public log. Run them by hand:

```sh
cargo test whisker::history::tests::real_weight_series -- --ignored --nocapture
```

## Roadmap

What's shipped is [What it does today](#what-it-does-today). What's next:

- [ ] Get the EcoFlow IoT Open API to accept our request signature, and verify the
      source against live hardware (currently `code 8521 signature is wrong`)
- [ ] Realtime Ambient Socket.IO ingest — removes the ~1 req/s REST rate-limit
      pressure that the poll loop lives under
- [ ] Thread `[whisker].drawer_full_pct` / `litter_low_pct` into the dashboard, so
      its litter thresholds can't disagree with the SmartThings alert
- [ ] More sources / sinks; richer SmartThings capabilities

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion shall be dual-licensed as above, with no
additional terms or conditions.
