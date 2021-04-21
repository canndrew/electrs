use super::*;

#[derive(Debug, Clone)]
pub enum JsonRpcResponses {
    Single(JsonRpcResponse),
    Batch(Vec<JsonRpcResponse>),
}

impl From<JsonRpcResponses> for JsonValue {
    fn from(json_rpc_responses: JsonRpcResponses) -> JsonValue {
        match json_rpc_responses {
            JsonRpcResponses::Single(response) => JsonValue::from(response),
            JsonRpcResponses::Batch(responses) => {
                JsonValue::Array(
                    responses
                    .into_iter()
                    .map(|response| JsonValue::from(response))
                    .collect()
                )
            },
        }
    }
}


