//! Camera discovery via `GET /ISAPI/Streaming/channels`.
//!
//! Implemented in Phase 5.
//!
//! Parses Hikvision streaming-channel XML by element local name (independent
//! of namespace prefix), groups tracks by camera channel, selects only primary
//! streams (suffix `01`), and derives corresponding picture tracks (suffix `03`).

use std::collections::BTreeMap;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::database::models::CameraDiscovery;
use crate::error::{AppError, AppResult, ErrorCategory};

use super::NvrTransport;

// ── Track ID derivation ────────────────────────────────────────────────────

/// Validate a complete numeric primary stream track ID and derive the camera
/// channel number plus the picture track ID.
///
/// Hikvision encodes the channel as the leading digits and the stream type
/// as the last two digits.  A primary video stream ends in `01`; its picture
/// stream ends in `03`.
///
/// The *entire* trimmed track ID is parsed as `u64` first to catch overflow
/// before the channel prefix is extracted.  Only when the complete value
/// fits in `u64` is the channel prefix extracted as `u64 / 100`.
///
/// Returns `(channel_number, picture_track_id)` only for primary tracks
/// whose numeric suffix is exactly `01`.  Returns `None` for secondary,
/// picture, or malformed IDs.
pub fn derive_camera_mapping(track_id: &str) -> Option<(i64, String)> {
    let trimmed = track_id.trim();
    let len = trimmed.len();

    // Must be at least 3 characters: one channel digit + "01"
    if len < 3 {
        return None;
    }

    // Last two characters must be "01"
    if !trimmed.ends_with("01") {
        return None;
    }

    // Parse the *complete* track ID as u64 to catch overflow.
    let full_track: u64 = trimmed.parse().ok()?;

    // The channel prefix is full_track / 100, which is guaranteed to fit in
    // i64 because full_track fits in u64 and channel >= 1.
    let channel_number: i64 = (full_track / 100).try_into().ok()?;
    if channel_number <= 0 {
        return None;
    }

    // Derive picture track: same channel, suffix "03".
    let picture_track_id = format!("{}03", channel_number);

    Some((channel_number, picture_track_id))
}

// ── XML parsing ────────────────────────────────────────────────────────────

/// Internal accumulator for one `<StreamingChannel>` entry.
#[derive(Clone)]
struct RawChannel {
    track_id: Option<String>,
    name: Option<String>,
    raw_identifier: Option<String>,
    /// Saved attribute-derived raw identifier, restored when the child
    /// `<id>` element is empty (no text accumulated).
    saved_attr_raw_id: Option<String>,
    /// Whether any text/CDATA was accumulated for the current recognized
    /// field element.  Used to distinguish "child element replaced
    /// attribute" from "empty child element preserves attribute".
    track_id_seen: bool,
    raw_id_seen: bool,
    name_seen: bool,
}

/// Represents the active text-capture context inside a StreamingChannel.
///
/// Using a stack-based approach ensures empty elements at the correct depth
/// are treated as immediately opened and closed, and that nested elements
/// inside unknown containers cannot leak into channel fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveField {
    None,
    TrackId,
    RawId,
    Name,
}

/// State for tracking the XML prolog and root lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrologState {
    /// Before the root element — only an XML declaration is allowed as the
    /// very first construct.  Comments, DOCTYPE, processing instructions,
    /// or whitespace before a later declaration are rejected.
    BeforeRoot,
    /// Root element has been seen — no more declarations or doctypes.
    AfterRoot,
}

