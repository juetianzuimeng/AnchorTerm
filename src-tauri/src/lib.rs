mod app_state;
mod auth;
mod config;
mod cwd;
mod error;
mod ops_log;
mod session;
mod ssh;

use app_state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let log_path = ops_log::init();
    ops_log::log("SYS", &format!("app start; session log={}", log_path.display()));

    // Also mirror tracing to stderr (and we call ops_log at key points).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anchorterm=info,russh=warn".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            session::connect,
            session::disconnect,
            session::close_session,
            session::list_sessions,
            session::write_bytes,
            session::submit_line,
            session::complete_draft,
            session::resize,
            session::get_session_snapshot,
            session::list_profiles,
            session::save_profile,
            session::delete_profile,
            ops_log::ops_log,
            ops_log::ops_log_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AnchorTerm");
}
