use super::*;

#[derive(Debug, Clone)]
pub enum JsonRpcRequests {
    Single(JsonRpcRequest),
    Batch(Vec<JsonRpcRequest>),
}

impl From<JsonRpcRequests> for JsonValue {
    fn from(json_rpc_requests: JsonRpcRequests) -> JsonValue {
        match json_rpc_requests {
            JsonRpcRequests::Single(request) => JsonValue::from(request),
            JsonRpcRequests::Batch(requests) => {
                JsonValue::Array(
                    requests
                    .into_iter()
                    .map(|request| JsonValue::from(request))
                    .collect()
                )
            },
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcRequestsFromJsonError {
    #[error("malformed request: {}", source)]
    MalformedRequest {
        source: JsonRpcRequestFromJsonError,
    },
    #[error("empty batch array")]
    EmptyBatchArray,
    #[error("expected a request json object or json array of requests")]
    InvalidJsonType,
}

impl TryFrom<JsonValue> for JsonRpcRequests {
    type Error = JsonRpcRequestsFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcRequests, JsonRpcRequestsFromJsonError> {
        match json {
            JsonValue::Object(..) => match JsonRpcRequest::try_from(json) {
                Ok(request) => Ok(JsonRpcRequests::Single(request)),
                Err(source) => Err(JsonRpcRequestsFromJsonError::MalformedRequest { source }),
            },
            JsonValue::Array(requests_json) => {
                if requests_json.is_empty() {
                    return Err(JsonRpcRequestsFromJsonError::EmptyBatchArray);
                }
                let mut requests = Vec::with_capacity(requests_json.len());
                for request_json in requests_json {
                    let request = match JsonRpcRequest::try_from(request_json) {
                        Ok(request) => request,
                        Err(source) => {
                            return Err(JsonRpcRequestsFromJsonError::MalformedRequest { source });
                        },
                    };
                    requests.push(request);
                }
                Ok(JsonRpcRequests::Batch(requests))
            },
            _ => Err(JsonRpcRequestsFromJsonError::InvalidJsonType),
        }
    }
}

