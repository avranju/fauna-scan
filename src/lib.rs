//! Fauna Scan — application library.
//!
//! Exposes all public modules for the binary entry point and integration tests.

pub mod app;
pub mod authentication;
pub mod classifier;
pub mod cli;
pub mod configuration;
pub mod database;
pub mod domain;
pub mod downloader;
pub mod error;
pub mod filesystem;
pub mod garbage_collector;
pub mod http;
pub mod logging;
pub mod nvr;
pub mod scanner;
pub mod service_lifecycle;
pub mod web;