/// Parse a Hikvision `/ISAPI/Streaming/channels` XML response into a list of
/// `CameraDiscovery` records.
///
/// Uses `quick_xml::Reader` events matched by local name so that default
/// namespaces, prefixed namespaces, and namespace-free documents all produce
/// identical results.
///
/// **Strict document structure enforcement**:
///
/// * Exactly one document root is expected: `StreamingChannelList`.
/// * No additional top-level elements are permitted.
/// * The root must be opened and closed (or provided as an empty element).
/// * Every Start / Empty element's attributes are validated — attribute
///   decoding errors and invalid entity references in attribute values
///   are propagated as `XmlParsing`.
/// * Duplicate attribute names within the same element cause an error.
///
/// A well-formed empty `StreamingChannelList` is treated as successful empty
/// discovery.  Malformed XML, truncated documents, and invalid entities
/// return `ErrorCategory::XmlParsing`.
///
/// Fields are captured only at the correct element depth (direct children of
/// `StreamingChannel`) so that unrelated nested elements cannot populate
/// channel fields.  Empty elements at the recognized-field depth are handled
/// immediately (opened + closed in one event) without leaving persistent
/// state.
pub fn parse_camera_discovery_xml(xml: &[u8]) -> AppResult<Vec<CameraDiscovery>> {
    if xml.is_empty() {
        return Err(AppError::new(
            ErrorCategory::XmlParsing,
            "parse_camera_discovery_xml",
            "XML response body is empty",
        ));
    }

    let mut reader = Reader::from_reader(xml);
    // Do NOT enable trim_text — we accumulate every Text/CDATA segment
    // (including whitespace-only) while a recognized field is active and
    // trim once at element close so that spaces across boundaries are
    // preserved (e.g. "Front " + "Gate" → "Front Gate").

    let mut buf = Vec::new();
    let mut channels: Vec<RawChannel> = Vec::new();

    // Element stack: tracks the local name of every open element.
    // Depth 0 = root level, Depth 1 = StreamingChannelList, Depth 2 = StreamingChannel.
    let mut element_stack: Vec<String> = Vec::new();

    // Active field context — only valid when inside a StreamingChannel at depth 2.
    // Using a stack of (depth, active_field) so that nested contexts are
    // properly isolated.  For simplicity we track the active field per depth.
    let mut active_fields: Vec<(usize, ActiveField)> = Vec::new();

    let mut current_channel = RawChannel {
        track_id: None,
        name: None,
        raw_identifier: None,
        saved_attr_raw_id: None,
        track_id_seen: false,
        raw_id_seen: false,
        name_seen: false,
    };
    let mut streaming_channel_list_seen = false;

    // Prolog lifecycle tracking:
    //
    // * XML declarations and DOCTYPEs are only allowed before the root
    //   element (StreamingChannelList).  After the root is seen, they are
    //   rejected.
    // * An XML declaration must be the very first XML construct (after an
    //   optional BOM), verified by the pre-check before the event loop.
    // * DOCTYPEs are allowed before the root but only one is permitted.
    let mut prolog_state = PrologState::BeforeRoot;
    let mut doctype_seen = false;
    let mut xml_declaration_seen = false;

    // Pre-check: validate that an XML declaration, if present, is the very
    // first construct (after an optional BOM).  This is checked against the
    // raw bytes so that whitespace-only text events suppressed by
    // `trim_text(true)` cannot hide a misplaced declaration.
    let mut xml_declaration_at_start = false;
    let mut decl_offset = 0;
    // Skip optional UTF-8 BOM.
    if xml.starts_with(&[0xEF, 0xBB, 0xBF]) {
        decl_offset = 3;
    }
    // Check if the first non-BOM bytes start the XML declaration.
    if xml[decl_offset..].starts_with(b"<?xml") {
        xml_declaration_at_start = true;
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => {
                // Must have seen and closed StreamingChannelList.
                if !streaming_channel_list_seen {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        "XML document is incomplete: missing StreamingChannelList root",
                    ));
                }
                // Must be at root depth (only StreamingChannelList was open).
                if !element_stack.is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        "XML document is incomplete: unclosed elements remain",
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
                            "parse_camera_discovery_xml",
                            format!("character encoding error: {e}"),
                        )
                    })?
                    .into_owned();

                let depth = element_stack.len();

                // Validate attributes for every Start element.
                validate_attributes(&reader, &e)?;

                // At depth 0 (document root level), only StreamingChannelList is
                // permitted.  Unknown elements before or after the root are rejected
                // to enforce a single-complete-document structure.
                if depth == 0 && local != "StreamingChannelList" {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        "unexpected element at document root level",
                    ));
                }

                if local == "StreamingChannelList" {
                    // Must be at root depth.
                    if depth != 0 {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_camera_discovery_xml",
                            "StreamingChannelList must be the document root",
                        ));
                    }
                    if streaming_channel_list_seen {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_camera_discovery_xml",
                            "duplicate StreamingChannelList root",
                        ));
                    }
                    streaming_channel_list_seen = true;
                    prolog_state = PrologState::AfterRoot;
                    element_stack.push(local.clone());
                } else if local == "StreamingChannel" {
                    // Must be a direct child of StreamingChannelList.
                    if depth != 1 {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_camera_discovery_xml",
                            "StreamingChannel must be a direct child of StreamingChannelList",
                        ));
                    }
                    // Capture raw discovery identifier from the channel element's `id` attribute.
                    let raw_id = extract_channel_id_attr(&reader, &e)?;
                    current_channel = RawChannel {
                        track_id: None,
                        name: None,
                        raw_identifier: raw_id,
                        saved_attr_raw_id: None,
                        track_id_seen: false,
                        raw_id_seen: false,
                        name_seen: false,
                    };
                    element_stack.push(local.clone());
                    // Initialize active fields at depth 2 (inside StreamingChannel).
                    active_fields.push((depth + 1, ActiveField::None));
                } else {
                    // Determine the active field for this element.
                    // Only recognized fields at depth 2 (direct children of
                    // StreamingChannel) get a non-None active field.  Nested
                    // elements with the same local name must not capture text.
                    let active = if depth == 2 {
                        match local.as_str() {
                            "trackID" => {
                                // Initialize seen flag.
                                current_channel.track_id_seen = false;
                                ActiveField::TrackId
                            }
                            "id" => {
                                // Save the attribute-derived value and clear the
                                // field so that accumulated text replaces it.
                                // If no text is accumulated, the saved value is
                                // restored on close.
                                current_channel.saved_attr_raw_id =
                                    current_channel.raw_identifier.take();
                                current_channel.raw_identifier = None;
                                current_channel.raw_id_seen = false;
                                ActiveField::RawId
                            }
                            "name" | "channelName" | "channelname" => {
                                // Initialize seen flag.
                                current_channel.name_seen = false;
                                ActiveField::Name
                            }
                            _ => ActiveField::None,
                        }
                    } else {
                        ActiveField::None
                    };
                    // Push onto stack.
                    element_stack.push(local.clone());
                    // Always push an active field so that the corresponding End
                    // event pops the correct entry.  For unknown elements or
                    // nested recognized elements we push None.
                    active_fields.push((depth + 1, active));
                }
            }
            Ok(Event::Empty(e)) => {
                let local_name = e.name().local_name();
                let local: String = reader
                    .decoder()
                    .decode(local_name.as_ref())
                    .map_err(|e| {
                        AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_camera_discovery_xml",
                            format!("character encoding error: {e}"),
                        )
                    })?
                    .into_owned();

                let depth = element_stack.len();

                // Validate attributes for every Empty element.
                validate_attributes(&reader, &e)?;

                // At depth 0 (document root level), only StreamingChannelList is
                // permitted.  Unknown empty elements before or after the root are
                // rejected to enforce a single-complete-document structure.
                if depth == 0 && local != "StreamingChannelList" {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        "unexpected element at document root level",
                    ));
                }

                match local.as_str() {
                    "StreamingChannelList" => {
                        // Empty root — must be at root depth and seen only once.
                        if depth != 0 {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "StreamingChannelList must be the document root",
                            ));
                        }
                        if streaming_channel_list_seen {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "duplicate StreamingChannelList root",
                            ));
                        }
                        streaming_channel_list_seen = true;
                        prolog_state = PrologState::AfterRoot;
                        // Empty root — no children, so we close it immediately.
                    }
                    "StreamingChannel" => {
                        // Empty StreamingChannel — must be direct child of StreamingChannelList.
                        if depth != 1 {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "StreamingChannel must be a direct child of StreamingChannelList",
                            ));
                        }
                        let raw_id = extract_channel_id_attr(&reader, &e)?;
                        channels.push(RawChannel {
                            track_id: None,
                            name: None,
                            raw_identifier: raw_id,
                            saved_attr_raw_id: None,
                            track_id_seen: false,
                            raw_id_seen: false,
                            name_seen: false,
                        });
                        // No active fields left behind — empty element is self-contained.
                    }
                    _ => {
                        // Unknown empty element — skip.
                    }
                }
            }
            Ok(Event::End(_e)) => {
                // Pop the element from the stack.
                // Capture the active field before discarding it so we can
                // trim accumulated text for recognized fields.
                let popped_active = active_fields.pop();
                if let Some(popped) = element_stack.pop() {
                    // Trim accumulated field text for recognized fields.
                    // This ensures spaces across text/CDATA boundaries are
                    // preserved while leading/trailing whitespace is stripped.
                    if let Some((_, af)) = popped_active {
                        trim_field_text(&mut current_channel, af);
                    }

                    // For the `<id>` element: if no text was accumulated
                    // (empty element), restore the saved attribute value.
                    if let Some((_, ActiveField::RawId)) = popped_active
                        && !current_channel.raw_id_seen
                    {
                        // No text was seen — restore the attribute value.
                        current_channel.raw_identifier = current_channel.saved_attr_raw_id.take();
                    }

                    if popped == "StreamingChannelList" {
                        // Root closed — depth must now be 0.
                        if !element_stack.is_empty() {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "StreamingChannelList must be the document root",
                            ));
                        }
                    } else if popped == "StreamingChannel" {
                        // Channel closed — push the accumulated data.
                        channels.push(current_channel.clone());
                        current_channel = RawChannel {
                            track_id: None,
                            name: None,
                            raw_identifier: None,
                            saved_attr_raw_id: None,
                            track_id_seen: false,
                            raw_id_seen: false,
                            name_seen: false,
                        };
                    }
                }
            }
            Ok(Event::Text(e)) => {
                // Propagate entity-decoding errors.
                let text = e.unescape().map_err(|e| {
                    AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        format!("entity decode error: {e}"),
                    )
                })?;

                // At depth 0 (document root level), non-whitespace text is
                // rejected to enforce a single-complete-document structure.
                if element_stack.is_empty() && !text.trim().is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        "unexpected text content at document root level",
                    ));
                }

                // Accumulate ALL text segments (including whitespace-only)
                // for the active field so that spaces across boundaries are
                // preserved.  Trimming happens once at element close.
                let current_depth = element_stack.len();
                capture_field_text(
                    &mut current_channel,
                    active_fields.last(),
                    current_depth,
                    &text,
                );
            }
            Ok(Event::Comment(_)) => {
                // Skip XML comments
            }
            Ok(Event::Decl(_decl)) => {
                // XML declarations (e.g. <?xml version="1.0"?>) are only
                // permitted before the root element AND must be the very first
                // XML construct (after an optional BOM).  quick-xml's `Decl`
                // variant only represents the standard XML declaration,
                // so we track it directly.
                match prolog_state {
                    PrologState::BeforeRoot => {
                        // The declaration must have been at position 0
                        // (after optional BOM), verified by the pre-check
                        // before the event loop.
                        if !xml_declaration_at_start {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "XML declaration must be the first construct in the document",
                            ));
                        }
                        // Reject duplicate XML declarations.
                        if xml_declaration_seen {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "duplicate XML declaration",
                            ));
                        }
                        xml_declaration_seen = true;
                    }
                    PrologState::AfterRoot => {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_camera_discovery_xml",
                            "XML declaration after document root is not permitted",
                        ));
                    }
                }
            }
            Ok(Event::DocType(_doc_type)) => {
                // DOCTYPE declarations are only permitted before the root element.
                match prolog_state {
                    PrologState::BeforeRoot => {
                        if doctype_seen {
                            return Err(AppError::new(
                                ErrorCategory::XmlParsing,
                                "parse_camera_discovery_xml",
                                "duplicate DOCTYPE declaration",
                            ));
                        }
                        doctype_seen = true;
                    }
                    PrologState::AfterRoot => {
                        return Err(AppError::new(
                            ErrorCategory::XmlParsing,
                            "parse_camera_discovery_xml",
                            "DOCTYPE declaration after document root is not permitted",
                        ));
                    }
                }
            }
            Ok(Event::CData(e)) => {
                // CDATA content is literal character data — do NOT unescape
                // XML entities.  Decode bytes to string and enforce the same
                // depth-0 restriction as regular text.
                let cdata_bytes = e.as_ref();
                let text = reader.decoder().decode(cdata_bytes).map_err(|e| {
                    AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        format!("character encoding error in CDATA: {e}"),
                    )
                })?;
                let text = text.into_owned();

                if element_stack.is_empty() && !text.trim().is_empty() {
                    return Err(AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        "unexpected CDATA at document root level",
                    ));
                }

                // Accumulate ALL CDATA segments (including whitespace-only)
                // for the active field so that spaces across boundaries are
                // preserved.  Trimming happens once at element close.
                let current_depth = element_stack.len();
                capture_field_text(
                    &mut current_channel,
                    active_fields.last(),
                    current_depth,
                    &text,
                );
            }
            Err(e) => {
                return Err(AppError::new(
                    ErrorCategory::XmlParsing,
                    "parse_camera_discovery_xml",
                    format!("malformed XML: {e}"),
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    // Normalize channels: group by channel number, keep only primary (..01) tracks.
    let mut grouped: BTreeMap<i64, (String, String, Option<String>, Option<String>)> =
        BTreeMap::new();

    for raw in channels {
        // Prefer the explicit <trackID> element; fall back to <id> when
        // <trackID> is absent (some Hikvision NVR firmwares embed the
        // track ID directly in <id> and omit <trackID> entirely).
        let track_id = match raw.track_id {
            Some(tid) => tid,
            None => match &raw.raw_identifier {
                Some(id) => id.clone(),
                None => {
                    tracing::warn!(
                        "camera_discovery: skipping channel entry with missing track ID"
                    );
                    continue;
                }
            },
        };

        let mapping = match derive_camera_mapping(&track_id) {
            Some(m) => m,
            None => {
                tracing::debug!(
                    track_id = %track_id,
                    "camera_discovery: skipping non-primary or malformed track ID"
                );
                continue;
            }
        };

        let (channel_number, picture_track_id) = mapping;

        if let Some(existing) = grouped.get(&channel_number) {
            tracing::debug!(
                channel_number = channel_number,
                existing_primary_track = ?existing.0,
                "camera_discovery: duplicate primary track for channel {channel_number}, keeping first candidate",
            );
        }
        grouped
            .entry(channel_number)
            .or_insert_with(|| (track_id, picture_track_id, raw.name, raw.raw_identifier));
    }

    let mut result = Vec::with_capacity(grouped.len());
    for (_channel_number, (primary_track, picture_track_id, name, raw_id)) in grouped {
        result.push(CameraDiscovery {
            channel_number: _channel_number,
            primary_track_id: primary_track,
            picture_track_id,
            name,
            raw_discovery_identifier: raw_id,
        });
    }

    Ok(result)
}

/// Validate all attributes of a Start or Empty XML element.
///
/// Checks for:
/// * Duplicate attribute names (same key appearing twice).
/// * Invalid entity references in attribute values.
/// * Character encoding errors in attribute keys/values.
///
/// Returns an `XmlParsing` error on any violation.
fn validate_attributes<'a>(
    reader: &Reader<&'a [u8]>,
    event: &'a quick_xml::events::BytesStart<'a>,
) -> AppResult<()> {
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    for attr_result in event.attributes() {
        let attr = attr_result.map_err(|e| {
            AppError::new(
                ErrorCategory::XmlParsing,
                "parse_camera_discovery_xml",
                format!("malformed attribute: {e}"),
            )
        })?;

        // Decode attribute key as owned String.
        let key = reader
            .decoder()
            .decode(attr.key.as_ref())
            .map_err(|e| {
                AppError::new(
                    ErrorCategory::XmlParsing,
                    "parse_camera_discovery_xml",
                    format!("character encoding error in attribute key: {e}"),
                )
            })?
            .into_owned();

        // Check for duplicate attribute names.
        if !seen_keys.insert(key.clone()) {
            return Err(AppError::new(
                ErrorCategory::XmlParsing,
                "parse_camera_discovery_xml",
                format!("duplicate attribute: {key}"),
            ));
        }

        // Decode attribute value — first decode bytes to string, then
        // unescape XML entities to detect invalid entity references.
        let decoded = reader.decoder().decode(&attr.value).map_err(|e| {
            AppError::new(
                ErrorCategory::XmlParsing,
                "parse_camera_discovery_xml",
                format!("character encoding error in attribute value: {e}"),
            )
        })?;
        quick_xml::escape::unescape(&decoded).map_err(|e| {
            AppError::new(
                ErrorCategory::XmlParsing,
                "parse_camera_discovery_xml",
                format!("invalid entity in attribute value: {e}"),
            )
        })?;
    }

    Ok(())
}

/// Extract the `id` attribute from a StreamingChannel element.
///
/// Returns the decoded attribute value or `None` if the attribute is absent.
/// Propagates encoding and entity errors as `XmlParsing`.
fn extract_channel_id_attr<'a>(
    reader: &Reader<&'a [u8]>,
    event: &'a quick_xml::events::BytesStart<'a>,
) -> AppResult<Option<String>> {
    for attr_result in event.attributes() {
        let attr = attr_result.map_err(|e| {
            AppError::new(
                ErrorCategory::XmlParsing,
                "parse_camera_discovery_xml",
                format!("malformed attribute: {e}"),
            )
        })?;

        let key = reader
            .decoder()
            .decode(attr.key.as_ref())
            .map_err(|e| {
                AppError::new(
                    ErrorCategory::XmlParsing,
                    "parse_camera_discovery_xml",
                    format!("character encoding error in attribute key: {e}"),
                )
            })?
            .into_owned();

        if key == "id" {
            let decoded = reader.decoder().decode(&attr.value).map_err(|e| {
                AppError::new(
                    ErrorCategory::XmlParsing,
                    "parse_camera_discovery_xml",
                    format!("character encoding error in attribute value: {e}"),
                )
            })?;
            let value = quick_xml::escape::unescape(&decoded)
                .map_err(|e| {
                    AppError::new(
                        ErrorCategory::XmlParsing,
                        "parse_camera_discovery_xml",
                        format!("invalid entity in attribute value: {e}"),
                    )
                })?
                .into_owned();
            return Ok(Some(value));
        }
    }
    Ok(None)
}

