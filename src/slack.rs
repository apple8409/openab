use crate::acp::ContentBlock;
use crate::adapter::{AdapterRouter, ChatAdapter, ChannelRef, MessageRef, SenderContext};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio_tungstenite::tungstenite;
use tracing::{error, info, warn};

const SLACK_API: &str = "https://slack.com/api";

/// Map Unicode emoji to Slack short names for reactions API.
fn unicode_to_slack_emoji(unicode: &str) -> &str {
    match unicode {
        "👀" => "eyes",
        "🤔" => "thinking_face",
        "🔥" => "fire",
        "👨\u{200d}💻" => "technologist",
        "⚡" => "zap",
        "🆗" => "ok",
        "😱" => "scream",
        "🚫" => "no_entry_sign",
        "😊" => "blush",
        "😎" => "sunglasses",
        "🫡" => "saluting_face",
        "🤓" => "nerd_face",
        "😏" => "smirk",
        "✌\u{fe0f}" => "v",
        "💪" => "muscle",
        "🦾" => "mechanical_arm",
        "🥱" => "yawning_face",
        "😨" => "fearful",
        "✅" => "white_check_mark",
        "❌" => "x",
        "🔧" => "wrench",
        _ => "grey_question",
    }
}

// --- SlackAdapter: implements ChatAdapter for Slack ---

pub struct SlackAdapter {
    client: reqwest::Client,
    bot_token: String,
}

impl SlackAdapter {
    pub fn new(bot_token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            bot_token,
        }
    }

    async fn api_post(&self, method: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .client
            .post(format!("{SLACK_API}/{method}"))
            .header("Authorization", format!("Bearer {}", self.bot_token))
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&body)
            .send()
            .await?;

        let json: serde_json::Value = resp.json().await?;
        if json["ok"].as_bool() != Some(true) {
            let err = json["error"].as_str().unwrap_or("unknown error");
            return Err(anyhow!("Slack API {method}: {err}"));
        }
        Ok(json)
    }
}

