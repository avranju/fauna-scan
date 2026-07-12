//! Hikvision NVR integration modules.
//!
//! Re-exports the Phase 4 transport types and the Phase 5 camera discovery
//! API for use by downstream modules.

pub mod authentication;
pub mod camera_discovery;
pub mod image_download;
pub mod image_search;

pub use authentication::{NvrRequest, NvrTransport};
pub use camera_discovery::{CameraDiscoveryClient, parse_camera_discovery_xml};
