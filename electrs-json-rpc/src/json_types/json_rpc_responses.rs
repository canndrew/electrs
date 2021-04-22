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

#[derive(Debug, Error)]
pub enum JsonRpcResponsesFromJsonError {
    #[error("malformed response: {}", source)]
    MalformedResponse {
        source: JsonRpcResponseFromJsonError,
    },
    #[error("empty batch array")]
    EmptyBatchArray,
    #[error("expected a response json object or json array of responses")]
    InvalidJsonType,
}

impl TryFrom<JsonValue> for JsonRpcResponses {
    type Error = JsonRpcResponsesFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcResponses, JsonRpcResponsesFromJsonError> {
        match json {
            JsonValue::Object(..) => match JsonRpcResponse::try_from(json) {
                Ok(response) => Ok(JsonRpcResponses::Single(response)),
                Err(source) => Err(JsonRpcResponsesFromJsonError::MalformedResponse { source }),
            },
            JsonValue::Array(responses_json) => {
                if responses_json.is_empty() {
                    return Err(JsonRpcResponsesFromJsonError::EmptyBatchArray);
                }
                let mut responses = Vec::with_capacity(responses_json.len());
                for response_json in responses_json {
                    let response = match JsonRpcResponse::try_from(response_json) {
                        Ok(response) => response,
                        Err(source) => {
                            return Err(JsonRpcResponsesFromJsonError::MalformedResponse { source });
                        },
                    };
                    responses.push(response);
                }
                Ok(JsonRpcResponses::Batch(responses))
            },
            _ => Err(JsonRpcResponsesFromJsonError::InvalidJsonType),
        }
    }
}

