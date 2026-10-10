use std::sync::Arc;
use axum::{Json, Router, extract::{State, Path, Query, DefaultBodyLimit}, http::{HeaderMap, StatusCode}, routing::{get, post, delete}};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait, Condition, sea_query::Expr};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;
use crate::{AppState, auth::*, entities::{users, tokens, codes, sessions}, lifecycle};

type Shared = State<Arc<AppState>>;
type Reply = ApiResult<Json<Value>>;

pub fn router(state: Arc<AppState>) -> Router {
	Router::new()
		.merge(crate::web::router())
		.route("/health", get(|| async { Json(json!({"status":"ok"})) }))
		.route("/api/auth/captcha", get(crate::verification::captcha))
		.route("/api/auth/email-code", post(crate::verification::send_email))
		.route("/api/auth/change-email", post(crate::verification::change_email))
		.route("/api/auth/register", post(register))
		.route("/api/auth/login", post(login))
		.route("/api/auth/logout", post(logout))
		.route("/api/auth/password", post(change_password))
		.route("/api/me", get(me))
		.route("/api/activation/redeem", post(redeem))
		.route("/api/connections", post(connect).get(list_connections))
		.route("/api/connections/{id}", delete(disconnect))
		.route("/api/connections/{id}/heartbeat", post(heartbeat))
		.route("/api/admin/activation-codes", post(generate_codes).get(list_codes))
		.route("/api/admin/users", get(list_users))
		.route("/api/admin/users/{id}/disabled", post(disable_user))
		.layer(DefaultBodyLimit::max(16 * 1024))
		.with_state(state)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials { username: String, password: String, captcha_id: String, captcha_code: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
	username: String,
	password: String,
	email: String,
	email_code: String,
}

fn user_json(user: &users::Model) -> Value {
	json!({"id":user.id,"username":user.username,"is_admin":user.is_admin,"disabled":user.disabled,
		"active_until":user.active_until,"active":user.active_until > now(),"created_at":user.created_at})
}

async fn register(State(s): Shared, Json(input): Json<Registration>) -> Reply {
	let _permit = s.password_slots.try_acquire().map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "auth_busy"))?;
	let username = normalize_username(&input.username)?;
	let email = crate::verification::normalize_email(&input.email)?;
	validate_password(&input.password)?;
	let _guard = s.mutations.lock().await;
	if find_username(&s, &username).await?.is_some() { return Err(conflict("username_taken")); }
	if crate::entities::user_emails::Entity::find_by_id(&email).one(&s.db).await?.is_some() { return Err(conflict("email_taken")); }
	s.verification.consume_email("register", &email, "", &input.email_code).await?;
	let password_hash = hash_password(input.password).await?;
	let txn = s.db.begin().await?;
	let user = users::ActiveModel {
		id: Set(Uuid::new_v4().to_string()), username: Set(username), password_hash: Set(password_hash),
		is_admin: Set(false), disabled: Set(false), active_until: Set(0), created_at: Set(now()),
	}.insert(&txn).await?;
	crate::entities::user_emails::ActiveModel { email: Set(email), user_id: Set(user.id.clone()), verified_at: Set(now()) }.insert(&txn).await?;
	txn.commit().await?;
	Ok(Json(user_json(&user)))
}

async fn login(State(s): Shared, Json(input): Json<Credentials>) -> Reply {
	let _permit = s.password_slots.try_acquire().map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "auth_busy"))?;
	s.verification.consume_image(&input.captcha_id, &input.captcha_code).await?;
	let username = normalize_username(&input.username)?;
	if input.password.len() > 128 { return Err(unauthorized()); }
	let candidate = find_username(&s, &username).await?;
	let hash = candidate.as_ref().map(|u| u.password_hash.clone()).unwrap_or(s.dummy_hash.clone());
	if !verify_password(input.password, hash.clone()).await { return Err(unauthorized()); }
	let _guard = s.mutations.lock().await;
	let user = candidate.ok_or_else(unauthorized)?;
	let user = users::Entity::find_by_id(user.id).one(&s.db).await?.ok_or_else(unauthorized)?;
	if user.disabled || user.password_hash != hash { return Err(unauthorized()); }
	let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
	let expires_at = now() + s.config.token_ttl_seconds;
	tokens::ActiveModel { hash: Set(token_hash(&token)), user_id: Set(user.id.clone()), expires_at: Set(expires_at) }
		.insert(&s.db).await?;
	Ok(Json(json!({"token":token,"token_type":"Bearer","expires_at":expires_at,"user":user_json(&user)})))
}

