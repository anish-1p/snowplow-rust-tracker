// Copyright (c) 2022 Snowplow Analytics Ltd. All rights reserved.
//
// This program is licensed to you under the Apache License Version 2.0,
// and you may not use this file except in compliance with the Apache License Version 2.0.
// You may obtain a copy of the Apache License Version 2.0 at http://www.apache.org/licenses/LICENSE-2.0.
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the Apache License Version 2.0 is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the Apache License Version 2.0 for the specific language governing permissions and limitations there under.

/*!
A snowplow event [`Emitter`]. This type manages the low-level details of sending
events over HTTP to a Collector. Generally you should prefer to use a
[`Tracker`][crate::tracker::Tracker], which wraps an [`Emitter`] handles a lot
of the bookkeeping required to construct full snowplow events.
 */

use std::future::ready;

use futures::TryStreamExt as _;
use reqwest::Client;

use serde::Serialize;
use url::Url;

use crate::payload::{Envelope, HasSchema, Schema, SchemaVersion, SnowplowEvent};

/// The outermost type that is actually sent to snowplow as a JSON payload.
/// Includes an outermost schema and a Vec of [`SnowplowEvent`].
// TODO: It will be exceedingly common to only need to send a single event;
// create an optimized version of this type to handle that use case.
type EventContainer<'a, Payload> = Envelope<Vec<SnowplowEvent<'a, Payload>>>;

impl<'a, Payload: HasSchema> EventContainer<'a, Payload> {
    /// Create a new event container. This will collect all of the given events
    /// into a [`Vec`].
    pub fn new(events: impl IntoIterator<Item = SnowplowEvent<'a, Payload>>) -> Self {
        Envelope(events.into_iter().collect())
    }
}

impl<'a, Payload: HasSchema> HasSchema for Vec<SnowplowEvent<'a, Payload>> {
    fn schema(&self) -> Schema {
        Schema::new_snowplow("payload_data", SchemaVersion::new(1, 0, 4))
    }
}

/// Emitter is responsible for emitting tracked events to the Snowplow
/// Collector. It takes care of the low-level HTTP stuff. You should probably
/// be using [`Tracker`][crate::Tracker] instead.
pub struct Emitter {
    collector_url: Url,
    client: Client,
}

impl Emitter {
    /// Create a new emitter that will send events to the given Url using the
    /// given client.
    pub const fn new(collector_url: Url, client: Client) -> Emitter {
        // TODO: log a warning if the Url doesn't look right
        Emitter {
            collector_url,
            client,
        }
    }

    /// Track a batch of events, sending them to the snowplow collector
    pub async fn track_events<Payload: HasSchema + Serialize>(
        &self,
        events: impl IntoIterator<Item = SnowplowEvent<'_, Payload>>,
    ) -> Result<(), reqwest::Error> {
        let events = EventContainer::new(events);

        let response = self
            .client
            .post(self.collector_url.clone())
            .json(&events)
            .send()
            .await?;

        // Snowplow responses don't contain anything useful, so just drain the
        // response content.
        response
            .bytes_stream()
            .try_for_each(|_chunk| ready(Ok(())))
            .await
    }

    /// Track a single event
    pub async fn track_event<Payload: HasSchema + Serialize>(
        &self,
        event: SnowplowEvent<'_, Payload>,
    ) -> Result<(), reqwest::Error> {
        self.track_events([event]).await
    }
}

#[cfg(test)]
mod tests {
    use crate::emitter::EventContainer;
    use crate::{
        payload::{EventType, PayloadWrapper, SnowplowEvent, SnowplowTimestamp},
        util::JsonString,
        HasSchema, Platform, Schema, SchemaVersion, TrackedEvent,
    };
    use serde::Serialize;
    use serde_test::{assert_ser_tokens, Configure, Token};
    use std::time::SystemTime;
    use uuid::Uuid;

    #[derive(Debug, Serialize)]
    struct WebPage {
        name: String,
        id: String,
    }

    impl HasSchema for WebPage {
        fn schema(&self) -> Schema {
            Schema::new(
                "com.snowplowanalytics.snowplow",
                "screen_view",
                SchemaVersion::new(1, 0, 0),
            )
        }
    }