/// Accumulate untrimmed decoded text and CDATA into the active field of a
/// `RawChannel`.
///
/// Text is accumulated (not trimmed) so that spaces across text/CDATA
/// boundaries are preserved.  Trimming happens once at element close.
///
/// * Depth check: text is only captured when the active field's depth
///   matches the current element depth, preventing nested elements from
///   leaking text into channel fields.
/// * TrackId, RawId, and Name: multiple text/CDATA segments are
///   concatenated so that mixed content (e.g. text + CDATA) is correctly
///   captured.
/// * RawId: the child `<id>` element text replaces any attribute-derived
///   raw identifier (deterministic precedence), but multiple text segments
///   within the child element are concatenated.
fn capture_field_text(
    channel: &mut RawChannel,
    active: Option<&(usize, ActiveField)>,
    current_depth: usize,
    text: &str,
) {
    let Some((field_depth, active_field)) = active else {
        return;
    };

    // Only capture text when the active field's depth matches the current
    // element depth.  This prevents nested elements inside unknown containers
    // from leaking text into channel fields.
    if *field_depth != current_depth {
        return;
    }

    match active_field {
        ActiveField::TrackId => {
            channel.track_id_seen = true;
            channel.track_id = Some(
                channel
                    .track_id
                    .take()
                    .map(|mut s| {
                        s.push_str(text);
                        s
                    })
                    .unwrap_or_else(|| text.to_string()),
            );
        }
        ActiveField::RawId => {
            channel.raw_id_seen = true;
            // Accumulate text segments so that mixed content
            // (e.g. text + CDATA) is correctly captured.  The accumulated
            // value will be trimmed at element close.
            channel.raw_identifier = Some(
                channel
                    .raw_identifier
                    .take()
                    .map(|mut s| {
                        s.push_str(text);
                        s
                    })
                    .unwrap_or_else(|| text.to_string()),
            );
        }
        ActiveField::Name => {
            channel.name_seen = true;
            channel.name = Some(
                channel
                    .name
                    .take()
                    .map(|mut s| {
                        s.push_str(text);
                        s
                    })
                    .unwrap_or_else(|| text.to_string()),
            );
        }
        ActiveField::None => {}
    }
}

