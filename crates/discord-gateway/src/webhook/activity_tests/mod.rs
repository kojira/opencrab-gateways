use super::*;
use crate::webhook::*;
use std::sync::atomic::AtomicUsize;

fn join_delivered(msgs: &[WebhookMessage]) -> String {
    msgs.iter()
        .map(WebhookMessage::delivered_text)
        .collect::<Vec<_>>()
        .join("\n")
}

include!("webhook_delivery.rs");
include!("argument_summaries.rs");
include!("activity_sink.rs");
