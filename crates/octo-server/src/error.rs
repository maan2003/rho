use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct AppError(pub anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("error: {:?}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        AppError(err.into())
    }
}
