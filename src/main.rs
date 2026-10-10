mod api;
mod auth;
mod config;
mod entities;
mod lifecycle;
mod migration;
mod xray;
mod web;
mod verification;

use std::{path::PathBuf, sync::Arc, time::Duration};
use anyhow::{Context, ensure};
use clap::{Parser, Subcommand};
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectOptions, Database, DatabaseConnection, EntityTrait, QueryFilter, Set, TransactionTrait, sea_query::Expr};
use sea_orm_migration::MigratorTrait;
use tokio::sync::{Mutex, Semaphore};
use tower_http::{trace::TraceLayer, set_header::SetResponseHeaderLayer};
use axum::http::{HeaderValue, header::CACHE_CONTROL};

pub struct AppState {
	pub db: DatabaseConnection,
	pub config: config::Config,
	pub xray: xray::Xray,
	// This release deliberately manages one embedded Xray in one process.
	pub mutations: Mutex<()>,
	pub password_slots: Semaphore,
	pub dummy_hash: String,
	pub verification: verification::Verification,
}

#[derive(Parser)]
#[command(version, about = "Xray dynamic-library account server")]
struct Args {
	#[arg(long, default_value = "config.toml", global = true)]
	config: PathBuf,
	#[command(subcommand)]
	command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
	Serve,
	/// Synchronize the administrator from [admin] in config.toml without starting Xray.
	CreateAdmin,
	/// Apply database migrations without starting Xray.
	Migrate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
		.unwrap_or_else(|_| "xray_manager=info,tower_http=info".into())).init();
	let args = Args::parse();
	let config = config::Config::load(&args.config)?;
	let mut options = ConnectOptions::new(config.database_url.clone());
	options.max_connections(if config.database_url.starts_with("sqlite:") { 1 } else { 10 })
		.connect_timeout(Duration::from_secs(10)).sqlx_logging(false);
	let db = Database::connect(options).await.context("database connection failed")?;
	migration::Migrator::up(&db, None).await?;
	match args.command.unwrap_or(Command::Serve) {
		Command::Migrate => return Ok(()),
		Command::CreateAdmin => {
			sync_admin(&db, config.admin.as_ref().context("configure [admin] username and password in config.toml")?).await?;
			return Ok(());
		}
		Command::Serve => {}
	}
	if let Some(admin) = &config.admin { sync_admin(&db, admin).await?; }
	let server_config = config.xray_json()?;
	let _runtime = xray::Runtime::start(&config.xray.library, &server_config)?;
	let xray = xray::Xray::connect(&config.xray).await?;
	let dummy_hash = auth::hash_password(uuid::Uuid::new_v4().to_string()).await.map_err(|e| anyhow::anyhow!(e.1))?;
	let verification = verification::Verification::new(config.smtp.as_ref())?;
	let state = Arc::new(AppState { db, config, xray, mutations: Mutex::new(()), password_slots: Semaphore::new(4), dummy_hash, verification });
	lifecycle::reconcile_fresh_runtime(&state).await?;
	let listener = tokio::net::TcpListener::bind(state.config.listen).await?;
	let app = api::router(state.clone()).layer(TraceLayer::new_for_http())
		.layer(SetResponseHeaderLayer::overriding(CACHE_CONTROL, HeaderValue::from_static("no-store")));
	let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
	let worker = tokio::spawn(lifecycle::run(state.clone(), stop_rx));
	tracing::info!(listen=%state.config.listen, "server ready");
	let result = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await;
	let _ = stop_tx.send(true);
	worker.await?;
	// Runtime drop closes all Xray connections. Next startup reconciles remaining DB records.
	result?;
	Ok(())
}

async fn sync_admin(db: &DatabaseConnection, config: &config::AdminConfig) -> anyhow::Result<()> {
	use entities::{users, tokens, sessions};
	let existing = users::Entity::find().filter(users::Column::Username.eq(&config.username)).one(db).await?;
	if let Some(user) = &existing {
		ensure!(user.is_admin, "configured admin username belongs to an ordinary user; choose another username");
		if !user.disabled && auth::verify_password(config.password.clone(), user.password_hash.clone()).await {
			sync_admin_email(db, config, &user.id).await?;
			return Ok(());
		}
	}
	let hash = auth::hash_password(config.password.clone()).await.map_err(|e| anyhow::anyhow!(e.1))?;
	let txn = db.begin().await?;
	if let Some(user) = existing {
		users::Entity::update_many().col_expr(users::Column::PasswordHash, Expr::value(hash))
			.col_expr(users::Column::Disabled, Expr::value(false)).filter(users::Column::Id.eq(&user.id)).exec(&txn).await?;
		tokens::Entity::delete_many().filter(tokens::Column::UserId.eq(&user.id)).exec(&txn).await?;
		sessions::Entity::update_many().col_expr(sessions::Column::Status, Expr::value("closing"))
			.col_expr(sessions::Column::CloseReason, Expr::value("password_changed"))
			.filter(sessions::Column::UserId.eq(&user.id)).filter(sessions::Column::Status.ne("closed")).exec(&txn).await?;
	} else {
		users::ActiveModel {
			id: Set(uuid::Uuid::new_v4().to_string()), username: Set(config.username.clone()), password_hash: Set(hash),
			is_admin: Set(true), disabled: Set(false), active_until: Set(0), created_at: Set(auth::now()),
		}.insert(&txn).await?;
	}
	txn.commit().await?;
	if let Some(user) = users::Entity::find().filter(users::Column::Username.eq(&config.username)).one(db).await? {
		sync_admin_email(db, config, &user.id).await?;
	}
	tracing::info!(username=%config.username, "administrator synchronized from config");
	Ok(())
}

async fn sync_admin_email(db: &DatabaseConnection, config: &config::AdminConfig, user_id: &str) -> anyhow::Result<()> {
	use entities::user_emails;
	let Some(email) = &config.email else { return Ok(()); };
	let email = verification::normalize_email(email).map_err(|e| anyhow::anyhow!(e.1))?;
	if let Some(existing) = user_emails::Entity::find_by_id(&email).one(db).await? {
		ensure!(existing.user_id == user_id, "configured admin email belongs to another user");
		return Ok(());
	}
	let txn = db.begin().await?;
	user_emails::Entity::delete_many().filter(user_emails::Column::UserId.eq(user_id)).exec(&txn).await?;
	user_emails::ActiveModel { email: Set(email), user_id: Set(user_id.into()), verified_at: Set(auth::now()) }.insert(&txn).await?;
	txn.commit().await?;
	Ok(())
}

async fn shutdown_signal() {
	#[cfg(unix)] {
		let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
		tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
	}
	#[cfg(not(unix))]
	let _ = tokio::signal::ctrl_c().await;
}
