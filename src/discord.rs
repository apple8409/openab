use crate::acp::ContentBlock;
use crate::adapter::{AdapterRouter, ChatAdapter, ChannelRef, MessageRef, SenderContext};
use crate::config::SttConfig;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::ImageReader;
use std::io::Cursor;
use std::sync::LazyLock;
use serenity::builder::{CreateThread, EditMessage};
use serenity::http::Http;
use serenity::model::channel::{AutoArchiveDuration, Message, ReactionType};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId};
use serenity::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, error, info};

/// Reusable HTTP client for downloading Discord attachments.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("static HTTP client must build")
});

// --- DiscordAdapter: implements ChatAdapter for Discord via serenity ---

pub struct DiscordAdapter {
    http: Arc<Http>,
}

impl DiscordAdapter {
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }
}

#[async_trait]
impl ChatAdapter for DiscordAdapter {
    fn platform(&self) -> &'static str {
        "discord"
    }

    fn message_limit(&self) -> usize {
        2000
    }

    async fn send_message(&self, channel: &ChannelRef, content: &str) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = channel.channel_id.parse()?;
        let msg = ChannelId::new(ch_id).say(&self.http, content).await?;
        Ok(MessageRef {
            channel: channel.clone(),
            message_id: msg.id.to_string(),
        })
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> anyhow::Result<()> {
        let ch_id: u64 = msg.channel.channel_id.parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        ChannelId::new(ch_id)
            .edit_message(
                &self.http,
                MessageId::new(msg_id),
                EditMessage::new().content(content),
            )
            .await?;
        Ok(())
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> anyhow::Result<ChannelRef> {
        let ch_id: u64 = channel.channel_id.parse()?;
        let msg_id: u64 = trigger_msg.message_id.parse()?;
        let thread = ChannelId::new(ch_id)
            .create_thread_from_message(
                &self.http,
                MessageId::new(msg_id),
                CreateThread::new(title).auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await?;
        Ok(ChannelRef {
            platform: "discord".into(),
            channel_id: thread.id.to_string(),
            thread_id: None,
            parent_id: Some(channel.channel_id.clone()),
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = msg.channel.channel_id.parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .create_reaction(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = msg.channel.channel_id.parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .delete_reaction_me(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }
}

// --- Handler: serenity EventHandler that delegates to AdapterRouter ---

pub struct Handler {
    pub router: Arc<AdapterRouter>,
    pub allowed_channels: HashSet<u64>,
    pub allowed_users: HashSet<u64>,
    pub stt_config: SttConfig,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let adapter: Arc<dyn ChatAdapter> = Arc::new(DiscordAdapter::new(ctx.http.clone()));
        let bot_id = ctx.cache.current_user().id;

        let channel_id = msg.channel_id.get();
        let in_allowed_channel =
            self.allowed_channels.is_empty() || self.allowed_channels.contains(&channel_id);

        let is_mentioned = msg.mentions_user_id(bot_id)
            || msg.content.contains(&format!("<@{}>", bot_id))
            || msg
                .mention_roles
                .iter()
                .any(|r| msg.content.contains(&format!("<@&{}>", r)));

        let in_thread = if !in_allowed_channel {
            match msg.channel_id.to_channel(&ctx.http).await {
                Ok(serenity::model::channel::Channel::Guild(gc)) => {
                    let result = gc
                        .parent_id
                        .is_some_and(|pid| self.allowed_channels.contains(&pid.get()));
                    tracing::debug!(channel_id = %msg.channel_id, parent_id = ?gc.parent_id, result, "thread check");
                    result
                }
                Ok(other) => {
                    tracing::debug!(channel_id = %msg.channel_id, kind = ?other, "not a guild channel");
                    false
                }
                Err(e) => {
                    tracing::debug!(channel_id = %msg.channel_id, error = %e, "to_channel failed");
                    false
                }
            }
        } else {
            false
        };

        if !in_allowed_channel && !in_thread {
            return;
        }
        if !in_thread && !is_mentioned {
            return;
        }

        if !self.allowed_users.is_empty() && !self.allowed_users.contains(&msg.author.id.get()) {
            tracing::info!(user_id = %msg.author.id, "denied user, ignoring");
            let msg_ref = discord_msg_ref(&msg);
            let _ = adapter.add_reaction(&msg_ref, "🚫").await;
            return;
        }

        let prompt = if is_mentioned {
            strip_mention(&msg.content)
        } else {
            msg.content.trim().to_string()
        };

        // No text and no attachments → skip
        if prompt.is_empty() && msg.attachments.is_empty() {
            return;
        }

        // Build content blocks: text + image/audio attachments
        let display_name = msg
            .member
            .as_ref()
            .and_then(|m| m.nick.as_ref())
            .unwrap_or(&msg.author.name);
        let sender = SenderContext {
            schema: "openab.sender.v1".into(),
            sender_id: msg.author.id.to_string(),
            sender_name: msg.author.name.clone(),
            display_name: display_name.to_string(),
            channel: "discord".into(),
            channel_id: msg.channel_id.to_string(),
            is_bot: msg.author.bot,
        };

        let sender_json = serde_json::to_string(&sender).unwrap();
        let prompt_with_sender = format!(
            "<sender_context>\n{}\n</sender_context>\n\n{}",
            sender_json, prompt
        );

        let mut content_blocks = vec![ContentBlock::Text {
            text: prompt_with_sender,
        }];

        // Process attachments: route by content type (audio → STT, image → encode)
        for attachment in &msg.attachments {
            if is_audio_attachment(attachment) {
                if self.stt_config.enabled {
                    if let Some(transcript) = download_and_transcribe(attachment, &self.stt_config).await {
                        debug!(filename = %attachment.filename, chars = transcript.len(), "voice transcript injected");
                        content_blocks.insert(0, ContentBlock::Text {
                            text: format!("[Voice message transcript]: {transcript}"),
                        });
                    }
                } else {
                    debug!(filename = %attachment.filename, "skipping audio attachment (STT disabled)");
                }
            } else if let Some(content_block) = download_and_encode_image(attachment).await {
                debug!(url = %attachment.url, filename = %attachment.filename, "adding image attachment");
                content_blocks.push(content_block);
            }
        }

        tracing::debug!(
            num_blocks = content_blocks.len(),
            num_attachments = msg.attachments.len(),
            in_thread,
            "processing"
        );

        let thread_channel = if in_thread {
            ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: None,
            }
        } else {
            match get_or_create_thread(&ctx, &adapter, &msg, &prompt).await {
                Ok(ch) => ch,
                Err(e) => {
                    error!("failed to create thread: {e}");
                    return;
                }
            }
        };

        let trigger_msg = discord_msg_ref(&msg);

        if let Err(e) = self
            .router
            .handle_message(&adapter, &thread_channel, &sender, content_blocks, &trigger_msg)
            .await
        {
            error!("handle_message error: {e}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord bot connected");
    }
}

// --- Discord-specific helpers ---

fn discord_msg_ref(msg: &Message) -> MessageRef {
    MessageRef {
        channel: ChannelRef {
            platform: "discord".into(),
            channel_id: msg.channel_id.get().to_string(),
            thread_id: None,
            parent_id: None,
        },
        message_id: msg.id.to_string(),
    }
}

/// Check if an attachment is an audio file (voice messages are typically audio/ogg).
fn is_audio_attachment(attachment: &serenity::model::channel::Attachment) -> bool {
    let mime = attachment.content_type.as_deref().unwrap_or("");
    mime.starts_with("audio/")
}

/// Download an audio attachment and transcribe it via the configured STT provider.
async fn download_and_transcribe(
    attachment: &serenity::model::channel::Attachment,
    stt_config: &SttConfig,
) -> Option<String> {
    const MAX_SIZE: u64 = 25 * 1024 * 1024; // 25 MB (Whisper API limit)

    if u64::from(attachment.size) > MAX_SIZE {
        error!(filename = %attachment.filename, size = attachment.size, "audio exceeds 25MB limit");
        return None;
    }

    let resp = HTTP_CLIENT.get(&attachment.url).send().await.ok()?;
    if !resp.status().is_success() {
        error!(url = %attachment.url, status = %resp.status(), "audio download failed");
        return None;
    }
    let bytes = resp.bytes().await.ok()?.to_vec();

    let mime_type = attachment.content_type.as_deref().unwrap_or("audio/ogg");
    let mime_type = mime_type.split(';').next().unwrap_or(mime_type).trim();

    crate::stt::transcribe(&HTTP_CLIENT, stt_config, bytes, attachment.filename.clone(), mime_type).await
}

/// Maximum dimension (width or height) for resized images.
const IMAGE_MAX_DIMENSION_PX: u32 = 1200;

/// JPEG quality for compressed output.
const IMAGE_JPEG_QUALITY: u8 = 75;

/// Download a Discord image attachment, resize/compress it, then base64-encode
/// as an ACP image content block.
async fn download_and_encode_image(attachment: &serenity::model::channel::Attachment) -> Option<ContentBlock> {
    const MAX_SIZE: u64 = 10 * 1024 * 1024; // 10 MB

    let url = &attachment.url;
    if url.is_empty() {
        return None;
    }

    let media_type = attachment
        .content_type
        .as_deref()
        .or_else(|| {
            attachment
                .filename
                .rsplit('.')
                .next()
                .and_then(|ext| match ext.to_lowercase().as_str() {
                    "png" => Some("image/png"),
                    "jpg" | "jpeg" => Some("image/jpeg"),
                    "gif" => Some("image/gif"),
                    "webp" => Some("image/webp"),
                    _ => None,
                })
        });

    let Some(mime) = media_type else {
        debug!(filename = %attachment.filename, "skipping non-image attachment");
        return None;
    };
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    if !mime.starts_with("image/") {
        debug!(filename = %attachment.filename, mime = %mime, "skipping non-image attachment");
        return None;
    }

    if u64::from(attachment.size) > MAX_SIZE {
        error!(filename = %attachment.filename, size = attachment.size, "image exceeds 10MB limit");
        return None;
    }

    let response = match HTTP_CLIENT.get(url).send().await {
        Ok(resp) => resp,
        Err(e) => { error!(url = %url, error = %e, "download failed"); return None; }
    };
    if !response.status().is_success() {
        error!(url = %url, status = %response.status(), "HTTP error downloading image");
        return None;
    }
    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => { error!(url = %url, error = %e, "read failed"); return None; }
    };

    if bytes.len() as u64 > MAX_SIZE {
        error!(filename = %attachment.filename, size = bytes.len(), "downloaded image exceeds limit");
        return None;
    }

    let (output_bytes, output_mime) = match resize_and_compress(&bytes) {
        Ok(result) => result,
        Err(e) => {
            if bytes.len() > 1024 * 1024 {
                error!(filename = %attachment.filename, error = %e, size = bytes.len(), "resize failed and original too large, skipping");
                return None;
            }
            debug!(filename = %attachment.filename, error = %e, "resize failed, using original");
            (bytes.to_vec(), mime.to_string())
        }
    };

    debug!(
        filename = %attachment.filename,
        original_size = bytes.len(),
        compressed_size = output_bytes.len(),
        "image processed"
    );

    let encoded = BASE64.encode(&output_bytes);
    Some(ContentBlock::Image {
        media_type: output_mime,
        data: encoded,
    })
}

/// Resize image so longest side <= IMAGE_MAX_DIMENSION_PX, then encode as JPEG.
/// GIFs are passed through unchanged to preserve animation.
fn resize_and_compress(raw: &[u8]) -> Result<(Vec<u8>, String), image::ImageError> {
    let reader = ImageReader::new(Cursor::new(raw))
        .with_guessed_format()?;

    let format = reader.format();

    if format == Some(image::ImageFormat::Gif) {
        return Ok((raw.to_vec(), "image/gif".to_string()));
    }

    let img = reader.decode()?;
    let (w, h) = (img.width(), img.height());

    let img = if w > IMAGE_MAX_DIMENSION_PX || h > IMAGE_MAX_DIMENSION_PX {
        let max_side = std::cmp::max(w, h);
        let ratio = f64::from(IMAGE_MAX_DIMENSION_PX) / f64::from(max_side);
        let new_w = (f64::from(w) * ratio) as u32;
        let new_h = (f64::from(h) * ratio) as u32;
        img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, IMAGE_JPEG_QUALITY);
    img.write_with_encoder(encoder)?;

    Ok((buf.into_inner(), "image/jpeg".to_string()))
}

async fn get_or_create_thread(
    ctx: &Context,
    adapter: &Arc<dyn ChatAdapter>,
    msg: &Message,
    prompt: &str,
) -> anyhow::Result<ChannelRef> {
    let channel = msg.channel_id.to_channel(&ctx.http).await?;
    if let serenity::model::channel::Channel::Guild(ref gc) = channel {
        if gc.thread_metadata.is_some() {
            return Ok(ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: None,
            });
        }
    }

    let thread_name = shorten_thread_name(prompt);
    let parent = ChannelRef {
        platform: "discord".into(),
        channel_id: msg.channel_id.get().to_string(),
        thread_id: None,
        parent_id: None,
    };
    let trigger_ref = discord_msg_ref(msg);
    adapter.create_thread(&parent, &trigger_ref, &thread_name).await
}

static MENTION_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"<@[!&]?\d+>").unwrap()
});

