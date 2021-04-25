use super::*;

#[derive(Debug, Clone)]
pub struct JsonRpcRequest {
    pub method: String,
    pub params: Option<JsonRpcParams>,
    pub id: Option<JsonRpcId>,
}

impl From<JsonRpcRequest> for JsonValue {
    fn from(json_rpc_request: JsonRpcRequest) -> JsonValue {
        match (json_rpc_request.params, json_rpc_request.id) {
            (None, None) => json! {{
                "jsonrpc": JSON_RPC_VERSION,
                "method": json_rpc_request.method,
            }},
            (None, Some(id)) => json! {{
                "jsonrpc": JSON_RPC_VERSION,
                "method": json_rpc_request.method,
                "id": JsonValue::from(id),
            }},
            (Some(params), None) => json! {{
                "jsonrpc": JSON_RPC_VERSION,
                "method": json_rpc_request.method,
                "params": JsonValue::from(params),
            }},
            (Some(params), Some(id)) => json! {{
                "jsonrpc": JSON_RPC_VERSION,
                "method": json_rpc_request.method,
                "params": JsonValue::from(params),
                "id": JsonValue::from(id),
            }},
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcRequestFromJsonError {
    #[error("json-rpc request must be a json object")]
    InvalidJsonType,
    #[error("malformed request id: {}", source)]
    MalformedId {
        source: JsonRpcIdFromJsonError,
    },
    #[error("unrecognized json-rpc protocol version ({})", version)]
    UnrecognizedVersion {
        version: String,
        id: Option<JsonRpcId>,
    },
    #[error("jsonrpc version field must be a string")]
    InvalidTypeForVersion {
        id: Option<JsonRpcId>,
    },
    #[error("missing jsonrpc version field")]
    MissingVersion {
        id: Option<JsonRpcId>,
    },
    #[error("request method field must be a string")]
    InvalidTypeForMethod {
        id: Option<JsonRpcId>,
    },
    #[error("missing request method field")]
    MissingMethod {
        id: Option<JsonRpcId>,
    },
    #[error("malformed request params: {}", source)]
    MalformedParams {
        source: JsonRpcParamsFromJsonError,
        id: Option<JsonRpcId>,
    },
    #[error("unrecognized field '{}'", field)]
    UnrecognizedField {
        field: String,
        id: Option<JsonRpcId>,
    },
}

impl JsonRpcRequestFromJsonError {
    pub fn original_request_id(&self) -> Option<&JsonRpcId> {
        match self {
            JsonRpcRequestFromJsonError::InvalidJsonType |
            JsonRpcRequestFromJsonError::MalformedId { .. } => None,
            JsonRpcRequestFromJsonError::UnrecognizedVersion { id, .. } |
            JsonRpcRequestFromJsonError::InvalidTypeForVersion { id } |
            JsonRpcRequestFromJsonError::MissingVersion { id } |
            JsonRpcRequestFromJsonError::InvalidTypeForMethod { id } |
            JsonRpcRequestFromJsonError::MissingMethod { id } |
            JsonRpcRequestFromJsonError::MalformedParams { id, .. } |
            JsonRpcRequestFromJsonError::UnrecognizedField { id, .. } => id.as_ref(),
        }
    }
}

impl TryFrom<JsonValue> for JsonRpcRequest {
    type Error = JsonRpcRequestFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcRequest, JsonRpcRequestFromJsonError> {
        let mut map = match json {
            JsonValue::Object(map) => map,
            _ => return Err(JsonRpcRequestFromJsonError::InvalidJsonType),
        };
        let id = match map.remove("id") {
            Some(id_json) => match JsonRpcId::try_from(id_json) {
                Ok(id) => Some(id),
                Err(source) => {
                    return Err(JsonRpcRequestFromJsonError::MalformedId { source });
                },
            },
            None => None,
        };
        match map.remove("jsonrpc") {
            Some(version_json) => match version_json {
                JsonValue::String(version) => {
                    if version != JSON_RPC_VERSION {
                        return Err(JsonRpcRequestFromJsonError::UnrecognizedVersion {
                            version, id,
                        });
                    }
                },
                _ => {
                    return Err(JsonRpcRequestFromJsonError::InvalidTypeForVersion { id });
                },
            },
            None => {
                return Err(JsonRpcRequestFromJsonError::MissingVersion { id });
            },
        }
        let method = match map.remove("method") {
            Some(method_json) => {
                match method_json {
                    JsonValue::String(method) => method,
                    _ => {
                        return Err(JsonRpcRequestFromJsonError::InvalidTypeForMethod { id });
                    },
                }
            },
            None => {
                return Err(JsonRpcRequestFromJsonError::MissingMethod { id });
            },
        };
        let params = match map.remove("params") {
            None => None,
            Some(params_json) => match JsonRpcParams::try_from(params_json) {
                Ok(params) => Some(params),
                Err(source) => {
                    return Err(JsonRpcRequestFromJsonError::MalformedParams { source, id });
                },
            },
        };
        if let Some((field, _value)) = map.into_iter().next() {
            return Err(JsonRpcRequestFromJsonError::UnrecognizedField { field, id });
        }
        Ok(JsonRpcRequest { id, method, params })
    }
}


