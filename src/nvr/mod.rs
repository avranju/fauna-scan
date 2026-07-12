//! Hikvision NVR integration modules.
//!
//! - `authentication` (Phase 4): Digest-authenticated transport.
//! - `camera_discovery` (Phase 5): Streaming-channel discovery and parsing.
//! - `image_download` (Phase 7): Downloaded image persistence.
//! - `image_search` (Phase 6): Search XML serialization, parsing, pagination,
//!   and the `ImageSearchClient` coordinator.
//!
//! The Phase 6 image-search API is publicly re-exported below.

pub mod authentication;
pub mod camera_discovery;
pub mod image_download;
pub mod image_search;

pub use authentication::{NvrRequest, NvrTransport};
pub use camera_discovery::{CameraDiscoveryClient, parse_camera_discovery_xml};

// ── Phase 6 re-exports ───────────────────────────────────────────────────

pub use image_search::{
    ImageSearchClient, SearchWindow, SearchWindowOutcome, canonical_playback_path_and_query,
    compute_image_key, configured_nvr_identity, generate_search_windows, parse_image_search_xml,
};
