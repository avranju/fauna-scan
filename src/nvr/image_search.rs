//! Image search via `POST /ISAPI/ContentMgmt/search`.
//!
//! Implemented in Phase 6.
//!
//! Provides half-open UTC search window generation, exact Hikvision XML
//! request serialization with per-request UUID v4, namespace-tolerant
//! response parsing, media filtering, pagination with loop detection,
//! canonical playback URI normalization, configured-origin NVR identity,
//! and SHA-256 stable image-key generation.
//!
//! The `ImageSearchClient` coordinates authenticated page retrieval,
//! validates responses, deduplicates via stable keys, and commits
//! all discovered images plus cursor advancement atomically — or
//! records a safe cursor error on any failure without advancing.

use std::collections::{BTreeSet, HashSet};

use chrono::Utc;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::configuration::NvrSearchConfig;
use crate::database::models::*;
use crate::database::repository::DatabaseOps;
use crate::domain::{CameraId, ImageKey, Timestamp, TrackId};
use crate::error::{AppError, AppResult, ErrorCategory};

use super::NvrTransport;

// ── SearchWindow ──────────────────────────────────────────────────────────

/// A half-open UTC search interval `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchWindow {
    pub start: Timestamp,
    pub end: Timestamp,
}

/// Divide a UTC range into contiguous half-open windows of the given duration.
///
/// Returns an error if `window_minutes` is zero.  Returns an empty vector
/// when `start >= end` (no windows to generate).  The final window is
/// truncated so it never exceeds `effective_end`.
pub fn generate_search_windows(
    start: Timestamp,
    end: Timestamp,
    window_minutes: u64,
) -> AppResult<Vec<SearchWindow>> {
    if window_minutes == 0 {
        return Err(AppError::new(
            ErrorCategory::Configuration,
            "generate_search_windows",
            "window_minutes must be greater than zero",
        ));
    }

    let mut windows = Vec::new();
    let mut current = start;

    while current.as_datetime() < end.as_datetime() {
        let window_end = if window_minutes > i64::MAX as u64 {
            *end.as_datetime()
        } else if let Some(candidate) = current
            .as_datetime()
            .checked_add_signed(chrono::Duration::minutes(window_minutes as i64))
        {
            if candidate > *end.as_datetime() {
                *end.as_datetime()
            } else {
                candidate
            }
        } else {
            *end.as_datetime()
        };

        if window_end > *current.as_datetime() {
            windows.push(SearchWindow {
                start: Timestamp::new(*current.as_datetime()),
                end: Timestamp::new(window_end),
            });
            current = Timestamp::new(window_end);
        } else {
            break;
        }
    }

    Ok(windows)
}

// ── Internal request model ────────────────────────────────────────────────

/// A generated search request carrying both the UUID and the serialized XML.
struct SearchRequestDocument {
    search_id: Uuid,
    xml: Vec<u8>,
}

/// Escape XML special characters in text content.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Serialize a CMSearchDescription XML request for one page.
///
/// Emits the exact `searchResultPostion` spelling, UTC Z timestamps with
/// fractional precision, metadata content type, the allPic descriptor,
/// and a freshly generated UUID v4 searchID.
fn serialize_search_request(
    track_id: &TrackId,
    window: &SearchWindow,
    position: u64,
    max_results: u64,
) -> AppResult<SearchRequestDocument> {
    let search_id = Uuid::new_v4();
    // Use nanosecond precision so fractional window boundaries are preserved.
    let start_str = window
        .start
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
    let end_str = window
        .end
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);

    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <CMSearchDescription>\
           <searchID>{search_id}</searchID>\
           <trackList>\
             <trackID>{tid}</trackID>\
           </trackList>\
           <timeSpanList>\
             <timeSpan>\
               <startTime>{start}</startTime>\
               <endTime>{end}</endTime>\
             </timeSpan>\
           </timeSpanList>\
           <contentTypeList>\
             <contentType>metadata</contentType>\
           </contentTypeList>\
           <maxResults>{max}</maxResults>\
           <searchResultPostion>{pos}</searchResultPostion>\
           <metadataList>\
             <metadataDescriptor>//recordType.meta.std-cgi.com/allPic</metadataDescriptor>\
           </metadataList>\
         </CMSearchDescription>",
        search_id = escape_xml(&search_id.to_string()),
        tid = escape_xml(track_id.as_str()),
        start = escape_xml(&start_str),
        end = escape_xml(&end_str),
        max = max_results,
        pos = position,
    );

    Ok(SearchRequestDocument {
        search_id,
        xml: xml.into_bytes(),
    })
}

// ── Response types ────────────────────────────────────────────────────────

/// Parsed result of a single search response page.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SearchResponse {
    search_id: Option<Uuid>,
    response_status: bool,
    response_status_string: String,
    num_of_matches: Option<u64>,
    items: Vec<ParsedMatchItem>,
    raw_item_count: usize,
}

/// Represents a valid parsed Hikvision match before media filtering.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct SearchMatch {
    track_id: TrackId,
    capture_start_at: Timestamp,
    capture_end_at: Option<Timestamp>,
    content_type: String,
    codec_type: String,
    playback_uri: String,
    nvr_reported_size: Option<i64>,
    metadata_descriptors: Vec<String>,
}

/// Outcome of parsing one `<searchMatchItem>` element.
#[derive(Debug)]
pub(crate) enum ParsedMatchItem {
    Valid(SearchMatch),
    Malformed {
        index: usize,
        reason: String,
        /// A non-reversible fingerprint of the ordered raw item fields.
        /// This differentiates malformed items for pagination loop detection
        /// without retaining or logging sensitive playback URI content.
        fingerprint: String,
    },
}

// ── Field presence tracking ───────────────────────────────────────────────

/// Track whether a field was present in the XML and its parsed value.
///
/// This distinguishes "field absent" (None) from "field present but invalid"
/// (Some(Err)) so we can reject present-but-invalid values without silently
/// treating them as absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum FieldState<T> {
    Absent,
    Present(T),
}

impl<T> FieldState<T> {
    /// Extract the value if present, cloning the inner value.
    fn get_value(&self) -> Option<T>
    where
        T: Clone,
    {
        match self {
            FieldState::Present(v) => Some(v.clone()),
            FieldState::Absent => None,
        }
    }
}

// ── Response parsing ──────────────────────────────────────────────────────

