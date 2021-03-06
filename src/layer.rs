use regex::Regex;
use serde::ser::{SerializeMap, Serializer};
use serde_json::Value;
use tracing::{Event, Subscriber};
use tracing_bunyan_formatter::JsonStorage;
use tracing_subscriber::{layer::Context, Layer};

use crate::filters::{EventFilters, Filter};
use crate::worker::{WorkerMessage, SlackBackgroundWorker};
use crate::{config::SlackConfig, message::SlackPayload, worker::worker, ChannelSender};
use std::collections::HashMap;

/// Layer for forwarding tracing events to Slack.
pub struct SlackLayer {
    /// Filter events by their target.
    ///
    /// Filter type semantics:
    /// - Subtractive: Exclude an event if the target does NOT MATCH a given regex.
    /// - Additive: Exclude an event if the target MATCHES a given regex.
    target_filters: EventFilters,

    /// Filter events by their message.
    ///
    /// Filter type semantics:
    /// - Positive: Exclude an event if the message MATCHES a given regex, and
    /// - Negative: Exclude an event if the message does NOT MATCH a given regex.
    message_filters: Option<EventFilters>,

    /// Filter events by fields.
    ///
    /// Filter type semantics:
    /// - Positive: Exclude the event if its key MATCHES a given regex.
    /// - Negative: Exclude the event if its key does NOT MATCH a given regex.
    event_by_field_filters: Option<EventFilters>,

    /// Filter fields of events from being sent to Slack.
    ///
    /// Filter type semantics:
    /// - Positive: Exclude event fields if the field's key MATCHES any provided regular expressions.
    field_exclusion_filters: Option<Vec<Regex>>,

    /// Configure the layer's connection to the Slack Webhook API.
    config: SlackConfig,

    /// An unbounded sender, which the caller must send `WorkerMessage::Shutdown` in order to cancel
    /// worker's receive-send loop.
    shutdown_sender: ChannelSender,
}

impl SlackLayer {
    /// Create a new layer for forwarding messages to Slack, using a specified
    /// configuration. This method spawns a task onto the tokio runtime to begin sending tracing
    /// events to Slack.
    ///
    /// Returns the tracing_subscriber::Layer impl to add to a registry, an unbounded-mpsc sender
    /// used to shutdown the background worker, and a future to spawn as a task on a tokio runtime
    /// to initialize the worker's processing and sending of HTTP requests to the Slack API.
    pub(crate) fn new(
        target_filters: EventFilters,
        message_filters: Option<EventFilters>,
        event_by_field_filters: Option<EventFilters>,
        field_exclusion_filters: Option<Vec<Regex>>,
        config: SlackConfig,
    ) -> (SlackLayer, SlackBackgroundWorker) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let layer = SlackLayer {
            target_filters,
            message_filters,
            field_exclusion_filters,
            event_by_field_filters,
            config,
            shutdown_sender: tx.clone(),
        };
        let worker = SlackBackgroundWorker {
            sender: tx,
            handle: tokio::spawn(worker(rx))
        };
        (layer, worker)
    }

    /// Create a new builder for SlackLayer.
    pub fn builder(target_filters: EventFilters) -> SlackLayerBuilder {
        SlackLayerBuilder::new(target_filters)
    }
}

/// A builder for creating a Slack layer.
///
/// The layer requires a regex for selecting events to be sent to Slack by their target. Specifying
/// no filter (e.g. ".*") will cause an explosion in the number of messages observed by the layer.
///
/// Several methods expose initialization of optional filtering mechanisms, along with Slack
/// configuration that defaults to searching in the local environment variables.
pub struct SlackLayerBuilder {
    target_filters: EventFilters,
    message_filters: Option<EventFilters>,
    event_by_field_filters: Option<EventFilters>,
    field_exclusion_filters: Option<Vec<Regex>>,
    config: Option<SlackConfig>,
}

impl SlackLayerBuilder {
    pub(crate) fn new(target_filters: EventFilters) -> Self {
        Self {
            target_filters,
            message_filters: None,
            event_by_field_filters: None,
            field_exclusion_filters: None,
            config: None,
        }
    }

    /// Filter events by their message.
    ///
    /// Filter type semantics:
    /// - Positive: Exclude an event if the message MATCHES a given regex, and
    /// - Negative: Exclude an event if the message does NOT MATCH a given regex.
    pub fn message_filters(mut self, filters: EventFilters) -> Self {
        self.message_filters = Some(filters);
        self
    }

