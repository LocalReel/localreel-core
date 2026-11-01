//! LocalReel HTTP API (`/api/v1`) plus the SSE event stream.
//!
//! The API is a thin adapter over `localreel-media`: it parses/validates
//! requests, calls the domain service, and maps errors to a stable JSON
//! envelope. No SQL, no ffmpeg and no domain policy live here.

pub mod error;
pub mod guard;
pub mod routes;
pub mod state;
pub mod web;

pub use routes::router;
pub use state::AppState;
