# RFC 002: Multi-Platform Adapter Architecture

**Tracking issues:** #86, #93
**Status:** Draft
**Author:** @chaodu-agent

---

## Summary

Define a platform-agnostic adapter layer for agent-broker so it can serve Discord, Telegram, Slack, and future chat platforms through a single unified architecture. The ACP session pool and agent backend remain unchanged — only the "front door" becomes pluggable.

## Motivation

- agent-broker is currently hard-wired to Discord via `serenity`
- #86 proposes a Telegram adapter, #93 proposes Slack — both require similar abstractions
- Without a shared trait, each adapter will duplicate session routing, message splitting, reaction handling, and streaming logic
- A clean adapter boundary also enables running multiple adapters simultaneously (e.g. Discord + Telegram in one deployment)

---

## Current Architecture

```
┌──────────────┐  Gateway WS   ┌──────────────────────────────────────┐  ACP stdio   ┌───────────┐
│   Discord    │◄──────────────►│            agent-broker              │─────────────►│ coding CLI│
│   User       │                │  discord.rs → pool.rs → connection.rs│◄── JSON-RPC ─│ (acp mode)│
└──────────────┘                └──────────────────────────────────────┘              └───────────┘
```

Everything in `discord.rs` — message handling, thread creation, edit-streaming, reactions — is Discord-specific. The session pool and ACP layer are already platform-agnostic.

---

## Proposed Architecture

```
                    ┌─────────────────┐
                    │  Discord Users  │
                    └────────┬────────┘
                             │ Gateway WS (serenity)
                             ▼
                    ┌─────────────────┐
                    │ DiscordAdapter  │──┐
                    └─────────────────┘  │
                                         │  impl ChatAdapter
┌─────────────────┐                      │
│ Telegram Users  │                      ▼
└────────┬────────┘             ┌─────────────────┐     ┌──────────────┐
         │ Bot API (teloxide)   │   AdapterRouter  │────►│ SessionPool  │
         ▼                      │                  │     │  (unchanged) │
┌─────────────────┐             │  - route message │     └──────┬───────┘
│ TelegramAdapter │────────────►│  - manage threads│            │ ACP stdio
└─────────────────┘             │  - stream edits  │     ┌──────▼───────┐
                                └─────────────────┘     │  AcpConnection│
┌─────────────────┐                      ▲              │  (unchanged)  │
│  Slack Users    │                      │              └───────────────┘
└────────┬────────┘             ┌─────────────────┐
         │ Socket Mode          │  SlackAdapter   │──┘
         ▼                      └─────────────────┘
┌─────────────────┐
│  SlackAdapter   │
└─────────────────┘
```

---

## 1. ChatAdapter Trait

The core abstraction. Each platform implements this trait.

```rust
#[async_trait]
pub trait ChatAdapter: Send + Sync + 'static {
    /// Platform name for logging and config ("discord", "telegram", "slack")
    fn platform(&self) -> &'static str;

    /// Start listening for events. Blocks until shutdown.
    async fn run(&self, router: Arc<AdapterRouter>) -> Result<()>;

    /// Send a new message, returns platform-specific message ID
    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef>;

    /// Edit an existing message in-place
    async fn edit_message(&self, msg: &MessageRef, content: &str) -> Result<()>;

    /// Create a thread/topic from a message, returns thread channel ref
    async fn create_thread(&self, channel: &ChannelRef, trigger_msg: &MessageRef, title: &str) -> Result<ChannelRef>;

    /// Add a reaction/emoji to a message
    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;

    /// Remove a reaction/emoji from a message
    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()>;
}
```

### Platform-Agnostic References

```rust
/// Identifies a channel/thread/topic across platforms
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ChannelRef {
    pub platform: String,      // "discord", "telegram", "slack"
    pub channel_id: String,    // platform-native ID
    pub parent_id: Option<String>,  // parent channel if this is a thread
}

/// Identifies a message across platforms
#[derive(Clone, Debug)]
pub struct MessageRef {
    pub platform: String,
    pub channel: ChannelRef,
    pub message_id: String,
}

/// Sender identity (already exists as sender_context JSON)
#[derive(Clone, Debug, Serialize)]
pub struct SenderContext {
    pub schema: String,        // "agent-broker.sender.v1"
    pub sender_id: String,
    pub sender_name: String,
    pub display_name: String,
    pub channel: String,       // platform name
    pub channel_id: String,
    pub is_bot: bool,
}
```

---

## 2. AdapterRouter

Shared logic extracted from current `discord.rs` that is platform-independent:

```rust
pub struct AdapterRouter {
    pool: Arc<SessionPool>,
    reactions_config: ReactionsConfig,
}

impl AdapterRouter {
    /// Called by any adapter when a user message arrives.
    /// Handles: session creation, prompt injection, streaming, reactions.
    pub async fn handle_message(
        &self,
        adapter: &dyn ChatAdapter,
        channel: &ChannelRef,
        sender: &SenderContext,
        prompt: &str,
        trigger_msg: &MessageRef,
    ) -> Result<()>;
}
```

The router owns:
- Session pool interaction (`get_or_create`, `with_connection`)
- Sender context injection (`<sender_context>` XML wrapping)
- Edit-streaming loop (1.5s interval, message splitting at 2000/4096 chars depending on platform)
- Reaction state machine (queued → thinking → tool → done/error)
- Thread creation decision (new thread vs. existing thread)

