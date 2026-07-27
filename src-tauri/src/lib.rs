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
    ops_log::log(
        "SYS",
        &format!(
            "app start; session log={} source={:?}",
            log_path.display(),
            ops_log::log_dir_source()
        ),
    );

    // Also mirror tracing to stderr (and we call ops_log at key points).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anchorterm=info,russh=warn".into()),
        )
        .init();

    // Probe OpenSSH early (UI also checks); log only — do not abort process.
    match ssh::openssh::find_ssh() {
        Ok(p) => ops_log::log("SYS", &format!("ssh available path={}", p.display())),
        Err(e) => ops_log::log("SYS", &format!("ssh missing: {e}")),
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            session::connect,
            session::disconnect,
            session::close_session,
            session::app_quit,
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
            ops_log::open_ops_log_dir,
            ssh::openssh::check_ssh,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AnchorTerm");
}
