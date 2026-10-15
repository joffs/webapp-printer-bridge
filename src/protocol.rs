//! JSON messages exchanged on the `/printer` WebSocket, compatible with the Java bridge.

use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// A print request as sent by `websocket-printer.js` (`printer.submit({...})`).
#[derive(Debug, Clone, Deserialize)]
pub struct PrintDocument {
    #[serde(default, rename = "type", deserialize_with = "lenient_string")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub url: Option<String>,
    /// Echoed back in the result so the web app can match responses to jobs.
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_qty")]
    pub qty: Option<u32>,
    /// Base64 encoded file (PDF/image); `url` then only carries the file name.
    #[serde(default, rename = "file_content")]
    pub file_content: Option<String>,
    /// Base64 encoded bytes sent to the printer unmodified (ESC/POS, ZPL, ...).
    #[serde(default, rename = "raw_content")]
    pub raw_content: Option<String>,
    #[serde(default, deserialize_with = "lenient_extras")]
    pub extras: Vec<Annotation>,
}

impl PrintDocument {
    pub fn copies(&self) -> u32 {
        self.qty.unwrap_or(1).max(1)
    }

    pub fn is_raw(&self) -> bool {
        self.raw_content.as_deref().is_some_and(|s| !s.is_empty())
    }

    /// File name part of `url`, used (like the Java bridge) to detect the document type.
    fn url_file_name(&self) -> &str {
        let url = self.url.as_deref().unwrap_or("");
        url.rsplit(['/', '\\']).next().unwrap_or("")
    }

    pub fn is_image(&self) -> bool {
        let name = self.url_file_name();
        [".jpg", ".jpeg", ".png", ".gif"]
            .iter()
            .any(|ext| name.ends_with(ext))
    }

    pub fn is_pdf(&self) -> bool {
        self.url_file_name().ends_with(".pdf")
    }
}

/// Text drawn on top of every printed page. Coordinates are in millimetres.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Annotation {
    #[serde(default, deserialize_with = "lenient_string")]
    pub text: Option<String>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub x: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub y: Option<f64>,
    /// Font size in points (default 10).
    #[serde(default, deserialize_with = "lenient_f64")]
    pub size: Option<f64>,
    /// The Java bridge renders bold whenever the key is present (even `false`);
    /// kept identical so existing labels print the same.
    #[serde(default, deserialize_with = "present")]
    pub bold: bool,
}

/// Reply broadcast to all `/printer` clients after each job.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintResult {
    pub success: bool,
    pub message: Option<String>,
    pub id: Option<String>,
    pub printer_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrintServiceDto {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionDto {
    pub app_name: &'static str,
    pub app_id: &'static str,
    pub version: &'static str,
}

/// Decode base64 the way commons-codec does: ignore characters outside the alphabet
/// (whitespace, line breaks, data-URL junk), accept URL-safe characters and missing padding.
pub fn decode_base64_lenient(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let cleaned: String = input
        .chars()
        .filter_map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => Some(c),
            '-' => Some('+'),
            '_' => Some('/'),
            _ => None,
        })
        .collect();
    // A dangling single character cannot encode a byte; commons-codec drops it.
    let usable = cleaned.len() - usize::from(cleaned.len() % 4 == 1);
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    engine.decode(&cleaned[..usable])
}

// --- lenient deserializers (Jackson coerces scalars, so web apps may rely on it) ---

fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Null => None,
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        other => Some(other.to_string()),
    })
}

fn lenient_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

fn lenient_qty<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(lenient_f64(d)?.map(|v| v.clamp(0.0, u32::MAX as f64) as u32))
}

fn present<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(!Value::deserialize(d)?.is_null())
}

fn lenient_extras<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Annotation>, D::Error> {
    match Value::deserialize(d)? {
        Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_demo_payloads() {
        let d: PrintDocument = serde_json::from_str(
            r#"{"type":"INVOICE","url":"https://x/y/sample.pdf","id":123,"qty":"3",
                "extras":[{"text":"Hi","x":10,"y":20.5,"size":12,"bold":false},{"text":"n"}]}"#,
        )
        .unwrap();
        assert_eq!(d.kind.as_deref(), Some("INVOICE"));
        assert_eq!(d.id.as_deref(), Some("123"));
        assert_eq!(d.copies(), 3);
        assert!(d.is_pdf() && !d.is_image() && !d.is_raw());
        assert_eq!(d.extras.len(), 2);
        assert!(d.extras[0].bold && !d.extras[1].bold);
        assert_eq!(d.extras[0].y, Some(20.5));

        let d: PrintDocument =
            serde_json::from_str(r#"{"type":"LABEL","raw_content":"Xlh"}"#).unwrap();
        assert!(d.is_raw());
        assert_eq!(d.copies(), 1);
    }

    #[test]
    fn type_detection_matches_java_regex() {
        let doc = |url: &str| PrintDocument {
            url: Some(url.into()),
            ..serde_json::from_str("{}").unwrap()
        };
        assert!(doc("a/b.png").is_image());
        assert!(doc("b.jpeg").is_image());
        assert!(!doc("b.PNG").is_image()); // Java regex is case-sensitive
        assert!(!doc("b.pdf?x=1").is_pdf()); // query string defeats detection in Java too
        assert!(!doc("x.pdf.txt").is_pdf());
    }

    #[test]
    fn base64_is_lenient() {
        assert_eq!(decode_base64_lenient("SGVs\r\nbG8=").unwrap(), b"Hello");
        assert_eq!(decode_base64_lenient("SGVsbG8").unwrap(), b"Hello");
        assert_eq!(decode_base64_lenient("-_8").unwrap(), [0xfb, 0xff]);
    }

    #[test]
    fn result_json_shape() {
        let r = PrintResult {
            success: false,
            message: Some("x".into()),
            id: None,
            printer_name: None,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"success":false,"message":"x","id":null,"printerName":null}"#
        );
    }
}
