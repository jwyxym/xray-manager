use sea_orm::{Schema, EntityTrait};
use sea_orm_migration::prelude::*;
use crate::entities::{users, tokens, codes, sessions};

pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
	fn migrations() -> Vec<Box<dyn MigrationTrait>> {
		vec![Box::new(Initial), Box::new(EmailVerification)]
	}
}

#[derive(DeriveMigrationName)]
struct Initial;

struct EmailVerification;
impl MigrationName for EmailVerification {
	fn name(&self) -> &str { "m20261009_email_verification" }
}
#[async_trait::async_trait]
impl MigrationTrait for EmailVerification {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		create(manager, crate::entities::user_emails::Entity).await
	}
	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		manager.drop_table(Table::drop().table(Alias::new("user_emails")).to_owned()).await
	}
}

async fn create<E: EntityTrait>(manager: &SchemaManager<'_>, entity: E) -> Result<(), DbErr> {
	let schema = Schema::new(manager.get_database_backend());
	manager.create_table(schema.create_table_from_entity(entity)).await?;
	for index in schema.create_index_from_entity(entity) {
		manager.create_index(index).await?;
	}
	Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Initial {
	async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		create(manager, users::Entity).await?;
		create(manager, tokens::Entity).await?;
		create(manager, codes::Entity).await?;
		create(manager, sessions::Entity).await?;
		Ok(())
	}
	async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
		for name in ["connection_sessions", "activation_codes", "tokens", "users"] {
			manager.drop_table(Table::drop().table(Alias::new(name)).to_owned()).await?;
		}
		Ok(())
	}
}