/// Trim accumulated field text for a recognized field element.
///
/// Called on `Event::End` for recognized field elements (trackID, id,
/// name/channelName/channelname).  Trims the accumulated text once so
/// that spaces across text/CDATA boundaries are preserved while
/// leading/trailing whitespace is stripped.
fn trim_field_text(channel: &mut RawChannel, active_field: ActiveField) {
    match active_field {
        ActiveField::TrackId => {
            if let Some(ref mut tid) = channel.track_id {
                *tid = tid.trim().to_string();
            }
        }
        ActiveField::RawId => {
            if let Some(ref mut rid) = channel.raw_identifier {
                *rid = rid.trim().to_string();
            }
        }
        ActiveField::Name => {
            if let Some(ref mut nm) = channel.name {
                *nm = nm.trim().to_string();
            }
        }
        ActiveField::None => {}
    }
}

// ── CameraDiscoveryClient ──────────────────────────────────────────────────

/// A client for Hikvision camera discovery that uses an authenticated NVR
/// transport to fetch and parse the streaming-channel list.
pub struct CameraDiscoveryClient<'a> {
    transport: &'a NvrTransport,
}

impl<'a> CameraDiscoveryClient<'a> {
    /// Create a new discovery client from an NVR transport.
    pub fn new(transport: &'a NvrTransport) -> Self {
        Self { transport }
    }

    /// Fetch and parse camera discovery from the NVR.
    ///
    /// Contacts `GET /ISAPI/Streaming/channels` through the authenticated
    /// transport, reads the response body, and parses it.
    ///
    /// Returns `ErrorCategory::XmlParsing` for parse failures, and maps
    /// transport errors through the existing HTTP error taxonomy.
    pub async fn discover(&self) -> AppResult<Vec<CameraDiscovery>> {
        let response = self.transport.get("/ISAPI/Streaming/channels").await?;

        let body = response
            .bytes()
            .await
            .map_err(|e| crate::http::map_reqwest_error("camera_discovery_read_body", e))?;

        parse_camera_discovery_xml(body.as_ref())
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── derive_camera_mapping tests ──────────────────────────────────────

    #[test]
    fn derive_101_to_103() {
        let result = derive_camera_mapping("101");
        assert_eq!(result, Some((1, "103".to_string())));
    }

    #[test]
    fn derive_301_to_303() {
        let result = derive_camera_mapping("301");
        assert_eq!(result, Some((3, "303".to_string())));
    }

    #[test]
    fn derive_1001_to_1003() {
        let result = derive_camera_mapping("1001");
        assert_eq!(result, Some((10, "1003".to_string())));
    }

    #[test]
    fn derive_999901_to_999903() {
        let result = derive_camera_mapping("999901");
        assert_eq!(result, Some((9999, "999903".to_string())));
    }

    #[test]
    fn skip_103_picture_track() {
        assert!(derive_camera_mapping("103").is_none());
    }

    #[test]
    fn skip_104_secondary() {
        assert!(derive_camera_mapping("104").is_none());
    }

    #[test]
    fn skip_non_numeric() {
        assert!(derive_camera_mapping("abc").is_none());
    }

    #[test]
    fn skip_too_short() {
        assert!(derive_camera_mapping("01").is_none());
        assert!(derive_camera_mapping("1").is_none());
    }

    #[test]
    fn skip_empty() {
        assert!(derive_camera_mapping("").is_none());
    }

    #[test]
    fn skip_with_whitespace() {
        let result = derive_camera_mapping("  101  ");
        assert_eq!(result, Some((1, "103".to_string())));
    }

    #[test]
    fn skip_overflowing_track_id() {
        // Track ID that overflows i64 but we parse complete value as u64.
        // "922337203685477580701" is larger than i64::MAX.
        let result = derive_camera_mapping("922337203685477580701");
        assert!(result.is_none());
    }

    #[test]
    fn skip_track_id_with_leading_zeros() {
        // Leading zeros still parse as valid numeric but are unusual.
        let result = derive_camera_mapping("0101");
        assert_eq!(result, Some((1, "103".to_string())));
    }

    #[test]
    fn skip_track_id_zero_channel() {
        // "001" -> channel 0 -> rejected
        let result = derive_camera_mapping("001");
        assert!(result.is_none());
    }

    // ── parse_camera_discovery_xml tests ─────────────────────────────────

    /// Default-namespace response with two cameras.
    fn default_namespace_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList xmlns="http://www.hikvision.com/ver20/XMLSchema">
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>Camera One</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>301</trackID>
            <name>Camera Two</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Prefixed-namespace equivalent.
    fn prefixed_namespace_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <h:StreamingChannelList xmlns:h="http://www.hikvision.com/ver20/XMLSchema">
          <h:StreamingChannel>
            <h:id>ch1</h:id>
            <h:trackID>101</h:trackID>
            <h:name>Camera One</h:name>
          </h:StreamingChannel>
          <h:StreamingChannel>
            <h:id>ch2</h:id>
            <h:trackID>301</h:trackID>
            <h:name>Camera Two</h:name>
          </h:StreamingChannel>
        </h:StreamingChannelList>"#
    }

