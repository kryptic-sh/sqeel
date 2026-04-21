use crate::state::QueryResult;
use sqlx::{Column, MySqlPool, Row, TypeInfo};

pub struct DbConnection {
    pool: MySqlPool,
    pub url: String,
}

impl DbConnection {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = MySqlPool::connect(url).await?;
        Ok(Self {
            pool,
            url: url.to_string(),
        })
    }

    pub async fn execute(&self, query: &str) -> anyhow::Result<QueryResult> {
        let rows = sqlx::query(query).fetch_all(&self.pool).await?;

        if rows.is_empty() {
            return Ok(QueryResult {
                columns: vec![],
                rows: vec![],
            });
        }

        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();

        let result_rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(i, col)| {
                        let type_name = col.type_info().name();
                        decode_cell(row, i, type_name)
                    })
                    .collect()
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: result_rows,
        })
    }

    pub async fn list_databases(&self) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query("SHOW DATABASES").fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
            .collect())
    }

    pub async fn list_tables(&self, database: &str) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(&format!("SHOW TABLES FROM `{database}`"))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
            .collect())
    }

    pub async fn list_columns(
        &self,
        database: &str,
        table: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        let rows = sqlx::query(&format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = '{database}' AND TABLE_NAME = '{table}' \
             ORDER BY ORDINAL_POSITION"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| ColumnInfo {
                name: r.try_get::<String, _>(0).unwrap_or_default(),
                type_name: r.try_get::<String, _>(1).unwrap_or_default(),
                nullable: r.try_get::<String, _>(2).unwrap_or_default() == "YES",
                is_pk: r.try_get::<String, _>(3).unwrap_or_default() == "PRI",
            })
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_pk: bool,
}

fn decode_cell(row: &sqlx::mysql::MySqlRow, idx: usize, type_name: &str) -> String {
    match type_name {
        "BIGINT" | "INT" | "SMALLINT" | "TINYINT" | "MEDIUMINT" => row
            .try_get::<i64, _>(idx)
            .map(|v| v.to_string())
            .or_else(|_| {
                row.try_get::<Option<i64>, _>(idx)
                    .map(|v| v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".into()))
            })
            .unwrap_or_else(|_| "?".into()),
        "FLOAT" | "DOUBLE" | "DECIMAL" => row
            .try_get::<f64, _>(idx)
            .map(|v| v.to_string())
            .or_else(|_| {
                row.try_get::<Option<f64>, _>(idx)
                    .map(|v| v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".into()))
            })
            .unwrap_or_else(|_| "?".into()),
        "BOOLEAN" | "BOOL" => row
            .try_get::<bool, _>(idx)
            .map(|v| v.to_string())
            .or_else(|_| {
                row.try_get::<Option<bool>, _>(idx)
                    .map(|v| v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".into()))
            })
            .unwrap_or_else(|_| "?".into()),
        _ => row
            .try_get::<String, _>(idx)
            .or_else(|_| {
                row.try_get::<Option<String>, _>(idx)
                    .map(|v| v.unwrap_or_else(|| "NULL".into()))
            })
            .unwrap_or_else(|_| "?".into()),
    }
}
