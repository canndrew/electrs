use super::*;

#[derive(Debug, Clone)]
pub enum JsonRpcParams {
    Array(Vec<JsonValue>),
    Object(serde_json::Map<String, JsonValue>),
}

impl From<JsonRpcParams> for JsonValue {
    fn from(json_rpc_params: JsonRpcParams) -> JsonValue {
        match json_rpc_params {
            JsonRpcParams::Array(array) => JsonValue::Array(array),
            JsonRpcParams::Object(map) => JsonValue::Object(map),
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcParamsFromJsonError {
    #[error("params must be an array or object")]
    InvalidJsonType,
}

impl TryFrom<JsonValue> for JsonRpcParams {
    type Error = JsonRpcParamsFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcParams, JsonRpcParamsFromJsonError> {
        match json {
            JsonValue::Array(array) => Ok(JsonRpcParams::Array(array)),
            JsonValue::Object(map) => Ok(JsonRpcParams::Object(map)),
            _ => Err(JsonRpcParamsFromJsonError::InvalidJsonType),
        }
    }
}


