use super::*;

#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequests),
    Response(JsonRpcResponses),
}

impl From<JsonRpcMessage> for JsonValue {
    fn from(json_rpc_message: JsonRpcMessage) -> JsonValue {
        match json_rpc_message {
            JsonRpcMessage::Request(request) => {
                JsonValue::from(request)
            },
            JsonRpcMessage::Response(response) => {
                JsonValue::from(response)
            },
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcMessageFromJsonError {
    #[error("malformed request: {}", source)]
    MalformedRequest {
        source: JsonRpcRequestFromJsonError,
    },
    #[error("malformed request at index {}: {}", index, source)]
    MalformedRequestInBatch {
        index: usize,
        source: JsonRpcRequestFromJsonError,
    },
    #[error("malformed response: {}", source)]
    MalformedResponse {
        source: JsonRpcResponseFromJsonError,
    },
    #[error("malformed response at index {}: {}", index, source)]
    MalformedResponseInBatch {
        index: usize,
        source: JsonRpcResponseFromJsonError,
    },
    #[error("malformed request/response object")]
    MalformedObject,
    #[error("malformed request/response object at index {}", index)]
    MalformedObjectInBatch {
        index: usize,
    },
    #[error("json-rpc messages must be json arrays or json objects")]
    InvalidJsonType,
    #[error("request/response at index {} is not an object", index)]
    InvalidJsonTypeInBatch {
        index: usize,
    },
    #[error("batch array contains both requests and responses")]
    HeterogeneousBatchMessage,
    #[error("empty batch array")]
    EmptyBatchArray,
}

impl JsonRpcMessageFromJsonError {
    pub fn original_request_id(&self) -> Option<&JsonRpcId> {
        match self {
            JsonRpcMessageFromJsonError::MalformedRequest { source } |
            JsonRpcMessageFromJsonError::MalformedRequestInBatch { source, .. } => {
                source.original_request_id()
            },
            JsonRpcMessageFromJsonError::MalformedResponse { .. } |
            JsonRpcMessageFromJsonError::MalformedResponseInBatch { .. } |
            JsonRpcMessageFromJsonError::MalformedObject |
            JsonRpcMessageFromJsonError::MalformedObjectInBatch { .. } |
            JsonRpcMessageFromJsonError::InvalidJsonType |
            JsonRpcMessageFromJsonError::InvalidJsonTypeInBatch { .. } |
            JsonRpcMessageFromJsonError::HeterogeneousBatchMessage |
            JsonRpcMessageFromJsonError::EmptyBatchArray => None,
        }
    }

    pub fn as_error_response(&self) -> Option<JsonRpcResponse> {
        match self {
            JsonRpcMessageFromJsonError::MalformedResponse { .. } |
            JsonRpcMessageFromJsonError::MalformedResponseInBatch { .. } => return None,
            _ => (),
        }
        let message = self.to_string();
        let id = self.original_request_id().cloned().unwrap_or(JsonRpcId::Null);
        Some(JsonRpcResponse {
            id,
            result: Err(JsonRpcError {
                code: JsonRpcErrorCode::INVALID_REQUEST,
                message,
                data: None,
            }),
        })
    }
}

impl TryFrom<JsonValue> for JsonRpcMessage {
    type Error = JsonRpcMessageFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcMessage, JsonRpcMessageFromJsonError> {
        let is_request = |map: &serde_json::Map<_, _>| -> Option<bool> {
            if map.contains_key("method") {
                Some(true)
            } else if map.contains_key("result") || map.contains_key("error") {
                Some(false)
            } else {
                None
            }
        };
        match json {
            JsonValue::Object(ref map) => {
                match is_request(map) {
                    Some(true) => match JsonRpcRequest::try_from(json) {
                        Ok(request) => {
                            Ok(JsonRpcMessage::Request(JsonRpcRequests::Single(request)))
                        },
                        Err(source) => {
                            Err(JsonRpcMessageFromJsonError::MalformedRequest { source })
                        },
                    },
                    Some(false) => match JsonRpcResponse::try_from(json) {
                        Ok(response) => {
                            Ok(JsonRpcMessage::Response(JsonRpcResponses::Single(response)))
                        },
                        Err(source) => {
                            Err(JsonRpcMessageFromJsonError::MalformedResponse { source })
                        },
                    },
                    None => Err(JsonRpcMessageFromJsonError::MalformedObject),
                }
            },
            JsonValue::Array(array) => {
                let mut all_requests = true;
                let mut all_responses = true;
                for (index, elem) in array.iter().enumerate() {
                    let map = match elem {
                        JsonValue::Object(map) => map,
                        _ => {
                            return Err(JsonRpcMessageFromJsonError::InvalidJsonTypeInBatch {
                                index,
                            });
                        },
                    };
                    let elem_is_request = match is_request(map) {
                        Some(elem_is_request) => elem_is_request,
                        None => {
                            return Err(JsonRpcMessageFromJsonError::MalformedObjectInBatch {
                                index,
                            });
                        },
                    };
                    all_requests &= elem_is_request;
                    all_responses &= !elem_is_request;
                }
                match (all_requests, all_responses) {
                    (false, false) => {
                        Err(JsonRpcMessageFromJsonError::HeterogeneousBatchMessage)
                    },
                    (true, false) => {
                        let mut requests = Vec::with_capacity(array.len());
                        for (index, elem) in array.into_iter().enumerate() {
                            let request = match JsonRpcRequest::try_from(elem) {
                                Ok(request) => request,
                                Err(source) => {
                                    return Err(JsonRpcMessageFromJsonError::MalformedRequestInBatch {
                                        index, source,
                                    })
                                },
                            };
                            requests.push(request);
                        }
                        Ok(JsonRpcMessage::Request(JsonRpcRequests::Batch(requests)))
                    },
                    (false, true) => {
                        let mut responses = Vec::with_capacity(array.len());
                        for (index, elem) in array.into_iter().enumerate() {
                            let response = match JsonRpcResponse::try_from(elem) {
                                Ok(response) => response,
                                Err(source) => {
                                    return Err(JsonRpcMessageFromJsonError::MalformedResponseInBatch {
                                        index, source,
                                    })
                                },
                            };
                            responses.push(response);
                        }
                        Ok(JsonRpcMessage::Response(JsonRpcResponses::Batch(responses)))
                    },
                    (true, true) => Err(JsonRpcMessageFromJsonError::EmptyBatchArray),
                }
            },
            _ => Err(JsonRpcMessageFromJsonError::InvalidJsonType),
        }
    }
}