fn strip_mention(content: &str) -> String {
    MENTION_RE.replace_all(content, "").trim().to_string()
}

fn shorten_thread_name(prompt: &str) -> String {
    let re = regex::Regex::new(r"https?://github\.com/([^/]+/[^/]+)/(issues|pull)/(\d+)").unwrap();
    let shortened = re.replace_all(prompt, "$1#$3");
    let name: String = shortened.chars().take(40).collect();
    if name.len() < shortened.len() {
        format!("{name}...")
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::new(width, height);
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn large_image_resized_to_max_dimension() {
        let png = make_png(3000, 2000);
        let (compressed, mime) = resize_and_compress(&png).unwrap();

        assert_eq!(mime, "image/jpeg");
        let result = image::load_from_memory(&compressed).unwrap();
        assert!(result.width() <= IMAGE_MAX_DIMENSION_PX);
        assert!(result.height() <= IMAGE_MAX_DIMENSION_PX);
    }

    #[test]
    fn small_image_keeps_original_dimensions() {
        let png = make_png(800, 600);
        let (compressed, mime) = resize_and_compress(&png).unwrap();

        assert_eq!(mime, "image/jpeg");
        let result = image::load_from_memory(&compressed).unwrap();
        assert_eq!(result.width(), 800);
        assert_eq!(result.height(), 600);
    }

    #[test]
    fn landscape_image_respects_aspect_ratio() {
        let png = make_png(4000, 2000);
        let (compressed, _) = resize_and_compress(&png).unwrap();

        let result = image::load_from_memory(&compressed).unwrap();
        assert_eq!(result.width(), 1200);
        assert_eq!(result.height(), 600);
    }

    #[test]
    fn portrait_image_respects_aspect_ratio() {
        let png = make_png(2000, 4000);
        let (compressed, _) = resize_and_compress(&png).unwrap();

        let result = image::load_from_memory(&compressed).unwrap();
        assert_eq!(result.width(), 600);
        assert_eq!(result.height(), 1200);
    }

    #[test]
    fn compressed_output_is_smaller_than_original() {
        let png = make_png(3000, 2000);
        let (compressed, _) = resize_and_compress(&png).unwrap();

        assert!(compressed.len() < png.len(), "compressed {} should be < original {}", compressed.len(), png.len());
    }

    #[test]
    fn gif_passes_through_unchanged() {
        let gif: Vec<u8> = vec![
            0x47, 0x49, 0x46, 0x38, 0x39, 0x61,
            0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
            0x02, 0x02, 0x44, 0x01, 0x00,
            0x3B,
        ];
        let (output, mime) = resize_and_compress(&gif).unwrap();

        assert_eq!(mime, "image/gif");
        assert_eq!(output, gif);
    }

    #[test]
    fn invalid_data_returns_error() {
        let garbage = vec![0x00, 0x01, 0x02, 0x03];
        assert!(resize_and_compress(&garbage).is_err());
    }
}
