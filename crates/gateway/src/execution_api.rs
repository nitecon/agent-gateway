use crate::{
    execution,
    routes::{require_admin, AppError},
    AppState,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};

type Result<T> = std::result::Result<Json<T>, AppError>;

pub async fn templates(headers: HeaderMap) -> Result<serde_json::Value> {
    require_admin(&headers)?;
    Ok(Json(serde_json::json!({
        "task": crate::execution_queue::prompt("PROJECT", Some("TASK_ID")),
        "cadence": crate::execution_queue::prompt("PROJECT", None),
    })))
}

pub async fn runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
) -> Result<Vec<crate::execution_queue::Run>> {
    require_admin(&headers)?;
    let conn = state.db.lock().unwrap();
    let ident = crate::projects::normalize_project_ident(&ident);
    if crate::db::get_project(&conn, &ident)?.is_none() {
        return Err(AppError(StatusCode::NOT_FOUND, "project not found".into()));
    }
    Ok(Json(crate::execution_queue::runs(&conn, &ident)?))
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

    #[tokio::test]
    async fn execution_metadata_requires_admin_for_browser_sessions() {
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
            templates(headers.clone()).await.err().unwrap().0,
            StatusCode::FORBIDDEN
        );
        headers.insert(crate::ui_auth::USER_ROLE_HEADER, "admin".parse().unwrap());
        assert!(templates(headers).await.is_ok());
    }
}
