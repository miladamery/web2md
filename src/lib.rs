//! web2md — crawl documentation sites and emit one Markdown file per HTML page.

pub mod cli;
pub mod codeblock;
pub mod config;
pub mod discover;
pub mod error;
pub mod extract;
pub mod fetch;
pub mod output;
pub mod pipeline;
pub mod postprocess;
pub mod profile;
pub mod robots;
pub mod state;
pub mod urlutil;

pub use error::{Error, Result};
