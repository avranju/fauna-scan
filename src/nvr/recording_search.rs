//! Time-bounded video recording lookup for web image details.

use std::sync::Arc;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use uuid::Uuid;

use crate::domain::{Timestamp, TrackId};
use crate::error::{AppError, AppResult, ErrorCategory};

use super::NvrTransport;

/// One NVR recording search result.
#[derive(Debug, Clone)]
pub struct RecordingMatch {
    pub track_id: String,
    pub start_at: Timestamp,
    pub end_at: Timestamp,
    pub playback_uri: String,
}

/// Authenticated recording-search client.
#[derive(Clone)]
pub struct RecordingSearchClient {
    transport: Arc<NvrTransport>,
    max_results: u64,
}

impl RecordingSearchClient {
    pub fn new(transport: Arc<NvrTransport>, max_results: u64) -> Self {
        Self {
            transport,
            max_results: max_results.max(1),
        }
    }

    /// Find the recording which best covers the target time.
    pub async fn find(
        &self,
        track_id: &TrackId,
        start: Timestamp,
        end: Timestamp,
        target: Timestamp,
    ) -> AppResult<Option<RecordingMatch>> {
        let mut position = 0_u64;
        let mut matches = Vec::new();
        let mut complete = false;

        for _ in 0..100 {
            let search_id = Uuid::new_v4();
            let body =
                serialize_request(search_id, track_id, start, end, position, self.max_results);
            let response = self
                .transport
                .post("/ISAPI/ContentMgmt/search", body.into_bytes())
                .await?;
            let bytes = response.bytes().await.map_err(|error| {
                AppError::with_source(
                    ErrorCategory::Network,
                    "recording_search_body",
                    "failed to read NVR recording search response",
                    error,
                )
            })?;
            if bytes.len() > 2_000_000 {
                return Err(AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "recording_search_body",
                    "NVR recording search response exceeds the size limit",
                ));
            }
            let page = parse_response(&bytes, search_id)?;
            let count = page.items.len() as u64;
            matches.extend(page.items);
            if !page.more {
                complete = true;
                break;
            }
            if count == 0 {
                return Err(AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "recording_search",
                    "NVR recording pagination did not make progress",
                ));
            }
            position = position.checked_add(count).ok_or_else(|| {
                AppError::new(
                    ErrorCategory::InvalidNvrResponse,
                    "recording_search",
                    "NVR recording pagination position overflowed",
                )
            })?;
        }

        if !complete {
            return Err(AppError::new(
                ErrorCategory::InvalidNvrResponse,
                "recording_search",
                "NVR recording search exceeded the pagination limit",
            ));
        }

        let target = *target.as_datetime();
        matches.retain(|item| {
            *item.start_at.as_datetime() <= target && target <= *item.end_at.as_datetime()
        });
        matches.sort_by_key(|item| {
            let covers_requested = item.start_at <= start && item.end_at >= end;
            let duration = (*item.end_at.as_datetime() - *item.start_at.as_datetime())
                .num_milliseconds()
                .max(0);
            (!covers_requested, duration)
        });
        Ok(matches.into_iter().next())
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn serialize_request(
    search_id: Uuid,
    track_id: &TrackId,
    start: Timestamp,
    end: Timestamp,
    position: u64,
    max_results: u64,
) -> String {
    let start = start
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end = end
        .as_datetime()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <CMSearchDescription>\
           <searchID>{search_id}</searchID>\
           <trackList><trackID>{track}</trackID></trackList>\
           <timeSpanList><timeSpan><startTime>{start}</startTime><endTime>{end}</endTime></timeSpan></timeSpanList>\
           <contentTypeList><contentType>video</contentType></contentTypeList>\
           <maxResults>{max_results}</maxResults>\
           <searchResultPostion>{position}</searchResultPostion>\
         </CMSearchDescription>",
        track = escape_xml(track_id.as_str()),
        start = escape_xml(&start),
        end = escape_xml(&end),
    )
}

struct ParsedPage {
    more: bool,
    items: Vec<RecordingMatch>,
}

