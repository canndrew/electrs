use super::*;

#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
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


