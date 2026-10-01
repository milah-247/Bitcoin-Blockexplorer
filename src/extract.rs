use axum::{
    async_trait,
    extract::{FromRequestParts, Query},
    http::request::Parts,
};
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// `Query<T>` whose rejection (e.g. `?limit=abc`) uses our JSON error shape
/// instead of axum's plain-text 400.
pub struct Q<T>(pub T);

#[async_trait]
impl<T, S> FromRequestParts<S> for Q<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| Q(v))
            .map_err(|e| AppError::InvalidInput(e.body_text()))
    }
}