Each adapter only needs to:
1. Listen for platform events
2. Determine if the message should be processed (allowed channels, @mention, thread check)
3. Call `router.handle_message()`

---

## 3. Platform Comparison

| Feature | Discord | Telegram | Slack |
|---------|---------|----------|-------|
| Connection | Gateway WebSocket (`serenity`) | Bot API polling / webhook (`teloxide`) | Socket Mode WebSocket / Events API |
| Threading | Discord threads | Forum topics | Slack threads |
| Message limit | 2000 chars | 4096 chars | 4000 chars (blocks: 3000) |
| Edit support | ✅ Full | ✅ Full | ✅ Full |
| Reactions | ✅ Unicode + custom emoji | ✅ Unicode emoji | ✅ Unicode + custom emoji |
| Trigger | `@mention` | `@mention` or any message (configurable) | `@mention` or app_mention event |
| Bot message filtering | `msg.author.bot` | `msg.from.is_bot` | `event.bot_id` present |
| Auth | Bot token | Bot token | Bot token + App token (Socket Mode) |

---

## 4. Config Design

```toml
# Enable one or more adapters. Multiple can run simultaneously.

[discord]
bot_token = "${DISCORD_BOT_TOKEN}"
allowed_channels = ["1234567890"]

[telegram]
bot_token = "${TELEGRAM_BOT_TOKEN}"
mode = "personal"                    # "personal" or "team" (see #86)
allowed_users = []                   # empty = deny all (secure by default, per #91)

[slack]
bot_token = "${SLACK_BOT_TOKEN}"
app_token = "${SLACK_APP_TOKEN}"     # for Socket Mode
allowed_channels = ["C1234567890"]

# Agent and pool config remain unchanged
[agent]
command = "kiro-cli"
args = ["acp", "--trust-all-tools"]
working_dir = "/home/agent"

[pool]
max_sessions = 10
session_ttl_hours = 24
```

### Startup Behavior

- agent-broker reads config and starts an adapter for each `[platform]` section present
- If no adapter section is configured → error and exit
- Each adapter runs as a separate tokio task, sharing the same `SessionPool`
- Session keys are namespaced: `discord:{thread_id}`, `telegram:{topic_id}`, `slack:{thread_ts}`

---

## 5. Message Size Handling

Each platform has different message limits. The `format::split_message` function should accept a configurable limit:

```rust
pub fn split_message(content: &str, max_len: usize) -> Vec<String>;
```

| Platform | Max chars | Current |
|----------|-----------|---------|
| Discord  | 2000      | ✅ Hardcoded 1900 |
| Telegram | 4096      | New |
| Slack    | 4000      | New |

---

## 6. Reaction Mapping

The `StatusReactionController` currently uses Unicode emoji which work across all three platforms. No changes needed for the emoji set, but the underlying API calls differ:

| Action | Discord | Telegram | Slack |
|--------|---------|----------|-------|
| Add reaction | `create_reaction()` | `set_message_reaction()` | `reactions.add` |
| Remove reaction | `delete_reaction()` | `set_message_reaction()` (empty) | `reactions.remove` |

The `ChatAdapter` trait's `add_reaction` / `remove_reaction` methods abstract this.

---

## 7. Security Considerations

- **Secure by default** (#91): empty allowlist = deny all, for every platform
- **Bot message filtering**: each adapter must ignore messages from bots to prevent loops
- **Token isolation**: each platform's tokens are independent env vars
- **Session namespace isolation**: `discord:123` and `slack:123` are separate sessions even if IDs collide
- **Rate limiting**: platform-specific rate limits should be respected (Discord 5/5s, Slack 1/s per channel, Telegram 30/s)

---

## 8. Implementation Phases

| Phase | Scope | Complexity | Depends on |
|-------|-------|------------|------------|
| **Phase 1** | Extract `ChatAdapter` trait + `AdapterRouter` from `discord.rs`, refactor Discord to implement trait | Medium | — |
| **Phase 2** | Telegram adapter (`teloxide`), personal + team modes | Medium | Phase 1, #86 |
| **Phase 3** | Slack adapter (Socket Mode), channel threading | Medium | Phase 1, #93 |
| **Phase 4** | Multi-adapter simultaneous mode (Discord + Telegram + Slack in one process) | Low | Phase 1-3 |
| **Phase 5** | Platform-specific features: Slack blocks, Telegram inline keyboards, Discord embeds | Low | Phase 1-3 |

Each phase is independently shippable. Phase 1 is a pure refactor with no behavior change.

---

## 9. Kubernetes / Helm Considerations

- Single image supports all adapters (all compiled in)
- Helm `values.yaml` gains `telegram.*` and `slack.*` sections
- Adapter selection is config-driven, not build-time
- PVC storage structure unchanged — auth tokens are per-agent, not per-platform

---

## Open Questions

1. Should adapters share a single `SessionPool` or have separate pools? (Shared is simpler, separate gives better isolation)
2. Should session keys include platform prefix (`discord:123`) or rely on ID uniqueness? (Prefix is safer)
3. Multi-adapter mode: should there be a global `max_sessions` or per-adapter limits?
4. Should the `ChatAdapter` trait support rich messages (embeds, blocks, inline keyboards) or keep it text-only for v1?
5. How to handle platform-specific features like Slack's `reply_broadcast` or Telegram's `parse_mode`?

---

_Comments and feedback welcome._
