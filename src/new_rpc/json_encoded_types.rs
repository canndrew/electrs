use super::*;

pub enum JsonRpcId {
    Null,
    Number(i64),
    String(String),
}

impl JsonRpcId {
    pub fn into_json(self) -> JsonValue {
        match self {
            JsonRpcId::Null => json! { null },
            JsonRpcId::Number(n) => json! { n },
            JsonRpcId::String(s) => json! { s },
        }
    }

    pub fn from_json(json: JsonValue) -> Result<JsonRpcId, String> {
        match json {
            JsonValue::Null => Ok(JsonRpcId::Null),
            JsonValue::Number(number) => {
                match number.as_i64() {
                    Some(n) => Ok(JsonRpcId::Number(n)),
                    None => Err(format!("numeric request id out of range or not an integer")),
                }
            },
            JsonValue::String(s) => Ok(JsonRpcId::String(s)),
            _ => Err(format!("request id must be null, a number, or a string")),
        }
    }
}

pub struct JsonRpcError {
    pub code: i16,
    pub message: String,
    pub data: Option<JsonValue>,
}

impl JsonRpcError {
    pub fn into_json(self) -> JsonValue {
        match self.data {
            Some(data) => json! {{
                "code": self.code,
                "message": self.message,
                "data": data,
            }},
            None => json! {{
                "code": self.code,
                "message": self.message,
            }},
        }
    }
}

pub struct JsonRpcResponse {
    pub id: JsonRpcId,
    pub result: Result<JsonValue, JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn into_json(self) -> JsonValue {
        match self.result {
            Ok(value) => json! {{
                "jsonrpc": "2.0",
                "id": self.id.into_json(),
                "result": value,
            }},
            Err(err) => json! {{
                "jsonrpc": "2.0",
                "id": self.id.into_json(),
                "error": err.into_json(),
            }},
        }
    }
}

pub enum JsonRpcMultiResponse {
    Single(JsonRpcResponse),
    Batch(Vec<JsonRpcResponse>),
}

impl JsonRpcMultiResponse {
    pub fn into_json(self) -> JsonValue {
        match self {
            JsonRpcMultiResponse::Single(response) => response.into_json(),
            JsonRpcMultiResponse::Batch(responses) => {
                JsonValue::Array(
                    responses
                    .into_iter()
                    .map(|response| response.into_json())
                    .collect()
                )
            },
        }
    }
}

pub enum JsonRpcParams {
    Array(Vec<JsonValue>),
    Object(serde_json::Map<String, JsonValue>),
}

impl JsonRpcParams {
    pub fn from_json(json: JsonValue) -> Result<JsonRpcParams, String> {
        match json {
            JsonValue::Array(array) => Ok(JsonRpcParams::Array(array)),
            JsonValue::Object(map) => Ok(JsonRpcParams::Object(map)),
            _ => Err(format!("request params must be an array or object")),
        }
    }
}

pub struct JsonRpcRequest {
    pub method: String,
    pub params: Option<JsonRpcParams>,
    pub id: Option<JsonRpcId>,
}

impl JsonRpcRequest {
    pub fn from_json(json: JsonValue) -> Result<JsonRpcRequest, (Option<JsonRpcId>, String)> {
        match json {
            JsonValue::Object(mut map) => {
                let id = match map.remove("id") {
                    Some(id_json) => match JsonRpcId::from_json(id_json) {
                        Ok(id) => Some(id),
                        Err(err) => return Err((None, err)),
                    },
                    None => None,
                };
                match map.remove("jsonrpc") {
                    Some(version_json) => {
                        match version_json.as_str() {
                            Some(version) => {
                                if version != "2.0" {
                                    return Err((
                                        id,
                                        format!("jsonrpc version number must be 2.0"),
                                    ));
                                }
                            },
                            None => {
                                return Err((
                                    id,
                                    format!("jsonrpc version must be a string"),
                                ));
                            },
                        }
                    },
                    None => return Err((id, format!("jsonrpc version not specified"))),
                }
                let method = match map.remove("method") {
                    Some(method_json) => {
                        match method_json {
                            JsonValue::String(method) => method,
                            _ => return Err((id, format!("method must be a string"))),
                        }
                    },
                    None => return Err((id, format!("method not specified"))),
                };
                let params = match map.remove("params") {
                    None => None,
                    Some(params_json) => match JsonRpcParams::from_json(params_json) {
                        Ok(params) => Some(params),
                        Err(err) => return Err((id, err)),
                    },
                };
                match map.keys().next() {
                    None => (),
                    Some(key) => return Err((id, format!("unexpected key in request: {}", key)))
                }
                Ok(JsonRpcRequest { id, method, params })
            },
            _ => Err((None, format!("request must be an object"))),
        }
    }
}

