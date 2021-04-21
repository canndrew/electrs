use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JsonRpcId {
    Null,
    Number(i64),
    String(String),
}

impl From<JsonRpcId> for JsonValue {
    fn from(json_rpc_id: JsonRpcId) -> JsonValue {
        match json_rpc_id {
            JsonRpcId::Null => json! { null },
            JsonRpcId::Number(n) => json! { n },
            JsonRpcId::String(s) => json! { s },
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcIdFromJsonError {
    #[error("numeric request id out of range or not an integer")]
    NumericIdOutOfRange,
    #[error("request id must be null, a number, or a string")]
    InvalidJsonType,
}

impl TryFrom<JsonValue> for JsonRpcId {
    type Error = JsonRpcIdFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcId, JsonRpcIdFromJsonError> {
        match json {
            JsonValue::Null => Ok(JsonRpcId::Null),
            JsonValue::Number(number) => {
                match number.as_i64() {
                    Some(n) => Ok(JsonRpcId::Number(n)),
                    None => Err(JsonRpcIdFromJsonError::NumericIdOutOfRange),
                }
            },
            JsonValue::String(s) => Ok(JsonRpcId::String(s)),
            _ => Err(JsonRpcIdFromJsonError::InvalidJsonType),
        }
    }
}

