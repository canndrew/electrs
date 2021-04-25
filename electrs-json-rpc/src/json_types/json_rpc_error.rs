use super::*;

#[derive(Debug, Clone)]
pub struct JsonRpcError {
    pub code: JsonRpcErrorCode,
    pub message: String,
    pub data: Option<JsonValue>,
}

#[derive(Debug, Error)]
pub enum JsonRpcErrorFromJsonError {
    #[error("json-rpc-error must be json object")]
    ExpectedObject,
    #[error("malformed error code: {}", source)]
    MalformedCode {
        source: JsonRpcErrorCodeFromJsonError,
    },
    #[error("missing error code")]
    MissingCode,
    #[error("error message is not a string")]
    ExpectedStringForMessage,
    #[error("missing error message")]
    MissingMessage,
    #[error("unrecognized field '{}'", field)]
    UnrecognizedField {
        field: String,
    },
}

impl From<JsonRpcError> for JsonValue {
    fn from(json_rpc_error: JsonRpcError) -> JsonValue {
        match json_rpc_error.data {
            Some(data) => json! {{
                "code": JsonValue::from(json_rpc_error.code),
                "message": json_rpc_error.message,
                "data": data,
            }},
            None => json! {{
                "code": JsonValue::from(json_rpc_error.code),
                "message": json_rpc_error.message,
            }},
        }
    }
}

impl TryFrom<JsonValue> for JsonRpcError {
    type Error = JsonRpcErrorFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcError, JsonRpcErrorFromJsonError> {
        let mut map = match json {
            JsonValue::Object(map) => map,
            _ => return Err(JsonRpcErrorFromJsonError::ExpectedObject),
        };
        let code = match map.remove("code") {
            Some(code_json) => match JsonRpcErrorCode::try_from(code_json) {
                Ok(code) => code,
                Err(source) => {
                    return Err(JsonRpcErrorFromJsonError::MalformedCode { source });
                },
            },
            None => return Err(JsonRpcErrorFromJsonError::MissingCode),
        };
        let message = match map.remove("message") {
            Some(message_json) => match message_json {
                JsonValue::String(message) => message,
                _ => return Err(JsonRpcErrorFromJsonError::ExpectedStringForMessage),
            },
            None => return Err(JsonRpcErrorFromJsonError::MissingMessage),
        };
        let data = map.remove("data");
        if let Some((field, _value)) = map.into_iter().next() {
            return Err(JsonRpcErrorFromJsonError::UnrecognizedField { field });
        }
        Ok(JsonRpcError { code, message, data })
    }
}

