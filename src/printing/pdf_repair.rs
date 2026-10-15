//! Repairs for PDF defects that make Windows' PDF engine drop content although other
//! viewers (and PDFBox in the Java bridge) render it.
//!
//! * Odd-length `CIDToGIDMap` streams: the map holds 2 bytes per CID; dompdf/CPDF labels
//!   were seen with the last entry cut in half. Windows then ignores the whole font and
//!   all text set in it disappears. Padding the map to an even length fixes it.
//!
//! PDFs without such defects are passed through untouched.

use std::borrow::Cow;

use anyhow::Result;
use lopdf::{Document, Object, ObjectId};

/// A CIDToGIDMap covers at most 65536 CIDs of 2 bytes; anything larger is not a map.
const MAX_MAP_BYTES: usize = 2 * 65536 + 2;

pub fn repair(data: &[u8]) -> Cow<'_, [u8]> {
    if !contains(data, b"CIDToGIDMap") {
        return Cow::Borrowed(data);
    }
    match try_repair(data) {
        Ok(Some(fixed)) => Cow::Owned(fixed),
        Ok(None) => Cow::Borrowed(data),
        Err(e) => {
            tracing::warn!("PDF repair skipped: {e:#}");
            Cow::Borrowed(data)
        }
    }
}

fn try_repair(data: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut doc = Document::load_mem(data)?;

    let maps: Vec<ObjectId> = doc
        .objects
        .values()
        .filter_map(|object| match object {
            Object::Dictionary(dict) => dict.get(b"CIDToGIDMap").ok()?.as_reference().ok(),
            _ => None,
        })
        .collect();

    let mut repaired = 0;
    for id in maps {
        let Ok(stream) = doc.get_object_mut(id).and_then(Object::as_stream_mut) else {
            continue;
        };
        let mut content = stream.get_plain_content_with_limit(MAX_MAP_BYTES)?;
        if content.len() % 2 == 1 {
            content.push(0);
            stream.set_plain_content(content);
            stream.compress()?;
            repaired += 1;
        }
    }

    if repaired == 0 {
        return Ok(None);
    }
    tracing::warn!("PDF repaired: padded {repaired} truncated CIDToGIDMap stream(s)");
    let mut out = Vec::with_capacity(data.len() + 64);
    doc.save_to(&mut out)?;
    Ok(Some(out))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Stream, dictionary};

    /// Minimal PDF with one CIDFontType2 whose CIDToGIDMap stream has `map_len` bytes.
    fn pdf_with_map(map_len: usize) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let map = doc.add_object(Stream::new(dictionary! {}, vec![0u8; map_len]));
        let cid_font = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => "Test",
            "CIDToGIDMap" => map,
        });
        let pages = doc.new_object_id();
        let page =
            doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages, "Font" => cid_font });
        doc.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn map_len(pdf: &[u8]) -> usize {
        let doc = Document::load_mem(pdf).unwrap();
        let id = doc
            .objects
            .values()
            .find_map(|o| {
                o.as_dict()
                    .ok()?
                    .get(b"CIDToGIDMap")
                    .ok()?
                    .as_reference()
                    .ok()
            })
            .unwrap();
        doc.get_object(id)
            .unwrap()
            .as_stream()
            .unwrap()
            .get_plain_content()
            .unwrap()
            .len()
    }

    #[test]
    fn pads_odd_length_map() {
        let broken = pdf_with_map(17607);
        let fixed = repair(&broken);
        assert!(matches!(fixed, Cow::Owned(_)));
        assert_eq!(map_len(&fixed), 17608);
    }

    #[test]
    fn leaves_valid_pdfs_untouched() {
        let valid = pdf_with_map(236);
        assert!(matches!(repair(&valid), Cow::Borrowed(_)));
        let no_cid_fonts = b"%PDF-1.4 nothing to see";
        assert!(matches!(repair(no_cid_fonts), Cow::Borrowed(_)));
        let garbage = b"%PDF-1.4 /CIDToGIDMap but not parseable";
        assert!(matches!(repair(garbage), Cow::Borrowed(_)));
    }
}
