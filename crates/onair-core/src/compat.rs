//! Compatibility marker constants shared by the config layer and the proxy.
//!
//! These are the only structural compat markers recognized by the
//! router. They are referenced from `[[route]].expose` to opt a public
//! model in to a client-to-upstream translation path.

pub const RESPONSES_VIA_CHAT_COMPLETIONS: &str = "responses_via_chat_completions";
pub const CHAT_COMPLETIONS_VIA_RESPONSES: &str = "chat_completions_via_responses";
pub const CHAT_COMPLETIONS_VIA_MESSAGES: &str = "chat_completions_via_messages";
