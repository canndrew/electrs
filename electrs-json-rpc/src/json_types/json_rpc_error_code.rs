use super::*;

#[macro_export]
macro_rules! json_rpc_error_code (
    ($lit:literal) => {{
        const __CODE: i16 = $lit;
        const __CODE_REVERSED: usize = (std::i16::MAX as i64 - __CODE as i64) as usize;
        const __CODE_OUT_OF_RANGE: usize = __CODE_REVERSED / ((std::i16::MAX as i64 - JsonRpcErrorCode::MAX_RESERVED_ERROR_CODE as i64) as usize);
        const __JSON_RPC_ERROR_CODE_OUT_OF_RANGE: [(); __CODE_OUT_OF_RANGE] = [];

        JsonRpcErrorCode::new_unchecked($lit)
    }}
);

#[derive(Debug, Clone, Copy)]
pub struct JsonRpcErrorCode {
    code: i16,
}

impl JsonRpcErrorCode {
    pub const PARSE_ERROR: JsonRpcErrorCode = JsonRpcErrorCode { code: -32700 };
    pub const INVALID_REQUEST: JsonRpcErrorCode = JsonRpcErrorCode { code: -32600 };
    pub const METHOD_NOT_FOUND: JsonRpcErrorCode = JsonRpcErrorCode { code: -32601 };
    pub const INVALID_PARAMS: JsonRpcErrorCode = JsonRpcErrorCode { code: -32602 };
    pub const INTERNAL_ERROR: JsonRpcErrorCode = JsonRpcErrorCode { code: -32603 };

    pub const MAX_RESERVED_ERROR_CODE: i16 = -32000;

    pub const fn new_unchecked(code: i16) -> JsonRpcErrorCode {
        JsonRpcErrorCode { code }
    }

    pub fn code(&self) -> i16 {
        self.code
    }
}

impl From<JsonRpcErrorCode> for JsonValue {
    fn from(json_rpc_error_code: JsonRpcErrorCode) -> JsonValue {
        json! { json_rpc_error_code.code }
    }
}

#[derive(Debug, Error)]
pub enum JsonRpcErrorCodeFromJsonError {
    #[error("out of range")]
    ErrorCodeOutOfRange,
    #[error("not a number")]
    ExpectedNumberForCode,
}

impl TryFrom<JsonValue> for JsonRpcErrorCode {
    type Error = JsonRpcErrorCodeFromJsonError;

    fn try_from(json: JsonValue) -> Result<JsonRpcErrorCode, JsonRpcErrorCodeFromJsonError> {
        match json {
            JsonValue::Number(number_json) => {
                let code_opt = number_json.as_i64().and_then(|val| val.try_into().ok());
                match code_opt {
                    Some(code) => Ok(JsonRpcErrorCode { code }),
                    None => Err(JsonRpcErrorCodeFromJsonError::ErrorCodeOutOfRange),
                }
            },
            _ => Err(JsonRpcErrorCodeFromJsonError::ExpectedNumberForCode),
        }
    }
}


