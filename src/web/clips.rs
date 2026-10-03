//! Bounded authenticated RTSP -> H.264/AAC MP4 preparation and range delivery.

use super::*;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::{Duration as StdDuration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

const MAX_CLIP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHED_CLIPS: usize = 8;
const CLIP_TTL: StdDuration = StdDuration::from_secs(15 * 60);

struct Clip {
    directory: tempfile::TempDir,
    length: u64,
    filename: String,
    expires: Instant,
}

#[derive(Clone)]
pub(super) struct ClipStore {
    executable: Option<PathBuf>,
    limit: Arc<Semaphore>,
    files: Arc<Mutex<BTreeMap<String, Arc<Clip>>>>,
}

impl ClipStore {
    pub(super) fn new(config: &WebConfig) -> Self {
        let executable = config
            .clips_enabled
            .then(|| find_executable(&config.ffmpeg_path))
            .flatten();
        Self {
            executable,
            limit: Arc::new(Semaphore::new(config.max_concurrent_clip_encodes)),
            files: Default::default(),
        }
    }

    pub(super) fn available(&self) -> bool {
        self.executable.is_some()
    }
}

fn find_executable(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if path.components().count() > 1 || path.is_absolute() {
        return executable(path).then(|| path.to_path_buf());
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(path))
        .find(|path| executable(path))
}

fn clip_error(status: StatusCode, code: &'static str, message: &str) -> WebError {
    WebError {
        status,
        code,
        message: message.into(),
    }
}

// Encode literal credentials exactly once, including percent signs in passwords.
fn credential(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn authenticated_uri(uri: &str, config: &NvrConfig) -> Result<String, WebError> {
    let mut url = Url::parse(uri).map_err(|_| WebError::bad_request("Invalid clip URI"))?;
    let password = config.password.as_ref().ok_or_else(|| {
        clip_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "nvr_credentials_missing",
            "NVR credentials are not configured.",
        )
    })?;
    url.set_username(&credential(&config.username))
        .map_err(|_| WebError::bad_request("Invalid clip URI"))?;
    url.set_password(Some(&credential(password.expose())))
        .map_err(|_| WebError::bad_request("Invalid clip URI"))?;
    Ok(url.to_string())
}

pub(super) async fn prepare(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<i64>,
    Query(params): Query<RecordingParams>,
) -> Result<Json<Value>, WebError> {
    let executable = state.clips.executable.as_ref().ok_or_else(|| {
        clip_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "clips_unavailable",
            "Browser clips require FFmpeg on the server and web.clips_enabled = true.",
        )
    })?;
    let _permit = state.clips.limit.clone().try_acquire_owned().map_err(|_| {
        clip_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "clip_busy",
            "The server is preparing another clip. Try again shortly.",
        )
    })?;
    let lookup = lookup_recording(&state, id, params).await?;
    let uri = lookup
        .playback_uri
        .ok_or_else(|| WebError::not_found("No recording covers this capture time"))?;
    let authenticated = authenticated_uri(&uri, &state.nvr)?;
    let directory = tempfile::Builder::new()
        .prefix("fauna-clip-")
        .tempdir()
        .map_err(|_| {
            clip_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "clip_storage_failed",
                "Could not create temporary clip storage.",
            )
        })?;
    let output = directory.path().join("clip.mp4");
    // Stderr can contain the credential-bearing input URI. Never log or return it.
    // kill_on_drop also handles HTTP disconnects and the outer request timeout.
    let mut child = tokio::process::Command::new(executable)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-rtsp_transport",
            "tcp",
            "-timeout",
            "15000000",
            "-threads",
            "2",
            "-i",
        ])
        .arg(authenticated)
        .args([
            // A nonempty MP4 can contain only headers (or audio). Require
            // packets in every mapped stream, including the required video.
            "-abort_on",
            "empty_output_stream",
            "-t",
            &lookup.duration.to_string(),
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(min(1920\\,iw)/2)*2:-2",
            "-threads",
            "2",
            "-filter_threads",
            "1",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
            "-fs",
            &MAX_CLIP_BYTES.to_string(),
            "-f",
            "mp4",
        ])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            clip_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "ffmpeg_unavailable",
                "Could not start FFmpeg. Check the server installation.",
            )
        })?;
    let deadline =
        StdDuration::from_secs(lookup.duration.saturating_mul(2).saturating_add(30).max(90));
    let result = tokio::time::timeout(deadline, child.wait()).await;
    match result {
        Ok(Ok(status)) if status.success() => {}
        Err(_) => {
            let _ = child.kill().await;
            return Err(clip_error(
                StatusCode::GATEWAY_TIMEOUT,
                "clip_timeout",
                "Clip preparation timed out. Try a shorter interval.",
            ));
        }
        _ => {
            return Err(clip_error(
                StatusCode::BAD_GATEWAY,
                "clip_encoding_failed",
                "Could not prepare the recording. Check NVR RTSP connectivity, credentials, and FFmpeg H.264/AAC support.",
            ));
        }
    }
    let length = tokio::fs::metadata(&output)
        .await
        .map_err(|_| {
            clip_error(
                StatusCode::BAD_GATEWAY,
                "clip_empty",
                "The NVR did not provide a playable clip.",
            )
        })?
        .len();
    if length == 0 || length >= MAX_CLIP_BYTES {
        return Err(clip_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "clip_size_limit",
            "Clip exceeds the 64 MiB limit or is empty. Try a shorter interval.",
        ));
    }
    let token = uuid::Uuid::new_v4().to_string();
    let clip = Arc::new(Clip {
        directory,
        length,
        filename: format!(
            "image-{id}-{}.mp4",
            lookup.start.as_datetime().format("%Y%m%dT%H%M%SZ")
        ),
        expires: Instant::now() + CLIP_TTL,
    });
    {
        let mut files = state.clips.files.lock().await;
        files.retain(|_, clip| clip.expires > Instant::now());
        if files.len() >= MAX_CACHED_CLIPS
            && let Some(oldest) = files
                .iter()
                .min_by_key(|(_, clip)| clip.expires)
                .map(|(token, _)| token.clone())
        {
            files.remove(&oldest);
        }
        files.insert(token.clone(), clip);
    }
    let files = Arc::downgrade(&state.clips.files);
    let expired_token = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CLIP_TTL).await;
        if let Some(files) = files.upgrade() {
            files.lock().await.remove(&expired_token);
        }
    });
    Ok(Json(json!({
        "playback_url": format!("/api/v1/clips/{token}"),
        "download_url": format!("/api/v1/clips/{token}?download=true"),
        "expires_in_seconds": CLIP_TTL.as_secs(),
        "requested_start_at": lookup.start.to_string(),
        "requested_end_at": lookup.end.to_string()
    })))
}

