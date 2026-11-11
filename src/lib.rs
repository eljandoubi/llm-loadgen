//! Open-loop load generator for OpenAI-compatible streaming endpoints.
//!
//! It sends requests on a schedule that does not depend on how fast the server answers, and
//! measures every latency from the moment a request was *meant* to start. That avoids
//! coordinated omission: a server that stalls cannot hide the requests it delayed.
//! The crate uses only the standard library.

pub mod arrival;
pub mod hist;
pub mod http;
pub mod json;
pub mod mock;
pub mod rng;
pub mod sse;
