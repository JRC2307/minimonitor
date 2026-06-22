//! Database operations for the `cf_zone` table.

use anyhow::Context;
use rusqlite::Connection;

use crate::cloudflare::CfZone;

/// Upsert a [`CfZone`] into `cf_zone`, stamping `synced_at` to now.
pub fn upsert_cf_zone(conn: &Connection, zone: &CfZone) -> anyhow::Result<()> {
    let synced_at = chrono::Utc::now().to_rfc3339();
    let min_cert_expiry = zone.min_cert_expiry.map(|dt| dt.to_rfc3339());

    conn.execute(
        "INSERT INTO cf_zone (zone_id, name, status, paused, healthy, min_cert_expiry, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(zone_id) DO UPDATE SET
             name            = excluded.name,
             status          = excluded.status,
             paused          = excluded.paused,
             healthy         = excluded.healthy,
             min_cert_expiry = excluded.min_cert_expiry,
             synced_at       = excluded.synced_at",
        rusqlite::params![
            zone.id,
            zone.name,
            zone.status,
            zone.paused as i64,
            zone.healthy as i64,
            min_cert_expiry,
            synced_at,
        ],
    )
    .context("upsert_cf_zone")?;
    Ok(())
}

/// Read back a single `cf_zone` row by `zone_id`.
pub fn get_cf_zone(conn: &Connection, zone_id: &str) -> anyhow::Result<Option<CfZone>> {
    let mut stmt = conn.prepare(
        "SELECT zone_id, name, status, paused, healthy, min_cert_expiry
         FROM cf_zone WHERE zone_id = ?1",
    )?;

    let mut rows = stmt.query_map([zone_id], |row| {
        let min_cert_expiry_str: Option<String> = row.get(5)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, bool>(3)?,
            row.get::<_, bool>(4)?,
            min_cert_expiry_str,
        ))
    })?;

    if let Some(row) = rows.next() {
        let (id, name, status, paused, healthy, min_cert_str) =
            row.context("reading cf_zone row")?;
        let min_cert_expiry = min_cert_str
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));
        Ok(Some(CfZone {
            id,
            name,
            status,
            paused,
            healthy,
            min_cert_expiry,
        }))
    } else {
        Ok(None)
    }
}

/// List all `cf_zone` rows, ordered by zone name (stable for rendering).
pub fn list_cf_zones(conn: &Connection) -> anyhow::Result<Vec<CfZone>> {
    let mut stmt = conn.prepare(
        "SELECT zone_id, name, status, paused, healthy, min_cert_expiry
         FROM cf_zone ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let min_cert_expiry_str: Option<String> = row.get(5)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
                min_cert_expiry_str,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("list_cf_zones")?;

    Ok(rows
        .into_iter()
        .map(|(id, name, status, paused, healthy, min_cert_str)| {
            let min_cert_expiry = min_cert_str
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            CfZone {
                id,
                name,
                status,
                paused,
                healthy,
                min_cert_expiry,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use chrono::{TimeZone, Utc};
    use tempfile::NamedTempFile;

    fn open_temp() -> (NamedTempFile, Connection) {
        let f = NamedTempFile::new().unwrap();
        let conn = db::open(f.path()).unwrap();
        (f, conn)
    }

    #[test]
    fn upsert_and_read_round_trip() {
        let (_f, conn) = open_temp();

        let expiry = Utc.with_ymd_and_hms(2026, 9, 20, 0, 0, 0).unwrap();
        let zone = CfZone {
            id: "z1".to_owned(),
            name: "example.com".to_owned(),
            status: "active".to_owned(),
            paused: false,
            healthy: true,
            min_cert_expiry: Some(expiry),
        };

        upsert_cf_zone(&conn, &zone).unwrap();

        let got = get_cf_zone(&conn, "z1").unwrap().unwrap();
        assert_eq!(got.id, "z1");
        assert_eq!(got.name, "example.com");
        assert_eq!(got.status, "active");
        assert!(!got.paused);
        assert!(got.healthy);
        // Expiry should round-trip (within second precision)
        assert_eq!(
            got.min_cert_expiry.unwrap().format("%Y-%m-%d").to_string(),
            "2026-09-20"
        );
    }

    #[test]
    fn upsert_updates_existing_row() {
        let (_f, conn) = open_temp();

        let zone1 = CfZone {
            id: "z1".to_owned(),
            name: "example.com".to_owned(),
            status: "active".to_owned(),
            paused: false,
            healthy: true,
            min_cert_expiry: None,
        };
        upsert_cf_zone(&conn, &zone1).unwrap();

        let zone2 = CfZone {
            id: "z1".to_owned(),
            name: "example.com".to_owned(),
            status: "inactive".to_owned(),
            paused: true,
            healthy: false,
            min_cert_expiry: None,
        };
        upsert_cf_zone(&conn, &zone2).unwrap();

        let got = get_cf_zone(&conn, "z1").unwrap().unwrap();
        assert_eq!(got.status, "inactive");
        assert!(got.paused);
        assert!(!got.healthy);

        // Only one row
        let cnt: i64 = conn
            .query_row("SELECT COUNT(*) FROM cf_zone", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 1, "should still have only one row after upsert");
    }

    #[test]
    fn get_missing_zone_returns_none() {
        let (_f, conn) = open_temp();
        let got = get_cf_zone(&conn, "no-such-zone").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn upsert_zone_with_no_expiry() {
        let (_f, conn) = open_temp();
        let zone = CfZone {
            id: "z2".to_owned(),
            name: "foo.com".to_owned(),
            status: "pending".to_owned(),
            paused: false,
            healthy: false,
            min_cert_expiry: None,
        };
        upsert_cf_zone(&conn, &zone).unwrap();
        let got = get_cf_zone(&conn, "z2").unwrap().unwrap();
        assert!(got.min_cert_expiry.is_none());
    }
}