#[async_trait]
impl ChatAdapter for SlackAdapter {
    fn platform(&self) -> &'static str {
        "slack"
    }

    fn message_limit(&self) -> usize {
        4000
    }

    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef> {
        let mut body = serde_json::json!({
            "channel": channel.channel_id,
            "text": content,
        });
        if let Some(thread_ts) = &channel.thread_id {
            body["thread_ts"] = serde_json::Value::String(thread_ts.clone());
        }
        let resp = self.api_post("chat.postMessage", body).await?;
        let ts = resp["ts"]
            .as_str()
            .ok_or_else(|| anyhow!("no ts in chat.postMessage response"))?;
        Ok(MessageRef {
            channel: ChannelRef {
                platform: "slack".into(),
                channel_id: channel.channel_id.clone(),
                thread_id: channel.thread_id.clone(),
                parent_id: None,
            },
            message_id: ts.to_string(),
        })
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> Result<()> {
        self.api_post(
            "chat.update",
            serde_json::json!({
                "channel": msg.channel.channel_id,
                "ts": msg.message_id,
                "text": content,
            }),
        )
        .await?;
        Ok(())
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        _title: &str,
    ) -> Result<ChannelRef> {
        // Slack threads are implicit — posting with thread_ts creates/continues a thread.
        // The "thread" is the channel + the trigger message's ts as thread_ts.
        Ok(ChannelRef {
            platform: "slack".into(),
            channel_id: channel.channel_id.clone(),
            thread_id: Some(trigger_msg.message_id.clone()),
            parent_id: None,
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
        let name = unicode_to_slack_emoji(emoji);
        self.api_post(
            "reactions.add",
            serde_json::json!({
                "channel": msg.channel.channel_id,
                "timestamp": msg.message_id,
                "name": name,
            }),
        )
        .await?;
        Ok(())
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
        let name = unicode_to_slack_emoji(emoji);
        self.api_post(
            "reactions.remove",
            serde_json::json!({
                "channel": msg.channel.channel_id,
                "timestamp": msg.message_id,
                "name": name,
            }),
        )
        .await?;
        Ok(())
    }
}

// --- Socket Mode event loop ---

/// Run the Slack adapter using Socket Mode (persistent WebSocket, no public URL needed).
/// Reconnects automatically on disconnect.
pub async fn run_slack_adapter(
    bot_token: String,
    app_token: String,
    allowed_channels: HashSet<String>,
    allowed_users: HashSet<String>,
    router: Arc<AdapterRouter>,
) -> Result<()> {
    let adapter = Arc::new(SlackAdapter::new(bot_token));

    loop {
        let ws_url = match get_socket_mode_url(&app_token).await {
            Ok(url) => url,
            Err(e) => {
                error!("failed to get Socket Mode URL: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        info!(url = %ws_url, "connecting to Slack Socket Mode");

        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("Slack Socket Mode connected");
                let (mut write, mut read) = ws_stream.split();

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(tungstenite::Message::Text(text)) => {
                            let envelope: serde_json::Value =
                                match serde_json::from_str(&text) {
                                    Ok(v) => v,
                                    Err(_) => continue,
                                };

                            // Acknowledge the envelope immediately
                            if let Some(envelope_id) = envelope["envelope_id"].as_str() {
                                let ack = serde_json::json!({"envelope_id": envelope_id});
                                let _ = write
                                    .send(tungstenite::Message::Text(ack.to_string()))
                                    .await;
                            }

                            // Route events
                            if envelope["type"].as_str() == Some("events_api") {
                                let event = &envelope["payload"]["event"];
                                if event["type"].as_str() == Some("app_mention") {
                                    handle_app_mention(
                                        event,
                                        &adapter,
                                        &allowed_channels,
                                        &allowed_users,
                                        &router,
                                    )
                                    .await;
                                }
                            }
                        }
                        Ok(tungstenite::Message::Ping(data)) => {
                            let _ = write.send(tungstenite::Message::Pong(data)).await;
                        }
                        Ok(tungstenite::Message::Close(_)) => {
                            warn!("Slack Socket Mode connection closed by server");
                            break;
                        }
                        Err(e) => {
                            error!("Socket Mode read error: {e}");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("failed to connect to Slack Socket Mode: {e}");
            }
        }

        warn!("reconnecting to Slack Socket Mode in 5s...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Call apps.connections.open to get a WebSocket URL for Socket Mode.
async fn get_socket_mode_url(app_token: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{SLACK_API}/apps.connections.open"))
        .header("Authorization", format!("Bearer {app_token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .await?;
    let json: serde_json::Value = resp.json().await?;
    if json["ok"].as_bool() != Some(true) {
        let err = json["error"].as_str().unwrap_or("unknown");
        return Err(anyhow!("apps.connections.open: {err}"));
    }
    json["url"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("no url in apps.connections.open response"))
}

async fn handle_app_mention(
    event: &serde_json::Value,
    adapter: &Arc<SlackAdapter>,
    allowed_channels: &HashSet<String>,
    allowed_users: &HashSet<String>,
    router: &Arc<AdapterRouter>,
) {
    let channel_id = match event["channel"].as_str() {
        Some(ch) => ch.to_string(),
        None => return,
    };
    let user_id = match event["user"].as_str() {
        Some(u) => u.to_string(),
        None => return,
    };
    let text = match event["text"].as_str() {
        Some(t) => t.to_string(),
        None => return,
    };
    let ts = match event["ts"].as_str() {
        Some(ts) => ts.to_string(),
        None => return,
    };
    let thread_ts = event["thread_ts"].as_str().map(|s| s.to_string());

    // Check allowed channels (empty = deny all, secure by default per #91)
    if !allowed_channels.is_empty() && !allowed_channels.contains(&channel_id) {
        return;
    }

    // Check allowed users
    if !allowed_users.is_empty() && !allowed_users.contains(&user_id) {
        tracing::info!(user_id, "denied Slack user, ignoring");
        let msg_ref = MessageRef {
            channel: ChannelRef {
                platform: "slack".into(),
                channel_id: channel_id.clone(),
                thread_id: thread_ts.clone(),
                parent_id: None,
            },
            message_id: ts.clone(),
        };
        let _ = adapter.add_reaction(&msg_ref, "🚫").await;
        return;
    }

    // Strip bot mention (<@UBOTID>) from text
    let prompt = strip_slack_mention(&text);
    if prompt.is_empty() {
        return;
    }

    let sender = SenderContext {
        schema: "openab.sender.v1".into(),
        sender_id: user_id.clone(),
        sender_name: user_id.clone(), // app_mention events don't include display name
        display_name: user_id.clone(),
        channel: "slack".into(),
        channel_id: channel_id.clone(),
        is_bot: false,
    };

    let sender_json = serde_json::to_string(&sender).unwrap();
    let prompt_with_sender = format!(
        "<sender_context>\n{}\n</sender_context>\n\n{}",
        sender_json, prompt
    );

    let content_blocks = vec![ContentBlock::Text {
        text: prompt_with_sender,
    }];

    let trigger_msg = MessageRef {
        channel: ChannelRef {
            platform: "slack".into(),
            channel_id: channel_id.clone(),
            thread_id: thread_ts.clone(),
            parent_id: None,
        },
        message_id: ts.clone(),
    };

    // Determine thread: if already in a thread, continue it; otherwise start a new thread
    let thread_channel = ChannelRef {
        platform: "slack".into(),
        channel_id: channel_id.clone(),
        thread_id: Some(thread_ts.unwrap_or(ts)),
        parent_id: None,
    };

    let adapter_dyn: Arc<dyn ChatAdapter> = adapter.clone();
    if let Err(e) = router
        .handle_message(&adapter_dyn, &thread_channel, &sender, content_blocks, &trigger_msg)
        .await
    {
        error!("Slack handle_message error: {e}");
    }
}

fn strip_slack_mention(text: &str) -> String {
    let re = regex::Regex::new(r"<@[A-Z0-9]+>").unwrap();
    re.replace_all(text, "").trim().to_string()
}
