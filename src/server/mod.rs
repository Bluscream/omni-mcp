//! Transports: stdio for IDE clients, HTTP for everything else.

pub mod http;
pub mod stdio;

pub use http::HttpConfig;
