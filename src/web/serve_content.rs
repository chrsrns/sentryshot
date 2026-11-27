// SPDX-License-Identifier: GPL-2.0-or-later

use axum::{
    body::{Body, Bytes},
    response::{IntoResponse, Response},
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use http_body::Frame;
use httpdate::HttpDate;
use pin_project::pin_project;
use std::{
    io::SeekFrom,
    pin::Pin,
    task::{Context, Poll},
    time::SystemTime,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
use tokio_util::io::ReaderStream;

// Replies to the request using the content in the
// provided ReadSeeker. The main benefit of serve_mp4_content over io::copy
// is that it handles Range requests properly, sets the MIME type, and
// handles If-Unmodified-Since and If-Modified-Since requests.
//
// If modtime is not the zero time or Unix epoch, serve_mp4_content
// includes it in a Last-Modified header in the response. If the
// request includes an If-Modified-Since header, serve_mp4_content uses
// modtime to decide whether the content needs to be sent at all.
//
// Content must be seeked to the beginning of the file.
#[allow(clippy::too_many_lines, clippy::unwrap_used)]
pub async fn serve_mp4_content<RS>(
    method: &Method,
    headers: &HeaderMap,
    last_modified: Option<std::time::SystemTime>,
    size: u64,
    mut content: RS,
) -> Response
where
    RS: AsyncRead + AsyncSeek + Unpin + Send + Sync + 'static,
{
    let last_modified = last_modified.map(LastModified::from);

    let mut response_headers = HeaderMap::new();
    if let Some(last_modified) = &last_modified {
        set_last_modified(&mut response_headers, last_modified);
    }

    let range_req = match check_preconditions(
        method,
        headers,
        response_headers.clone(),
        last_modified.as_ref(),
    ) {
        PreconditionsResult::Done(response) => return response,
        PreconditionsResult::Range(v) => v,
    };

    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));

    let mut response_code = StatusCode::OK;

    // handle Content-Range header.
    let mut send_size = size;
    let mut ranges = match parse_range(range_req, size) {
        Ok(v) => v,
        Err(e) => {
            if matches!(e, ParseRangeError::NoOverlap) {
                response_headers.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{size}")).unwrap(),
                );
            }
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                response_headers,
                e.to_string(),
            )
                .into_response();
        }
    };

    if sum_ranges_size(&ranges) > size {
        // The total number of bytes in all the ranges
        // is larger than the size of the file by
        // itself, so this is probably an attack, or a
        // dumb client. Ignore the range request.
        ranges = Vec::new();
    }

    if ranges.len() > 1 {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            response_headers,
            "Cannot serve multipart range requests",
        )
            .into_response();
    }

    if ranges.len() == 1 {
        // RFC 7233, Section 4.1:
        // "If a single part is being transferred, the server
        // generating the 206 response MUST generate a
        // Content-Range header field, describing what range
        // of the selected representation is enclosed, and a
        // payload consisting of the range.
        // ...
        // A server MUST NOT generate a multipart response to
        // a request for a single range, since a client that
        // does not request multiple parts might not support
        // multipart responses."
        let ra = &ranges[0];
        if let Err(e) = content.seek(SeekFrom::Start(ra.start)).await {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                response_headers,
                e.to_string(),
            )
                .into_response();
        }

        send_size = ra.length;
        response_code = StatusCode::PARTIAL_CONTENT;
        response_headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&ra.content_range(size)).unwrap(),
        );
    }

    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if get_header(headers, header::CONTENT_ENCODING).is_none() {
        response_headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&send_size.to_string()).unwrap(),
        );
    }

    let body = axum::body::Body::new(AsyncReadBody::limited(content, send_size));
    (response_code, response_headers, body).into_response()
}

enum PreconditionsResult {
    Done(Response),
    Range(Option<String>),
}

// Evaluates request preconditions and reports whether a precondition
// resulted in sending StatusNotModified or StatusPreconditionFailed.
fn check_preconditions(
    method: &Method,
    headers: &HeaderMap,
    response_headers: HeaderMap,
    last_modified: Option<&LastModified>,
) -> PreconditionsResult {
    // Only date-form preconditions are evaluated — no ETag is ever
    // emitted, so entity-tag checks (If-Match, If-None-Match, If-Range)
    // would have nothing to compare against.
    if check_if_unmodified_since(headers, last_modified) == CondResult::False {
        return PreconditionsResult::Done(
            (StatusCode::PRECONDITION_FAILED, response_headers, "1").into_response(),
        );
    }

    if check_if_modified_since(method, headers, last_modified) == CondResult::False {
        return PreconditionsResult::Done(not_modified_response(response_headers));
    }

    let range_header = get_header(headers, header::RANGE);
    PreconditionsResult::Range(range_header)
}

