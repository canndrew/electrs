use super::*;

#[derive(Debug, Clone)]
pub struct JsonRpcResponse {
    pub id: JsonRpcId,
    pub result: Result<JsonValue, JsonRpcError>,
}

impl From<JsonRpcResponse> for JsonValue {
    fn from(json_rpc_response: JsonRpcResponse) -> JsonValue {
        match json_rpc_response.result {
            Ok(value) => json! {{
                "jsonrpc": JSON_RPC_VERSION,
                "id": JsonValue::from(json_rpc_response.id),
                "result": value,
            }},
            Err(err) => json! {{
                "jsonrpc": JSON_RPC_VERSION,
                "id": JsonValue::from(json_rpc_response.id),
                "error": JsonValue::from(err),
            }},
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcResponseFromJsonError {
    #[error("json-rpc response must be a json object")]
    ExpectedObject,
    #[error("malformed response id: {}", source)]
    MalformedId {
        source: JsonRpcIdFromJsonError,
    },
    #[error("missing response id")]
    MissingId,
    #[error("unrecognized json-rpc protocol version ({})", version)]
    UnrecognizedVersion {
        version: String,
        id: JsonRpcId,
    },
    #[error("jsonrpc version field must be a string")]
    InvalidTypeForVersion {
        id: JsonRpcId,
    },
    #[error("missing jsonrpc version field")]
    MissingVersion {
        id: JsonRpcId,
    },
    #[error("malformed error field: {}", source)]
    MalformedErrorField {
        source: JsonRpcErrorFromJsonError,
        id: JsonRpcId,
    },
    #[error("response missing result/error field")]
    MissingResultAndError {
        id: JsonRpcId,
    },
    #[error("unrecognized response field '{}'", field)]
    UnrecognizedField {
        field: String,
        id: JsonRpcId,
    },
}

impl TryFrom<JsonValue> for JsonRpcResponse {
    type Error = JsonRpcResponseFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcResponse, JsonRpcResponseFromJsonError> {
        let mut map = match json {
            JsonValue::Object(map) => map,
            _ => return Err(JsonRpcResponseFromJsonError::ExpectedObject),
        };
        let id = match map.remove("id") {
            Some(id_json) => {
                JsonRpcId::try_from(id_json)
                .map_err(|source| JsonRpcResponseFromJsonError::MalformedId { source })?
            },
            None => return Err(JsonRpcResponseFromJsonError::MissingId),
        };
        match map.remove("jsonrpc") {
            Some(jsonrpc_json) => match jsonrpc_json {
                JsonValue::String(jsonrpc_string) => {
                    if jsonrpc_string != JSON_RPC_VERSION {
                        return Err(JsonRpcResponseFromJsonError::UnrecognizedVersion {
                            version: jsonrpc_string,
                            id,
                        });
                    }
                },
                _ => {
                    return Err(JsonRpcResponseFromJsonError::InvalidTypeForVersion { id });
                },
            },
            None => return Err(JsonRpcResponseFromJsonError::MissingVersion { id }),
        };
        let result = match map.remove("result") {
            Some(result_json) => Ok(result_json),
            None => match map.remove("error") {
                Some(error_json) => match JsonRpcError::try_from(error_json) {
                    Ok(error) => Err(error),
                    Err(source) => {
                        return Err(JsonRpcResponseFromJsonError::MalformedErrorField {
                            source, id,
                        });
                    },
                },
                None => return Err(JsonRpcResponseFromJsonError::MissingResultAndError { id }),
            },
        };
        if let Some((field, _value)) = map.into_iter().next() {
            return Err(JsonRpcResponseFromJsonError::UnrecognizedField { field, id });
        }
        Ok(JsonRpcResponse { id, result })
    }
}


