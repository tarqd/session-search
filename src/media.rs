//! Keeping binary payloads out of the text index.
//!
//! A transcript carries images the way it carries prose: inline, in the same fields. A pasted
//! screenshot is a `{"type":"image","source":{"type":"base64",...}}` block in `message.content`;
//! a `Read` of a PNG comes back as `{"type":"image","file":{"base64":...}}` in `toolUseResult`;
//! a `Bash` command that emits image bytes sets `isImage` and puts them in `stdout`. One phone
//! photo is ~300 KB of base64 on a single JSONL line.
//!
//! None of it is text. It matches no query anyone would type, it dilutes the term statistics
//! that rank the documents that *are* text, and it costs its own size again in the index. What
//! is worth keeping is everything *about* the payload — its media type, its size, and the path
//! it came from, which lives in the tool input and is untouched by any of this.
//!
//! So this module never indexes the bytes and always indexes a description of them. Two layers,
//! because transcripts are an open format (`docs/TRANSCRIPT-FORMAT.md` §4-6) and the shapes
//! below are the ones that exist *today*:
//!
//! 1. [`describe_block`] and [`describe_payload`] recognise the known media shapes and render
//!    the good placeholder — `[image/jpeg 231 KiB]` — from the metadata beside the bytes.
//! 2. [`scrub`] and [`redacted`] recognise a base64 blob *by looking at it*, wherever it turns
//!    up: a shape nobody modelled, a notebook cell's `image/png` output, a data URI pasted into
//!    a prompt. This is the layer that holds when the format moves.

use std::borrow::Cow;

use serde_json::{Map, Value};

/// Shortest run of base64 characters [`scrub`] will elide.
///
/// Under 512 bytes there is nothing worth saving and the risk of eliding something real is not
/// zero; over it, an unbroken 512-character run of `[A-Za-z0-9+/=]` that carries both cases and
/// a digit is not prose, not code (no punctuation survives the alphabet) and not a path. It is
/// an encoded payload, and the smallest image anyone pastes is several times this long.
const BLOB_MIN: usize = 512;

/// The `type`s of a `toolUseResult` whose `file` holds bytes rather than text.
const BINARY_RESULTS: &[&str] = &["image", "pdf", "audio", "video"];

fn is_b64(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
}

/// Does this run of base64 characters look like encoded bytes rather than a long word?
///
/// Mixed case *and* a digit: base64 of anything binary has all three within a few characters,
/// while the false positives this rules out — a hex digest, a `SCREAMING_RUN`, a lowercase
/// identifier chain — have at most two.
fn looks_encoded(run: &str) -> bool {
    run.len() >= BLOB_MIN
        && run.bytes().any(|b| b.is_ascii_lowercase())
        && run.bytes().any(|b| b.is_ascii_uppercase())
        && run.bytes().any(|b| b.is_ascii_digit())
}

/// Decoded size of a base64 string, from its length and padding — the bytes the payload
/// *would* occupy, which is the number a reader recognises as "how big is that image".
fn decoded_len(b64: &str) -> usize {
    let pad = b64.bytes().rev().take_while(|b| *b == b'=').count();
    (b64.len() / 4 * 3).saturating_sub(pad)
}

/// `231 KiB`, `1.4 MiB`, `900 B`.
fn human_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let n = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if n < KIB * KIB {
        format!("{:.0} KiB", n / KIB)
    } else {
        format!("{:.1} MiB", n / (KIB * KIB))
    }
}

/// The one placeholder shape: `[image/jpeg 231 KiB]`, `[image]`, `[base64 12 KiB]`.
///
/// It reads as a description in a snippet and it tokenizes into terms someone would actually
/// search — `image`, `jpeg`, `pdf` — because the default tokenizer splits `image/jpeg` into the
/// same two adjacent terms the placeholder holds, so the phrase query for it matches.
fn placeholder(label: &str, bytes: Option<usize>) -> String {
    match bytes {
        Some(b) => format!("[{label} {}]", human_size(b)),
        None => format!("[{label}]"),
    }
}

/// Placeholder for a bare base64 run, sized from the run itself.
pub fn blob_placeholder(b64: &str) -> String {
    placeholder("base64", Some(decoded_len(b64)))
}

