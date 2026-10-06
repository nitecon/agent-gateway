use crate::{
    execution,
    routes::{require_admin, AppError},
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};

type Result<T> = std::result::Result<Json<T>, AppError>;

#[derive(Default, serde::Deserialize)]
pub struct RunQuery {
    pub task_id: Option<String>,
}

pub async fn all_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Vec<crate::execution_queue::Run>> {
    require_admin(&headers)?;
    Ok(Json(crate::execution_queue::list_runs(
        &state.db.lock().unwrap(),
        None,
        None,
    )?))
}

pub async fn templates(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<execution::Templates> {
    require_admin(&headers)?;
    Ok(Json(execution::templates(&state.db.lock().unwrap())?))
}

pub async fn put_templates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(value): Json<execution::Templates>,
) -> Result<execution::Templates> {
    require_admin(&headers)?;
    execution::save_templates(&state.db.lock().unwrap(), &value)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(value))
}

pub async fn runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Query(query): Query<RunQuery>,
) -> Result<Vec<crate::execution_queue::Run>> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    let ident = crate::projects::normalize_project_ident(&ident);
    if crate::db::get_project(&conn, &ident)?.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "project not found".into()));
    }
    Ok(Json(match query.task_id.as_deref() {
        Some(task) => crate::execution_queue::list_runs(&conn, Some(&ident), Some(task))?,
        None => crate::execution_queue::runs(&conn, &ident)?,
    }))
}

pub async fn clients(headers: HeaderMap) -> Result<Vec<crate::execution_client::DiscoveredClient>> {
    require_admin(&headers)?;
    Ok(Json(crate::execution_client::discover()))
}

pub async fn get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<execution::Settings> {
    require_admin(&headers)?;
    Ok(Json(execution::settings(&state.db.lock().unwrap())?))
}

pub async fn put_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(value): Json<execution::Settings>,
) -> Result<execution::Settings> {
    require_admin(&headers)?;
    execution::save_settings(&state.db.lock().unwrap(), &value)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(value))
}

pub async fn get_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
) -> Result<execution::ProjectSettings> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    if crate::db::get_project(&conn, &ident)?.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "project not found".into()));
    }
    Ok(Json(execution::project_settings(&conn, &ident)?))
}

pub async fn put_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Json(value): Json<execution::ProjectSettings>,
) -> Result<execution::ProjectSettings> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    if crate::db::get_project(&conn, &ident)?.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "project not found".into()));
    }
    execution::save_project_settings(&conn, &ident, &value)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState {
            db: crate::db::open(":memory:").unwrap(),
            plugins: Default::default(),
            #[cfg(feature = "whatsapp")]
            whatsapp: None,
            default_channel: String::new(),
            api_key: String::new(),
            ui_auth_enabled: true,
            retention_days: 30,
            bot_retention_days: 7,
            artifact_operations: Default::default(),
            artifact_body_schema_enabled: false,
            artifact_auth_enforced: false,
            update_available: Default::default(),
        }
    }

    #[tokio::test]
    async fn execution_metadata_requires_admin_for_browser_sessions() {
        let state = state();
        let mut headers = HeaderMap::new();
        headers.insert(
            crate::ui_auth::USER_HEADER,
            "1:member:Member".parse().unwrap(),
        );
        headers.insert(crate::ui_auth::USER_ROLE_HEADER, "member".parse().unwrap());
        assert_eq!(
            clients(headers.clone()).await.err().unwrap().0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            all_runs(State(state.clone()), headers.clone())
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            crate::execution_socket::sessions(State(state.clone()), headers.clone())
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            runs(
                State(state.clone()),
                headers.clone(),
                Path("fixture".into()),
                Query(RunQuery::default())
            )
            .await
            .err()
            .unwrap()
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            templates(State(state.clone()), headers.clone())
                .await
                .err()
                .unwrap()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            put_templates(
                State(state.clone()),
                headers.clone(),
                Json(execution::Templates::default())
            )
            .await
            .err()
            .unwrap()
            .0,
            StatusCode::FORBIDDEN
        );
        headers.insert(crate::ui_auth::USER_ROLE_HEADER, "admin".parse().unwrap());
        assert!(templates(State(state), headers).await.is_ok());
    }

    #[tokio::test]
    async fn instructions_round_trip_and_reject_empty_updates() {
        let state = state();
        let defaults = templates(State(state.clone()), HeaderMap::new())
            .await
            .unwrap()
            .0;
        assert_eq!(defaults, execution::Templates::default());
        let value = execution::Templates {
            task: "\nTask {{task_id}} in {{project}}: preserve <text> & whitespace.\n".into(),
            cadence: "Scheduled {{project}} / {{task_id}}".into(),
        };
        assert_eq!(
            put_templates(State(state.clone()), HeaderMap::new(), Json(value.clone()))
                .await
                .unwrap()
                .0,
            value
        );
        let saved = templates(State(state.clone()), HeaderMap::new())
            .await
            .unwrap()
            .0;
        assert_eq!(saved, value);
        assert_eq!(
            saved.render("fixture", "123", "task"),
            "\nTask 123 in fixture: preserve <text> & whitespace.\n"
        );
        assert_eq!(
            saved.render("fixture", "123", "cadence"),
            "Scheduled fixture / 123"
        );
        for invalid in [
            execution::Templates {
                task: " \n".into(),
                ..value.clone()
            },
            execution::Templates {
                cadence: String::new(),
                ..value.clone()
            },
        ] {
            assert_eq!(
                put_templates(State(state.clone()), HeaderMap::new(), Json(invalid))
                    .await
                    .err()
                    .unwrap()
                    .0,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                templates(State(state.clone()), HeaderMap::new())
                    .await
                    .unwrap()
                    .0,
                value
            );
        }
    }
}
