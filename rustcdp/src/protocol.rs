use serde::{Serialize, de::DeserializeOwned};

/// A CDP command and its statically associated response payload.
pub trait Command: Serialize {
    type Response: DeserializeOwned;
    const METHOD: &'static str;
}

/// A typed event payload. Subscribe before sending the command that causes it.
pub trait Event: DeserializeOwned {
    const METHOD: &'static str;
}

// Missing `any` fields use serde's default; present null must remain a JSON value.
pub(crate) fn present_json<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use crate::cdp::web_audio::BaseAudioContext;
    use serde_json::json;

    #[test]
    fn web_audio_accepts_older_contexts_without_defaulting_missing_required_fields() {
        let mut payload = json!({
            "contextId": "audio", "contextType": "realtime", "contextState": "running",
            "callbackBufferSize": 128, "maxOutputChannelCount": 2, "sampleRate": 48000,
        });
        let context: BaseAudioContext = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(context.render_quantum_size, None);
        assert!(
            serde_json::to_value(context)
                .unwrap()
                .get("renderQuantumSize")
                .is_none()
        );
        payload["renderQuantumSize"] = json!(128);
        let context: BaseAudioContext = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(context.render_quantum_size, Some(128.0));
        payload.as_object_mut().unwrap().remove("sampleRate");
        assert!(serde_json::from_value::<BaseAudioContext>(payload).is_err());
    }
}
