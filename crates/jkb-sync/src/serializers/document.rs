//! The `document` serializer: the whole file is one item's text content.
//!
//! This is the v1 serializer, generalized onto [`SyncDoc`] as a single item with an
//! empty `local_id` (so the engine binds it to the bare `file://<path>` uri, exactly
//! as before). It never meaningfully fails to `parse` except on non-UTF-8 input, which
//! stays a hard error (`quarantine_on_parse_error` is `false`) — preserving the v1
//! behaviour and its tests.

use jkb_types::Error as TypeError;

use super::{SyncDoc, SyncItem, SyncSerializer};
use crate::{Error, Result};

/// The `document` serializer (whole file ⇄ one item).
pub struct DocumentSerializer;

impl SyncSerializer for DocumentSerializer {
    fn name(&self) -> &'static str {
        "document"
    }

    fn parse(&self, bytes: &[u8]) -> Result<SyncDoc> {
        let content = String::from_utf8(bytes.to_vec()).map_err(|_| {
            Error::Types(TypeError::Validation(
                "file is not valid UTF-8; the `document` serializer handles text files".to_owned(),
            ))
        })?;
        Ok(SyncDoc {
            items: vec![SyncItem::new(String::new(), "document", content)],
            ..SyncDoc::default()
        })
    }

    fn render(&self, doc: &SyncDoc) -> Result<Vec<u8>> {
        let content = doc.items.first().map_or("", |i| i.content.as_str());
        Ok(content.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::{DocumentSerializer, SyncSerializer};

    #[test]
    fn document_round_trips_text_as_one_item() {
        let s = DocumentSerializer;
        let doc = s.parse(b"hello\nworld").unwrap();
        assert_eq!(doc.items.len(), 1);
        assert_eq!(doc.items[0].local_id, "");
        assert_eq!(doc.items[0].content, "hello\nworld");
        assert_eq!(s.render(&doc).unwrap(), b"hello\nworld");
    }

    #[test]
    fn document_rejects_non_utf8_and_does_not_quarantine() {
        let s = DocumentSerializer;
        assert!(s.parse(&[0xff, 0xfe]).is_err());
        assert!(!s.quarantine_on_parse_error());
    }
}
