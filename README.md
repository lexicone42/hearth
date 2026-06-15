# hearth

[![CI](https://github.com/lexicone42/hearth/actions/workflows/ci.yml/badge.svg)](https://github.com/lexicone42/hearth/actions/workflows/ci.yml)

A small, self-hosted **home-automation hub in Rust**. It ingests data from
several **sources**, normalizes everything through one vendor-neutral domain
model, and pushes it to **sinks** — today a [SmartThings](https://smartthings.com)
account, so readings show up on a Samsung Family Hub fridge.

It began as an [Ambient Weather](https://ambientweather.net) → SmartThings bridge
and grew into a general hub: it now runs **three source types** and publishes
live to SmartThings.

## What it does today

| Source | Transport | Status |
|---|---|---|
| **Ambient Weather** station | REST poll | ✅ live — temp, humidity, wind, rain, PM2.5, remote sensors |
| **Dyson** purifier/fan | local **MQTT** (push) | ✅ live — air quality, temp, humidity, filter life, fan speed |
| **EcoFlow** power station | HMAC-signed REST | ✅ code-complete — awaiting developer API credentials |

| Sink | How |
|---|---|
| **SmartThings** | Virtual Devices API (outbound `POST .../events`), OAuth-refreshed |

## Architecture

`source → domain → sink`, decoupled by an internal event bus:

- Each **source** is an independent async task. Poll sources (Ambient, EcoFlow)
  tick on an interval; push sources (Dyson) emit whenever the device sends an
  MQTT message. All produce canonical `Observation`s onto a `tokio::mpsc` bus.
- A single **router** drains the bus and fans each batch out to the **sink(s)** —
  the only place a sink is called.
- The **domain** is vendor-neutral: `DeviceClass` (temperature, humidity, PM2.5,
  battery, power, …) is the pivot every sink maps from, so SmartThings is just
  the first of potentially many outputs.

Adding a sensor kind = one `DeviceClass` + a per-sink mapping. Adding a source =
a new module that emits `Observation`s onto the bus.

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

Runs in the wild as an OpenRC service (see `packaging/`); a `systemd` unit looks
similar. Secrets and runtime state (`config.toml`, `token_store.json`, …) are
gitignored — see `config.example.toml` for the shape.

## Roadmap

- [x] Config + REST poll loop; vendor-neutral `domain` model, unit-tested
- [x] SmartThings sink: capability mapping + virtual-device push + OAuth refresh
- [x] EcoFlow source (HMAC-signed IoT Open API) — *awaiting developer API keys*
- [x] Event bus + **Dyson** local-MQTT push source (live on real hardware)
- [ ] Realtime Ambient Socket.IO ingest
- [ ] More sources / sinks; richer SmartThings capabilities

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion shall be dual-licensed as above, with no
additional terms or conditions.