async fn logout(State(s): Shared, headers: HeaderMap) -> Reply {
	let _guard = s.mutations.lock().await;
	authenticate(&s, &headers).await?;
	tokens::Entity::delete_by_id(token_hash(bearer(&headers)?)).exec(&s.db).await?;
	Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordChange { old_password: String, new_password: String, captcha_id: String, captcha_code: String, email_code: String }
async fn change_password(State(s): Shared, headers: HeaderMap, Json(input): Json<PasswordChange>) -> Reply {
	let _permit = s.password_slots.try_acquire().map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "auth_busy"))?;
	validate_password(&input.new_password)?;
	if input.old_password.len() > 128 { return Err(unauthorized()); }
	let user = authenticate(&s, &headers).await?;
	s.verification.consume_image(&input.captcha_id, &input.captcha_code).await?;
	if !verify_password(input.old_password, user.password_hash.clone()).await { return Err(unauthorized()); }
	let hash = hash_password(input.new_password).await?;
	let _guard = s.mutations.lock().await;
	let fresh = authenticate(&s, &headers).await?;
	if fresh.password_hash != user.password_hash { return Err(conflict("password_changed_retry")); }
	let email = crate::verification::bound_email(&s, &user.id).await?;
	s.verification.consume_email("change_password", &email, &user.id, &input.email_code).await?;
	let txn = s.db.begin().await?;
	users::Entity::update_many().col_expr(users::Column::PasswordHash, Expr::value(hash))
		.filter(users::Column::Id.eq(&user.id)).exec(&txn).await?;
	tokens::Entity::delete_many().filter(tokens::Column::UserId.eq(&user.id)).exec(&txn).await?;
	sessions::Entity::update_many().col_expr(sessions::Column::Status, Expr::value("closing"))
		.col_expr(sessions::Column::CloseReason, Expr::value("password_changed"))
		.filter(sessions::Column::UserId.eq(&user.id)).filter(sessions::Column::Status.ne("closed")).exec(&txn).await?;
	txn.commit().await?;
	Ok(Json(json!({"ok":true,"login_required":true})))
}