fn str_at<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Describe an API content block of type `image` or `document` — the pasted-screenshot shape,
/// and what a `tool_result` carries when a tool returned an image.
///
/// Both the API form (`{source:{type:"base64",media_type,data}}`) and the MCP form
/// (`{data,mimeType}`) are seen in the wild; `source.type:"url"` and `"file"` carry no bytes at
/// all, and their locator is worth keeping.
pub fn describe_block(kind: &str, source: Option<&Value>, extra: &Map<String, Value>) -> String {
    let source = source.and_then(Value::as_object);
    let media = source
        .and_then(|s| str_at(s, "media_type"))
        .or_else(|| str_at(extra, "mimeType"))
        .or_else(|| str_at(extra, "media_type"));
    // A locator instead of bytes: keep it, it is the only searchable thing there.
    if let Some(src) = source
        && let Some(url) = str_at(src, "url").or_else(|| str_at(src, "file_id"))
    {
        return format!("[{} {url}]", media.unwrap_or(kind));
    }
    let data = source
        .and_then(|s| str_at(s, "data"))
        .or_else(|| str_at(extra, "data"));
    placeholder(media.unwrap_or(kind), data.map(decoded_len))
}

/// Describe a `toolUseResult` that is a binary payload rather than text: a `Read` of an image,
/// a PDF rendered to pages, a `Bash` command whose `stdout` is image bytes.
///
/// `None` means "not one of those" — the caller keeps its own handling, which is the answer for
/// every ordinary text-bearing result.
pub fn describe_payload(obj: &Map<String, Value>) -> Option<String> {
    // `Bash` with `isImage` puts image bytes in `stdout` and leaves `stderr` real text.
    if obj.get("isImage").and_then(Value::as_bool) == Some(true) {
        let stdout = str_at(obj, "stdout").map_or(0, decoded_len);
        let shot = placeholder("image", (stdout > 0).then_some(stdout));
        return Some(match str_at(obj, "stderr") {
            Some(err) => format!("{shot}\n{err}"),
            None => shot,
        });
    }
    let kind = str_at(obj, "type")?;
    if !BINARY_RESULTS.contains(&kind) {
        return None;
    }
    let file = obj.get("file").and_then(Value::as_object)?;
    let media = str_at(file, "type").or_else(|| str_at(file, "mediaType"));
    // `originalSize` is the size on disk, which beats the size of whatever compressed
    // derivative the CLI actually attached.
    let bytes = file
        .get("originalSize")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .or_else(|| str_at(file, "base64").map(decoded_len));
    let mut out = placeholder(media.unwrap_or(kind), bytes);
    // A path is not a payload: `Read` of an image omits it, a PDF carries it, and when it is
    // there it is the most searchable fact about the result.
    if let Some(path) = str_at(file, "filePath") {
        out.push('\n');
        out.push_str(path);
    }
    Some(out)
}