/// Parse and semantically validate a namespace-independent Hikvision search response.
pub fn parse_image_search_xml(xml: &[u8], expected_search_id: Uuid) -> AppResult<SearchResponse> {
    if xml.is_empty() {
        return Err(AppError::new(
            ErrorCategory::XmlParsing,
            "parse_image_search_xml",
            "XML response body is empty",
        ));
    }

    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut element_stack: Vec<String> = Vec::new();

    // Document-level field presence tracking
    let mut search_id_state: FieldState<Uuid> = FieldState::Absent;
    let mut response_status_state: FieldState<bool> = FieldState::Absent;
    let mut response_status_strg_state: FieldState<String> = FieldState::Absent;
    let mut num_of_matches_state: FieldState<u64> = FieldState::Absent;
    let mut root_element: Option<String> = None;
    let mut root_closed = false;
    let mut xml_declaration_seen = false;
    let mut doctype_seen = false;

    // Per-item capture context
    let mut item_index: usize = 0;
    let mut item_track_id: FieldState<String> = FieldState::Absent;
    let mut item_start_time: FieldState<String> = FieldState::Absent;
    let mut item_end_time: FieldState<String> = FieldState::Absent;
    let mut item_content_type: FieldState<String> = FieldState::Absent;
    let mut item_codec_type: FieldState<String> = FieldState::Absent;
    let mut item_playback_uri: FieldState<String> = FieldState::Absent;
    // Preserve decoded size text until validation so distinct malformed
    // values remain distinct in repeated-page fingerprints.
    let mut item_nvr_size: FieldState<String> = FieldState::Absent;
    let mut item_metadata: Vec<String> = Vec::new();

    let mut items: Vec<ParsedMatchItem> = Vec::new();

    #[derive(Clone, Copy, PartialEq)]
    enum ActiveCaptureField {
        None,
        SearchId,
        ResponseStatus,
        ResponseStatusStrg,
        NumOfMatches,
        ItemTrackId,
        ItemStartTime,
        ItemEndTime,
        ItemContentType,
        ItemCodecType,
        ItemPlaybackUri,
        ItemNvrSize,
        ItemMetadataDescriptor,
    }
    let mut active_fields: Vec<(usize, ActiveCaptureField)> = Vec::new();
    let mut current_capture = String::new();

    // Track whether we are currently inside a searchMatchItem
    let mut inside_match_item: bool = false;

    // Helper: determine the capture field for a start element by local name
    fn field_for_start(
        local: &str,
        inside_match: bool,
        is_document_child: bool,
        _ancestors: &[String],
    ) -> ActiveCaptureField {
        if inside_match {
            // Match fields are selected by local name because firmware places
            // optional wrappers differently between versions. Document-level
            // fields below remain restricted to direct response children.
            return match local {
                "trackID" => ActiveCaptureField::ItemTrackId,
                "startTime" => ActiveCaptureField::ItemStartTime,
                "endTime" => ActiveCaptureField::ItemEndTime,
                "contentType" => ActiveCaptureField::ItemContentType,
                "codecType" => ActiveCaptureField::ItemCodecType,
                "playbackURI" => ActiveCaptureField::ItemPlaybackUri,
                "size" => ActiveCaptureField::ItemNvrSize,
                "metadataDescriptor" => ActiveCaptureField::ItemMetadataDescriptor,
                _ => ActiveCaptureField::None,
            };
        }

        // Page-level fields are valid only as direct children of the response
        // root. Unknown wrappers must not overwrite document status.
        if is_document_child {
            return match local {
                "searchID" => ActiveCaptureField::SearchId,
                "responseStatus" => ActiveCaptureField::ResponseStatus,
                "responseStatusStrg" => ActiveCaptureField::ResponseStatusStrg,
                "numOfMatches" => ActiveCaptureField::NumOfMatches,
                _ => ActiveCaptureField::None,
            };
        }

        ActiveCaptureField::None
    }

    // Helper: set a document-level field with presence tracking
    fn set_doc_field(
        search_id_state: &mut FieldState<Uuid>,
        response_status_state: &mut FieldState<bool>,
        response_status_strg_state: &mut FieldState<String>,
        num_of_matches_state: &mut FieldState<u64>,
        capture: ActiveCaptureField,
        trimmed: String,
    ) -> AppResult<()> {
        match capture {
            ActiveCaptureField::SearchId => {
                if !matches!(search_id_state, FieldState::Absent) {
                    return Err(AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        "search response contains duplicate searchID fields",
                    ));
                }
                // Reject a present but invalid searchID
                let parsed = parse_search_id(&trimmed).ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        format!("present searchID value is not a valid UUID: {trimmed}"),
                    )
                })?;
                *search_id_state = FieldState::Present(parsed);
            }
            ActiveCaptureField::ResponseStatus => {
                if !matches!(response_status_state, FieldState::Absent) {
                    return Err(AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        "search response contains duplicate responseStatus fields",
                    ));
                }
                // Reject a present but invalid responseStatus
                let parsed = parse_bool(&trimmed).ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        format!("present responseStatus value is not a valid boolean: {trimmed}"),
                    )
                })?;
                *response_status_state = FieldState::Present(parsed);
            }
            ActiveCaptureField::ResponseStatusStrg => {
                if !matches!(response_status_strg_state, FieldState::Absent) {
                    return Err(AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        "search response contains duplicate responseStatusStrg fields",
                    ));
                }
                *response_status_strg_state = FieldState::Present(trimmed);
            }
            ActiveCaptureField::NumOfMatches => {
                if !matches!(num_of_matches_state, FieldState::Absent) {
                    return Err(AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        "search response contains duplicate numOfMatches fields",
                    ));
                }
                // Reject a present but invalid numOfMatches
                let parsed = trimmed.parse::<u64>().map_err(|_| {
                    AppError::new(
                        ErrorCategory::InvalidNvrResponse,
                        "parse_image_search_xml",
                        format!(
                            "present numOfMatches value is not a valid unsigned integer: {trimmed}"
                        ),
                    )
                })?;
                *num_of_matches_state = FieldState::Present(parsed);
            }
            _ => {}
        }
        Ok(())
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => {
                if !root_closed || !element_stack.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "XML response document is incomplete",
                    ));
                }
                break;
            }
            Ok(Event::Start(e)) => {
                let local_name = e.name().local_name();
                let local: String = reader
                    .decoder()
                    .decode(local_name.as_ref())
                    .map_err(|e| {
                        AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            format!("character encoding error: {e}"),
                        )
                    })?
                    .into_owned();

                let depth = element_stack.len();
                if depth == 0 {
                    if root_element.is_some() || root_closed {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            "multiple root elements in response document",
                        ));
                    }
                    if local != "CMSearchResult" {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            format!("expected root element CMSearchResult, got {local}"),
                        ));
                    }
                    root_element = Some(local.clone());
                }
                if local == "CMSearchResult" && depth != 0 {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "CMSearchResult must be the document root",
                    ));
                }
                let capture =
                    field_for_start(&local, inside_match_item, depth == 1, &element_stack);

                if local == "searchMatchItem" && !inside_match_item {
                    inside_match_item = true;
                    item_track_id = FieldState::Absent;
                    item_start_time = FieldState::Absent;
                    item_end_time = FieldState::Absent;
                    item_content_type = FieldState::Absent;
                    item_codec_type = FieldState::Absent;
                    item_playback_uri = FieldState::Absent;
                    item_nvr_size = FieldState::Absent;
                    item_metadata.clear();
                }
                current_capture.clear();

                element_stack.push(local);
                active_fields.push((depth + 1, capture));
            }
            Ok(Event::Empty(e)) => {
                let local_name = e.name().local_name();
                let local: String = reader
                    .decoder()
                    .decode(local_name.as_ref())
                    .map_err(|e| {
                        AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            format!("character encoding error: {e}"),
                        )
                    })?
                    .into_owned();

                let depth = element_stack.len();
                if depth == 0 {
                    if root_element.is_some() || root_closed {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            "multiple root elements in response document",
                        ));
                    }
                    if local != "CMSearchResult" {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            format!("expected root element CMSearchResult, got {local}"),
                        ));
                    }
                    root_element = Some(local.clone());
                    root_closed = true;
                }
                if local == "CMSearchResult" && depth != 0 {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "CMSearchResult must be the document root",
                    ));
                }
                let capture =
                    field_for_start(&local, inside_match_item, depth == 1, &element_stack);

                if local == "searchMatchItem" && !inside_match_item {
                    inside_match_item = true;
                    item_track_id = FieldState::Absent;
                    item_start_time = FieldState::Absent;
                    item_end_time = FieldState::Absent;
                    item_content_type = FieldState::Absent;
                    item_codec_type = FieldState::Absent;
                    item_playback_uri = FieldState::Absent;
                    item_nvr_size = FieldState::Absent;
                    item_metadata.clear();
                }
                current_capture.clear();

                element_stack.push(local.clone());
                active_fields.push((depth + 1, capture));

                let popped_active = active_fields.pop();
                let popped_local = element_stack.pop();

                if let (Some(popped), Some((_, af))) = (popped_local, popped_active) {
                    let trimmed = current_capture.trim().to_string();
                    current_capture.clear();

                    if af != ActiveCaptureField::None
                        && !matches!(
                            af,
                            ActiveCaptureField::ItemTrackId
                                | ActiveCaptureField::ItemStartTime
                                | ActiveCaptureField::ItemEndTime
                                | ActiveCaptureField::ItemContentType
                                | ActiveCaptureField::ItemCodecType
                                | ActiveCaptureField::ItemPlaybackUri
                                | ActiveCaptureField::ItemNvrSize
                                | ActiveCaptureField::ItemMetadataDescriptor
                        )
                    {
                        set_doc_field(
                            &mut search_id_state,
                            &mut response_status_state,
                            &mut response_status_strg_state,
                            &mut num_of_matches_state,
                            af,
                            trimmed.clone(),
                        )?;
                    }

                    // Handle item-level fields (document-level fields are handled
                    // by set_doc_field above).
                    match af {
                        ActiveCaptureField::ItemTrackId => {
                            item_track_id = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemStartTime => {
                            item_start_time = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemEndTime => {
                            item_end_time = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemContentType => {
                            item_content_type = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemCodecType => {
                            item_codec_type = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemPlaybackUri => {
                            item_playback_uri = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemNvrSize => {
                            item_nvr_size = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemMetadataDescriptor => {
                            item_metadata.push(trimmed);
                        }
                        ActiveCaptureField::None
                        | ActiveCaptureField::SearchId
                        | ActiveCaptureField::ResponseStatus
                        | ActiveCaptureField::ResponseStatusStrg
                        | ActiveCaptureField::NumOfMatches => {}
                    }

                    if popped == "searchMatchItem" {
                        let idx = item_index;
                        item_index += 1;
                        inside_match_item = false;

                        let result = validate_search_match_item(
                            idx,
                            item_track_id.get_value(),
                            item_start_time.get_value(),
                            item_end_time.get_value(),
                            item_content_type.get_value(),
                            item_codec_type.get_value(),
                            item_playback_uri.get_value(),
                            item_nvr_size.get_value(),
                            std::mem::take(&mut item_metadata),
                        );
                        items.push(result);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let end_local = reader
                    .decoder()
                    .decode(e.name().local_name().as_ref())
                    .map_err(|err| {
                        AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_image_search_xml",
                            format!("character encoding error: {err}"),
                        )
                    })?
                    .into_owned();
                let expected_local = element_stack.last().ok_or_else(|| {
                    AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "unexpected closing element at document root level",
                    )
                })?;
                if expected_local != &end_local {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "mismatched XML closing element",
                    ));
                }

                let popped_active = active_fields.pop();
                let popped_local = element_stack.pop();

                if let (Some(popped), Some((_, af))) = (popped_local, popped_active) {
                    let trimmed = current_capture.trim().to_string();
                    current_capture.clear();

                    if popped == "CMSearchResult" {
                        root_closed = true;
                    }

                    // Handle document-level fields with presence tracking
                    if af != ActiveCaptureField::None {
                        set_doc_field(
                            &mut search_id_state,
                            &mut response_status_state,
                            &mut response_status_strg_state,
                            &mut num_of_matches_state,
                            af,
                            trimmed.clone(),
                        )?;
                    }

                    // Handle item-level fields (document-level fields are handled
                    // by set_doc_field above).
                    match af {
                        ActiveCaptureField::ItemTrackId => {
                            item_track_id = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemStartTime => {
                            item_start_time = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemEndTime => {
                            item_end_time = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemContentType => {
                            item_content_type = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemCodecType => {
                            item_codec_type = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemPlaybackUri => {
                            item_playback_uri = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemNvrSize => {
                            item_nvr_size = FieldState::Present(trimmed);
                        }
                        ActiveCaptureField::ItemMetadataDescriptor => {
                            item_metadata.push(trimmed);
                        }
                        ActiveCaptureField::None
                        | ActiveCaptureField::SearchId
                        | ActiveCaptureField::ResponseStatus
                        | ActiveCaptureField::ResponseStatusStrg
                        | ActiveCaptureField::NumOfMatches => {}
                    }

                    if popped == "searchMatchItem" {
                        let idx = item_index;
                        item_index += 1;
                        inside_match_item = false;

                        let result = validate_search_match_item(
                            idx,
                            item_track_id.get_value(),
                            item_start_time.get_value(),
                            item_end_time.get_value(),
                            item_content_type.get_value(),
                            item_codec_type.get_value(),
                            item_playback_uri.get_value(),
                            item_nvr_size.get_value(),
                            std::mem::take(&mut item_metadata),
                        );
                        items.push(result);
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().map_err(|e| {
                    AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        format!("entity decode error: {e}"),
                    )
                })?;
                if element_stack.is_empty() && !text.trim().is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "unexpected non-whitespace text outside response root",
                    ));
                }
                current_capture.push_str(&text);
            }
            Ok(Event::CData(e)) => {
                let cdata = reader.decoder().decode(e.as_ref()).map_err(|e| {
                    AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        format!("encoding error in CDATA: {e}"),
                    )
                })?;
                if element_stack.is_empty() && !cdata.trim().is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "unexpected non-whitespace CDATA outside response root",
                    ));
                }
                current_capture.push_str(&cdata);
            }
            Ok(Event::Comment(_)) => {}
            Ok(Event::Decl(_)) => {
                if xml_declaration_seen || doctype_seen || root_closed || !element_stack.is_empty()
                {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "XML declaration is not valid in this document position",
                    ));
                }
                xml_declaration_seen = true;
            }
            Ok(Event::DocType(_)) => {
                if doctype_seen || root_closed || !element_stack.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_image_search_xml",
                        "DOCTYPE is not valid in this document position",
                    ));
                }
                doctype_seen = true;
            }
            Err(e) => {
                return Err(AppError::new(
                    ErrorCategory::XmlParsing,
                    "parse_image_search_xml",
                    format!("malformed XML: {e}"),
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    // Validate the root element
    let root = root_element.as_deref().ok_or_else(|| {
        AppError::new(
            ErrorCategory::XmlParsing,
            "parse_image_search_xml",
            "no root element found in response document",
        )
    })?;
    if root != "CMSearchResult" {
        return Err(AppError::new(
            ErrorCategory::XmlParsing,
            "parse_image_search_xml",
            format!("expected root element CMSearchResult, got {root}"),
        ));
    }

    // Validate document-level fields — reject absent fields
    let response_status = match response_status_state {
        FieldState::Present(v) => v,
        FieldState::Absent => {
            return Err(AppError::new(
                ErrorCategory::InvalidNvrResponse,
                "parse_image_search_xml",
                "responseStatus field is missing from search response",
            ));
        }
    };

    let response_status_string = match response_status_strg_state {
        FieldState::Present(v) => v,
        FieldState::Absent => {
            return Err(AppError::new(
                ErrorCategory::InvalidNvrResponse,
                "parse_image_search_xml",
                "responseStatusStrg field is missing from search response",
            ));
        }
    };

    // Some firmware omits the echoed search ID. If supplied, however, it
    // must be a valid UUID matching this logical request.
    let resp_id = match search_id_state {
        FieldState::Present(id) => {
            if id != expected_search_id {
                return Err(AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "parse_image_search_xml",
                    "response searchID does not match request searchID",
                ));
            }
            Some(id)
        }
        FieldState::Absent => None,
    };

    // Validate responseStatus and status string
    if !response_status {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "parse_image_search_xml",
            format!("responseStatus=false with status string \"{response_status_string}\""),
        ));
    }

    match response_status_string.as_str() {
        // Hikvision commonly uses OK for a terminal successful page; retain
        // Success for firmware compatibility and MORE for pagination.
        "OK" | "Success" | "MORE" => {}
        "Failure" => {
            return Err(AppError::new(
                ErrorCategory::InvalidNvrResponse,
                "parse_image_search_xml",
                "NVR declared search failure",
            ));
        }
        other => {
            return Err(AppError::new(
                ErrorCategory::InvalidNvrResponse,
                "parse_image_search_xml",
                format!("unsupported responseStatusStrg: {other}"),
            ));
        }
    }

    let num_of_matches = match num_of_matches_state {
        FieldState::Present(v) => Some(v),
        FieldState::Absent => None,
    };

    let raw_item_count = items.len();
    Ok(SearchResponse {
        search_id: resp_id,
        response_status,
        response_status_string,
        num_of_matches,
        items,
        raw_item_count,
    })
}

