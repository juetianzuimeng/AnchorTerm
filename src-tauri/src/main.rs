// Prevents additional console window on Windows in release, DO NOT REMOVE!!
// Note: `mcp-stdio` still works with piped stdio under the windows subsystem
// (MCP clients spawn with redirected pipes). For interactive debug, stderr may
// need a parent console (AttachConsole is best-effort in the bridge).
//
// Password auth: OpenSSH spawns this same PE as SSH_ASKPASS (see openssh.rs).
// That path must run *before* Tauri and must be a real .exe (not a .cmd).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Highest priority: act as OpenSSH askpass helper (password login on Windows).
    if try_run_as_ssh_askpass() {
        return;
    }

    // Subcommands before Tauri GUI (PR-M5).
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("mcp-stdio") => {
            if let Err(e) = anchorterm_lib::run_mcp_stdio() {
                // Prefer stderr; GUI subsystem may drop it when no console.
                eprintln!("anchorterm mcp-stdio failed:\n{e}");
                std::process::exit(1);
            }
        }
        Some("mcp-help") | Some("--mcp-help") => {
            print_mcp_help();
        }
        Some("-h") | Some("--help") if is_mcp_help_context() => {
            print_mcp_help();
        }
        _ => {
            anchorterm_lib::run();
        }
    }
}

/// When OpenSSH needs a password under CREATE_NO_WINDOW, it launches
/// `SSH_ASKPASS` (= this executable) with inherited env:
/// - `ANCHORTERM_SSH_ASKPASS=1`
/// - `ANCHORTERM_ASKPASS_FILE=<temp secret path>`
///
/// We print the secret to stdout and exit. Must not start the GUI.
fn try_run_as_ssh_askpass() -> bool {
    use std::io::Write;

    if std::env::var_os("ANCHORTERM_SSH_ASKPASS").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return false;
    }

    let path = match std::env::var("ANCHORTERM_ASKPASS_FILE") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            let _ = writeln!(std::io::stderr(), "anchorterm askpass: missing secret file env");
            std::process::exit(1);
        }
    };

    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut out = std::io::stdout();
            // OpenSSH reads the full stdout as the password response.
            let _ = out.write_all(&bytes);
            let _ = out.flush();
            std::process::exit(0);
        }
        Err(e) => {
            let _ = writeln!(
                std::io::stderr(),
                "anchorterm askpass: cannot read secret file: {e}"
            );
            std::process::exit(1);
        }
    }
}

fn is_mcp_help_context() -> bool {
    // Only hijack --help when user clearly asks about mcp; otherwise let app start.
    std::env::args().any(|a| a.contains("mcp"))
}

fn print_mcp_help() {
    eprintln!(
        "\
AnchorTerm MCP
  mcp-stdio     Stdio bridge to the running GUI MCP server (PR-M5)
  mcp-help      Show this help

Typical Cursor / Claude Desktop config:
  {{
    \"mcpServers\": {{
      \"anchorterm\": {{
        \"command\": \"C:\\\\Path\\\\to\\\\anchorterm.exe\",
        \"args\": [\"mcp-stdio\"]
      }}
    }}
  }}

Prerequisites:
  1. Start AnchorTerm (GUI)
  2. 工具 → MCP 服务器 → enable → Apply
  3. Connect SSH sessions as needed
"
    );
}