fn parse_response(xml: &[u8], expected_search_id: Uuid) -> AppResult<ParsedPage> {
    let mut reader = Reader::from_reader(xml);
    let mut buffer = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut response_search_id = None;
    let mut response_status = None;
    let mut status_string = String::new();
    let mut inside_item = false;
    let mut track_id = None;
    let mut start_at = None;
    let mut end_at = None;
    let mut playback_uri = None;
    let mut items = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(event)) => {
                let name = reader
                    .decoder()
                    .decode(event.name().local_name().as_ref())
                    .map_err(|_| xml_error("invalid element name encoding"))?
                    .into_owned();
                if name == "searchMatchItem" {
                    inside_item = true;
                    track_id = None;
                    start_at = None;
                    end_at = None;
                    playback_uri = None;
                }
                stack.push(name);
                text.clear();
            }
            Ok(Event::Text(event)) => {
                text.push_str(
                    &event
                        .unescape()
                        .map_err(|_| xml_error("invalid XML entity"))?,
                );
            }
            Ok(Event::CData(event)) => {
                let value = reader
                    .decoder()
                    .decode(event.as_ref())
                    .map_err(|_| xml_error("invalid CDATA encoding"))?;
                text.push_str(&value);
            }
            Ok(Event::End(event)) => {
                let name = reader
                    .decoder()
                    .decode(event.name().local_name().as_ref())
                    .map_err(|_| xml_error("invalid element name encoding"))?
                    .into_owned();
                let value = text.trim().to_string();
                if inside_item {
                    match name.as_str() {
                        "trackID" if track_id.is_none() => track_id = Some(value),
                        "startTime" if start_at.is_none() => start_at = value.parse().ok(),
                        "endTime" if end_at.is_none() => end_at = value.parse().ok(),
                        "playbackURI" if playback_uri.is_none() => playback_uri = Some(value),
                        "searchMatchItem" => {
                            inside_item = false;
                            if let (
                                Some(track_id),
                                Some(start_at),
                                Some(end_at),
                                Some(playback_uri),
                            ) = (
                                track_id.take(),
                                start_at.take(),
                                end_at.take(),
                                playback_uri.take(),
                            ) && !playback_uri.is_empty()
                            {
                                items.push(RecordingMatch {
                                    track_id,
                                    start_at,
                                    end_at,
                                    playback_uri,
                                });
                            }
                        }
                        _ => {}
                    }
                } else {
                    match name.as_str() {
                        "searchID" => {
                            response_search_id =
                                Uuid::parse_str(value.trim_matches(['{', '}'])).ok()
                        }
                        "responseStatus" => response_status = parse_bool(&value),
                        "responseStatusStrg" => status_string = value,
                        _ => {}
                    }
                }
                if stack.pop().as_deref() != Some(name.as_str()) {
                    return Err(xml_error("mismatched XML elements"));
                }
                text.clear();
            }
            Ok(Event::Decl(_) | Event::Comment(_)) => {}
            Ok(_) => {}
            Err(_) => return Err(xml_error("malformed NVR recording search XML")),
        }
        buffer.clear();
    }

    if response_search_id != Some(expected_search_id) {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "recording_search_parse",
            "NVR recording searchID did not match the request",
        ));
    }
    if response_status != Some(true) {
        return Err(AppError::new(
            ErrorCategory::InvalidNvrResponse,
            "recording_search_parse",
            "NVR reported a failed recording search",
        ));
    }
    Ok(ParsedPage {
        more: status_string.eq_ignore_ascii_case("MORE"),
        items,
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn xml_error(message: &'static str) -> AppError {
    AppError::new(ErrorCategory::XmlParsing, "recording_search_parse", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recording_search_result() {
        let search_id = Uuid::new_v4();
        let xml = format!(
            r#"<CMSearchResult>
                <searchID>{search_id}</searchID>
                <responseStatus>true</responseStatus>
                <responseStatusStrg>OK</responseStatusStrg>
                <matchList><searchMatchItem>
                  <trackID>301</trackID>
                  <timeSpan><startTime>2026-07-21T10:00:00Z</startTime><endTime>2026-07-21T10:10:00Z</endTime></timeSpan>
                  <mediaSegmentDescriptor><playbackURI>rtsp://nvr/Streaming/tracks/301/?starttime=1&amp;endtime=2</playbackURI></mediaSegmentDescriptor>
                </searchMatchItem></matchList>
            </CMSearchResult>"#
        );
        let parsed = parse_response(xml.as_bytes(), search_id).unwrap();
        assert!(!parsed.more);
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].track_id, "301");
        assert_eq!(
            parsed.items[0].playback_uri,
            "rtsp://nvr/Streaming/tracks/301/?starttime=1&endtime=2"
        );
    }

    #[test]
    fn rejects_mismatched_search_id() {
        let expected = Uuid::new_v4();
        let actual = Uuid::new_v4();
        let xml = format!(
            "<CMSearchResult><searchID>{actual}</searchID><responseStatus>true</responseStatus><responseStatusStrg>OK</responseStatusStrg></CMSearchResult>"
        );
        let error = parse_response(xml.as_bytes(), expected).err().unwrap();
        assert_eq!(error.category, ErrorCategory::InvalidNvrResponse);
    }
}