/// Parse a search ID string, accepting optional surrounding braces.
fn parse_search_id(s: &str) -> Option<Uuid> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(trimmed);
    Uuid::parse_str(inner).ok()
}

/// Parse a boolean string ("true" or "1" → true, "false" or "0" → false).
fn parse_bool(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// Hash the ordered, decoded item fields for malformed-item repetition
/// detection. The field values are never exposed in diagnostics; only this
/// non-reversible hexadecimal digest is retained.
#[allow(clippy::too_many_arguments)]
fn malformed_item_fingerprint(
    track_id: Option<&str>,
    start_time: Option<&str>,
    end_time: Option<&str>,
    content_type: Option<&str>,
    codec_type: Option<&str>,
    playback_uri: Option<&str>,
    nvr_size: Option<&str>,
    metadata: &[String],
) -> String {
    let mut hasher = Sha256::new();

    fn field(hasher: &mut Sha256, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                hasher.update([1]);
                hasher.update((value.len() as u64).to_le_bytes());
                hasher.update(value);
            }
            None => hasher.update([0]),
        }
    }

    field(&mut hasher, track_id.map(str::as_bytes));
    field(&mut hasher, start_time.map(str::as_bytes));
    field(&mut hasher, end_time.map(str::as_bytes));
    field(&mut hasher, content_type.map(str::as_bytes));
    field(&mut hasher, codec_type.map(str::as_bytes));
    field(&mut hasher, playback_uri.map(str::as_bytes));
    // Hash the raw decoded text, not a parsed sentinel, so e.g. "abc" and
    // "def" cannot collapse into the same malformed-item fingerprint.
    field(&mut hasher, nvr_size.map(str::as_bytes));
    hasher.update((metadata.len() as u64).to_le_bytes());
    for descriptor in metadata {
        field(&mut hasher, Some(descriptor.as_bytes()));
    }

    format!("{:x}", hasher.finalize())
}

