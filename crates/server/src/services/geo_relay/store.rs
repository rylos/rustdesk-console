use entity::system_setting;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Set, TransactionTrait,
};
use serde::{de::DeserializeOwned, Serialize};

use crate::services::now;

use super::types::{validate_settings, GeoSettings, MmdbSources, MmdbUpdatePolicy};

const GEO_SETTINGS_KEY: &str = "rustdesk_geo_settings";
const MMDB_SOURCES_KEY: &str = "rustdesk_mmdb_sources";
const MMDB_UPDATE_POLICY_KEY: &str = "rustdesk_mmdb_update_policy";

pub async fn load_settings(db: &DatabaseConnection) -> Result<GeoSettings, String> {
    load_json(db, GEO_SETTINGS_KEY).await
}

pub async fn load_sources(db: &DatabaseConnection) -> Result<MmdbSources, String> {
    load_json(db, MMDB_SOURCES_KEY).await
}

pub async fn load_update_policy(db: &DatabaseConnection) -> Result<MmdbUpdatePolicy, String> {
    load_json(db, MMDB_UPDATE_POLICY_KEY).await
}

pub async fn save_update_policy(
    db: &DatabaseConnection,
    policy: MmdbUpdatePolicy,
) -> Result<MmdbUpdatePolicy, String> {
    policy.validate()?;
    upsert_json(db, MMDB_UPDATE_POLICY_KEY, &policy).await?;
    Ok(policy)
}

pub async fn save_settings(
    db: &DatabaseConnection,
    mut input: GeoSettings,
    relay_pool: &[String],
) -> Result<GeoSettings, String> {
    validate_settings(&mut input, relay_pool)?;
    let transaction = db.begin().await.map_err(|error| error.to_string())?;
    let current_row = system_setting::Entity::find()
        .filter(system_setting::Column::Key.eq(GEO_SETTINGS_KEY))
        .one(&transaction)
        .await
        .map_err(|error| error.to_string())?;
    let current = match &current_row {
        Some(row) => serde_json::from_str::<GeoSettings>(&row.value)
            .map_err(|error| format!("stored setting '{GEO_SETTINGS_KEY}' is invalid: {error}"))?,
        None => GeoSettings::default(),
    };
    if input.revision != current.revision {
        return Err(format!(
            "settings revision conflict: submitted {}, current {}",
            input.revision, current.revision
        ));
    }
    if same_content(&input, &current) {
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(current);
    }
    input.revision = current
        .revision
        .checked_add(1)
        .ok_or_else(|| "settings revision is exhausted".to_owned())?;
    let encoded = serde_json::to_string(&input).map_err(|error| error.to_string())?;
    match current_row {
        Some(row) => {
            let result = system_setting::Entity::update_many()
                .col_expr(
                    system_setting::Column::Value,
                    sea_orm::sea_query::Expr::value(encoded),
                )
                .col_expr(
                    system_setting::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(now()),
                )
                .filter(system_setting::Column::Id.eq(row.id))
                .filter(system_setting::Column::Value.eq(row.value))
                .exec(&transaction)
                .await
                .map_err(|error| error.to_string())?;
            if result.rows_affected != 1 {
                return Err("settings revision conflict; reload and try again".to_owned());
            }
        }
        None => {
            system_setting::ActiveModel {
                key: Set(GEO_SETTINGS_KEY.to_owned()),
                value: Set(encoded),
                created_at: Set(now()),
                updated_at: Set(now()),
                ..Default::default()
            }
            .insert(&transaction)
            .await
            .map_err(|_| "settings revision conflict; reload and try again".to_owned())?;
        }
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(input)
}

pub async fn save_source(
    db: &DatabaseConnection,
    kind: super::types::MmdbKind,
    source_url: String,
) -> Result<MmdbSources, String> {
    let transaction = db.begin().await.map_err(|error| error.to_string())?;
    let mut sources: MmdbSources = load_json(&transaction, MMDB_SOURCES_KEY).await?;
    sources.set(kind, source_url);
    upsert_json(&transaction, MMDB_SOURCES_KEY, &sources).await?;
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(sources)
}

async fn load_json<C, T>(connection: &C, key: &str) -> Result<T, String>
where
    C: ConnectionTrait,
    T: Default + DeserializeOwned,
{
    let row = system_setting::Entity::find()
        .filter(system_setting::Column::Key.eq(key))
        .one(connection)
        .await
        .map_err(|error| error.to_string())?;
    match row {
        Some(row) => serde_json::from_str(&row.value)
            .map_err(|error| format!("stored setting '{key}' is invalid: {error}")),
        None => Ok(T::default()),
    }
}

async fn upsert_json<C, T>(connection: &C, key: &str, value: &T) -> Result<(), String>
where
    C: ConnectionTrait,
    T: Serialize,
{
    let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
    let row = system_setting::Entity::find()
        .filter(system_setting::Column::Key.eq(key))
        .one(connection)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(row) = row {
        let mut active: system_setting::ActiveModel = row.into();
        active.value = Set(encoded);
        active.updated_at = Set(now());
        active
            .update(connection)
            .await
            .map_err(|error| error.to_string())?;
    } else {
        system_setting::ActiveModel {
            key: Set(key.to_owned()),
            value: Set(encoded),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        }
        .insert(connection)
        .await
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn same_content(left: &GeoSettings, right: &GeoSettings) -> bool {
    left.version == right.version && left.enabled == right.enabled && left.rules == right.rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::geo_relay::types::{EndpointExpressions, GeoRule};
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(
            db.get_database_backend()
                .build(&schema.create_table_from_entity(system_setting::Entity)),
        )
        .await
        .unwrap();
        db
    }

    fn settings(revision: u64) -> GeoSettings {
        GeoSettings {
            revision,
            enabled: true,
            rules: vec![GeoRule {
                name: "Mainland".to_owned(),
                symmetric: true,
                expressions: EndpointExpressions {
                    client_a: "country:CN".to_owned(),
                    client_b: "*".to_owned(),
                },
                relays: vec!["relay.example.com:21117".to_owned()],
            }],
            ..GeoSettings::default()
        }
    }

    #[tokio::test]
    async fn saves_settings_and_rejects_stale_revision() {
        let db = test_db().await;
        let relays = ["relay.example.com:21117".to_owned()];
        let saved = save_settings(&db, settings(0), &relays).await.unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(load_settings(&db).await.unwrap(), saved);

        let error = save_settings(&db, settings(0), &relays).await.unwrap_err();
        assert!(error.contains("revision conflict"));
        assert_eq!(load_settings(&db).await.unwrap(), saved);
    }

    #[tokio::test]
    async fn invalid_settings_do_not_write_database() {
        let db = test_db().await;
        let mut invalid = settings(0);
        invalid.rules[0].expressions.client_a = "unsupported:value".to_owned();
        assert!(
            save_settings(&db, invalid, &["relay.example.com:21117".to_owned()])
                .await
                .is_err()
        );
        assert_eq!(load_settings(&db).await.unwrap(), GeoSettings::default());
    }

    #[tokio::test]
    async fn relay_pool_changes_must_preserve_geo_rule_references() {
        let db = test_db().await;
        let original_relays = ["relay.example.com:21117".to_owned()];
        save_settings(&db, settings(0), &original_relays)
            .await
            .unwrap();

        assert!(crate::services::geo_relay::validate_relay_pool_change(
            &db,
            &["other.example.com:21117".to_owned()]
        )
        .await
        .is_err());
        assert!(
            crate::services::geo_relay::validate_relay_pool_change(&db, &original_relays)
                .await
                .is_ok()
        );
    }
}