async fn me(State(s): Shared, headers: HeaderMap) -> Reply {
	let user = authenticate(&s, &headers).await?;
	let binding = crate::entities::user_emails::Entity::find().filter(crate::entities::user_emails::Column::UserId.eq(&user.id)).one(&s.db).await?;
	let mut result = user_json(&user);
	result["email"] = json!(binding.map(|v| v.email));
	Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Redeem { code: String }
async fn redeem(State(s): Shared, headers: HeaderMap, Json(input): Json<Redeem>) -> Reply {
	let id = Uuid::parse_str(&input.code).map_err(|_| bad("invalid_activation_code"))?.to_string();
	let _guard = s.mutations.lock().await;
	let user = authenticate(&s, &headers).await?;
	let timestamp = now();
	let txn = s.db.begin().await?;
	let code = codes::Entity::find_by_id(&id).one(&txn).await?.ok_or_else(|| bad("invalid_activation_code"))?;
	let result = codes::Entity::update_many()
		.col_expr(codes::Column::UsedBy, Expr::value(&user.id)).col_expr(codes::Column::UsedAt, Expr::value(timestamp))
		.filter(codes::Column::Id.eq(&id)).filter(codes::Column::UsedAt.is_null())
		.filter(Condition::any().add(codes::Column::ExpiresAt.is_null()).add(codes::Column::ExpiresAt.gt(timestamp)))
		.exec(&txn).await?;
	if result.rows_affected != 1 {
		txn.rollback().await?;
		return Err(conflict("activation_code_used_or_expired"));
	}
	// Portable atomic max(active_until, now) + duration, independent of SQL dialect.
	users::Entity::update_many().col_expr(users::Column::ActiveUntil,
		Expr::expr(Expr::case(Expr::col(users::Column::ActiveUntil).gt(timestamp),
			Expr::col(users::Column::ActiveUntil)).finally(timestamp)).add(code.duration_seconds))
		.filter(users::Column::Id.eq(&user.id)).exec(&txn).await?;
	let updated = users::Entity::find_by_id(user.id).one(&txn).await?.ok_or_else(unauthorized)?;
	txn.commit().await?;
	Ok(Json(json!({"active_until":updated.active_until})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateCodes { duration_seconds: i64, count: u32, expires_at: Option<i64> }
async fn generate_codes(State(s): Shared, headers: HeaderMap, Json(input): Json<GenerateCodes>) -> Reply {
	let _guard = s.mutations.lock().await;
	let admin = admin(&s, &headers).await?;
	let timestamp = now();
	if !(1..=100).contains(&input.count) || !(1..=315_360_000).contains(&input.duration_seconds)
		|| input.expires_at.is_some_and(|t| t <= timestamp) { return Err(bad("invalid_code_parameters")); }
	let txn = s.db.begin().await?;
	let mut result = Vec::new();
	for _ in 0..input.count {
		let id = Uuid::new_v4().to_string();
		codes::ActiveModel { id: Set(id.clone()), duration_seconds: Set(input.duration_seconds),
			created_by: Set(admin.id.clone()), created_at: Set(timestamp), expires_at: Set(input.expires_at),
			used_by: Set(None), used_at: Set(None) }.insert(&txn).await?;
		result.push(id);
	}
	txn.commit().await?;
	Ok(Json(json!({"codes":result,"duration_seconds":input.duration_seconds,"expires_at":input.expires_at})))
}

#[derive(Deserialize)]
struct Page { #[serde(default)] offset: u64 }
async fn list_codes(State(s): Shared, headers: HeaderMap, Query(page): Query<Page>) -> Reply {
	admin(&s, &headers).await?;
	let rows = codes::Entity::find().order_by_desc(codes::Column::CreatedAt).order_by_asc(codes::Column::Id)
		.offset(page.offset).limit(100).all(&s.db).await?;
	Ok(Json(json!({"items":rows.into_iter().map(|c| json!({"code":c.id,"duration_seconds":c.duration_seconds,
		"expires_at":c.expires_at,"used_at":c.used_at,"used_by":c.used_by,
		"expired":c.used_at.is_some() || c.expires_at.is_some_and(|t| t <= now())})).collect::<Vec<_>>()})))
}
async fn list_users(State(s): Shared, headers: HeaderMap, Query(page): Query<Page>) -> Reply {
	admin(&s, &headers).await?;
	let rows = users::Entity::find().order_by_desc(users::Column::CreatedAt).order_by_asc(users::Column::Id)
		.offset(page.offset).limit(100).all(&s.db).await?;
	Ok(Json(json!({"items":rows.iter().map(user_json).collect::<Vec<_>>()})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Disabled { disabled: bool }
async fn disable_user(State(s): Shared, headers: HeaderMap, Path(id): Path<String>, Json(input): Json<Disabled>) -> Reply {
	let _guard = s.mutations.lock().await;
	let actor = admin(&s, &headers).await?;
	if actor.id == id { return Err(bad("cannot_disable_self")); }
	let target = users::Entity::find_by_id(&id).one(&s.db).await?.ok_or(ApiError(StatusCode::NOT_FOUND, "user_not_found"))?;
	if target.is_admin { return Err(bad("cannot_disable_admin")); }
	let txn = s.db.begin().await?;
	users::Entity::update_many().col_expr(users::Column::Disabled, Expr::value(input.disabled))
		.filter(users::Column::Id.eq(&id)).exec(&txn).await?;
	if input.disabled {
		tokens::Entity::delete_many().filter(tokens::Column::UserId.eq(&id)).exec(&txn).await?;
		sessions::Entity::update_many().col_expr(sessions::Column::Status, Expr::value("closing"))
			.col_expr(sessions::Column::CloseReason, Expr::value("user_disabled"))
			.filter(sessions::Column::UserId.eq(&id)).filter(sessions::Column::Status.ne("closed")).exec(&txn).await?;
	}
	txn.commit().await?;
	Ok(Json(json!({"ok":true,"cleanup_pending":input.disabled})))
}

async fn connect(State(s): Shared, headers: HeaderMap) -> Reply {
	let _guard = s.mutations.lock().await;
	let user = authenticate(&s, &headers).await?;
	let timestamp = now();
	if user.active_until <= timestamp { return Err(ApiError(StatusCode::FORBIDDEN, "activation_required")); }
	if sessions::Entity::find().filter(sessions::Column::UserId.eq(&user.id))
		.filter(sessions::Column::Status.ne("closed")).one(&s.db).await?.is_some() {
		return Err(conflict("connection_already_exists_or_cleanup_pending"));
	}
	let id = Uuid::new_v4().to_string();
	let email = format!("session-{id}");
	let credential = Uuid::new_v4().to_string();
	let session = sessions::ActiveModel {
		id: Set(id.clone()), user_id: Set(user.id), email: Set(email.clone()), status: Set("provisioning".into()),
		created_at: Set(timestamp), expires_at: Set(user.active_until.min(timestamp + s.config.session_ttl_seconds)),
		heartbeat_at: Set(timestamp), seen_online: Set(false), offline_since: Set(None), closed_at: Set(None), close_reason: Set(None),
	}.insert(&s.db).await?;
	if let Err(error) = s.xray.add(&email, &credential).await {
		tracing::error!(%error, session_id=%id, "Xray account creation failed; will reconcile");
		// A timeout can occur after Xray has created the account. Never discard this record.
		if let Err(error) = lifecycle::close(&s, session, "provision_failed").await { tracing::error!(%error, "cleanup pending"); }
		return Err(unavailable());
	}
	let expires_at = session.expires_at;
	let mut record: sessions::ActiveModel = session.into();
	record.status = Set("active".into());
	record.update(&s.db).await?;
	Ok(Json(json!({"session_id":id,"expires_at":expires_at,
		"heartbeat_interval_seconds":(s.config.heartbeat_timeout_seconds / 3).max(1),
		"requires_server_address":s.config.xray.public_address.is_none(),
		"config":s.config.client_config(&credential)})))
}

fn session_json(row: &sessions::Model) -> Value {
	json!({"id":row.id,"status":row.status,"created_at":row.created_at,"expires_at":row.expires_at,
		"heartbeat_at":row.heartbeat_at,"seen_online":row.seen_online,"closed_at":row.closed_at,"close_reason":row.close_reason})
}
async fn list_connections(State(s): Shared, headers: HeaderMap) -> Reply {
	let user = authenticate(&s, &headers).await?;
	let rows = sessions::Entity::find().filter(sessions::Column::UserId.eq(user.id))
		.order_by_desc(sessions::Column::CreatedAt).limit(100).all(&s.db).await?;
	Ok(Json(json!({"items":rows.iter().map(session_json).collect::<Vec<_>>()})))
}
async fn owned(s: &AppState, id: String, user: &str) -> ApiResult<sessions::Model> {
	sessions::Entity::find_by_id(id).filter(sessions::Column::UserId.eq(user)).one(&s.db).await?
		.ok_or(ApiError(StatusCode::NOT_FOUND, "session_not_found"))
}
async fn heartbeat(State(s): Shared, headers: HeaderMap, Path(id): Path<String>) -> Reply {
	let _guard = s.mutations.lock().await;
	let user = authenticate(&s, &headers).await?;
	let row = owned(&s, id, &user.id).await?;
	let timestamp = now();
	if row.status != "active" || row.expires_at <= timestamp || user.active_until <= timestamp
		|| row.heartbeat_at + s.config.heartbeat_timeout_seconds <= timestamp {
		return Err(conflict("session_expired_or_closed"));
	}
	let mut record: sessions::ActiveModel = row.into();
	record.heartbeat_at = Set(timestamp);
	let record = record.update(&s.db).await?;
	Ok(Json(session_json(&record)))
}
async fn disconnect(State(s): Shared, headers: HeaderMap, Path(id): Path<String>) -> Reply {
	let _guard = s.mutations.lock().await;
	let user = authenticate(&s, &headers).await?;
	let row = owned(&s, id, &user.id).await?;
	lifecycle::close(&s, row, "client_disconnect").await.map_err(|error| {
		tracing::error!(%error, "disconnect cleanup pending"); unavailable()
	})?;
	Ok(Json(json!({"ok":true})))
}
