/// Structural request markers that the router understands.
///
/// Keep this list in sync with new structural markers; it intentionally
/// only contains the typo-prone structural set, not every possible
/// forward-compatible path family.
pub const KNOWN_MARKERS: &[&str] = &[
    "all",
    "streaming",
    "chat",
    "chat_completions",
    "completions",
    "messages",
    "responses",
    "response",
    "tools",
    "tool_calls",
    "function_calling",
    "functions",
    "responses_via_chat_completions",
    "chat_completions_via_responses",
    "chat_completions_via_messages",
    "embeddings",
    "embedding",
    "images",
    "image",
    "audio",
    "files",
    "file",
    "models",
    "model",
    "batches",
    "batch",
    "fine_tuning",
    "assistants",
    "assistant",
    "threads",
    "thread",
    "vector_stores",
    "vector_store",
    "uploads",
    "upload",
];

pub fn is_known_marker(value: &str) -> bool {
    KNOWN_MARKERS.contains(&value)
}
