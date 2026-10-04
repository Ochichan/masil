use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, value::RawValue};

#[derive(Debug)]
pub(crate) struct Evidence {
    raw: Box<RawValue>,
    state: Box<str>,
    visible_blocker: bool,
    skip_state_update: bool,
    visible_idle: bool,
    /// The screen's turn-end marker digest when an idle rule won.
    ends_turn: Option<Box<str>>,
    /// The digest of any turn-end marker on screen, whichever rule won.
    turn_end_marker: Option<Box<str>>,
}

fn digest(value: &Value, key: &str) -> Option<Box<str>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| text.len() <= 64)
        .map(Into::into)
}

impl Evidence {
    pub(crate) fn from_value(value: Value) -> Self {
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .into();
        let visible_blocker = matches!(value.get("visible_blocker"), Some(Value::Bool(true)));
        let skip_state_update = matches!(value.get("skip_state_update"), Some(Value::Bool(true)));
        let visible_idle = matches!(value.get("visible_idle"), Some(Value::Bool(true)));
        let ends_turn = digest(&value, "ends_turn");
        let turn_end_marker = digest(&value, "turn_end_marker");
        let raw = serde_json::value::to_raw_value(&value)
            .expect("serializing a serde_json::Value cannot fail");
        Self {
            raw,
            state,
            visible_blocker,
            skip_state_update,
            visible_idle,
            ends_turn,
            turn_end_marker,
        }
    }

    pub(crate) fn report_overlay(
        original: &Self,
        state: &str,
        sequence: u64,
        observed_at_ms: u64,
    ) -> Self {
        #[derive(Serialize)]
        struct Overlay<'a> {
            state: &'a str,
            source: &'static str,
            sequence: u64,
            observed_at_ms: u64,
            screen: &'a Evidence,
        }

        let raw = serde_json::value::to_raw_value(&Overlay {
            state,
            source: "run_report",
            sequence,
            observed_at_ms,
            screen: original,
        })
        .expect("serializing run report evidence cannot fail");
        Self {
            raw,
            state: state.into(),
            visible_blocker: false,
            skip_state_update: false,
            visible_idle: false,
            ends_turn: None,
            // What the screen under the report shows.
            turn_end_marker: original.turn_end_marker.clone(),
        }
    }

    pub(crate) fn state(&self) -> &str {
        &self.state
    }

    pub(crate) fn visible_blocker(&self) -> bool {
        self.visible_blocker
    }

    pub(crate) fn skip_state_update(&self) -> bool {
        self.skip_state_update
    }

    pub(crate) fn visible_idle(&self) -> bool {
        self.visible_idle
    }

    pub(crate) fn ends_turn(&self) -> Option<&str> {
        self.ends_turn.as_deref()
    }

    pub(crate) fn turn_end_marker(&self) -> Option<&str> {
        self.turn_end_marker.as_deref()
    }
}

impl Serialize for Evidence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Evidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Value::deserialize(deserializer).map(Self::from_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arbitrary_json_shape_roundtrips_through_value() {
        let value = json!([
            null,
            {"nested": [true, false, 17, -2.5], "state": {"not": "a string"}},
            "tail"
        ]);

        let evidence: Evidence = serde_json::from_value(value.clone()).unwrap();

        assert_eq!(serde_json::to_value(&evidence).unwrap(), value);
        assert_eq!(evidence.state(), "unknown");
    }

    #[test]
    fn escaped_and_unicode_text_roundtrips_through_json_text() {
        let source =
            r#"{"state":"idle","text":"한글 π \"quoted\" \\ slash\nline","emoji":"\uD83D\uDE80"}"#;
        let expected: Value = serde_json::from_str(source).unwrap();

        let evidence: Evidence = serde_json::from_str(source).unwrap();

        assert_eq!(serde_json::to_value(&evidence).unwrap(), expected);
        assert_eq!(evidence.state(), "idle");
    }

    #[test]
    fn summaries_only_accept_boolean_true_flags() {
        let evidence = Evidence::from_value(json!({
            "state": "blocked",
            "visible_blocker": true,
            "skip_state_update": true
        }));
        assert_eq!(evidence.state(), "blocked");
        assert!(evidence.visible_blocker());
        assert!(evidence.skip_state_update());

        let lookalikes = Evidence::from_value(json!({
            "state": null,
            "visible_blocker": 1,
            "skip_state_update": "true"
        }));
        assert_eq!(lookalikes.state(), "unknown");
        assert!(!lookalikes.visible_blocker());
        assert!(!lookalikes.skip_state_update());
    }

    #[test]
    fn report_overlay_preserves_the_original_screen_shape() {
        let original_value = json!({
            "state": "blocked",
            "source": "screen",
            "visible_blocker": true,
            "skip_state_update": true,
            "explanations": [{"rule": "question", "detail": "Proceed? 한글"}]
        });
        let original = Evidence::from_value(original_value.clone());

        let overlay = Evidence::report_overlay(&original, "working", 7, 1234);

        assert_eq!(
            serde_json::to_value(&overlay).unwrap(),
            json!({
                "state": "working",
                "source": "run_report",
                "sequence": 7,
                "observed_at_ms": 1234,
                "screen": original_value
            })
        );
        assert_eq!(overlay.state(), "working");
        assert!(!overlay.visible_blocker());
        assert!(!overlay.skip_state_update());
    }
}
