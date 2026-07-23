//! Whisker source: polls a Litter-Robot 5 from Whisker's (unofficial,
//! reverse-engineered) cloud and normalizes it into canonical
//! [`crate::domain::Observation`]s so it reaches the local API sink / watch tile.
//!
//! Mirrors the `schlage` module's shape (both are Cognito-auth'd cloud sources):
//!   - [`auth`]      performs the AWS Cognito SRP handshake (and cheap refresh),
//!   - [`client`]    fetches the LR5 endpoints with the id token as a `Bearer`,
//!   - [`model`]     deserializes the robots (REST) / pets (GraphQL) / activity
//!     (REST) responses,
//!   - [`canonical`] maps a box to litter-level / waste-drawer / status (+ last
//!     visitor weight) and a cat to its measured weight,
//!   - [`history`]   the append-only weight archive: persists every PET_VISIT
//!     (per-cat weight) forever, since the cloud retains only ~30 days.
//!
//! LR5 splits the data across TWO live-verified endpoints, both authed with the
//! id token as `Authorization: Bearer <IdToken>` (the `Bearer` prefix is
//! required — a raw token 401s):
//!   - **robots** — `GET https://ub.prod.iothings.site/robots` (also needs the
//!     public `x-api-key`): each box's litter / waste / status.
//!   - **pets** — `POST https://pet-profile.iothings.site/graphql/`
//!     (`getPetsByUser`): each cat's measured weight — the hub owner's #1 goal.
//!
//! The whole source is a no-op when the `[whisker]` config section is absent —
//! `main` only constructs a [`client::WhiskerClient`] when credentials exist.
//!
//! AWS Cognito SRP auth (us-east-1); the Cognito pool/client constants are
//! verified against `natekspencer/pylitterbot`. Being an unofficial API, it can
//! break whenever Whisker changes it — every error here is logged, never fatal.
//!
//! Focus (per the hub owner's goals): each cat's **weight**, plus the two
//! "time to change it" signals per box — a full **waste drawer** and **low litter**.
pub mod auth;
pub mod canonical;
pub mod client;
pub mod error;
pub mod history;
pub mod model;

pub use error::WhiskerError;