    /// Filter events by fields.
    ///
    /// Filter type semantics:
    /// - Positive: Exclude the event if its key MATCHES a given regex.
    /// - Negative: Exclude the event if its key does NOT MATCH a given regex.
    pub fn event_by_field_filters(mut self, filters: EventFilters) -> Self {
        self.event_by_field_filters = Some(filters);
        self
    }

    /// Filter fields of events from being sent to Slack.
    ///
    /// Filter type semantics:
    /// - Positive: Exclude event fields if the field's key MATCHES any provided regular expressions.
    pub fn field_exclusion_filters(mut self, filters: Vec<Regex>) -> Self {
        self.field_exclusion_filters = Some(filters);
        self
    }

    /// Configure the layer's connection to the Slack Webhook API.
    pub fn slack_config(mut self, config: SlackConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Create a SlackLayer and its corresponding background worker to (async) send the messages.
    pub fn build(self) -> (SlackLayer, SlackBackgroundWorker) {
        SlackLayer::new(
            self.target_filters,
            self.message_filters,
            self.event_by_field_filters,
            self.field_exclusion_filters,
            self.config.unwrap_or_else(SlackConfig::new_from_env),
        )
    }
}

impl<S> Layer<S> for SlackLayer
    where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let current_span = ctx.lookup_current();
        let mut event_visitor = JsonStorage::default();
        event.record(&mut event_visitor);

        let format = || {
            const KEYWORDS: [&str; 2] = ["message", "error"];

            let target = event.metadata().target();
            self.target_filters.process(target)?;

            // Extract the "message" field, if provided. Fallback to the target, if missing.
            let message = event_visitor
                .values()
                .get("message")
                .map(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .flatten()
                .or_else(|| {
                    event_visitor
                        .values()
                        .get("error")
                        .map(|v| match v {
                            Value::String(s) => Some(s.as_str()),
                            _ => None,
                        })
                        .flatten()
                })
                .unwrap_or_else(|| "No message");
            self.message_filters.process(message)?;

            let mut metadata_buffer = Vec::new();
            let mut serializer = serde_json::Serializer::new(&mut metadata_buffer);
            let mut map_serializer = serializer.serialize_map(None)?;
            // Add all the other fields associated with the event, expect the message we
            // already used.
            for (key, value) in event_visitor
                .values()
                .iter()
                .filter(|(&key, _)| !KEYWORDS.contains(&key))
                .filter(|(&key, _)| self.field_exclusion_filters.process(key).is_ok())
            {
                self.event_by_field_filters.process(key)?;
                map_serializer.serialize_entry(key, value)?;
            }
            // Add all the fields from the current span, if we have one.
            if let Some(span) = &current_span {
                let extensions = span.extensions();
                if let Some(visitor) = extensions.get::<JsonStorage>() {
                    for (key, value) in visitor.values() {
                        map_serializer.serialize_entry(key, value)?;
                    }
                }
            }
            map_serializer.end()?;

            let span = match &current_span {
                Some(span) => {
                    span.metadata().name()
                }
                None => "None".into()
            };

            let metadata = {
                let data: HashMap<String, Value> = serde_json::from_slice(metadata_buffer.as_slice()).unwrap();
                serde_json::to_string_pretty(&data).unwrap()
            };

            let message = format!(
                concat!(
                    "*Event [{}]*: \"{}\"\n",
                    "*Span*: _{}_\n",
                    "*Target*: _{}_\n",
                    "*Source*: _{}#L{}_\n",
                    "*Metadata*:\n",
                    "```",
                    "{}",
                    "```",
                ),
                event.metadata().level().to_string(), message,
                span,
                target,
                event.metadata().file().unwrap_or("Unknown"), event.metadata().line().unwrap_or(0),
                metadata
            );

            Ok(message)
        };

        let result: Result<String, crate::filters::FilterError> = format();
        if let Ok(formatted) = result {
            let payload = SlackPayload::new(
                self.config.channel_name.clone(),
                self.config.username.clone(),
                formatted,
                self.config.webhook_url.clone(),
                self.config.icon_emoji.clone(),
            );
            if let Err(e) = self.shutdown_sender.send(WorkerMessage::Data(payload)) {
                tracing::error!(err = %e, "failed to send slack payload to given channel")
            };
        }
    }
}
