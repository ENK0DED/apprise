use crate::notify::Attachment;

/// Create an attachment from in-memory data.
pub fn from_memory(name: &str, data: Vec<u8>, mime_type: &str) -> Attachment {
    Attachment {
        name: name.to_string(),
        data,
        mime_type: mime_type.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_memory_basic() {
        let data = b"hello world".to_vec();
        let att = from_memory("test.txt", data.clone(), "text/plain");
        assert_eq!(att.name, "test.txt");
        assert_eq!(att.data, data);
        assert_eq!(att.mime_type, "text/plain");
    }

    #[test]
    fn test_from_memory_binary() {
        let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic bytes
        let att = from_memory("image.png", data.clone(), "image/png");
        assert_eq!(att.name, "image.png");
        assert_eq!(att.data, data);
        assert_eq!(att.mime_type, "image/png");
    }

    #[test]
    fn test_from_memory_empty() {
        let att = from_memory("empty.bin", Vec::new(), "application/octet-stream");
        assert_eq!(att.name, "empty.bin");
        assert!(att.data.is_empty());
        assert_eq!(att.mime_type, "application/octet-stream");
    }
}