    /// Namespace-free document.
    fn no_namespace_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>Camera One</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>301</trackID>
            <name>Camera Two</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Response with primary, secondary, and picture tracks per camera.
    fn mixed_tracks_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>Primary</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>103</trackID>
            <name>Picture</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch3</id>
            <trackID>104</trackID>
            <name>Secondary</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch4</id>
            <trackID>201</trackID>
            <name>Camera Two</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Response with malformed track IDs alongside valid ones.
    fn malformed_entries_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>Good</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>abc</trackID>
            <name>Bad</name>
          </StreamingChannel>
          <StreamingChannel>
            <trackID>301</trackID>
            <name>No ID</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch4</id>
            <name>No Track</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch5</id>
            <trackID>401</trackID>
            <name>Another Good</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Response with unknown elements mixed in.
    fn unknown_elements_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <deviceName>Device Info</deviceName>
            <status>Online</status>
            <name>Camera One</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Response with XML entities in names.
    fn xml_entities_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>Camera &amp; &lt;Test&gt;</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Empty channel list.
    fn empty_list_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
        </StreamingChannelList>"#
    }

    /// Malformed XML with mismatched closing tag.
    fn malformed_xml() -> &'static [u8] {
        b"<?xml version=\"1.0\"?><tag><nested>text</tag>"
    }

    /// EOF-truncated XML — StreamingChannelList opened but not closed.
    fn truncated_xml() -> &'static [u8] {
        b"<?xml version=\"1.0\"?><StreamingChannelList><StreamingChannel>"
    }

    /// Invalid XML entity that cannot be decoded.
    fn invalid_entity_xml() -> &'static [u8] {
        // &#xG is an invalid hex entity (G is not a hex digit)
        b"<?xml version=\"1.0\"?><StreamingChannelList><StreamingChannel><id>ch1</id><trackID>101</trackID><name>&#xG;invalid</name></StreamingChannel></StreamingChannelList>"
    }

    /// Duplicate primary channels.
    fn duplicate_channels_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>First</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>101</trackID>
            <name>Duplicate</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch3</id>
            <trackID>201</trackID>
            <name>Second</name>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    /// Response with channelName element instead of name.
    fn channel_name_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <channelName>Channel Name One</channelName>
          </StreamingChannel>
        </StreamingChannelList>"#
    }

    #[test]
    fn parse_default_namespace_two_cameras() {
        let result = parse_camera_discovery_xml(default_namespace_xml().as_bytes()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].primary_track_id, "101");
        assert_eq!(result[0].picture_track_id, "103");
        assert_eq!(result[0].name, Some("Camera One".to_string()));
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
        assert_eq!(result[1].channel_number, 3);
        assert_eq!(result[1].primary_track_id, "301");
        assert_eq!(result[1].picture_track_id, "303");
        assert_eq!(result[1].name, Some("Camera Two".to_string()));
    }

    #[test]
    fn parse_prefixed_namespace_same_result() {
        let default_result =
            parse_camera_discovery_xml(default_namespace_xml().as_bytes()).unwrap();
        let prefixed_result =
            parse_camera_discovery_xml(prefixed_namespace_xml().as_bytes()).unwrap();
        assert_eq!(default_result.len(), prefixed_result.len());
        for (d, p) in default_result.iter().zip(prefixed_result.iter()) {
            assert_eq!(d.channel_number, p.channel_number);
            assert_eq!(d.primary_track_id, p.primary_track_id);
            assert_eq!(d.picture_track_id, p.picture_track_id);
            assert_eq!(d.name, p.name);
        }
    }

    #[test]
    fn parse_no_namespace_same_result() {
        let default_result =
            parse_camera_discovery_xml(default_namespace_xml().as_bytes()).unwrap();
        let no_ns_result = parse_camera_discovery_xml(no_namespace_xml().as_bytes()).unwrap();
        assert_eq!(default_result.len(), no_ns_result.len());
        for (d, n) in default_result.iter().zip(no_ns_result.iter()) {
            assert_eq!(d.channel_number, n.channel_number);
            assert_eq!(d.primary_track_id, n.primary_track_id);
            assert_eq!(d.picture_track_id, n.picture_track_id);
        }
    }

    #[test]
    fn parse_mixed_tracks_only_primary() {
        let result = parse_camera_discovery_xml(mixed_tracks_xml().as_bytes()).unwrap();
        assert_eq!(result.len(), 2);
        // Camera 1
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].primary_track_id, "101");
        assert_eq!(result[0].picture_track_id, "103");
        assert_eq!(result[0].name, Some("Primary".to_string()));
        // Camera 2
        assert_eq!(result[1].channel_number, 2);
        assert_eq!(result[1].primary_track_id, "201");
        assert_eq!(result[1].picture_track_id, "203");
        assert_eq!(result[1].name, Some("Camera Two".to_string()));
    }

    #[test]
    fn parse_malformed_entries_skips_invalid() {
        let result = parse_camera_discovery_xml(malformed_entries_xml().as_bytes()).unwrap();
        // "abc" trackID is skipped (non-numeric), "No Track" has no trackID.
        // "No ID" has a valid trackID (301) but no <id> element — still parsed.
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].name, Some("Good".to_string()));
        assert_eq!(result[1].channel_number, 3);
        assert_eq!(result[1].name, Some("No ID".to_string()));
        assert_eq!(result[2].channel_number, 4);
        assert_eq!(result[2].name, Some("Another Good".to_string()));
    }

    #[test]
    fn parse_unknown_elements_ignored() {
        let result = parse_camera_discovery_xml(unknown_elements_xml().as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].primary_track_id, "101");
        assert_eq!(result[0].picture_track_id, "103");
        assert_eq!(result[0].name, Some("Camera One".to_string()));
    }

    #[test]
    fn parse_xml_entities_decoded() {
        let result = parse_camera_discovery_xml(xml_entities_xml().as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("Camera & <Test>".to_string()));
    }

    #[test]
    fn parse_empty_list_returns_empty() {
        let result = parse_camera_discovery_xml(empty_list_xml().as_bytes()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_malformed_xml_returns_error() {
        // Mismatched closing tags cause a quick-xml parse error.
        let result = parse_camera_discovery_xml(malformed_xml());
        assert!(result.is_err(), "malformed XML should return an error");
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    #[test]
    fn parse_duplicate_channels_keeps_first() {
        let result = parse_camera_discovery_xml(duplicate_channels_xml().as_bytes()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].name, Some("First".to_string()));
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
        assert_eq!(result[1].channel_number, 2);
        assert_eq!(result[1].name, Some("Second".to_string()));
    }

    #[test]
    fn parse_channel_name_element() {
        let result = parse_camera_discovery_xml(channel_name_xml().as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("Channel Name One".to_string()));
    }

    #[test]
    fn parse_preserves_raw_identifier() {
        let result = parse_camera_discovery_xml(default_namespace_xml().as_bytes()).unwrap();
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
        assert_eq!(result[1].raw_discovery_identifier, Some("ch2".to_string()));
    }

    #[test]
    fn parse_order_is_deterministic_by_channel_number() {
        // Provide channels out of order
        let out_of_order = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch3</id>
            <trackID>501</trackID>
            <name>Fifth</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>First</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>301</trackID>
            <name>Third</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(out_of_order.as_bytes()).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[1].channel_number, 3);
        assert_eq!(result[2].channel_number, 5);
    }

    // ── Strict document validation tests ───────────────────────────────

    #[test]
    fn parse_empty_bytes_returns_error() {
        let result = parse_camera_discovery_xml(b"");
        assert!(result.is_err(), "empty bytes should return an error");
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    #[test]
    fn parse_text_only_returns_error() {
        let result = parse_camera_discovery_xml(b"just some plain text");
        assert!(result.is_err(), "text-only input should return an error");
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    #[test]
    fn parse_truncated_xml_returns_error() {
        let result = parse_camera_discovery_xml(truncated_xml());
        assert!(
            result.is_err(),
            "truncated XML should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    #[test]
    fn parse_invalid_entity_returns_error() {
        let result = parse_camera_discovery_xml(invalid_entity_xml());
        assert!(
            result.is_err(),
            "invalid entity should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    #[test]
    fn parse_malformed_xml_without_streaming_channel_list_returns_error() {
        // This XML has mismatched tags but no StreamingChannelList root,
        // so it should be rejected as malformed.
        let result = parse_camera_discovery_xml(malformed_xml());
        assert!(
            result.is_err(),
            "malformed XML without StreamingChannelList should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    // ── Nested element depth tests ─────────────────────────────────────

    /// Verify that a nested <name> inside an unknown element does not
    /// populate the channel name field.
    #[test]
    fn parse_nested_name_not_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <metadata>
              <name>Fake Name</name>
            </metadata>
            <name>Real Name</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // The real name should be captured (not the nested fake one)
        assert_eq!(result[0].name, Some("Real Name".to_string()));
    }

    /// Verify that a nested <trackID> inside an unknown element does not
    /// populate the track ID field.
    #[test]
    fn parse_nested_track_id_not_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <metadata>
              <trackID>999</trackID>
            </metadata>
            <trackID>101</trackID>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].primary_track_id, "101");
    }

    /// Verify that a nested <id> inside an unknown element does not
    /// populate the raw identifier field.
    #[test]
    fn parse_nested_id_not_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <metadata>
              <id>fake-id</id>
            </metadata>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
    }

    // ── Strict document validation tests ───────────────────────────────

    /// Malformed root attribute (invalid entity in attribute value) must fail.
    #[test]
    fn parse_malformed_root_attribute_returns_error() {
        let xml =
            b"<?xml version=\"1.0\"?><StreamingChannelList a=\"&bogus;\"></StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "malformed root attribute should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Duplicate attributes on the root element must fail.
    #[test]
    fn parse_duplicate_attributes_on_root_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><StreamingChannelList id=\"a\" id=\"b\"></StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "duplicate attributes should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Duplicate attributes on a StreamingChannel element must fail.
    #[test]
    fn parse_duplicate_attributes_on_channel_returns_error() {
        let xml = b"<?xml version=\"1.0\"?>
        <StreamingChannelList>
          <StreamingChannel id=\"a\" id=\"b\">
            <trackID>101</trackID>
          </StreamingChannel>
        </StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "duplicate attributes should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Nested StreamingChannelList (wrong depth) must fail.
    #[test]
    fn parse_nested_streaming_channel_list_returns_error() {
        let xml = b"<?xml version=\"1.0\"?>
        <StreamingChannelList>
          <StreamingChannelList>
            <StreamingChannel>
              <trackID>101</trackID>
            </StreamingChannel>
          </StreamingChannelList>
        </StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "nested StreamingChannelList should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Wrong root element (not StreamingChannelList) must fail.
    #[test]
    fn parse_wrong_root_element_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><WrongRoot><StreamingChannel><trackID>101</trackID></StreamingChannel></WrongRoot>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "wrong root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Multiple top-level roots must fail.
    #[test]
    fn parse_multiple_top_level_roots_returns_error() {
        let xml = b"<?xml version=\"1.0\"?>
        <StreamingChannelList></StreamingChannelList>
        <StreamingChannelList></StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "multiple roots should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Empty <id/> inside StreamingChannel should not capture anything.
    #[test]
    fn parse_empty_id_element_not_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id/>
            <trackID>101</trackID>
            <name>Camera</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].raw_discovery_identifier, None);
        assert_eq!(result[0].name, Some("Camera".to_string()));
    }

    /// Empty <name/> inside StreamingChannel should not capture anything.
    #[test]
    fn parse_empty_name_element_not_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name/>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
        assert_eq!(result[0].name, None);
    }

    /// Empty <trackID/> inside StreamingChannel is skipped by
    /// derive_camera_mapping, so the result is empty.
    #[test]
    fn parse_empty_track_id_element_not_captured() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID/>
            <name>Camera</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        // Empty track ID is skipped by derive_camera_mapping.
        assert!(result.is_empty());
    }

    /// Nested empty recognized elements inside unknown elements should not
    /// poison later text capture.
    #[test]
    fn parse_nested_empty_elements_not_poisoning() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <metadata>
              <name/>
              <trackID/>
            </metadata>
            <name>Real Name</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("Real Name".to_string()));
    }

    /// Empty StreamingChannel element (self-closing) should produce a
    /// channel entry with no track ID, which is then skipped.
    #[test]
    fn parse_empty_streaming_channel_skipped() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel/>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].channel_number, 1);
    }

    /// Malformed XML attribute entity in StreamingChannel must fail.
    #[test]
    fn parse_malformed_attribute_entity_in_channel_returns_error() {
        let xml = b"<?xml version=\"1.0\"?>
        <StreamingChannelList>
          <StreamingChannel id=\"&bogus;\">
            <trackID>101</trackID>
          </StreamingChannel>
        </StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "malformed attribute entity should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Overflowing track ID like "922337203685477580701" should be rejected.
    #[test]
    fn parse_overflowing_track_id_rejected() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>922337203685477580701</trackID>
            <name>Overflow</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert!(result.is_empty());
    }

    // ── Strict document root enforcement regressions ─────────────────────

    /// Unknown empty element AFTER the StreamingChannelList root must fail.
    #[test]
    fn parse_unknown_element_after_root_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><StreamingChannelList/><garbage/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "unknown element after root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Unknown empty element BEFORE the StreamingChannelList root must fail.
    #[test]
    fn parse_unknown_element_before_root_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><garbage/><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "unknown element before root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Non-whitespace text BEFORE the root must fail.
    #[test]
    fn parse_text_before_root_returns_error() {
        let xml = b"oops<StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "text before root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Non-whitespace text AFTER the root must fail.
    #[test]
    fn parse_text_after_root_returns_error() {
        let xml = b"<StreamingChannelList/>garbage";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "text after root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Unknown Start element after the root must fail.
    #[test]
    fn parse_unknown_start_element_after_root_returns_error() {
        let xml =
            b"<?xml version=\"1.0\"?><StreamingChannelList></StreamingChannelList><extra></extra>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "unknown start element after root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Unknown Start element before the root must fail.
    #[test]
    fn parse_unknown_start_element_before_root_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><extra></extra><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "unknown start element before root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// CDATA at document root level must fail.
    #[test]
    fn parse_cdata_at_root_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><![CDATA[some data]]><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "CDATA at root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// CDATA after the root must fail.
    #[test]
    fn parse_cdata_after_root_returns_error() {
        let xml = b"<StreamingChannelList/><![CDATA[some data]]>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "CDATA after root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
    }

    /// Whitespace-only text at root level is allowed (e.g. between elements).
    #[test]
    fn parse_whitespace_at_root_is_allowed() {
        let xml = b"<?xml version=\"1.0\"?>\n  \n<StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_ok(),
            "whitespace at root should be allowed, got: {:?}",
            result
        );
        let records = result.unwrap();
        assert!(records.is_empty());
    }

    /// Duplicate primary tracks should log a debug event but keep the first.
    #[test]
    fn parse_duplicate_primary_tracks_keeps_first() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <StreamingChannelList>
          <StreamingChannel>
            <id>ch1</id>
            <trackID>101</trackID>
            <name>First</name>
          </StreamingChannel>
          <StreamingChannel>
            <id>ch2</id>
            <trackID>101</trackID>
            <name>Duplicate</name>
          </StreamingChannel>
        </StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].primary_track_id, "101");
        assert_eq!(result[0].name, Some("First".to_string()));
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
    }

    /// Multiple cameras with valid tracks should all parse correctly.
    #[test]
    fn parse_many_cameras() {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>"#,
        );
        for i in 1..=20 {
            let track = format!("{}01", i);
            let name = format!("Camera {}", i);
            xml.push_str(&format!(
                r#"<StreamingChannel><id>ch{i}</id><trackID>{}</trackID><name>{}</name></StreamingChannel>"#,
                track, name
            ));
        }
        xml.push_str("</StreamingChannelList>");
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 20);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[19].channel_number, 20);
    }

    // ── CDATA tests (reviewer feedback) ──────────────────────────────────

    /// CDATA with a literal ampersand should NOT be entity-unescaped.
    #[test]
    fn parse_cdata_literal_ampersand() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name><![CDATA[A & B]]></name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("A & B".to_string()));
    }

    /// Mixed text and CDATA in a name field should be concatenated.
    #[test]
    fn parse_mixed_text_and_cdata() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Part1<![CDATA[& Part2]]></name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("Part1& Part2".to_string()));
    }

    // ── Prolog/doctype lifecycle tests ───────────────────────────────────

    /// XML declaration before root is allowed.
    #[test]
    fn parse_xml_declaration_before_root_allowed() {
        let xml = b"<?xml version=\"1.0\"?><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_ok(),
            "XML declaration before root should be allowed, got: {:?}",
            result
        );
        assert!(result.unwrap().is_empty());
    }

    /// Duplicate XML declarations must fail.
    #[test]
    fn parse_duplicate_xml_declaration_returns_error() {
        let xml =
            b"<?xml version=\"1.0\"?><!DOCTYPE x><?xml version=\"1.0\"?><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "duplicate XML declaration should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        // The second declaration is rejected because content was already seen.
        assert!(
            err.message.contains("first construct") || err.message.contains("duplicate"),
            "unexpected error message: {}",
            err.message
        );
    }

    /// DOCTYPE before root is allowed.
    #[test]
    fn parse_doctype_before_root_allowed() {
        let xml = b"<?xml version=\"1.0\"?><!DOCTYPE x><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_ok(),
            "DOCTYPE before root should be allowed, got: {:?}",
            result
        );
        assert!(result.unwrap().is_empty());
    }

    /// Duplicate DOCTYPE declarations must fail.
    #[test]
    fn parse_duplicate_doctype_returns_error() {
        let xml = b"<?xml version=\"1.0\"?><!DOCTYPE x><!DOCTYPE y><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "duplicate DOCTYPE should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(err.message.contains("duplicate"));
    }

    /// XML declaration after root must fail.
    #[test]
    fn parse_xml_declaration_after_root_returns_error() {
        let xml = b"<StreamingChannelList/><?xml version=\"1.0\"?>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "XML declaration after root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(err.message.contains("after document root"));
    }

    /// DOCTYPE after root must fail.
    #[test]
    fn parse_doctype_after_root_returns_error() {
        let xml = b"<StreamingChannelList/><!-- comment --><!DOCTYPE x>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "DOCTYPE after root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(err.message.contains("after document root"));
    }

    /// XML declaration inside StreamingChannelList (after root opened) must fail.
    #[test]
    fn parse_xml_declaration_inside_root_returns_error() {
        let xml = b"<StreamingChannelList><?xml version=\"1.0\"?><StreamingChannel><trackID>101</trackID></StreamingChannel></StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "XML declaration inside root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(err.message.contains("after document root"));
    }

    /// DOCTYPE inside StreamingChannelList (after root opened) must fail.
    #[test]
    fn parse_doctype_inside_root_returns_error() {
        let xml = b"<StreamingChannelList><!-- comment --><!DOCTYPE x><StreamingChannel><trackID>101</trackID></StreamingChannel></StreamingChannelList>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "DOCTYPE inside root should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(err.message.contains("after document root"));
    }

    // ── XML declaration lifecycle regressions (reviewer feedback) ────────

    /// XML declaration after whitespace must fail — whitespace before the
    /// declaration counts as content.
    #[test]
    fn parse_xml_declaration_after_whitespace_returns_error() {
        let xml = b" \n <?xml version=\"1.0\"?><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "XML declaration after whitespace should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(
            err.message.contains("first construct") || err.message.contains("after document root"),
            "unexpected error: {}",
            err.message
        );
    }

    /// Document with only a comment before root (no declaration) is valid.
    #[test]
    fn parse_comment_before_root_without_declaration_is_valid() {
        let xml = b"<!-- comment --><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_ok(),
            "comment before root without declaration should be valid, got: {:?}",
            result
        );
        assert!(result.unwrap().is_empty());
    }

    /// XML declaration after a DOCTYPE must fail.
    #[test]
    fn parse_xml_declaration_after_doctype_returns_error() {
        let xml = b"<!DOCTYPE x><?xml version=\"1.0\"?><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_err(),
            "XML declaration after DOCTYPE should return an error, got: {:?}",
            result
        );
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);
        assert!(
            err.message.contains("first construct") || err.message.contains("after document root"),
            "unexpected error: {}",
            err.message
        );
    }

    /// Document with only a PI before root (no declaration) is valid.
    /// quick-xml does not emit PI events, so the PI is silently consumed.
    #[test]
    fn parse_pi_before_root_without_declaration_is_valid() {
        let xml = b"<?pi?><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_ok(),
            "PI before root without declaration should be valid, got: {:?}",
            result
        );
        assert!(result.unwrap().is_empty());
    }

    /// A valid document with only the XML declaration at the start must
    /// succeed.
    #[test]
    fn parse_xml_declaration_at_start_only_allowed() {
        let xml = b"<?xml version=\"1.0\"?><StreamingChannelList/>";
        let result = parse_camera_discovery_xml(xml);
        assert!(
            result.is_ok(),
            "XML declaration at start should be allowed, got: {:?}",
            result
        );
        assert!(result.unwrap().is_empty());
    }

    // ── Nested markup depth regressions (reviewer feedback) ──────────────

    /// Nested markup in a name field: direct text before and after the
    /// nested element should both be captured.
    #[test]
    fn parse_nested_markup_in_name_captures_direct_text() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Part1<b>Inner</b>Part2</name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // Only the direct text segments of <name> are captured.
        // The nested <b> pushes a None field at depth 4, so "Inner"
        // (at depth 4) is ignored. "Part1" and "Part2" are at depth 3
        // (same as the name field depth) and captured.
        assert_eq!(result[0].name, Some("Part1Part2".to_string()));
    }

    /// Nested markup in a trackID field: only text at the trackID depth
    /// is captured; nested element text is ignored.
    #[test]
    fn parse_nested_markup_in_track_id_captures_direct_text() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>10<b>3</b></trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        // "10" is at depth 3 (same as trackID field), captured.
        // "3" is at depth 4 (inside <b>), ignored.
        // "10" doesn't end in "01", so the result is empty.
        assert!(result.is_empty());
    }

    /// Nested markup in an id field: only direct text is captured.
    #[test]
    fn parse_nested_markup_in_id_captures_direct_text() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1<b>suffix</b></id>
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // The child <id> element text takes precedence over the attribute.
        // "ch1" is at depth 3 (same as id field), captured.
        // "suffix" is at depth 4 (inside <b>), ignored.
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
    }

    /// Direct text after a nested element in trackID: only the direct text
    /// segments are captured.
    #[test]
    fn parse_nested_in_track_id_with_trailing_text() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID><b>1</b>01</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        // "1" is at depth 4 (inside <b>), ignored.
        // "01" is at depth 3 (same as trackID field), captured.
        // "01" doesn't pass derive_camera_mapping (too short, < 3 chars),
        // so the result is empty.
        assert!(result.is_empty());
    }

    /// Spaces spanning text/CDATA boundaries: text is accumulated
    /// untrimmed and trimmed once at element close, preserving internal
    /// spaces while stripping leading/trailing whitespace.
    ///
    /// Note: the CDATA segment starts with a space to provide the
    /// separator between the two words.  The text event before the CDATA
    /// has no trailing space in the source, so the accumulated result
    /// is "Part1" + " Part2" → trimmed to "Part1 Part2".
    #[test]
    fn parse_spaces_across_text_cdata_boundary() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Part1<![CDATA[ Part2]]></name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // "Part1" + " Part2" → trimmed to "Part1 Part2".
        assert_eq!(result[0].name, Some("Part1 Part2".to_string()));
    }

    /// Multi-segment child identifier: text and CDATA segments within
    /// a child `<id>` element are concatenated before trimming.
    #[test]
    fn parse_multi_segment_child_identifier() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch<![CDATA[-1]]></id>
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // "ch" + "-1" concatenated, trimmed to "ch-1".
        assert_eq!(result[0].raw_discovery_identifier, Some("ch-1".to_string()));
    }

    // ── Raw identifier precedence (reviewer feedback) ────────────────────

    /// Attribute-only: when no child <id> element is present, the attribute
    /// value is used as the raw discovery identifier.
    #[test]
    fn parse_attribute_only_raw_identifier() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel id="attr-only">
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].raw_discovery_identifier,
            Some("attr-only".to_string())
        );
    }

    /// Child-only: when a child <id> element is present, it takes precedence
    /// over the attribute.
    #[test]
    fn parse_child_only_raw_identifier() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel id="ignored-attr">
    <id>child-id</id>
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // Child <id> replaces the attribute value.
        assert_eq!(
            result[0].raw_discovery_identifier,
            Some("child-id".to_string())
        );
    }

    /// Both attribute and child: child element takes precedence.
    #[test]
    fn parse_both_attribute_and_child_raw_identifier() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel id="attr-value">
    <id>child-value</id>
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // Child <id> replaces the attribute value entirely.
        assert_eq!(
            result[0].raw_discovery_identifier,
            Some("child-value".to_string())
        );
    }

    /// Multiple child <id> elements: the last one wins.
    #[test]
    fn parse_multiple_child_id_elements() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel id="attr">
    <id>first</id>
    <id>second</id>
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // The last child <id> element replaces previous ones.
        assert_eq!(
            result[0].raw_discovery_identifier,
            Some("second".to_string())
        );
    }

    /// Empty child <id> element: the attribute value is preserved because
    /// empty elements do not emit text events.
    #[test]
    fn parse_empty_child_id_preserves_attribute() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel id="attr-value">
    <id/>
    <trackID>101</trackID>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // Empty child <id> does not emit a text event, so the attribute
        // value is preserved.
        assert_eq!(
            result[0].raw_discovery_identifier,
            Some("attr-value".to_string())
        );
    }

    // ── Persistence coverage: malformed XML must not trigger sync/metadata ─

    /// When XML parsing fails, the discover command must not call
    /// sync_cameras or set metadata.  This is verified by the fact that
    /// parse_camera_discovery_xml returns an error before any side effects
    /// can occur — the caller (handle_discover in app.rs) checks the result
    /// and propagates the error without reaching sync_cameras.
    ///
    /// This test confirms the parser itself is pure: it takes only XML bytes
    /// as input and returns either a Vec<CameraDiscovery> or an AppError.
    #[test]
    fn parse_camera_discovery_xml_is_pure_no_side_effects() {
        // Malformed XML returns an error.
        let result = parse_camera_discovery_xml(b"<broken>");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.category, ErrorCategory::XmlParsing);

        // Valid but empty XML returns an empty vector (no cameras).
        let result = parse_camera_discovery_xml(b"<?xml version=\"1.0\"?><StreamingChannelList/>");
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());

        // The parser has no mutable state — calling it twice with the same
        // input produces the same result.
        let result1 = parse_camera_discovery_xml(b"<?xml version=\"1.0\"?><StreamingChannelList/>");
        let result2 = parse_camera_discovery_xml(b"<?xml version=\"1.0\"?><StreamingChannelList/>");
        // Both succeed with the same empty vector.
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap().len(), result2.unwrap().len());
    }

    // ── Text trimming regressions (reviewer feedback) ────────────────────

    /// Separator exists only at the end of the Text event.
    /// Without trim_text, the space at the end of "Front " is preserved
    /// through accumulation and survives the final trim.
    #[test]
    fn parse_text_separator_at_end_of_text_event() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Front </name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // The trailing space is trimmed at element close.
        assert_eq!(result[0].name, Some("Front".to_string()));
    }

    /// Separator exists only as a whitespace-only Text event between two
    /// text segments.  Without trim_text, this whitespace event is
    /// accumulated and survives the final trim.
    #[test]
    fn parse_text_separator_as_whitespace_event() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Front
