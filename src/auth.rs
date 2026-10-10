use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::{SaltString, rand_core::OsRng}};
use axum::{http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sha2::{Digest, Sha256};
use crate::{entities::{tokens, users}, AppState};

pub struct ApiError(pub StatusCode, pub &'static str);
pub type ApiResult<T> = Result<T, ApiError>;
impl IntoResponse for ApiError {
	fn into_response(self) -> Response { (self.0, Json(serde_json::json!({"error":self.1}))).into_response() }
}
impl From<sea_orm::DbErr> for ApiError {
	fn from(error: sea_orm::DbErr) -> Self {
		tracing::error!(%error, "database operation failed");
		Self(StatusCode::INTERNAL_SERVER_ERROR, "database_error")
	}
}
pub fn bad(message: &'static str) -> ApiError { ApiError(StatusCode::BAD_REQUEST, message) }
pub fn unauthorized() -> ApiError { ApiError(StatusCode::UNAUTHORIZED, "unauthorized") }
pub fn conflict(message: &'static str) -> ApiError { ApiError(StatusCode::CONFLICT, message) }
pub fn unavailable() -> ApiError { ApiError(StatusCode::SERVICE_UNAVAILABLE, "xray_unavailable") }
pub fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock before Unix epoch").as_secs() as i64
}
pub fn token_hash(value: &str) -> String { format!("{:x}", Sha256::digest(value.as_bytes())) }
pub fn bearer(headers: &HeaderMap) -> ApiResult<&str> {
	let token = headers.get("authorization").and_then(|v| v.to_str().ok())
		.and_then(|v| v.strip_prefix("Bearer ")).filter(|v| v.len() == 64).ok_or_else(unauthorized)?;
	Ok(token)
}
pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> ApiResult<users::Model> {
	let token = tokens::Entity::find_by_id(token_hash(bearer(headers)?)).one(&state.db).await?
		.filter(|v| v.expires_at > now()).ok_or_else(unauthorized)?;
	users::Entity::find_by_id(token.user_id).one(&state.db).await?
		.filter(|v| !v.disabled).ok_or_else(unauthorized)
}
pub async fn admin(state: &AppState, headers: &HeaderMap) -> ApiResult<users::Model> {
	let user = authenticate(state, headers).await?;
	if !user.is_admin { return Err(ApiError(StatusCode::FORBIDDEN, "admin_required")); }
	Ok(user)
}
pub fn validate_password(password: &str) -> ApiResult<()> {
	if !(10..=128).contains(&password.len()) { return Err(bad("password_must_be_10_to_128_bytes")); }
	Ok(())
}
pub fn normalize_username(username: &str) -> ApiResult<String> {
	let username = username.to_ascii_lowercase();
	if !(3..=64).contains(&username.len()) || !username.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)) {
		return Err(bad("username_must_be_3_to_64_ascii_letters_digits_underscore_hyphen"));
	}
	Ok(username)
}
pub async fn hash_password(password: String) -> ApiResult<String> {
	tokio::task::spawn_blocking(move || Argon2::default().hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
		.map(|hash| hash.to_string())).await.map_err(|_| bad("password_hash_failed"))?
		.map_err(|_| bad("password_hash_failed"))
}
pub async fn verify_password(password: String, hash: String) -> bool {
	tokio::task::spawn_blocking(move || PasswordHash::new(&hash).ok()
		.is_some_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())).await.unwrap_or(false)
}
pub async fn find_username(state: &AppState, username: &str) -> Result<Option<users::Model>, sea_orm::DbErr> {
	users::Entity::find().filter(users::Column::Username.eq(username)).one(&state.db).await
}