/// Validate a single searchMatchItem's required fields.
#[allow(clippy::too_many_arguments)]
fn validate_search_match_item(
    index: usize,
    track_id: Option<String>,
    start_time: Option<String>,
    end_time: Option<String>,
    content_type: Option<String>,
    codec_type: Option<String>,
    playback_uri: Option<String>,
    nvr_size: Option<String>,
    metadata: Vec<String>,
) -> ParsedMatchItem {
    let fingerprint = malformed_item_fingerprint(
        track_id.as_deref(),
        start_time.as_deref(),
        end_time.as_deref(),
        content_type.as_deref(),
        codec_type.as_deref(),
        playback_uri.as_deref(),
        nvr_size.as_deref(),
        &metadata,
    );

    let track_id = match track_id {
        Some(tid) if !tid.is_empty() => tid,
        _ => {
            return ParsedMatchItem::Malformed {
                index,
                reason: "missing or empty trackID".to_string(),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let start_time = match start_time {
        Some(st) if !st.is_empty() => st,
        _ => {
            return ParsedMatchItem::Malformed {
                index,
                reason: "missing or empty startTime".to_string(),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let content_type = match content_type {
        Some(ct) if !ct.is_empty() => ct,
        _ => {
            return ParsedMatchItem::Malformed {
                index,
                reason: "missing or empty contentType".to_string(),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let codec_type = match codec_type {
        Some(cd) if !cd.is_empty() => cd,
        _ => {
            return ParsedMatchItem::Malformed {
                index,
                reason: "missing or empty codecType".to_string(),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let playback_uri = match playback_uri {
        Some(uri) if !uri.is_empty() => uri,
        _ => {
            return ParsedMatchItem::Malformed {
                index,
                reason: "missing or empty playbackURI".to_string(),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let parsed_url = match url::Url::parse(&playback_uri) {
        Ok(u) => u,
        Err(_) => {
            return ParsedMatchItem::Malformed {
                index,
                reason: "invalid playbackURI".to_string(),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let scheme = parsed_url.scheme();
    if scheme != "http" && scheme != "https" {
        return ParsedMatchItem::Malformed {
            index,
            reason: format!("playbackURI scheme must be http or https, got {scheme}"),
            fingerprint: fingerprint.clone(),
        };
    }

    if parsed_url.host().is_none() {
        return ParsedMatchItem::Malformed {
            index,
            reason: "playbackURI must contain an absolute HTTP(S) host".to_string(),
            fingerprint: fingerprint.clone(),
        };
    }

    if !parsed_url.username().is_empty() || parsed_url.password().is_some() {
        return ParsedMatchItem::Malformed {
            index,
            reason: "playbackURI contains embedded credentials".to_string(),
            fingerprint: fingerprint.clone(),
        };
    }

    let capture_start_at = match start_time.parse::<Timestamp>() {
        Ok(ts) => ts,
        Err(_) => {
            return ParsedMatchItem::Malformed {
                index,
                reason: format!("invalid capture start time: {start_time}"),
                fingerprint: fingerprint.clone(),
            };
        }
    };

    let capture_end_at = match end_time {
        Some(et) if !et.is_empty() => match et.parse::<Timestamp>() {
            Ok(ts) => Some(ts),
            Err(_) => {
                return ParsedMatchItem::Malformed {
                    index,
                    reason: format!("invalid capture end time: {et}"),
                    fingerprint: fingerprint.clone(),
                };
            }
        },
        _ => None,
    };

    let nvr_reported_size = match nvr_size {
        None => None,
        Some(raw_size) => match raw_size.parse::<i64>() {
            Ok(size) if size >= 0 => Some(size),
            Ok(_) => {
                return ParsedMatchItem::Malformed {
                    index,
                    reason: "negative NVR reported size".to_string(),
                    fingerprint: fingerprint.clone(),
                };
            }
            Err(_) => {
                return ParsedMatchItem::Malformed {
                    index,
                    reason: "invalid NVR reported size".to_string(),
                    fingerprint: fingerprint.clone(),
                };
            }
        },
    };
    ParsedMatchItem::Valid(SearchMatch {
        track_id: TrackId::new(track_id),
        capture_start_at,
        capture_end_at,
        content_type,
        codec_type,
        playback_uri,
        nvr_reported_size,
        metadata_descriptors: metadata,
    })
}

// ── Media filtering ───────────────────────────────────────────────────────

fn is_accepted_picture(item: &SearchMatch, expected_track_id: &TrackId) -> bool {
    let is_picture = item.content_type.trim().eq_ignore_ascii_case("picture");
    let is_jpeg = item.codec_type.trim().eq_ignore_ascii_case("jpeg");
    let is_same_track = item.track_id.as_str() == expected_track_id.as_str();
    is_picture && is_jpeg && is_same_track
}

// ── Playback URI canonicalization ─────────────────────────────────────────

/// Produce the origin-independent playback identity component.
///
/// Only accepts absolute HTTP or HTTPS URLs.  Rejects embedded credentials
/// and non-HTTP(S) schemes.
///
/// Uses `Url::query()` presence rather than non-empty text when deciding
/// whether to append the query delimiter, so that `/path` and `/path?`
/// produce distinct canonical identities.
pub fn canonical_playback_path_and_query(playback_uri: &str) -> AppResult<String> {
    let url = url::Url::parse(playback_uri).map_err(|e| {
        AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "canonical_playback_path_and_query",
            format!("invalid playbackURI: {e}"),
        )
    })?;

    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "canonical_playback_path_and_query",
            format!(
                "playbackURI scheme must be http or https, got {}",
                url.scheme()
            ),
        ));
    }

    if url.host().is_none() {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "canonical_playback_path_and_query",
            "playbackURI must contain an absolute HTTP(S) host",
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "canonical_playback_path_and_query",
            "playbackURI must not contain embedded credentials",
        ));
    }

    // Use query().is_some() to distinguish no-query from empty-query.
    let path = url.path();
    let mut result = path.to_string();
    if url.query().is_some() {
        result.push('?');
        // Preserve the raw query string (which may include encoded characters).
        // If there's a query, use the original query text; otherwise empty.
        if let Some(q) = url.query() {
            result.push_str(q);
        }
    }
    Ok(result)
}

/// Build the configured NVR origin identity string.
///
/// Canonicalizes scheme to lowercase, normalizes hostname to lowercase,
/// and formats IPv6 addresses with brackets.
pub fn configured_nvr_identity(scheme: &str, host: &str, port: u16) -> String {
    let scheme = scheme.to_lowercase();
    let host = host.to_lowercase();
    let host_with_brackets = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    format!("{scheme}://{host_with_brackets}:{port}")
}

// ── Image key generation ──────────────────────────────────────────────────

/// Generate a stable SHA-256 database uniqueness key.
///
/// Formats capture time with full nanosecond precision so that fractional
/// timestamps produce distinct keys from whole-second timestamps.
pub fn compute_image_key(
    nvr_identity: &str,
    track_id: &TrackId,
    capture_start_at: &Timestamp,
    canonical_playback_uri: &str,
) -> ImageKey {
    let mut hasher = Sha256::new();
    hasher.update(nvr_identity.as_bytes());
    hasher.update(b"\n");
    hasher.update(track_id.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(
        capture_start_at
            .as_datetime()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            .as_bytes(),
    );
    hasher.update(b"\n");
    hasher.update(canonical_playback_uri.as_bytes());
    let digest = hasher.finalize();
    ImageKey::new(format!("{:x}", digest))
}

// ── Pagination helpers ────────────────────────────────────────────────────

/// Calculate the next pagination position with overflow protection.
fn next_search_position(
    current: u64,
    observed_items: usize,
    reported_count: Option<u64>,
    max_results: u64,
) -> AppResult<u64> {
    if observed_items == 0 {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "next_search_position",
            "MORE returned with zero results — pagination loop detected",
        ));
    }

    // The number of parsed match items is the safest offset. Some firmware
    // reports numOfMatches as zero even when it returned a non-empty page;
    // only in that demonstrably unusable case use the configured request
    // size, while still refusing a zero-progress fallback.
    let increment = if reported_count == Some(0) {
        max_results
    } else {
        observed_items as u64
    };
    if increment == 0 {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "next_search_position",
            "pagination fallback would not advance the result position",
        ));
    }

    let next = current.checked_add(increment).ok_or_else(|| {
        AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "next_search_position",
            "pagination position overflow",
        )
    })?;

    if next <= current {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "next_search_position",
            "pagination position did not increase",
        ));
    }

    Ok(next)
}

/// Build a page signature for repeated-page detection (excludes searchID).
///
/// Includes all ordered response content relevant to item identity and
/// pagination — playback URI, media fields, time span including
/// capture_end_at, size, metadata, and safe malformed-item distinctions —
/// while excluding only searchID.
///
/// capture_end_at is encoded with an unambiguous "None" marker so that
/// pages differing only in end time are not falsely rejected.
fn build_page_signature(items: &[ParsedMatchItem], status_string: &str) -> String {
    let mut hasher = Sha256::new();

    // Length-prefix every field. Newlines alone are not a sufficient framing
    // scheme because XML text can legally contain them, and an ambiguous
    // signature could make a real page look like a repeated page.
    fn field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    fn timestamp(ts: &Timestamp) -> String {
        ts.as_datetime()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    field(&mut hasher, status_string.as_bytes());
    for item in items {
        match item {
            ParsedMatchItem::Valid(m) => {
                field(&mut hasher, b"valid");
                field(&mut hasher, m.track_id.as_str().as_bytes());
                field(&mut hasher, timestamp(&m.capture_start_at).as_bytes());
                match m.capture_end_at {
                    Some(ts) => {
                        field(&mut hasher, b"end-present");
                        field(&mut hasher, timestamp(&ts).as_bytes());
                    }
                    None => field(&mut hasher, b"end-absent"),
                }
                field(&mut hasher, m.content_type.as_bytes());
                field(&mut hasher, m.codec_type.as_bytes());
                field(&mut hasher, m.playback_uri.as_bytes());
                match m.nvr_reported_size {
                    Some(size) => {
                        field(&mut hasher, b"size-present");
                        field(&mut hasher, &size.to_le_bytes());
                    }
                    None => field(&mut hasher, b"size-absent"),
                }
                field(
                    &mut hasher,
                    &(m.metadata_descriptors.len() as u64).to_le_bytes(),
                );
                for desc in &m.metadata_descriptors {
                    field(&mut hasher, desc.as_bytes());
                }
            }
            ParsedMatchItem::Malformed {
                index,
                reason,
                fingerprint,
            } => {
                field(&mut hasher, b"malformed");
                field(&mut hasher, &(*index as u64).to_le_bytes());
                field(&mut hasher, reason.as_bytes());
                field(&mut hasher, fingerprint.as_bytes());
            }
        }
    }
    let digest = hasher.finalize();
    format!("{:x}", digest)
}

// ── DiscoveredImage conversion ────────────────────────────────────────────

fn search_match_to_discovered(
    item: &SearchMatch,
    camera_id: CameraId,
    nvr_identity: &str,
    canonical_uri: &str,
) -> DiscoveredImage {
    let image_key = compute_image_key(
        nvr_identity,
        &item.track_id,
        &item.capture_start_at,
        canonical_uri,
    );
    let now = Timestamp::new(Utc::now());

    DiscoveredImage {
        image_key,
        camera_id,
        track_id: item.track_id.clone(),
        capture_start_at: item.capture_start_at,
        capture_end_at: item.capture_end_at,
        playback_uri: item.playback_uri.clone(),
        canonical_playback_uri: canonical_uri.to_string(),
        codec_type: Some(item.codec_type.clone()),
        content_type: Some(item.content_type.clone()),
        nvr_reported_size: item.nvr_reported_size,
        discovered_at: now,
    }
}

// ── ImageSearchClient ─────────────────────────────────────────────────────

/// A client for Hikvision image search operations.
pub struct ImageSearchClient<'a> {
    pub transport: &'a NvrTransport,
    pub database: DatabaseOps,
    pub search_config: NvrSearchConfig,
    pub nvr_identity: String,
    pub camera_id: CameraId,
    /// Picture track ID to filter search results against.
    pub picture_track: String,
}

/// Statistics for a completed one-window search operation.
#[derive(Debug, Clone)]
pub struct SearchWindowOutcome {
    pub pages_fetched: u64,
    pub records_found: u64,
    pub records_skipped: u64,
    pub records_inserted: u64,
}

impl<'a> ImageSearchClient<'a> {
    fn safe_context(&self, window: &SearchWindow, position: u64) -> String {
        format!(
            "camera_id={}, track_id={}, window=[{}, {}), position={}",
            self.camera_id, self.picture_track, window.start, window.end, position
        )
    }

    /// Execute a complete search for one window.
    ///
    /// On any failure (transport, parsing, NVR-status, pagination,
    /// conversion, or commit), records a safe cursor error and returns
    /// without committing discoveries or advancing the cursor.
    pub async fn search_one_window(&self, window: &SearchWindow) -> AppResult<SearchWindowOutcome> {
        match self.do_search_one_window(window).await {
            Ok(outcome) => Ok(outcome),
            Err(original_error) => {
                // Keep the durable diagnostic deliberately bounded: do not
                // copy parser messages or playback URLs into the cursor row.
                let error_msg = format!(
                    "image search failed: category={:?}, camera_id={}, track_id={}, window=[{}, {})",
                    original_error.category,
                    self.camera_id,
                    self.picture_track,
                    window.start,
                    window.end,
                );
                let now = Timestamp::new(Utc::now());
                match self
                    .database
                    .record_cursor_error(self.camera_id, &error_msg, &now)
                    .await
                {
                    Ok(()) => Err(original_error),
                    Err(cursor_error) => Err(AppError::with_source(
                        ErrorCategory::Database,
                        "search_one_window",
                        format!("failed to record image-search cursor error: {cursor_error}"),
                        original_error,
                    )),
                }
            }
        }
    }

    /// Internal search logic without cursor-error wrapping.
    async fn do_search_one_window(&self, window: &SearchWindow) -> AppResult<SearchWindowOutcome> {
        let mut position: u64 = 0;
        let mut pages_fetched: u64 = 0;
        let mut records_found: u64 = 0;
        let mut records_skipped: u64 = 0;
        let mut all_accepted_images: Vec<DiscoveredImage> = Vec::new();

        let mut seen_positions: HashSet<u64> = HashSet::new();
        let mut seen_page_signatures: BTreeSet<String> = BTreeSet::new();

        let searched_track = TrackId::new(&self.picture_track);

        loop {
            if !seen_positions.insert(position) {
                return Err(AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "search_one_window",
                    format!(
                        "{}: repeated pagination position",
                        self.safe_context(window, position)
                    ),
                ));
            }

            let doc = serialize_search_request(
                &searched_track,
                window,
                position,
                self.search_config.max_results,
            )?;

            // Execute the request — preserve the original error category.
            let response = self
                .transport
                .post("/ISAPI/ContentMgmt/search", doc.xml.clone())
                .await;

            // Map transport errors while preserving their category.
            let response = match response {
                Ok(resp) => resp,
                Err(e) => {
                    // Preserve the original category and HTTP status from
                    // the transport error, adding safe positional context.
                    let category = e.category;
                    let status = e.http_status();
                    let mut wrapped = AppError::with_source(
                        category,
                        "search_one_window",
                        format!(
                            "{}: NVR transport error",
                            self.safe_context(window, position)
                        ),
                        e,
                    );
                    if let Some(status) = status {
                        wrapped = wrapped.with_http_status(status);
                    }
                    return Err(wrapped);
                }
            };

            let body = response.bytes().await.map_err(|e| {
                let category = if e.is_timeout() {
                    ErrorCategory::Timeout
                } else {
                    ErrorCategory::Network
                };
                let mut error = AppError::new(
                    category,
                    "search_one_window",
                    format!(
                        "{}: failed to read NVR response",
                        self.safe_context(window, position)
                    ),
                );
                if let Some(status) = e.status() {
                    error = error.with_http_status(status.as_u16());
                }
                error
            })?;

            let parsed = parse_image_search_xml(&body, doc.search_id).map_err(|e| {
                // Preserve the parser's category and source chain while only
                // adding safe camera/track/window/position context.
                let category = e.category;
                AppError::with_source(
                    category,
                    "search_one_window",
                    format!(
                        "{}: NVR response error",
                        self.safe_context(window, position)
                    ),
                    e,
                )
            })?;

            pages_fetched += 1;

            let page_sig = build_page_signature(&parsed.items, &parsed.response_status_string);
            if seen_page_signatures.contains(&page_sig) {
                return Err(AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "search_one_window",
                    format!(
                        "{}: repeated page signature",
                        self.safe_context(window, position)
                    ),
                ));
            }
            seen_page_signatures.insert(page_sig);

            let total_items = parsed.raw_item_count;

            for item in parsed.items {
                match item {
                    ParsedMatchItem::Valid(match_item) => {
                        records_found += 1;

                        if !is_accepted_picture(&match_item, &searched_track) {
                            records_skipped += 1;
                            continue;
                        }

                        let canonical =
                            match canonical_playback_path_and_query(&match_item.playback_uri) {
                                Ok(c) => c,
                                Err(e) => {
                                    records_skipped += 1;
                                    tracing::warn!(
                                        camera = %self.camera_id,
                                        position = position,
                                        item_index = records_found,
                                        "skipping malformed playback URI: {e}"
                                    );
                                    continue;
                                }
                            };

                        let discovered = search_match_to_discovered(
                            &match_item,
                            self.camera_id,
                            &self.nvr_identity,
                            &canonical,
                        );
                        all_accepted_images.push(discovered);
                    }
                    ParsedMatchItem::Malformed { index, reason, .. } => {
                        records_skipped += 1;
                        tracing::warn!(
                            camera = %self.camera_id,
                            position = position,
                            item_index = index,
                            reason = %reason,
                            "skipping malformed match item"
                        );
                    }
                }
            }

            let is_more = parsed.response_status_string == "MORE";

            if !is_more {
                break;
            }

            if total_items == 0 {
                return Err(AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "search_one_window",
                    format!(
                        "{}: MORE returned with zero results — pagination loop detected",
                        self.safe_context(window, position)
                    ),
                ));
            }

            position = next_search_position(
                position,
                total_items,
                parsed.num_of_matches,
                self.search_config.max_results,
            )
            .map_err(|e| {
                AppError::new(
                    e.category,
                    "search_one_window",
                    format!("{}: pagination error", self.safe_context(window, position)),
                )
            })?;
        }

        // Commit all discoveries and cursor advancement atomically.
        let now = Timestamp::new(Utc::now());
        let window_commit = SearchWindowCommit {
            camera_id: self.camera_id,
            window_start: window.start,
            window_end: window.end,
            next_search_at: window.end,
            polled_at: now,
            updated_at: now,
        };

        let records_inserted = self
            .database
            .commit_search_window(&window_commit, &all_accepted_images)
            .await
            .map_err(|e| {
                // commit_search_window already rolled back the transaction.
                // Keep its database error as the source for diagnostics.
                AppError::with_source(
                    ErrorCategory::Database,
                    "search_one_window",
                    format!(
                        "{}: database commit error",
                        self.safe_context(window, position)
                    ),
                    e,
                )
            })?;

        Ok(SearchWindowOutcome {
            pages_fetched,
            records_found,
            records_skipped,
            records_inserted,
        })
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn ts(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> Timestamp {
        Timestamp::new(Utc.with_ymd_and_hms(y, m, d, h, min, s).unwrap())
    }

    fn ts_frac(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32, ns: u32) -> Timestamp {
        Timestamp::new(
            Utc.with_ymd_and_hms(y, m, d, h, min, s)
                .unwrap()
                .with_nanosecond(ns)
                .unwrap(),
        )
    }

    // ── Window generation tests ──────────────────────────────────────────

    #[test]
    fn generate_windows_contiguous_half_open() {
        let start = ts(2026, 7, 11, 0, 0, 0);
        let end = ts(2026, 7, 11, 3, 0, 0);
        let windows = generate_search_windows(start, end, 60).unwrap();
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].start, start);
        assert_eq!(windows[0].end, ts(2026, 7, 11, 1, 0, 0));
        assert_eq!(windows[1].start, ts(2026, 7, 11, 1, 0, 0));
        assert_eq!(windows[1].end, ts(2026, 7, 11, 2, 0, 0));
        assert_eq!(windows[2].start, ts(2026, 7, 11, 2, 0, 0));
        assert_eq!(windows[2].end, end);
    }

    #[test]
    fn generate_windows_truncates_final() {
        let start = ts(2026, 7, 11, 0, 0, 0);
        let end = ts(2026, 7, 11, 1, 30, 0);
        let windows = generate_search_windows(start, end, 60).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].end, end);
    }

    #[test]
    fn generate_windows_empty_when_start_equals_end() {
        let t = ts(2026, 7, 11, 0, 0, 0);
        let windows = generate_search_windows(t, t, 60).unwrap();
        assert!(windows.is_empty());
    }

    #[test]
    fn generate_windows_empty_when_start_after_end() {
        let start = ts(2026, 7, 11, 2, 0, 0);
        let end = ts(2026, 7, 11, 1, 0, 0);
        let windows = generate_search_windows(start, end, 60).unwrap();
        assert!(windows.is_empty());
    }

    #[test]
    fn generate_windows_rejects_zero_duration() {
        let result = generate_search_windows(ts(2026, 7, 11, 0, 0, 0), ts(2026, 7, 11, 1, 0, 0), 0);
        assert!(result.is_err());
    }

    // ── Serialization tests ──────────────────────────────────────────────

    #[test]
    fn serialize_contains_search_result_position_spelling() {
        let doc = serialize_search_request(
            &TrackId::new("103"),
            &SearchWindow {
                start: ts(2026, 7, 11, 0, 0, 0),
                end: ts(2026, 7, 11, 1, 0, 0),
            },
            0,
            50,
        )
        .unwrap();
        let xml = String::from_utf8_lossy(&doc.xml);
        assert!(xml.contains("<searchResultPostion>0</searchResultPostion>"));
        assert!(!xml.contains("<searchResultPosition>"));
    }

    #[test]
    fn serialize_emits_uuid_v4() {
        let doc = serialize_search_request(
            &TrackId::new("103"),
            &SearchWindow {
                start: ts(2026, 7, 11, 0, 0, 0),
                end: ts(2026, 7, 11, 1, 0, 0),
            },
            0,
            50,
        )
        .unwrap();
        let xml = String::from_utf8_lossy(&doc.xml);
        assert!(xml.contains("<searchID>"));
        let start = xml.find("<searchID>").unwrap() + 10;
        let end = xml[start..].find("</searchID>").unwrap();
        let uuid_str = &xml[start..start + end];
        let uuid: Uuid = uuid_str.parse().unwrap();
        assert_eq!(uuid.get_version(), Some(uuid::Version::Random));
    }

    #[test]
    fn serialize_two_pages_different_uuids() {
        let window = SearchWindow {
            start: ts(2026, 7, 11, 0, 0, 0),
            end: ts(2026, 7, 11, 1, 0, 0),
        };
        let doc1 = serialize_search_request(&TrackId::new("103"), &window, 0, 50).unwrap();
        let doc2 = serialize_search_request(&TrackId::new("103"), &window, 50, 50).unwrap();
        assert_ne!(doc1.search_id, doc2.search_id);
    }

    #[test]
    fn serialize_uses_utc_z_timestamps() {
        let doc = serialize_search_request(
            &TrackId::new("103"),
            &SearchWindow {
                start: ts(2026, 7, 11, 0, 0, 0),
                end: ts(2026, 7, 11, 1, 0, 0),
            },
            0,
            50,
        )
        .unwrap();
        let xml = String::from_utf8_lossy(&doc.xml);
        assert!(xml.contains("2026-07-11T00:00:00Z"));
        assert!(xml.contains("2026-07-11T01:00:00Z"));
    }

    #[test]
    fn serialize_uses_fractional_precision() {
        let window = SearchWindow {
            start: ts_frac(2026, 7, 11, 0, 0, 0, 123_456_789),
            end: ts_frac(2026, 7, 11, 1, 0, 0, 987_654_321),
        };
        let doc = serialize_search_request(&TrackId::new("103"), &window, 0, 50).unwrap();
        let xml = String::from_utf8_lossy(&doc.xml);
        // Should contain fractional seconds
        assert!(xml.contains(".123456789Z"));
        assert!(xml.contains(".987654321Z"));
    }

    #[test]
    fn serialize_contains_required_elements() {
        let doc = serialize_search_request(
            &TrackId::new("103"),
            &SearchWindow {
                start: ts(2026, 7, 11, 0, 0, 0),
                end: ts(2026, 7, 11, 1, 0, 0),
            },
            0,
            50,
        )
        .unwrap();
        let xml = String::from_utf8_lossy(&doc.xml);
        assert!(xml.contains("<CMSearchDescription>"));
        assert!(xml.contains("<searchID>"));
        assert!(xml.contains("<trackID>103</trackID>"));
        assert!(xml.contains("<maxResults>50</maxResults>"));
        assert!(xml.contains("<searchResultPostion>0</searchResultPostion>"));
        assert!(xml.contains(
            "<metadataDescriptor>//recordType.meta.std-cgi.com/allPic</metadataDescriptor>"
        ));
    }

    // ── Parsing tests ────────────────────────────────────────────────────

    fn default_search_response(search_id: Uuid) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{search_id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>1</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/picture/1.jpg</playbackURI>
    <size>12345</size>
  </searchMatchItem>
</CMSearchResult>"#,
            search_id = search_id,
        )
    }

    #[test]
    fn parse_default_namespace_response() {
        let id = Uuid::new_v4();
        let xml = default_search_response(id).into_bytes();
        let result = parse_image_search_xml(&xml, id).unwrap();
        assert_eq!(result.search_id, Some(id));
        assert!(result.response_status);
        assert_eq!(result.response_status_string, "Success");
        assert_eq!(result.num_of_matches, Some(1));
        assert_eq!(result.raw_item_count, 1);
        match &result.items[0] {
            ParsedMatchItem::Valid(m) => {
                assert_eq!(m.track_id.as_str(), "103");
                assert_eq!(m.content_type, "picture");
                assert_eq!(m.codec_type, "jpeg");
                assert_eq!(m.playback_uri, "http://nvr/picture/1.jpg");
                assert_eq!(m.nvr_reported_size, Some(12345));
            }
            _ => panic!("expected valid item"),
        }
    }

    #[test]
    fn parse_ok_terminal_status() {
        let id = Uuid::new_v4();
        let xml = default_search_response(id).replace(
            "<responseStatusStrg>Success</responseStatusStrg>",
            "<responseStatusStrg>OK</responseStatusStrg>",
        );
        let response = parse_image_search_xml(xml.as_bytes(), id).unwrap();
        assert_eq!(response.response_status_string, "OK");
        assert_eq!(response.raw_item_count, 1);
    }

    #[test]
    fn parse_prefixed_namespace_response() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<h:CMSearchResult xmlns:h="http://www.hikvision.com/ver20/XMLSchema">
  <h:searchID>{id}</h:searchID>
  <h:responseStatus>true</h:responseStatus>
  <h:responseStatusStrg>Success</h:responseStatusStrg>
  <h:numOfMatches>1</h:numOfMatches>
  <h:searchMatchItem>
    <h:trackID>103</h:trackID>
    <h:timeSpan>
      <h:startTime>2026-07-11T02:00:00Z</h:startTime>
      <h:endTime>2026-07-11T02:00:01Z</h:endTime>
    </h:timeSpan>
    <h:contentType>picture</h:contentType>
    <h:codecType>jpeg</h:codecType>
    <h:playbackURI>http://nvr/picture/1.jpg</h:playbackURI>
    <h:size>12345</h:size>
  </h:searchMatchItem>
</h:CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id).unwrap();
        assert_eq!(result.search_id, Some(id));
        assert!(result.response_status);
        match &result.items[0] {
            ParsedMatchItem::Valid(m) => {
                assert_eq!(m.track_id.as_str(), "103");
            }
            _ => panic!("expected valid item"),
        }
    }

    #[test]
    fn parse_no_namespace_response() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>1</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/picture/1.jpg</playbackURI>
  </searchMatchItem>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id).unwrap();
        assert_eq!(result.search_id, Some(id));
    }

    #[test]
    fn parse_braced_search_id() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{{{id}}}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id).unwrap();
        assert_eq!(result.search_id, Some(id));
    }

    #[test]
    fn parse_rejects_mismatched_search_id() {
        let id = Uuid::new_v4();
        let wrong_id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, wrong_id);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_allows_missing_search_id() {
        let xml = b"<?xml version=\"1.0\"?><CMSearchResult><responseStatus>true</responseStatus><responseStatusStrg>Success</responseStatusStrg></CMSearchResult>";
        let id = Uuid::new_v4();
        let result = parse_image_search_xml(xml.as_slice(), id).unwrap();
        assert_eq!(result.search_id, None);
    }

    #[test]
    fn parse_rejects_present_invalid_search_id() {
        let xml = b"<?xml version=\"1.0\"?><CMSearchResult><searchID>not-a-uuid</searchID><responseStatus>true</responseStatus><responseStatusStrg>Success</responseStatusStrg></CMSearchResult>";
        let id = Uuid::new_v4();
        let result = parse_image_search_xml(xml.as_slice(), id);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_rejects_present_invalid_response_status() {
        let xml = b"<?xml version=\"1.0\"?><CMSearchResult><searchID>00000000-0000-0000-0000-000000000000</searchID><responseStatus>not-a-bool</responseStatus><responseStatusStrg>Success</responseStatusStrg></CMSearchResult>";
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap();
        let result = parse_image_search_xml(xml.as_slice(), id);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_rejects_present_invalid_num_of_matches() {
        let xml = b"<?xml version=\"1.0\"?><CMSearchResult><searchID>00000000-0000-0000-0000-000000000000</searchID><responseStatus>true</responseStatus><responseStatusStrg>Success</responseStatusStrg><numOfMatches>not-a-number</numOfMatches></CMSearchResult>";
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap();
        let result = parse_image_search_xml(xml.as_slice(), id);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_rejects_page_fields_hidden_in_unknown_wrapper() {
        let xml = b"<CMSearchResult><unknown><responseStatus>true</responseStatus><responseStatusStrg>Success</responseStatusStrg></unknown></CMSearchResult>";
        let result = parse_image_search_xml(xml, Uuid::new_v4());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_invalid_item_size_is_local_malformed_result() {
        let id = Uuid::new_v4();
        let xml = format!(
            "<CMSearchResult><searchID>{id}</searchID><responseStatus>true</responseStatus><responseStatusStrg>Success</responseStatusStrg><searchMatchItem><trackID>103</trackID><timeSpan><startTime>2026-07-11T02:00:00Z</startTime></timeSpan><contentType>picture</contentType><codecType>jpeg</codecType><playbackURI>http://nvr/pic.jpg</playbackURI><size>not-a-number</size></searchMatchItem></CMSearchResult>"
        );
        let response = parse_image_search_xml(xml.as_bytes(), id).unwrap();
        assert_eq!(response.raw_item_count, 1);
        assert!(matches!(
            response.items.as_slice(),
            [ParsedMatchItem::Malformed { .. }]
        ));
    }

    #[test]
    fn parse_rejects_duplicate_document_fields() {
        let xml = b"<CMSearchResult><responseStatus>true</responseStatus><responseStatus>false</responseStatus><responseStatusStrg>Success</responseStatusStrg></CMSearchResult>";
        let result = parse_image_search_xml(xml, Uuid::new_v4());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_nvr_failure_rejected() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Failure</responseStatusStrg>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn parse_missing_status_field_rejected() {
        let xml = b"<?xml version=\"1.0\"?><CMSearchResult></CMSearchResult>";
        let id = Uuid::new_v4();
        let result = parse_image_search_xml(xml.as_slice(), id);
        assert!(result.is_err());
    }

    #[test]
    fn parse_malformed_xml_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><CMSearchResult><broken>";
        let id = Uuid::new_v4();
        let result = parse_image_search_xml(xml.as_slice(), id);
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_xml_returns_error() {
        let id = Uuid::new_v4();
        let result = parse_image_search_xml(b"", id);
        assert!(result.is_err());
    }

    #[test]
    fn parse_malformed_item_retains_valid_siblings() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>2</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/1.jpg</playbackURI>
  </searchMatchItem>
  <searchMatchItem>
    <timeSpan>
      <startTime>2026-07-11T03:00:00Z</startTime>
      <endTime>2026-07-11T03:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/2.jpg</playbackURI>
  </searchMatchItem>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id).unwrap();
        assert_eq!(result.raw_item_count, 2);
        match &result.items[0] {
            ParsedMatchItem::Valid(_) => {}
            _ => panic!("expected valid first item"),
        }
        match &result.items[1] {
            ParsedMatchItem::Malformed { reason, .. } => {
                assert!(reason.contains("trackID"));
            }
            _ => panic!("expected malformed second item"),
        }
    }

    #[test]
    fn parse_xml_entity_decoding() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>1</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/1.jpg&amp;param=1</playbackURI>
  </searchMatchItem>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id).unwrap();
        match &result.items[0] {
            ParsedMatchItem::Valid(m) => {
                assert!(m.playback_uri.contains("&"));
            }
            _ => panic!("expected valid item"),
        }
    }

    #[test]
    fn parse_with_metadata_descriptors() {
        let id = Uuid::new_v4();
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<CMSearchResult>
  <searchID>{id}</searchID>
  <responseStatus>true</responseStatus>
  <responseStatusStrg>Success</responseStatusStrg>
  <numOfMatches>1</numOfMatches>
  <searchMatchItem>
    <trackID>103</trackID>
    <timeSpan>
      <startTime>2026-07-11T02:00:00Z</startTime>
      <endTime>2026-07-11T02:00:01Z</endTime>
    </timeSpan>
    <contentType>picture</contentType>
    <codecType>jpeg</codecType>
    <playbackURI>http://nvr/pic/1.jpg</playbackURI>
    <metadataList>
      <metadataDescriptor>desc1</metadataDescriptor>
      <metadataDescriptor>desc2</metadataDescriptor>
    </metadataList>
  </searchMatchItem>
</CMSearchResult>"#,
            id = id,
        )
        .into_bytes();

        let result = parse_image_search_xml(&xml, id).unwrap();
        match &result.items[0] {
            ParsedMatchItem::Valid(m) => {
                assert_eq!(m.metadata_descriptors.len(), 2);
                assert_eq!(m.metadata_descriptors[0], "desc1");
                assert_eq!(m.metadata_descriptors[1], "desc2");
            }
            _ => panic!("expected valid item"),
        }
    }

    // ── Filtering tests ──────────────────────────────────────────────────

    fn make_match(track: &str, ct: &str, codec: &str) -> SearchMatch {
        SearchMatch {
            track_id: TrackId::new(track),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: None,
            content_type: ct.to_string(),
            codec_type: codec.to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        }
    }

    #[test]
    fn filter_accepts_picture_jpeg() {
        let m = make_match("103", "picture", "jpeg");
        assert!(is_accepted_picture(&m, &TrackId::new("103")));
    }

    #[test]
    fn filter_rejects_video() {
        let m = make_match("103", "video", "jpeg");
        assert!(!is_accepted_picture(&m, &TrackId::new("103")));
    }

    #[test]
    fn filter_rejects_non_jpeg_codec() {
        let m = make_match("103", "picture", "h264");
        assert!(!is_accepted_picture(&m, &TrackId::new("103")));
    }

    #[test]
    fn filter_case_insensitive() {
        let m = make_match("103", "Picture", "JPEG");
        assert!(is_accepted_picture(&m, &TrackId::new("103")));
    }

    #[test]
    fn filter_wrong_track() {
        let m = make_match("203", "picture", "jpeg");
        assert!(!is_accepted_picture(&m, &TrackId::new("103")));
    }

    // ── Canonicalization tests ───────────────────────────────────────────

    #[test]
    fn canonicalization_removes_origin() {
        let result =
            canonical_playback_path_and_query("http://192.168.1.50:8080/picture/1.jpg").unwrap();
        assert_eq!(result, "/picture/1.jpg");
    }

    #[test]
    fn canonicalization_preserves_query() {
        let result = canonical_playback_path_and_query(
            "http://nvr/pic/1.jpg?starttime=20260711T020000Z&endtime=20260711T020001Z",
        )
        .unwrap();
        assert_eq!(
            result,
            "/pic/1.jpg?starttime=20260711T020000Z&endtime=20260711T020001Z"
        );
    }

    #[test]
    fn canonicalization_rejects_credentials() {
        let result = canonical_playback_path_and_query("http://user:pass@nvr/pic/1.jpg");
        assert!(result.is_err());
    }

    #[test]
    fn canonicalization_rejects_invalid_url() {
        let result = canonical_playback_path_and_query("not-a-url");
        assert!(result.is_err());
    }

    #[test]
    fn canonicalization_preserves_query_encoding() {
        let result = canonical_playback_path_and_query("http://nvr/pic/1.jpg?param=a%26b").unwrap();
        assert_eq!(result, "/pic/1.jpg?param=a%26b");
    }

    #[test]
    fn canonicalization_removes_fragment() {
        let result = canonical_playback_path_and_query("http://nvr/pic/1.jpg#section").unwrap();
        assert_eq!(result, "/pic/1.jpg");
    }

    #[test]
    fn canonicalization_distinguishes_no_query_from_empty_query() {
        // No query → /pic/1.jpg
        let no_query = canonical_playback_path_and_query("http://nvr/pic/1.jpg").unwrap();
        assert_eq!(no_query, "/pic/1.jpg");

        // Empty query → /pic/1.jpg?
        let empty_query = canonical_playback_path_and_query("http://nvr/pic/1.jpg?").unwrap();
        assert_eq!(empty_query, "/pic/1.jpg?");

        assert_ne!(no_query, empty_query);
    }

    // ── Image key tests ──────────────────────────────────────────────────

    #[test]
    fn image_key_is_deterministic() {
        let nvr_id = configured_nvr_identity("http", "192.168.1.50", 8080);
        let key1 = compute_image_key(
            &nvr_id,
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key2 = compute_image_key(
            &nvr_id,
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        assert_eq!(key1, key2);
    }

    #[test]
    fn image_key_is_64_hex_chars() {
        let key = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        assert_eq!(key.as_str().len(), 64);
        assert!(key.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        // SHA-256 hex output from {:x} is always lowercase
        let lower = key.as_str().to_lowercase();
        assert_eq!(key.as_str(), lower);
    }

    #[test]
    fn image_key_changes_with_nvr_identity() {
        let key1 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key2 = compute_image_key(
            "http://192.168.1.51:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        assert_ne!(key1, key2);
    }

    #[test]
    fn image_key_changes_with_track() {
        let key1 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key2 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("203"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        assert_ne!(key1, key2);
    }

    #[test]
    fn image_key_changes_with_timestamp() {
        let key1 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key2 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 1),
            "/pic/1.jpg",
        );
        assert_ne!(key1, key2);
    }

    #[test]
    fn image_key_changes_with_playback_uri() {
        let key1 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key2 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/2.jpg",
        );
        assert_ne!(key1, key2);
    }

    #[test]
    fn image_key_distinguishes_no_query_from_empty_query() {
        let key_no_query = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key_empty_query = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg?",
        );
        assert_ne!(key_no_query, key_empty_query);
    }

    #[test]
    fn image_key_distinguishes_different_end_times() {
        let key1 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 0),
            "/pic/1.jpg",
        );
        let key2 = compute_image_key(
            "http://192.168.1.50:8080",
            &TrackId::new("103"),
            &ts(2026, 7, 11, 2, 0, 1),
            "/pic/1.jpg",
        );
        assert_ne!(key1, key2);
    }

    // ── Pagination tests ─────────────────────────────────────────────────

    #[test]
    fn pagination_advances_by_observed_count() {
        let next = next_search_position(0, 50, Some(100), 50).unwrap();
        assert_eq!(next, 50);
    }

    #[test]
    fn pagination_rejects_zero_results() {
        let result = next_search_position(0, 0, Some(50), 50);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().category,
            ErrorCategory::InvalidNvrResponse
        );
    }

    #[test]
    fn pagination_uses_fallback_when_no_reported_count() {
        let next = next_search_position(0, 50, None, 50).unwrap();
        assert_eq!(next, 50);
    }

    #[test]
    fn pagination_rejects_overflow() {
        let result = next_search_position(u64::MAX, 1, None, 50);
        assert!(result.is_err());
    }

    #[test]
    fn pagination_rejects_non_increasing() {
        let result = next_search_position(50, 0, None, 50);
        assert!(result.is_err());
    }

    #[test]
    fn page_signature_excludes_search_id() {
        let items = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: None,
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let sig1 = build_page_signature(&items, "Success");
        let sig2 = build_page_signature(&items, "Success");
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn malformed_page_signature_includes_raw_item_fingerprint() {
        let malformed_one = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            None,
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("not-a-playback-uri-one".to_string()),
            None,
            Vec::new(),
        );
        let malformed_two = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            None,
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("not-a-playback-uri-two".to_string()),
            None,
            Vec::new(),
        );

        let (
            ParsedMatchItem::Malformed {
                reason: reason_one, ..
            },
            ParsedMatchItem::Malformed {
                reason: reason_two, ..
            },
        ) = (&malformed_one, &malformed_two)
        else {
            panic!("expected malformed items");
        };
        assert_eq!(reason_one, reason_two);
        assert_ne!(
            build_page_signature(&[malformed_one], "MORE"),
            build_page_signature(&[malformed_two], "MORE"),
            "malformed items with different raw content must not look repeated"
        );
    }

    #[test]
    fn malformed_size_fingerprint_preserves_raw_distinctions_without_diagnostics() {
        let malformed_one = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            None,
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://nvr/pic/1.jpg".to_string()),
            Some("abc".to_string()),
            Vec::new(),
        );
        let malformed_two = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            None,
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://nvr/pic/1.jpg".to_string()),
            Some("def".to_string()),
            Vec::new(),
        );

        let (
            ParsedMatchItem::Malformed {
                reason: reason_one, ..
            },
            ParsedMatchItem::Malformed {
                reason: reason_two, ..
            },
        ) = (&malformed_one, &malformed_two)
        else {
            panic!("expected malformed items");
        };
        assert_eq!(reason_one, reason_two);
        assert!(!reason_one.contains("abc"));
        assert!(!reason_one.contains("def"));
        assert_ne!(
            build_page_signature(&[malformed_one], "MORE"),
            build_page_signature(&[malformed_two], "MORE")
        );
    }

    #[test]
    fn page_signature_differs_for_different_items() {
        let items1 = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: None,
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let items2 = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 3, 0, 0),
            capture_end_at: None,
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/2.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let sig1 = build_page_signature(&items1, "Success");
        let sig2 = build_page_signature(&items2, "Success");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn page_signature_distinguishes_different_end_times() {
        let items1 = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: Some(ts(2026, 7, 11, 2, 0, 1)),
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let items2 = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: Some(ts(2026, 7, 11, 2, 0, 2)),
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let sig1 = build_page_signature(&items1, "Success");
        let sig2 = build_page_signature(&items2, "Success");
        assert_ne!(
            sig1, sig2,
            "signatures should differ when only end time differs"
        );
    }

    #[test]
    fn page_signature_distinguishes_none_vs_some_end_time() {
        let items1 = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: None,
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let items2 = vec![ParsedMatchItem::Valid(SearchMatch {
            track_id: TrackId::new("103"),
            capture_start_at: ts(2026, 7, 11, 2, 0, 0),
            capture_end_at: Some(ts(2026, 7, 11, 2, 0, 1)),
            content_type: "picture".to_string(),
            codec_type: "jpeg".to_string(),
            playback_uri: "http://nvr/pic/1.jpg".to_string(),
            nvr_reported_size: None,
            metadata_descriptors: Vec::new(),
        })];
        let sig1 = build_page_signature(&items1, "Success");
        let sig2 = build_page_signature(&items2, "Success");
        assert_ne!(
            sig1, sig2,
            "signatures should differ when end time is None vs Some"
        );
    }

    // ── NVR identity tests ───────────────────────────────────────────────

    #[test]
    fn nvr_identity_normalizes_ipv6() {
        let identity = configured_nvr_identity("http", "::1", 8080);
        assert_eq!(identity, "http://[::1]:8080");
    }

    #[test]
    fn nvr_identity_normalizes_ipv4() {
        let identity = configured_nvr_identity("http", "192.168.1.50", 8080);
        assert_eq!(identity, "http://192.168.1.50:8080");
    }

    #[test]
    fn nvr_identity_normalizes_hostname() {
        let identity = configured_nvr_identity("https", "pigate", 443);
        assert_eq!(identity, "https://pigate:443");
    }

    // ── Malformed item validation tests ──────────────────────────────────

    #[test]
    fn malformed_item_missing_track_id() {
        let result = validate_search_match_item(
            0,
            None,
            Some("2026-07-11T02:00:00Z".to_string()),
            Some("2026-07-11T02:00:01Z".to_string()),
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://nvr/pic/1.jpg".to_string()),
            None,
            Vec::new(),
        );
        match result {
            ParsedMatchItem::Malformed { reason, .. } => {
                assert!(reason.contains("trackID"));
            }
            _ => panic!("expected malformed"),
        }
    }

    #[test]
    fn malformed_item_invalid_timestamp() {
        let result = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("not-a-timestamp".to_string()),
            Some("2026-07-11T02:00:01Z".to_string()),
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://nvr/pic/1.jpg".to_string()),
            None,
            Vec::new(),
        );
        match result {
            ParsedMatchItem::Malformed { reason, .. } => {
                assert!(reason.contains("time"));
            }
            _ => panic!("expected malformed"),
        }
    }

    #[test]
    fn malformed_item_invalid_playback_uri() {
        let result = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            Some("2026-07-11T02:00:01Z".to_string()),
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("not-a-url".to_string()),
            None,
            Vec::new(),
        );
        match result {
            ParsedMatchItem::Malformed { reason, .. } => {
                assert!(reason.contains("playbackURI"));
            }
            _ => panic!("expected malformed"),
        }
    }

    #[test]
    fn malformed_item_credentials_in_uri() {
        let result = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            Some("2026-07-11T02:00:01Z".to_string()),
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://user:pass@nvr/pic/1.jpg".to_string()),
            None,
            Vec::new(),
        );
        match result {
            ParsedMatchItem::Malformed { reason, .. } => {
                assert!(reason.contains("credentials"));
            }
            _ => panic!("expected malformed"),
        }
    }

    #[test]
    fn malformed_item_negative_size() {
        let result = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            Some("2026-07-11T02:00:01Z".to_string()),
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://nvr/pic/1.jpg".to_string()),
            Some("-1".to_string()),
            Vec::new(),
        );
        match result {
            ParsedMatchItem::Malformed { reason, .. } => {
                assert!(reason.contains("size"));
            }
            _ => panic!("expected malformed"),
        }
    }

    #[test]
    fn valid_item_with_optional_end_time() {
        let result = validate_search_match_item(
            0,
            Some("103".to_string()),
            Some("2026-07-11T02:00:00Z".to_string()),
            Some("2026-07-11T02:00:01Z".to_string()),
            Some("picture".to_string()),
            Some("jpeg".to_string()),
            Some("http://nvr/pic/1.jpg".to_string()),
            Some("12345".to_string()),
            vec!["desc1".to_string()],
        );
        match result {
            ParsedMatchItem::Valid(m) => {
                assert_eq!(m.capture_end_at.unwrap().as_datetime().hour(), 2);
                assert_eq!(m.nvr_reported_size, Some(12345));
                assert_eq!(m.metadata_descriptors.len(), 1);
            }
            _ => panic!("expected valid"),
        }
    }
}