    #[test]
    fn test_envelope_serialization() {
        let test_payload = WebPage {
            name: "test".to_owned(),
            id: "test id".to_owned(),
        };
        let wrapper = PayloadWrapper::new(test_payload);
        assert_ser_tokens(
            &wrapper,
            &[
                Token::Struct {
                    name: "Envelope",
                    len: 2,
                },
                Token::Str("schema"),
                Token::Str("iglu:com.snowplowanalytics.snowplow/unstruct_event/jsonschema/1-0-0"),
                Token::Str("data"),
                Token::Struct {
                    name: "Envelope",
                    len: 2,
                },
                Token::Str("schema"),
                Token::Str("iglu:com.snowplowanalytics.snowplow/screen_view/jsonschema/1-0-0"),
                Token::Str("data"),
                Token::Struct {
                    name: "WebPage",
                    len: 2,
                },
                Token::Str("name"),
                Token::Str("test"),
                Token::Str("id"),
                Token::Str("test id"),
                Token::StructEnd,
                Token::StructEnd,
                Token::StructEnd,
            ],
        );
    }

    #[test]
    fn test_emitter_event_construction() {
        let test_payload = WebPage {
            name: "test".to_owned(),
            id: "test id".to_owned(),
        };
        let mut test_event = TrackedEvent::new(test_payload);
        let test_uuid =
            Uuid::parse_str("a1a2a3a4b1b2c1c2d1d2d3d4d5d6d7d8").expect("failed to create UUID");
        let time_since_epoch;
        let current_timestamp = SystemTime::now();
        match current_timestamp.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => {
                test_event.timestamp = Some(SnowplowTimestamp::from(current_timestamp));
                time_since_epoch = duration.as_millis();
            }
            Err(_) => panic!("SystemTime before UNIX EPOCH!"),
        }

        let event_timestamp: &'static str =
            Box::leak(time_since_epoch.to_string().into_boxed_str());

        test_event.id = Some(test_uuid.clone());
        let now = SnowplowTimestamp::now();
        let events = [test_event].into_iter().map(|event| SnowplowEvent {
            event_type: EventType::SelfDescribingEvent,
            payload: JsonString(PayloadWrapper::new(event.payload)),
            platform: Platform::Desktop,
            app_id: "test id",
            tracker_id: "test tracker ID",
            namespace: "test namespace",
            event_id: event.id,
            created_timestamp: event.timestamp.unwrap_or(now),
            sent_timestamp: event.timestamp.unwrap_or(now),
        });

        let events = EventContainer::new(events);
        assert_ser_tokens(
                    &events.readable(),
                    &[
                        Token::Struct {
                            name: "Envelope",
                            len: 2,
                        },
                        Token::Str("schema"),
                        Token::Str("iglu:com.snowplowanalytics.snowplow/payload_data/jsonschema/1-0-4"),
                        Token::Str("data"),
                        Token::Seq { len: Some(1), },
                        Token::Struct { name: "SnowplowEvent", len: 9, },
                        Token::Str("e"),
                        Token::UnitVariant { name: "EventType", variant: "ue", },
                        Token::Str("ue_pr"),
                        Token::Str("{\"schema\":\"iglu:com.snowplowanalytics.snowplow/unstruct_event/jsonschema/1-0-0\",\"data\":{\"schema\":\"iglu:com.snowplowanalytics.snowplow/screen_view/jsonschema/1-0-0\",\"data\":{\"name\":\"test\",\"id\":\"test id\"}}}"),
                        Token::Str("p"),
                        Token::UnitVariant { name: "Platform", variant: "pc", },
                        Token::Str("aid"),
                        Token::Str("test id"),
                        Token::Str("tv"),
                        Token::Str("test tracker ID"),
                        Token::Str("tna"),
                        Token::Str("test namespace"),
                        Token::Str("eid"),
                        Token::Some,
                        Token::Str("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8"),
                        Token::Str("dtm"),
                        Token::Str(event_timestamp),
                        Token::Str("stm"),
                        Token::Str(event_timestamp),
                        Token::StructEnd,
                        Token::SeqEnd,
                        Token::StructEnd,
                    ]
                );
    }
}
