# hearth

A small, self-hosted Rust home-automation hub. Today it bridges an
[Ambient Weather](https://ambientweather.net) station to [SmartThings](https://smartthings.com);
it's built to grow more **sources** (EcoFlow next) and **sinks** behind one vendor-neutral core.

## Why a bridge (and not a hub app)

SmartThings shut down its Groovy cloud platform (2022–2023), and its modern
on-hub runtime is Lua Edge drivers — neither runs Rust. So this is an **external
service**, not a port of the old Groovy `STAmbientWeather`: it talks to Ambient
Weather on one side and the SmartThings **cloud API** on the other, as an
always-on daemon (Raspberry Pi / NAS / small box).

## Design decisions

- **Ingest:** Ambient Weather **REST** to start (simple, typed), then their
  **Socket.IO realtime** feed for near-instant push, with a slow REST poll kept
  as a reconcile/safety net.
- **SmartThings output:** **Virtual Devices API** (outbound-only `POST .../events`),
  not the Cloud Connector. No public inbound webhook to host behind home NAT.
- **Auth:** SmartThings Personal Access Tokens now expire after 24h, so the
  daemon uses an OAuth `authorization_code` flow and refreshes its access token
  from a stored refresh token.

## Roadmap

1. **[done]** Scaffold + config + REST poll loop
2. **[done]** Canonical `domain` model + Ambient Weather → `Observation` mapper,
   unit-tested. Vendor-neutral on purpose: `DeviceClass` is the pivot every sink
   maps from, so SmartThings is just the first of potentially many outputs.
3. **[done]** SmartThings sink: capability mapping + virtual-device event push,
   plus OAuth `authorize`/refresh with a persisted token store. Run
   `cargo run -- auth` once to authorize (or set a 24h PAT for a quick test).
4. Realtime Socket.IO ingest + reconnect/backoff
5. Hardening: retries, graceful shutdown, `systemd` unit

## Running

```sh
cp config.example.toml config.toml   # then fill in your keys
cargo run
```