// condResult is the result of an HTTP request precondition check.
// See https://tools.ietf.org/html/rfc7232 section 3.
#[derive(Debug, PartialEq)]
enum CondResult {
    None,
    True,
    False,
}

fn get_header(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    let Ok(header) = headers.get(name)?.to_str() else {
        return None;
    };
    Some(header.to_owned())
}

fn check_if_unmodified_since(headers: &HeaderMap, modified: Option<&LastModified>) -> CondResult {
    let if_unmodified_since = headers
        .get(header::IF_UNMODIFIED_SINCE)
        .and_then(IfUnmodifiedSince::from_header_value);

    if if_unmodified_since.is_none() {
        return CondResult::None;
    }

    if let Some(since) = if_unmodified_since {
        let precondition = modified
            .as_ref()
            .is_some_and(|time| since.precondition_passes(time));

        if !precondition {
            return CondResult::False;
        }
        return CondResult::True;
    }
    CondResult::None
}

fn check_if_modified_since(
    method: &Method,
    headers: &HeaderMap,
    modified: Option<&LastModified>,
) -> CondResult {
    if method != Method::GET && method != Method::HEAD {
        return CondResult::None;
    }

    let if_modified_since = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(IfModifiedSince::from_header_value);

    if let Some(since) = if_modified_since {
        let unmodified = modified
            .as_ref()
            .is_some_and(|time| !since.is_modified(time));
        if unmodified {
            return CondResult::False;
        }
    }

    CondResult::True
}

struct LastModified(HttpDate);

impl From<SystemTime> for LastModified {
    fn from(time: SystemTime) -> Self {
        LastModified(time.into())
    }
}

struct IfModifiedSince(HttpDate);

impl IfModifiedSince {
    /// Check if the supplied time means the resource has been modified.
    fn is_modified(&self, last_modified: &LastModified) -> bool {
        self.0 < last_modified.0
    }

    /// convert a header value into a `IfModifiedSince`, invalid values are silentely ignored
    fn from_header_value(value: &HeaderValue) -> Option<IfModifiedSince> {
        std::str::from_utf8(value.as_bytes())
            .ok()
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .map(|time| IfModifiedSince(time.into()))
    }
}

struct IfUnmodifiedSince(HttpDate);

impl IfUnmodifiedSince {
    /// Check if the supplied time passes the precondtion.
    fn precondition_passes(&self, last_modified: &LastModified) -> bool {
        self.0 >= last_modified.0
    }

    /// Convert a header value into a `IfModifiedSince`, invalid values are silentely ignored
    fn from_header_value(value: &HeaderValue) -> Option<IfUnmodifiedSince> {
        std::str::from_utf8(value.as_bytes())
            .ok()
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .map(|time| IfUnmodifiedSince(time.into()))
    }
}

#[allow(clippy::unwrap_used)]
fn set_last_modified(headers: &mut HeaderMap, modtime: &LastModified) {
    headers.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&modtime.0.to_string()).unwrap(),
    );
}

fn not_modified_response(mut response_headers: HeaderMap) -> Response {
    // RFC 7232 section 4.1:
    // a sender SHOULD NOT generate representation metadata other than the
    // above listed fields unless said metadata exists for the purpose of
    // guiding cache updates (e.g., Last-Modified might be useful if the
    // response does not have an ETag field).
    response_headers.remove(header::CONTENT_TYPE);
    response_headers.remove(header::CONTENT_LENGTH);
    response_headers.remove(header::CONTENT_ENCODING);
    if response_headers.get(header::ETAG).is_some() {
        response_headers.remove(header::LAST_MODIFIED);
    }

    (StatusCode::NOT_MODIFIED, response_headers).into_response()
}

// Specifies the byte range to be sent to the client.
#[derive(Debug)]
struct HttpRange {
    start: u64,
    length: u64,
}

