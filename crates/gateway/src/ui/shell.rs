//! Shared page shell: `<head>`, sidebar, header, project tab strip, and the
//! closing runtime script. Defined once here and rendered through askama so
//! both the template-based pages and the older `format!`-based pages in
//! `routes.rs` show identical navigation.

use askama::Template;

/// CDN base for the ndesign runtime and theme stylesheets.
pub const NDESIGN_BASE: &str = "https://storage.googleapis.com/ndesign-cdn/ndesign/v0.4.0";

/// Gateway-specific styles layered on top of ndesign: breadcrumb, project tab
/// strip, inbox rows, presence strip. Inlined into every page head so the
/// binary stays self-contained.
pub const GATEWAY_CSS: &str = r#"<style>
.gw-breadcrumb a { color: inherit; text-decoration: none; }
.gw-breadcrumb a:hover { text-decoration: underline; }
.gw-tabs { display: flex; gap: 0.25rem; flex-wrap: wrap; padding: 0 1rem; border-bottom: 1px solid var(--nd-border, rgba(127,127,127,.25)); }
.gw-tabs a { padding: 0.5rem 0.75rem; text-decoration: none; color: inherit; border-bottom: 2px solid transparent; font-size: 0.9rem; }
.gw-tabs a:hover { border-bottom-color: var(--nd-border, rgba(127,127,127,.5)); }
.gw-tabs a.gw-tab-active { border-bottom-color: var(--nd-primary, #2563eb); font-weight: 600; }
.gw-list { list-style: none; margin: 0; padding: 0; }
.gw-list > li { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 0.75rem; align-items: start; padding: 0.6rem 0; border-bottom: 1px solid var(--nd-border, rgba(127,127,127,.2)); }
.gw-list > li:last-child { border-bottom: none; }
.gw-list .gw-item-title { font-weight: 600; overflow-wrap: anywhere; }
.gw-list .gw-item-meta { font-size: 0.8rem; opacity: 0.75; margin-top: 0.15rem; overflow-wrap: anywhere; }
.gw-list .gw-item-actions { display: flex; gap: 0.35rem; flex-wrap: wrap; justify-content: flex-end; }
.gw-chip { display: inline-block; font-size: 0.7rem; padding: 0.05rem 0.45rem; border-radius: 999px; border: 1px solid var(--nd-border, rgba(127,127,127,.4)); margin-right: 0.35rem; vertical-align: middle; white-space: nowrap; }
.gw-chip-warn { border-color: #d97706; color: #d97706; }
.gw-chip-danger { border-color: #dc2626; color: #dc2626; }
.gw-chip-ok { border-color: #16a34a; color: #16a34a; }
.gw-grid { display: grid; gap: 1rem; grid-template-columns: repeat(auto-fit, minmax(18rem, 1fr)); }
.gw-presence { display: grid; gap: 0.75rem; grid-template-columns: repeat(auto-fill, minmax(15rem, 1fr)); }
.gw-presence .nd-card-body { padding: 0.6rem 0.8rem; }
.gw-empty { padding: 1.25rem; text-align: center; opacity: 0.7; }
.gw-empty code { font-size: 0.8rem; }
.gw-filters { display: flex; gap: 0.5rem; flex-wrap: wrap; align-items: center; margin-bottom: 0.75rem; }
.gw-filters a { text-decoration: none; }
.gw-filters a.gw-filter-active { font-weight: 600; text-decoration: underline; }
.gw-inline-form { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; }
.gw-inline-form input, .gw-inline-form select { min-width: 8rem; max-width: 18rem; }
.gw-kv { display: grid; grid-template-columns: max-content 1fr; gap: 0.35rem 1rem; font-size: 0.9rem; }
.gw-kv dt { opacity: 0.7; }
.gw-kv dd { margin: 0; overflow-wrap: anywhere; }
.gw-muted { opacity: 0.7; }
@media (max-width: 640px) {
  .gw-list > li { grid-template-columns: 1fr; }
  .gw-list .gw-item-actions { justify-content: flex-start; }
}
</style>"#;

/// Minimal HTML escaper for values interpolated into `format!` markup.
pub fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A tab in the project tab strip.
pub struct ProjectTab {
    pub key: &'static str,
    pub label: &'static str,
    pub href: String,
}

/// Project context for project-scoped pages: drives the breadcrumb and tabs.
pub struct ProjectNav<'a> {
    pub ident: &'a str,
    pub active_tab: &'a str,
}

impl ProjectNav<'_> {
    pub fn tabs(&self) -> Vec<ProjectTab> {
        let ident = self.ident;
        [
            ("overview", "Overview", format!("/projects/{ident}")),
            ("inbox", "Inbox", format!("/projects/{ident}/inbox")),
            ("tasks", "Tasks", format!("/projects/{ident}/tasks")),
            (
                "artifacts",
                "Artifacts",
                format!("/projects/{ident}/artifacts"),
            ),
            (
                "documentation",
                "Documentation",
                format!("/projects/{ident}/documentation"),
            ),
            (
                "memories",
                "Memories",
                format!("/projects/{ident}/memories"),
            ),
            (
                "settings",
                "Settings",
                format!("/projects/{ident}/settings"),
            ),
        ]
        .into_iter()
        .map(|(key, label, href)| ProjectTab { key, label, href })
        .collect()
    }
}

#[derive(Template)]
#[template(path = "shell_open.html")]
struct ShellOpen<'a> {
    page_title: &'a str,
    nav: &'a str,
    project: Option<ProjectNav<'a>>,
}

#[derive(Template)]
#[template(path = "shell_close.html")]
struct ShellClose<'a> {
    ndesign_base: &'a str,
}

/// Map legacy `active` keys used by older pages onto the sidebar entries.
fn nav_key(active: &str) -> &'static str {
    match active {
        "home" | "dashboard" => "home",
        "activity" => "activity",
        "projects" | "documentation" | "api-docs" | "memories" | "artifacts" => "projects",
        "tasks" => "tasks",
        "patterns" => "patterns",
        "skills" => "skills",
        "commands" => "commands",
        "agents" => "agents",
        "settings" => "settings",
        _ => "",
    }
}

