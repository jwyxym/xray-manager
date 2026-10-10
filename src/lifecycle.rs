use std::sync::Arc;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::Expr};
use crate::{AppState, auth::now, entities::{sessions, users, tokens}};

// Caller holds mutations. Intent is persisted before touching the external system.
pub async fn close(s: &AppState, row: sessions::Model, reason: &str) -> anyhow::Result<()> {
	if row.status == "closed" { return Ok(()); }
	let email = row.email.clone();
	let mut record: sessions::ActiveModel = row.into();
	record.status = Set("closing".into());
	if record.close_reason.as_ref().is_none() { record.close_reason = Set(Some(reason.into())); }
	let record = record.update(&s.db).await?;
	s.xray.remove(&email).await?;
	let mut record: sessions::ActiveModel = record.into();
	record.status = Set("closed".into());
	record.closed_at = Set(Some(now()));
	record.update(&s.db).await?;
	Ok(())
}

async fn inspect(s: &AppState, id: String) -> anyhow::Result<()> {
	let _guard = s.mutations.lock().await;
	let Some(row) = sessions::Entity::find_by_id(id).one(&s.db).await? else { return Ok(()); };
	if row.status == "closed" { return Ok(()); }
	let timestamp = now();
	let user = users::Entity::find_by_id(&row.user_id).one(&s.db).await?;
	let reason = if row.status != "active" { Some("incomplete_operation") }
		else if user.as_ref().is_none_or(|u| u.disabled || u.active_until <= timestamp) { Some("activation_expired_or_disabled") }
		else if row.expires_at <= timestamp { Some("session_expired") }
		else if row.heartbeat_at + s.config.heartbeat_timeout_seconds <= timestamp { Some("heartbeat_timeout") }
		else { None };
	if let Some(reason) = reason { return close(s, row, reason).await; }
	// Transport/RPC errors are not evidence that a user is offline.
	let online = s.xray.online(&row.email).await?;
	if online > 0 {
		if !row.seen_online || row.offline_since.is_some() {
			let mut record: sessions::ActiveModel = row.into();
			record.seen_online = Set(true);
			record.offline_since = Set(None);
			record.update(&s.db).await?;
		}
	} else if !row.seen_online {
		if timestamp >= row.created_at + s.config.connect_grace_seconds { close(s, row, "never_connected").await?; }
	} else if let Some(since) = row.offline_since {
		if timestamp >= since + s.config.offline_grace_seconds { close(s, row, "offline").await?; }
	} else {
		let mut record: sessions::ActiveModel = row.into();
		record.offline_since = Set(Some(timestamp));
		record.update(&s.db).await?;
	}
	Ok(())
}

pub async fn run(s: Arc<AppState>, mut stop: tokio::sync::watch::Receiver<bool>) {
	let mut interval = tokio::time::interval(std::time::Duration::from_secs(s.config.cleanup_interval_seconds));
	interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	loop {
		tokio::select! {
			_ = stop.changed() => break,
			_ = interval.tick() => {
				match sessions::Entity::find().filter(sessions::Column::Status.ne("closed")).all(&s.db).await {
					Ok(rows) => for row in rows {
						if let Err(error) = inspect(&s, row.id.clone()).await {
							tracing::warn!(%error, session_id=%row.id, "session cleanup will retry");
						}
					},
					Err(error) => tracing::error!(%error, "cannot scan sessions"),
				}
				if let Err(error) = tokens::Entity::delete_many().filter(tokens::Column::ExpiresAt.lte(now())).exec(&s.db).await {
					tracing::warn!(%error, "expired token cleanup failed");
				}
			}
		}
	}
}

pub async fn reconcile_fresh_runtime(s: &AppState) -> anyhow::Result<()> {
	// The managed inbound is verified empty on startup; no old credential is restored.
	sessions::Entity::update_many().col_expr(sessions::Column::Status, Expr::value("closed"))
		.col_expr(sessions::Column::ClosedAt, Expr::value(now()))
		.col_expr(sessions::Column::CloseReason, Expr::value("server_restarted"))
		.filter(sessions::Column::Status.ne("closed")).exec(&s.db).await?;
	Ok(())
}
