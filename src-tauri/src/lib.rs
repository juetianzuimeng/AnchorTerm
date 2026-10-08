mod app_state;
mod auth;
mod config;
mod cwd;
mod error;
mod external_sftp;
mod mcp;
mod ops_log;
mod output_ring;
mod session;
mod ssh;

use app_state::AppState;

/// CLI: `anchorterm mcp-stdio` — stdio MCP bridge (no GUI). See [`mcp::run_mcp_stdio`].
pub fn run_mcp_stdio() -> Result<(), String> {
    mcp::run_mcp_stdio()
}

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
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                mcp::bootstrap(handle).await;
            });
            Ok(())
        })
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
            session::export_profiles,
            session::import_profiles,
            session::save_profile,
            session::delete_profile,
            ops_log::ops_log,
            ops_log::ops_log_info,
            ops_log::open_ops_log_dir,
            ssh::openssh::check_ssh,
            external_sftp::detect_xftp,
            external_sftp::launch_xftp,
            mcp::mcp_get_status,
            mcp::mcp_set_enabled,
            mcp::mcp_regenerate_token,
            mcp::mcp_update_settings,
            mcp::mcp_apply,
            mcp::mcp_ui_transfer_start,
            mcp::mcp_ui_transfer_list,
            mcp::mcp_ui_transfer_cancel,
            mcp::mcp_ui_transfer_clear_done,
            ssh::forward::start_local_forward,
            ssh::forward::stop_local_forward,
            ssh::forward::list_local_forwards,
            ssh::forward::get_forward_auto_restore,
            ssh::forward::set_forward_auto_restore,
        ])
        .run(tauri::generate_context!())
        .expect("error while running AnchorTerm");
}