/// Render the `<head>` contents for a control-panel page.
///
/// Emits charset + viewport meta, the page `<title>`, ndesign base CSS, the
/// active theme stylesheet (class `theme` so the runtime switcher can swap it),
/// the two theme-registration meta tags, the gateway stylesheet, plus the
/// `endpoint:api` and `csrf-token` meta tags the ndesign runtime expects.
/// `extra` is appended verbatim.
///
/// `theme` must be `"light"` or `"dark"`; any other value falls back to
/// `"dark"`.
pub fn control_panel_head(title: &str, theme: &str, extra: &str) -> String {
    let theme = if theme == "light" { "light" } else { "dark" };
    format!(
        r#"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="{base}/ndesign.min.css">
<link rel="stylesheet" class="theme" data-theme="{theme}" href="{base}/themes/{theme}.min.css">
<meta name="nd-theme" content="light" data-href="{base}/themes/light.min.css">
<meta name="nd-theme" content="dark" data-href="{base}/themes/dark.min.css">
<meta name="endpoint:api" content="">
<meta name="csrf-token" content="">
{css}
{extra}"#,
        title = he(title),
        base = NDESIGN_BASE,
        theme = theme,
        css = GATEWAY_CSS,
        extra = extra,
    )
}

/// Open the control-panel body up to the start of `<main class="app-content">`
/// for a global (non-project) page. `active` selects the highlighted sidebar
/// entry; legacy keys such as `"dashboard"` and `"documentation"` still work.
pub fn control_panel_open(page_title: &str, active: &str) -> String {
    ShellOpen {
        page_title,
        nav: nav_key(active),
        project: None,
    }
    .render()
    .expect("shell_open template renders")
}

/// Open the control-panel body for a project-scoped page: adds the breadcrumb
/// and the project tab strip with `active_tab` highlighted.
pub fn control_panel_open_project(page_title: &str, ident: &str, active_tab: &str) -> String {
    ShellOpen {
        page_title,
        nav: "projects",
        project: Some(ProjectNav { ident, active_tab }),
    }
    .render()
    .expect("shell_open template renders")
}

/// Close the control-panel body and emit the ndesign runtime plus the inline
/// config that tags same-origin XHR with `X-Gateway-UI` and persists theme
/// changes. The API key is never embedded: pages authenticate with the
/// session cookie issued by `/login`.
pub fn control_panel_close() -> String {
    ShellClose {
        ndesign_base: NDESIGN_BASE,
    }
    .render()
    .expect("shell_close template renders")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_shell_highlights_mapped_nav_entry() {
        let html = control_panel_open("Documentation", "documentation");
        assert!(html.contains(r#"<a href="/projects" class="nd-active">Projects</a>"#));
        assert!(!html.contains("gw-tabs"));
        let home = control_panel_open("Home", "dashboard");
        assert!(home.contains(r#"<a href="/" class="nd-active">Home</a>"#));
    }

    #[test]
    fn project_shell_renders_breadcrumb_and_active_tab() {
        let html = control_panel_open_project("Tasks", "demo", "tasks");
        assert!(html.contains(r#"<a href="/projects/demo">demo</a>"#));
        assert!(html.contains(
            r#"<a href="/projects/demo/tasks" class="gw-tab-active" aria-current="page">Tasks</a>"#
        ));
        assert!(html.contains(r#"<a href="/projects/demo/inbox">Inbox</a>"#));
        assert!(html.contains(r#"<a href="/projects" class="nd-active">Projects</a>"#));
    }

    #[test]
    fn shell_escapes_titles_and_never_embeds_credentials() {
        let html = control_panel_open("<script>", "home");
        assert!(!html.contains("<script>"));
        assert!(html.contains("script&#62;") || html.contains("script&gt;"));
        let close = control_panel_close();
        assert!(!close.contains("Bearer"));
        assert!(close.contains("'X-Gateway-UI': '1'"));
    }
}
