use super::*;

pub enum HandleMethodError {
    MethodNotFound,
    InvalidParams {
        message: String,
        data: Option<JsonValue>,
    },
    InternalError {
        message: String,
        data: Option<JsonValue>,
    },
    ApplicationError(JsonRpcError),
}

impl HandleMethodError {
    pub fn into_json_rpc_error(self, method: &str) -> JsonRpcError {
        match self {
            HandleMethodError::MethodNotFound => {
                JsonRpcError {
                    code: error_codes::METHOD_NOT_FOUND,
                    message: format!("method '{}' not found", method),
                    data: None,
                }
            },
            HandleMethodError::InvalidParams { message, data } => {
                JsonRpcError {
                    code: error_codes::INVALID_PARAMS,
                    message: format!("invalid params: {}", message),
                    data,
                }
            },
            HandleMethodError::InternalError { message, data } => {
                JsonRpcError {
                    code: error_codes::INTERNAL_ERROR,
                    message,
                    data,
                }
            },
            HandleMethodError::ApplicationError(err) => err,
        }
    }
}

#[async_trait]
pub trait JsonRpcService {
    async fn handle_method<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    ) -> Result<JsonValue, HandleMethodError>;

    async fn handle_notification<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    );
}

pub struct NullService;

#[async_trait]
impl JsonRpcService for NullService {
    async fn handle_method<'s, 'm>(
        &'s self,
        _method: &'m str,
        _params: Option<JsonRpcParams>,
    ) -> Result<JsonValue, HandleMethodError> {
        Err(HandleMethodError::MethodNotFound)
    }

    async fn handle_notification<'s, 'm>(
        &'s self,
        _method: &'m str,
        _params: Option<JsonRpcParams>,
    ) {
    }
}