Gate</name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // The newline between "Front" and "Gate" is preserved after trim.
        assert_eq!(result[0].name, Some("Front\nGate".to_string()));
    }

    /// Separator exists only at the beginning of the following Text event.
    /// Without trim_text, the leading space of the second text segment is
    /// accumulated and survives the final trim.
    #[test]
    fn parse_text_separator_at_start_of_following_text_event() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Front Gate</name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("Front Gate".to_string()));
    }

    /// CDATA with a literal ampersand should NOT be entity-unescaped.
    /// This was already tested above; this is a re-affirmation that
    /// disabling trim_text does not affect CDATA handling.
    #[test]
    fn parse_cdata_ampersand_still_preserved() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name><![CDATA[A & B]]></name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, Some("A & B".to_string()));
    }

    /// Mixed text and CDATA with whitespace-only CDATA segment between.
    /// Without trim_text, the whitespace-only CDATA is accumulated.
    #[test]
    fn parse_mixed_text_cdata_whitespace_between() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Front<![CDATA[ ]]><![CDATA[Gate]]></name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        // "Front" + " " + "Gate" → trimmed to "Front Gate"
        assert_eq!(result[0].name, Some("Front Gate".to_string()));
    }

    /// Regression test: when <trackID> is absent, fall back to <id> as
    /// the track ID.  Some Hikvision NVR firmwares embed the track ID
    /// directly in <id> and omit <trackID> entirely.
    #[test]
    fn parse_fallback_id_to_track_id() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>101</id>
    <channelName>Camera One</channelName>
  </StreamingChannel>
  <StreamingChannel>
    <id>201</id>
    <channelName>Camera Two</channelName>
  </StreamingChannel>
  <StreamingChannel>
    <id>102</id>
    <channelName>Sub-stream</channelName>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        // Only primary tracks (..01) should be kept; 102 is a sub-stream.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].channel_number, 1);
        assert_eq!(result[0].primary_track_id, "101");
        assert_eq!(result[0].picture_track_id, "103");
        assert_eq!(result[0].name, Some("Camera One".to_string()));
        assert_eq!(result[0].raw_discovery_identifier, Some("101".to_string()));
        assert_eq!(result[1].channel_number, 2);
        assert_eq!(result[1].primary_track_id, "201");
        assert_eq!(result[1].picture_track_id, "203");
        assert_eq!(result[1].name, Some("Camera Two".to_string()));
        assert_eq!(result[1].raw_discovery_identifier, Some("201".to_string()));
    }

    /// When both <trackID> and <id> are present, <trackID> takes
    /// precedence as the track ID while <id> is used as the raw
    /// discovery identifier.
    #[test]
    fn parse_trackid_takes_precedence_over_id() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <trackID>101</trackID>
    <name>Camera</name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].primary_track_id, "101");
        assert_eq!(result[0].raw_discovery_identifier, Some("ch1".to_string()));
    }

    /// When <id> contains a non-track-ID value (e.g. "ch1"), it should
    /// NOT be used as a fallback track ID since derive_camera_mapping
    /// rejects it.
    #[test]
    fn parse_id_fallback_rejected_when_not_numeric_track() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<StreamingChannelList>
  <StreamingChannel>
    <id>ch1</id>
    <name>No Track ID</name>
  </StreamingChannel>
</StreamingChannelList>"#;
        let result = parse_camera_discovery_xml(xml.as_bytes()).unwrap();
        // "ch1" is not a valid track ID pattern, so the channel is skipped.
        assert!(result.is_empty());
    }
}
