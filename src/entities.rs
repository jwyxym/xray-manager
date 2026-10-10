pub mod users {
	use sea_orm::entity::prelude::*;
	#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
	#[sea_orm(table_name = "users")]
	pub struct Model {
		#[sea_orm(primary_key, auto_increment = false)]
		pub id: String,
		#[sea_orm(unique)]
		pub username: String,
		pub password_hash: String,
		pub is_admin: bool,
		pub disabled: bool,
		pub active_until: i64,
		pub created_at: i64,
	}
	#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
	pub enum Relation {}
	impl ActiveModelBehavior for ActiveModel {}
}

pub mod user_emails {
	use sea_orm::entity::prelude::*;
	#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
	#[sea_orm(table_name = "user_emails")]
	pub struct Model {
		#[sea_orm(primary_key, auto_increment = false)]
		pub email: String,
		#[sea_orm(unique)]
		pub user_id: String,
		pub verified_at: i64,
	}
	#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
	pub enum Relation {}
	impl ActiveModelBehavior for ActiveModel {}
}

pub mod tokens {
	use sea_orm::entity::prelude::*;
	#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
	#[sea_orm(table_name = "tokens")]
	pub struct Model {
		#[sea_orm(primary_key, auto_increment = false)]
		pub hash: String,
		#[sea_orm(indexed)]
		pub user_id: String,
		pub expires_at: i64,
	}
	#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
	pub enum Relation {}
	impl ActiveModelBehavior for ActiveModel {}
}

pub mod codes {
	use sea_orm::entity::prelude::*;
	#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
	#[sea_orm(table_name = "activation_codes")]
	pub struct Model {
		#[sea_orm(primary_key, auto_increment = false)]
		pub id: String,
		pub duration_seconds: i64,
		pub created_by: String,
		pub created_at: i64,
		pub expires_at: Option<i64>,
		pub used_by: Option<String>,
		pub used_at: Option<i64>,
	}
	#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
	pub enum Relation {}
	impl ActiveModelBehavior for ActiveModel {}
}

pub mod sessions {
	use sea_orm::entity::prelude::*;
	#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
	#[sea_orm(table_name = "connection_sessions")]
	pub struct Model {
		#[sea_orm(primary_key, auto_increment = false)]
		pub id: String,
		#[sea_orm(indexed)]
		pub user_id: String,
		pub email: String,
		// provisioning / active / closing / closed; closing is durable retry intent.
		pub status: String,
		pub created_at: i64,
		pub expires_at: i64,
		pub heartbeat_at: i64,
		pub seen_online: bool,
		pub offline_since: Option<i64>,
		pub closed_at: Option<i64>,
		pub close_reason: Option<String>,
	}
	#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
	pub enum Relation {}
	impl ActiveModelBehavior for ActiveModel {}
}
