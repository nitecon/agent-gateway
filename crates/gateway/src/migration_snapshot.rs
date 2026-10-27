//! Operator-only whole-database snapshot for the amux migration wizard.
//!
//! The snapshot contains every gateway record, index and secret, so it requires
//! the exact `GATEWAY_API_KEY` bearer; browser sessions are never accepted.

use crate::{routes::AppError, AppState};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use sha2::{Digest, Sha256};
use std::{
    io::{Seek, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{Semaphore, SemaphorePermit},
};

pub const SHA256_HEADER: &str = "x-amux-snapshot-sha256";
pub const TEMP_PREFIX: &str = "agent-gateway-snapshot-";

/// One snapshot at a time bounds temporary disk use and database lock time.
static SNAPSHOTS: Semaphore = Semaphore::const_new(1);

fn api_key_bearer(headers: &HeaderMap, api_key: &str) -> bool {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    token.len() == api_key.len()
        && token
            .bytes()
            .zip(api_key.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Streams the snapshot and deletes its temporary file once the response body is
/// dropped, whether it completed or the client disconnected.
struct Snapshot {
    file: tokio::fs::File,
    _path: tempfile::TempPath,
    _permit: SemaphorePermit<'static>,
}

impl AsyncRead for Snapshot {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_read(cx, buf)
    }
}

/// `GET /v1/admin/migration/snapshot`: a consistent SQLite Online Backup of the
/// whole application database. The source is never modified.
pub async fn snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !api_key_bearer(&headers, &state.api_key) {
        return Err(AppError(
            StatusCode::UNAUTHORIZED,
            "migration snapshots require the gateway API key".into(),
        ));
    }
    let permit = SNAPSHOTS.try_acquire().map_err(|_| {
        AppError(
            StatusCode::TOO_MANY_REQUESTS,
            "a migration snapshot is already in progress".into(),
        )
    })?;
    let database = state.db.clone();
    let (file, path, size, digest) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .suffix(".db")
            .tempfile()?;
        database
            .lock()
            .unwrap()
            .backup(rusqlite::MAIN_DB, temp.path(), None)?;
        // A rollback-journal header keeps the snapshot a single self-contained file.
        rusqlite::Connection::open(temp.path())?.query_row(
            "PRAGMA journal_mode=DELETE",
            [],
            |_| Ok(()),
        )?;
        let (mut file, path) = temp.into_parts();
        let mut hasher = Sha256::new();
        let size = std::io::copy(&mut file, &mut hasher)?;
        file.seek(SeekFrom::Start(0))?;
        Ok((file, path, size, hex::encode(hasher.finalize())))
    })
    .await??;
    let body = Snapshot {
        file: tokio::fs::File::from_std(file),
        _path: path,
        _permit: permit,
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/vnd.sqlite3")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"agent-gateway-snapshot.db\"",
        )
        .header(SHA256_HEADER, digest)
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(body)))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use axum::body::to_bytes;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    fn state(database: db::Db) -> AppState {
        AppState {
            db: database,
            plugins: Arc::new(HashMap::new()),
            #[cfg(feature = "whatsapp")]
            whatsapp: None,
            default_channel: "discord".into(),
            api_key: "fixture-bearer".into(),
            ui_auth_enabled: true,
            retention_days: 30,
            bot_retention_days: 7,
            artifact_operations: db::ArtifactOperationsEnvelope::production_defaults(),
            artifact_body_schema_enabled: true,
            artifact_auth_enforced: false,
            update_available: Arc::new(Mutex::new(None)),
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.insert(*name, value.parse().unwrap());
        }
        h
    }

    fn leftover_snapshots() -> usize {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .count()
    }

    fn table_rows(conn: &rusqlite::Connection) -> Vec<(String, i64)> {
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        names
            .into_iter()
            .map(|n| {
                let count = conn
                    .query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))
                    .unwrap();
                (n, count)
            })
            .collect()
    }

    fn schema(conn: &rusqlite::Connection) -> Vec<(String, String)> {
        conn.prepare("SELECT type, name FROM sqlite_master ORDER BY type, name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    // One test owns the process-wide snapshot semaphore to avoid parallel interference.
    #[tokio::test]
    async fn snapshot_requires_api_key_and_streams_consistent_verified_database() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.db");
        let database = db::open(source.to_str().unwrap()).unwrap();
        {
            let conn = database.lock().unwrap();
            // Keep the newest commits only in the WAL to prove they are captured.
            conn.execute_batch(
                "PRAGMA wal_autocheckpoint=0;
                 INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('snapshot','none','',0);",
            )
            .unwrap();
            db::insert_task(
                &conn,
                "snapshot",
                "WAL-only task",
                None,
                None,
                &[],
                None,
                "fixture",
            )
            .unwrap();
        }
        assert!(
            std::fs::metadata(dir.path().join("source.db-wal"))
                .unwrap()
                .len()
                > 0
        );
        let state = state(database.clone());
        let before = leftover_snapshots();

        for rejected in [
            headers(&[]),
            headers(&[("authorization", "Bearer wrong-bearer")]),
            headers(&[("authorization", "fixture-bearer")]),
            headers(&[
                ("x-requested-with", "XMLHttpRequest"),
                ("cookie", "gw_session=x"),
            ]),
        ] {
            let error = snapshot(State(state.clone()), rejected).await.unwrap_err();
            assert_eq!(error.0, StatusCode::UNAUTHORIZED);
        }

        let auth = headers(&[("authorization", "Bearer fixture-bearer")]);
        let response = snapshot(State(state.clone()), auth.clone()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let h = response.headers().clone();
        assert_eq!(h[header::CONTENT_TYPE], "application/vnd.sqlite3");
        assert_eq!(h[header::CACHE_CONTROL], "private, no-store");
        let digest = h[SHA256_HEADER].to_str().unwrap().to_string();
        assert!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(leftover_snapshots(), before + 1);
        // Only one snapshot runs at a time.
        assert_eq!(
            snapshot(State(state.clone()), auth.clone())
                .await
                .unwrap_err()
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            leftover_snapshots(),
            before,
            "completed body removes its temp file"
        );
        assert_eq!(
            bytes.len().to_string(),
            h[header::CONTENT_LENGTH].to_str().unwrap()
        );
        assert_eq!(hex::encode(Sha256::digest(&bytes)), digest);

        let copy = dir.path().join("copy.db");
        std::fs::write(&copy, &bytes).unwrap();
        let snap = rusqlite::Connection::open(&copy).unwrap();
        let ok: String = snap
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let mode: String = snap
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        {
            let conn = database.lock().unwrap();
            assert_eq!(schema(&snap), schema(&conn), "all tables and indexes");
            assert_eq!(table_rows(&snap), table_rows(&conn), "all records");
        }
        let wal_task: i64 = snap
            .query_row(
                "SELECT count(*) FROM tasks WHERE title='WAL-only task'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wal_task, 1);

        // A cancelled download also releases the slot and removes its file.
        let cancelled = snapshot(State(state.clone()), auth.clone()).await.unwrap();
        assert_eq!(leftover_snapshots(), before + 1);
        drop(cancelled);
        assert_eq!(leftover_snapshots(), before);
        snapshot(State(state), auth).await.unwrap();
    }
}
