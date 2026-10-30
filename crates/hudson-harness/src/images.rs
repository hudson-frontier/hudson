//! Inline user images. No path, URL, decoder, credential or filesystem access.
use crate::{Content, HarnessError, Message, Role};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_IMAGE_TEXT_BYTES: usize = 4 * MAX_IMAGE_BYTES.div_ceil(3);
pub const MAX_TURN_IMAGES: usize = 4;
pub const MAX_REQUEST_IMAGE_TEXT_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub media_type: String,
    pub data: String,
}
impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("media_type", &self.media_type)
            .field("encoded_bytes", &self.data.len())
            .finish()
    }
}
impl Image {
    /// Validate transport encoding and size. Image parsing remains the chosen
    /// provider's responsibility; Hudson never decompresses untrusted images.
    pub fn validate(&self) -> Result<(), HarnessError> {
        if !matches!(
            self.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp"
        ) || self.data.is_empty()
            || self.data.len() > MAX_IMAGE_TEXT_BYTES
        {
            return Err(HarnessError::Invalid(
                "unsupported image media type or encoded size".into(),
            ));
        }
        let bytes = STANDARD.decode(&self.data).map_err(|_| {
            HarnessError::Invalid("image must contain standard base64 bytes".into())
        })?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(HarnessError::Invalid(
                "image exceeds decoded byte limit".into(),
            ));
        }
        Ok(())
    }
}
pub fn validate_turn(images: &[Image]) -> Result<(), HarnessError> {
    if images.len() > MAX_TURN_IMAGES {
        return Err(HarnessError::Invalid("too many images in one turn".into()));
    }
    if images.iter().map(|image| image.data.len()).sum::<usize>() > MAX_REQUEST_IMAGE_TEXT_BYTES {
        return Err(HarnessError::Invalid(
            "image turn exceeds encoded byte ceiling".into(),
        ));
    }
    for image in images {
        image.validate()?;
    }
    Ok(())
}
pub fn validate_history(messages: &[Message]) -> Result<(), HarnessError> {
    let mut count = 0;
    let mut bytes: usize = 0;
    for message in messages {
        for content in &message.content {
            if let Content::Image { image } = content {
                count += 1;
                bytes = bytes.saturating_add(image.data.len());
                if message.role != Role::User || count > 16 || bytes > MAX_REQUEST_IMAGE_TEXT_BYTES
                {
                    return Err(HarnessError::Invalid(
                        "images require user messages and at most 16 images per request".into(),
                    ));
                }
                image.validate()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_validation_bounds_base64_and_never_resolves_paths_or_urls() {
        let valid = Image {
            media_type: "image/png".into(),
            data: STANDARD.encode(b"fixture bytes"),
        };
        valid.validate().unwrap();
        for data in [
            "".into(),
            "https://internal.invalid/image".into(),
            "file:///private/image".into(),
            "x".repeat(MAX_IMAGE_TEXT_BYTES + 1),
        ] {
            assert!(Image {
                data,
                ..valid.clone()
            }
            .validate()
            .is_err());
        }
        assert!(Image {
            media_type: "text/html".into(),
            ..valid.clone()
        }
        .validate()
        .is_err());
        assert!(!format!("{valid:?}").contains(&valid.data));
        assert!(validate_turn(&vec![valid.clone(); 5]).is_err());
        let mut message = Message {
            role: Role::Assistant,
            content: vec![Content::Image { image: valid }],
        };
        assert!(validate_history(&[message.clone()]).is_err());
        message.role = Role::User;
        validate_history(&[message]).unwrap();
    }
    #[test]
    fn context_archival_keeps_live_images_and_archives_other_complete_exchanges() {
        let user = |content| Message {
            role: Role::User,
            content,
        };
        let history = crate::context::History {
            archive: None,
            messages: vec![
                user(vec![Content::Text {
                    text: "original task".into(),
                }]),
                user(vec![Content::Image {
                    image: Image {
                        media_type: "image/png".into(),
                        data: STANDARD.encode(b"bytes"),
                    },
                }]),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text {
                        text: "old explanation".into(),
                    }],
                },
                user(vec![Content::Text {
                    text: "latest question".into(),
                }]),
            ],
        };
        assert_eq!(history.oldest_complete_exchange(), Some(2..3));
        assert_eq!(history.complete_exchanges(), vec![2..3]);
    }
}