impl HttpRange {
    fn content_range(&self, size: u64) -> String {
        format!(
            "bytes {}-{}/{}",
            self.start,
            self.start + self.length - 1,
            size,
        )
    }

}

#[derive(Debug, Error)]
enum ParseRangeError {
    #[error("invalid range")]
    InvalidRange,

    // If first-byte-pos of all of the byte-range-spec values is greater than the content size.
    #[error("invalid range: failed to overlap")]
    NoOverlap,
}

// parses a Range header string as per RFC 7233.
// errNoOverlap is returned if none of the ranges overlap.
#[allow(clippy::items_after_statements)]
fn parse_range(s: Option<String>, size: u64) -> Result<Vec<HttpRange>, ParseRangeError> {
    use ParseRangeError::*;
    let Some(s) = s else {
        return Ok(Vec::new());
    };

    const B: &str = "bytes=";

    if !s.starts_with(B) {
        return Err(InvalidRange);
    }

    let mut ranges = Vec::new();
    let mut no_overlap = false;

    for ra in s[B.len()..].split(',') {
        let ra = trim_text_proto_string(Some(ra.to_owned()));
        let Some(ra) = ra else { continue };

        let i = ra.chars().position(|c| c == '-').ok_or(InvalidRange)?;

        let start = trim_text_proto_string(Some(ra[..i].to_owned()));
        let end = trim_text_proto_string(Some(ra[i + 1..].to_owned()));

        let mut r = HttpRange {
            start: 0,
            length: 0,
        };

        if let Some(start) = start {
            let i: u64 = start.parse().map_err(|_| InvalidRange)?;
            if i >= size {
                // If the range begins after the size of the content,
                // then it does not overlap.
                no_overlap = true;
                continue;
            }
            r.start = i;
            if let Some(end) = end {
                let mut i: u64 = end.parse().map_err(|_| InvalidRange)?;
                if r.start > i {
                    return Err(InvalidRange);
                }
                if i >= size {
                    i = size - 1;
                }
                r.length = i - r.start + 1;
            } else {
                // If no end is specified, range extends to end of the file.
                r.length = size - r.start;
            }
        } else {
            // If no start is specified, end specifies the
            // range start relative to the end of the file,
            // and we are dealing with <suffix-length>
            // which has to be a non-negative integer as per
            // RFC 7233 Section 2.1 "Byte-Ranges".
            let Some(end) = end else {
                return Err(InvalidRange);
            };
            if end.as_bytes()[0] == b'-' {
                return Err(InvalidRange);
            }
            let mut i: u64 = end.parse().map_err(|_| InvalidRange)?;
            if i > size {
                i = size;
            }
            r.start = size - i;
            r.length = size - r.start;
        }

        ranges.push(r);
    }

    if no_overlap && ranges.is_empty() {
        // The specified ranges did not overlap with the content.
        return Err(NoOverlap);
    }

    Ok(ranges)
}

#[allow(clippy::unwrap_used)]
fn trim_text_proto_string(s: Option<String>) -> Option<String> {
    let mut s = s?;
    while !s.is_empty() && is_ascii_space(s.as_bytes()[0]) {
        s = s[1..].to_string();
    }
    while !s.is_empty() && is_ascii_space(*s.as_bytes().last().unwrap()) {
        s.pop();
    }
    if s.is_empty() {
        return None;
    }
    Some(s)
}

fn is_ascii_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

fn sum_ranges_size(ranges: &[HttpRange]) -> u64 {
    let mut size = 0;
    for ra in ranges {
        size += ra.length;
    }
    size
}


/// Adapter that turns an [`impl AsyncRead`][tokio::io::AsyncRead] to an [`impl Body`][http_body::Body].
#[derive(Debug)]
#[pin_project]
pub struct AsyncReadBody {
    #[pin]
    body: Body,
}

const CAPACITY: usize = 65536;

impl AsyncReadBody {
    fn limited<R>(read: R, max_read_bytes: u64) -> Self
    where
        R: AsyncRead + Send + 'static,
    {
        Self {
            body: Body::from_stream(ReaderStream::with_capacity(
                read.take(max_read_bytes),
                CAPACITY,
            )),
        }
    }
}

impl http_body::Body for AsyncReadBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().body.poll_frame(cx)
    }
}