#[derive(Deserialize)]
pub(super) struct ClipParams {
    #[serde(default)]
    download: bool,
}

fn byte_range(header: Option<&str>, length: u64) -> Result<(u64, u64), ()> {
    let Some(header) = header else {
        return Ok((0, length - 1));
    };
    let range = header.strip_prefix("bytes=").ok_or(())?;
    let (start, end) = range.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok((length.saturating_sub(suffix), length - 1));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    if start >= length || end < start {
        return Err(());
    }
    Ok((start, end))
}

pub(super) async fn serve(
    State(state): State<WebState>,
    AxumPath(token): AxumPath<String>,
    Query(params): Query<ClipParams>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let clip = {
        let mut files = state.clips.files.lock().await;
        files.retain(|_, clip| clip.expires > Instant::now());
        files.get(&token).cloned()
    }
    .ok_or_else(|| WebError::not_found("Clip expired or unavailable. Prepare the clip again."))?;
    let range = headers.get("range");
    let (start, end) = match byte_range(range.and_then(|header| header.to_str().ok()), clip.length)
    {
        Ok(bounds) => bounds,
        Err(()) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [
                    ("content-range", format!("bytes */{}", clip.length)),
                    ("accept-ranges", "bytes".into()),
                ],
            )
                .into_response());
        }
    };
    let mut file = tokio::fs::File::open(clip.directory.path().join("clip.mp4"))
        .await
        .map_err(|_| WebError::not_found("Clip file unavailable. Prepare the clip again."))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| WebError::not_found("Clip file unavailable"))?;
    let length = end - start + 1;
    // Hold the temporary directory until the response body has finished streaming,
    // even if the cache entry expires or is evicted during playback.
    let stream = futures_util::stream::try_unfold(
        (file.take(length), clip.clone()),
        |(mut reader, clip)| async move {
            let mut buffer = vec![0; 64 * 1024];
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            buffer.truncate(count);
            Ok(Some((bytes::Bytes::from(buffer), (reader, clip))))
        },
    );
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert("accept-ranges", HeaderValue::from_static("bytes"));
    headers.insert(
        "content-length",
        HeaderValue::from_str(&length.to_string()).unwrap(),
    );
    let disposition = if params.download {
        "attachment"
    } else {
        "inline"
    };
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disposition}; filename=\"{}\"", clip.filename)).unwrap(),
    );
    if range.is_some() {
        headers.insert(
            "content-range",
            HeaderValue::from_str(&format!("bytes {start}-{end}/{}", clip.length)).unwrap(),
        );
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_browser_seeking_and_reject_invalid_requests() {
        assert_eq!(byte_range(None, 100), Ok((0, 99)));
        assert_eq!(byte_range(Some("bytes=10-20"), 100), Ok((10, 20)));
        assert_eq!(byte_range(Some("bytes=90-"), 100), Ok((90, 99)));
        assert_eq!(byte_range(Some("bytes=-10"), 100), Ok((90, 99)));
        assert_eq!(byte_range(Some("bytes=10-200"), 100), Ok((10, 99)));
        for value in [
            "bytes=100-",
            "bytes=-0",
            "bytes=20-10",
            "bytes=0-1,4-5",
            "bad",
        ] {
            assert!(byte_range(Some(value), 100).is_err());
        }
    }

    #[test]
    fn literal_passwords_and_usernames_are_url_encoded_once() {
        assert_eq!(credential("u@s:er"), "u%40s%3Aer");
        assert_eq!(credential("p%2F/@?#"), "p%252F%2F%40%3F%23");
    }

    async fn recording_fixture() -> (tempfile::TempDir, WebState, wiremock::MockServer) {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let (root, mut state) = super::super::tests::test_state().await;
        let server = MockServer::start().await;
        let base = Url::parse(&server.uri()).unwrap();
        state.nvr.host = base.host_str().unwrap().into();
        state.nvr.port = base.port().unwrap();
        state.nvr.username = "u@ser".into();
        state.nvr.password = Some(crate::configuration::Secret::new("p%2F/@?#".into()));
        let transport = Arc::new(NvrTransport::from_config(&state.nvr).unwrap());
        state.recording_search = Arc::new(RecordingSearchClient::new(transport, 50));
        Mock::given(method("POST")).and(path("/ISAPI/ContentMgmt/search"))
            .respond_with(|request: &wiremock::Request| {
                let body = std::str::from_utf8(&request.body).unwrap();
                assert!(body.contains("2026-07-21T09:59:50Z"));
                assert!(body.contains("2026-07-21T10:00:20Z"));
                assert!(body.contains("<trackID>301</trackID>"));
                let id = body.split("<searchID>").nth(1).unwrap().split("</searchID>").next().unwrap();
                ResponseTemplate::new(200).set_body_string(format!(
                    "<CMSearchResult><searchID>{id}</searchID><responseStatus>true</responseStatus><responseStatusStrg>OK</responseStatusStrg><matchList><searchMatchItem><trackID>301</trackID><timeSpan><startTime>2026-07-21T09:00:00Z</startTime><endTime>2026-07-21T11:00:00Z</endTime></timeSpan><mediaSegmentDescriptor><playbackURI>rtsp://127.0.0.1:8080/Streaming/tracks/301/?starttime=old&amp;endtime=old&amp;name=segment&amp;size=123</playbackURI></mediaSegmentDescriptor></searchMatchItem></matchList></CMSearchResult>"))
            }).mount(&server).await;
        (root, state, server)
    }

    #[tokio::test]
    async fn prepares_authenticated_bounded_clips_and_serves_ranges_and_downloads() {
        use axum::http::Request;
        use std::os::unix::fs::PermissionsExt;
        use tower::ServiceExt;

        let (_root, mut state, _server) = recording_fixture().await;
        // An executable fixture asserts the credentials, TCP transport and bounded
        // duration delivered to FFmpeg without needing a real NVR in the test suite.
        let runner = tempfile::tempdir().unwrap();
        let executable = runner.path().join("ffmpeg-fixture");
        std::fs::write(&executable, r#"#!/usr/bin/env python3
import sys
args = sys.argv[1:]
assert args[args.index('-rtsp_transport') + 1] == 'tcp'
assert args[args.index('-t') + 1] == '30'
assert args[args.index('-i') + 1] == 'rtsp://u%40ser:p%252F%2F%40%3F%23@127.0.0.1:554/Streaming/tracks/301/?starttime=20260721T095950Z&endtime=20260721T100020Z'
assert args[args.index('-c:v') + 1] == 'libx264'
assert args[args.index('-c:a') + 1] == 'aac'
with open(args[-1], 'wb') as output:
    output.write(b'0123456789')
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        state.web.ffmpeg_path = executable;
        state.clips = ClipStore::new(&state.web);
        let app = router(state.clone());
        let uri = "/api/v1/images/1/clip?pre_roll_seconds=10&post_roll_seconds=20";
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 10000)
            .await
            .unwrap();
        let prepared: Value = serde_json::from_slice(&bytes).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("u%40ser") && !text.contains("p%252F"));
        let playback = prepared["playback_url"].as_str().unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(playback)
                    .header("Range", "bytes=2-5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"], "bytes 2-5/10");
        assert_eq!(response.headers()[CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 100)
                .await
                .unwrap()
                .as_ref(),
            b"2345"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(prepared["download_url"].as_str().unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.headers()[CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 100)
                .await
                .unwrap()
                .as_ref(),
            b"0123456789"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(playback)
                    .header("Range", "bytes=100-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let _permit = state.clips.limit.clone().acquire_owned().await.unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let mut files = state.clips.files.lock().await;
        let token = playback.rsplit('/').next().unwrap();
        let directory = files[token].directory.path().to_path_buf();
        files.clear();
        drop(files);
        assert!(!directory.exists());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(playback)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg with lavfi, libx264, and AAC support"]
    async fn real_ffmpeg_rejects_empty_video_before_caching() {
        use axum::http::Request;
        use std::os::unix::fs::PermissionsExt;
        use tower::ServiceExt;

        let ffmpeg = find_executable(Path::new("ffmpeg")).expect("FFmpeg must be installed");
        let (_root, mut state, _server) = recording_fixture().await;
        let runner = tempfile::tempdir().unwrap();
        let executable = runner.path().join("ffmpeg-wrapper");
        let mode = runner.path().join("mode");
        let audit = runner.path().join("audit.json");
        // Replace only the RTSP input with local synthetic streams. Forward the
        // production output arguments, so removing its empty-stream guard makes
        // this endpoint regression fail. The baseline reproduces successful
        // encoding of nonempty output with no video frames.
        let script = format!(
            r#"#!/usr/bin/env python3
import json, pathlib, subprocess, sys
ffmpeg = {ffmpeg}
mode = pathlib.Path({mode}).read_text()
args = sys.argv[1:]
output_args = args[args.index('-i') + 2:]
source = 'color=size=16x16:rate=1:duration=1'
if mode == 'audio_only':
    source += '[out0];sine=duration=1[out1]'
    output_args[-1:-1] = ['-vf', 'select=0']
else:
    output_args[-1:-1] = ['-frames:v', '0' if mode == 'header_only' else '1']
prefix = [ffmpeg, '-hide_banner', '-loglevel', 'error', '-nostdin', '-y', '-f', 'lavfi', '-i', source]
baseline_args = output_args.copy()
if '-abort_on' in baseline_args:
    index = baseline_args.index('-abort_on')
    del baseline_args[index:index + 2]
baseline = subprocess.run(prefix + baseline_args, capture_output=True)
output = pathlib.Path(output_args[-1])
length = output.stat().st_size
result = subprocess.run(prefix + output_args, capture_output=True)
pathlib.Path({audit}).write_text(json.dumps({{'baseline_status': baseline.returncode, 'baseline_length': length, 'status': result.returncode, 'directory': str(output.parent)}}))
sys.exit(result.returncode)
"#,
            ffmpeg = serde_json::to_string(&ffmpeg.to_string_lossy()).unwrap(),
            mode = serde_json::to_string(&mode.to_string_lossy()).unwrap(),
            audit = serde_json::to_string(&audit.to_string_lossy()).unwrap(),
        );
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        state.web.ffmpeg_path = executable;
        state.clips = ClipStore::new(&state.web);
        let app = router(state.clone());
        for scenario in ["header_only", "audio_only", "video"] {
            std::fs::write(&mode, scenario).unwrap();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/images/1/clip?pre_roll_seconds=10&post_roll_seconds=20")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let expected = if scenario == "video" {
                StatusCode::OK
            } else {
                StatusCode::BAD_GATEWAY
            };
            assert_eq!(response.status(), expected, "scenario: {scenario}");
            let bytes = axum::body::to_bytes(response.into_body(), 10000)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let details: Value = serde_json::from_slice(&std::fs::read(&audit).unwrap()).unwrap();
            assert_eq!(details["baseline_status"], 0);
            assert!(details["baseline_length"].as_u64().unwrap() > 0);
            if scenario == "video" {
                assert_eq!(details["status"], 0);
                assert!(body["playback_url"].is_string());
                assert_eq!(state.clips.files.lock().await.len(), 1);
            } else {
                assert_ne!(details["status"], 0);
                assert_eq!(body["error"]["code"], "clip_encoding_failed");
                assert!(body.get("playback_url").is_none());
                assert!(body.get("download_url").is_none());
                assert!(state.clips.files.lock().await.is_empty());
                assert!(!Path::new(details["directory"].as_str().unwrap()).exists());
            }
        }
    }
}