/// Replace every base64 blob in `s` with [`blob_placeholder`], leaving everything else byte for
/// byte as it was.
///
/// This is the layer that does not depend on knowing the shape. It works on a JSON line as
/// happily as on prose because a 512-character base64 run cannot span a quote, a comma or a
/// brace — the alphabet has none of them — so the surrounding structure is never touched.
pub fn scrub(s: &str) -> Cow<'_, str> {
    // Cheap rejection for the overwhelmingly common case: nothing long enough to matter.
    if s.len() < BLOB_MIN {
        return Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !is_b64(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_b64(bytes[i]) {
            i += 1;
        }
        // The run is ASCII by construction, so these indices are char boundaries.
        let run = &s[start..i];
        if !looks_encoded(run) {
            continue;
        }
        let out = out.get_or_insert_with(|| String::with_capacity(s.len() / 2));
        out.push_str(&s[copied..start]);
        out.push_str(&blob_placeholder(run));
        copied = i;
    }
    match out {
        Some(mut out) => {
            out.push_str(&s[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(s),
    }
}

/// A copy of `v` with [`scrub`] applied to every string leaf.
///
/// For the structured fields — `tool_input`, `bash_cmd` — where a blob would otherwise be
/// indexed as a term of its own.
pub fn redacted(v: &Value) -> Value {
    match v {
        Value::String(s) => match scrub(s) {
            Cow::Borrowed(_) => v.clone(),
            Cow::Owned(s) => Value::String(s),
        },
        Value::Array(items) => Value::Array(items.iter().map(redacted).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), redacted(v))).collect())
        }
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Base64 of `n` bytes' worth, with the character mix real encoded data always has.
    fn blob(n: usize) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        (0..n)
            .map(|i| ALPHABET[i % ALPHABET.len()] as char)
            .collect()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn a_pasted_screenshot_indexes_its_media_type_and_size_not_its_bytes() {
        let source = json!({"type": "base64", "media_type": "image/jpeg", "data": blob(400_000)});
        let got = describe_block("image", Some(&source), &Map::new());
        assert_eq!(got, "[image/jpeg 293 KiB]");
        assert!(!got.contains("ABCD"), "not one byte of the payload");
    }

    #[test]
    fn a_block_that_carries_a_locator_instead_of_bytes_keeps_the_locator() {
        let source = json!({"type": "url", "url": "https://example.com/shot.png"});
        assert_eq!(
            describe_block("image", Some(&source), &Map::new()),
            "[image https://example.com/shot.png]"
        );

        // The MCP form flattens the payload onto the block itself.
        let mcp = obj(json!({"data": blob(2_000), "mimeType": "image/png"}));
        assert_eq!(describe_block("image", None, &mcp), "[image/png 1 KiB]");
    }

    #[test]
    fn an_image_read_describes_the_file_a_text_read_is_left_alone() {
        let image = obj(json!({
            "type": "image",
            "file": {"base64": blob(40_000), "type": "image/png", "originalSize": 51_200},
        }));
        assert_eq!(describe_payload(&image).unwrap(), "[image/png 50 KiB]");

        let text = obj(json!({
            "type": "text",
            "file": {"filePath": "/tmp/a.rs", "content": "fn main() {}"},
        }));
        assert!(
            describe_payload(&text).is_none(),
            "a text result is the caller's business, not this module's"
        );
    }

    #[test]
    fn a_pdf_keeps_its_path_because_a_path_is_not_a_payload() {
        let pdf = obj(json!({
            "type": "pdf",
            "file": {"filePath": "/docs/spec.pdf", "originalSize": 2_202_010},
            "pages": [{"base64": blob(9_000), "mediaType": "image/png"}],
        }));
        assert_eq!(
            describe_payload(&pdf).unwrap(),
            "[pdf 2.1 MiB]\n/docs/spec.pdf"
        );
    }

    /// `Bash` signals image bytes in `stdout` with a flag; `stderr` beside it is still text.
    #[test]
    fn bash_is_image_drops_stdout_and_keeps_stderr() {
        let result = obj(json!({
            "stdout": blob(20_000),
            "stderr": "warning: no display",
            "isImage": true,
            "interrupted": false,
        }));
        let got = describe_payload(&result).unwrap();
        assert_eq!(got, "[image 15 KiB]\nwarning: no display");
    }

    #[test]
    fn scrub_elides_a_blob_and_leaves_the_prose_around_it_untouched() {
        let text = format!(
            "here is the icon: data:image/png;base64,{} — see it?",
            blob(4_000)
        );
        let got = scrub(&text);
        assert!(got.starts_with("here is the icon: data:image/png;base64,"));
        assert!(got.ends_with(" — see it?"));
        assert!(got.contains("[base64 3 KiB]"), "got: {got}");
    }

    #[test]
    fn scrub_leaves_a_json_line_valid_because_a_blob_cannot_span_a_quote() {
        let line = json!({"data": blob(2_000), "path": "/tmp/x.png"}).to_string();
        let got = scrub(&line);
        let back: Value = serde_json::from_str(&got).expect("still JSON");
        assert_eq!(back["data"], json!("[base64 1 KiB]"));
        assert_eq!(back["path"], json!("/tmp/x.png"), "the rest is verbatim");
    }

    /// The predicate has to be narrow enough that ordinary content survives it — a long word,
    /// a hex digest, minified code and a path all live in the same fields blobs do.
    #[test]
    fn scrub_leaves_everything_that_is_not_encoded_bytes() {
        let cases = [
            "x".repeat(4_000),                                   // one case, no digits
            "0123456789abcdef".repeat(200),                      // a hex digest, no upper case
            "SCREAMING_SNAKE_".repeat(200),                      // underscores break the run
            format!("let x={};", "a1B2+c3D4/".repeat(20)),       // too short to elide
            "/home/user/session-search/src/parse.rs".repeat(30), // a path: `.` breaks every run
        ];
        for case in cases {
            assert!(
                matches!(scrub(&case), Cow::Borrowed(_)),
                "must not touch: {}...",
                &case[..40.min(case.len())]
            );
        }
    }

    #[test]
    fn redacted_reaches_string_leaves_at_any_depth() {
        let v = json!({
            "file_path": "/tmp/a.png",
            "parts": [{"inline_data": {"data": blob(3_000)}}],
        });
        let got = redacted(&v);
        assert_eq!(got["file_path"], json!("/tmp/a.png"));
        assert_eq!(
            got["parts"][0]["inline_data"]["data"],
            json!("[base64 2 KiB]")
        );
    }
}
