//! MCP tool registry and dispatch (PR-M2 … M4 + file transfer).

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use super::config::McpConfig;
use super::exec::{
    check_session_allowed, resolve_session_id, session_exec, ToolError,
};
use super::transfer;
use crate::app_state::{AppState, SessionState};
use crate::ops_log;

/// JSON Schema-ish tool descriptors for `tools/list`.
pub fn tools_list_payload() -> Value {
    json!({
        "tools": [
            {
                "name": "sessions_list",
                "description": "列出 AnchorTerm 中的 SSH 会话快照（session_id、状态、主机、用户名、当前工作目录 cwd、mcp_inflight 并发信道数等）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "connected_only": {
                            "type": "boolean",
                            "description": "为 true 时仅返回已连接（Connected）的会话。默认 false。"
                        }
                    }
                }
            },
            {
                "name": "session_get",
                "description": "按 session_id 获取单个会话的快照信息（含 mcp_inflight 并发数）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "会话 UUID（session_id）"
                        }
                    },
                    "required": ["session_id"]
                }
            },
            {
                "name": "session_exec",
                "description": "通过侧信道 SSH 在会话对应主机上执行命令（不占用交互式 PTY）。会尽量 cd 到已跟踪的 cwd。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "会话 UUID；可省略则使用默认会话"
                        },
                        "command": {
                            "type": "string",
                            "description": "要在远端执行的 shell 命令"
                        },
                        "timeout_secs": {
                            "type": "number",
                            "description": "超时秒数，范围 1–120"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "覆盖工作目录（不填则用会话跟踪的 cwd）"
                        }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "session_read_output",
                "description": "读取某会话交互终端近期输出（环形缓冲区），即用户在终端里看到的内容。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "会话 UUID；可省略则使用默认会话"
                        },
                        "max_bytes": {
                            "type": "number",
                            "description": "从缓冲区末尾读取的最大字节数（默认 16384，最大 131072）"
                        }
                    }
                }
            },
            {
                "name": "session_file_upload",
                "description": "上传文件到会话主机（侧信道）。content=小文件内联(≤1MiB)；local_path=本机大文件，优先 rsync、不可用则 scp。单文件 staging+mv 原子提交；支持通过传入 resume_job_id 进行安全的跨任务断点续传（依赖 rsync，scp 将从头覆盖）。如果不提供 resume_job_id 且同路径存在失败的旧任务，将作为新任务重新发起。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "会话 UUID；可省略则使用默认会话"
                        },
                        "remote_path": {
                            "type": "string",
                            "description": "远端目标路径（绝对路径、~/…，或相对会话 cwd）"
                        },
                        "content": {
                            "type": "string",
                            "description": "内联文件内容（与 local_path 二选一；勿用于大文件；整段写入，不续传）"
                        },
                        "encoding": {
                            "type": "string",
                            "description": "content 的编码：utf8（默认）或 base64"
                        },
                        "local_path": {
                            "type": "string",
                            "description": "本机源文件路径（与 content 二选一；大文件/500MB+ 必须用此模式）"
                        },
                        "create_dirs": {
                            "type": "boolean",
                            "description": "为 true 时先 mkdir -p 远端父目录。默认 false。"
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "为 false 时若远端已存在则失败。默认 true。"
                        },
                        "async": {
                            "type": "boolean",
                            "description": "local_path 模式默认 true：立即返回 job_id；false 则同步等待传输结束（可能很长）"
                        },
                        "verify": {
                            "type": "string",
                            "description": "校验：size（默认，比对字节数）、sha256（哈希，远端无工具时回退 size）、none"
                        },
                        "cleanup_on_fail": {
                            "type": "boolean",
                            "description": "失败/取消时删除远端 staging 临时文件。默认 true。如果设为 false，则保留半截文件，后续可使用 resume_job_id 精确续传。"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "目录递归上传（rsync -a / scp -r）。默认 false。递归无原子 staging。"
                        },
                        "prefer_rsync": {
                            "type": "boolean",
                            "description": "优先 rsync（--partial）。无 rsync 则 scp（不续传）。省略则读 mcp.json（默认 true）。"
                        },
                        "resume_job_id": {
                            "type": "string",
                            "description": "如果传入之前的 job_id，系统将复用该 ID 以进行精确的跨任务断点续传（必须配合 local_path 并确保 remote_path 一致）。若该远端路径正在被其他任务传输，系统将拒绝本次操作以防并发覆盖。"
                        },
                        "timeout_secs": {
                            "type": "number",
                            "description": "content: 1–120s；local_path: 默认 mcp.json transfer_timeout_secs"
                        },
                        "max_bytes": {
                            "type": "number",
                            "description": "content 模式最大字节数（默认/最大 1048576，即 1MB）"
                        }
                    },
                    "required": ["remote_path"]
                }
            },
            {
                "name": "session_file_download",
                "description": "从会话主机下载文件（侧信道）。省略 local_path 则 content 内联返回；指定 local_path 则 rsync/scp（默认异步）。单文件 staging 后 rename；支持通过传入 resume_job_id 恢复失败/取消的历史任务进行断点续传（依赖 rsync，scp 将从头覆盖）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "会话 UUID；可省略则使用默认会话"
                        },
                        "remote_path": {
                            "type": "string",
                            "description": "远端源文件路径"
                        },
                        "local_path": {
                            "type": "string",
                            "description": "保存到本机路径（rsync/scp，大文件必须）；省略则 content 内联返回"
                        },
                        "encoding": {
                            "type": "string",
                            "description": "返回 content 时：auto（默认）/ utf8 / base64"
                        },
                        "async": {
                            "type": "boolean",
                            "description": "local_path 模式默认 true：立即返回 job_id；false 则同步等待传输结束（可能很长）"
                        },
                        "verify": {
                            "type": "string",
                            "description": "校验：size（默认）、sha256、none"
                        },
                        "cleanup_on_fail": {
                            "type": "boolean",
                            "description": "失败/取消时删除本机 staging。默认 true。如果设为 false，则保留半截文件，后续可使用 resume_job_id 精确续传。"
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "local_path 已存在时是否覆盖。默认 true。"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "目录递归下载（rsync -a / scp -r）。默认 false。递归无原子 staging。"
                        },
                        "prefer_rsync": {
                            "type": "boolean",
                            "description": "优先 rsync（--partial）。无 rsync 则 scp（不续传）。省略则读 mcp.json（默认 true）。"
                        },
                        "resume_job_id": {
                            "type": "string",
                            "description": "传入被中断的任务 job_id，尝试复用该 ID 进行断点续传。防止并发覆盖。"
                        },
                        "timeout_secs": {
                            "type": "number",
                            "description": "content: 1–120s；local_path: 默认 mcp.json transfer_timeout_secs"
                        },
                        "max_bytes": {
                            "type": "number",
                            "description": "content 模式最大返回字节数（默认/最大 1MiB）"
                        }
                    },
                    "required": ["remote_path"]
                }
            },
            {
                "name": "session_file_transfer_status",
                "description": "查询异步文件传输任务状态与进度。status=queued/running/succeeded/failed/cancelled；phase=preparing/transferring/verifying/finalizing/done；bytes_done/bytes_total/percent/bytes_per_sec 来自 staging 大小轮询（约每 2s）。建议轮询间隔 2–5s。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "session_file_upload/download 返回的 job_id"
                        }
                    },
                    "required": ["job_id"]
                }
            },
            {
                "name": "session_file_transfer_cancel",
                "description": "取消进行中的异步传输（杀死 rsync/scp 子进程）。默认 cleanup_on_fail 会删除 staging。取消后再 upload/download 是新 job，默认不从断点续传。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "要取消的 job_id"
                        }
                    },
                    "required": ["job_id"]
                }
            },
            {
                "name": "session_file_transfer_pause",
                "description": "暂停进行中的异步传输任务。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "要暂停的 job_id"
                        }
                    },
                    "required": ["job_id"]
                }
            },
            {
                "name": "session_file_transfer_resume",
                "description": "恢复已暂停的异步传输任务。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "要恢复的 job_id"
                        }
                    },
                    "required": ["job_id"]
                }
            },
            {
                "name": "session_file_transfer_list",
                "description": "列出近期异步传输任务（默认最多保留 64 条、完成后约 1 小时）。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "可选：仅列出该会话的任务"
                        }
                    }
                }
            }
        ]
    })
}

/// Dispatch a tool by name. Returns JSON value for tool result content.
pub async fn call_tool(
    app: &AppHandle,
    cfg: &McpConfig,
    name: &str,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let t0 = std::time::Instant::now();
    let out = match name {
        "sessions_list" => tool_sessions_list(app, cfg, arguments),
        "session_get" => tool_session_get(app, cfg, arguments),
        "session_exec" => tool_session_exec(app, cfg, arguments).await,
        "session_read_output" => tool_session_read_output(app, cfg, arguments),
        "session_file_upload" => tool_session_file_upload(app, cfg, arguments).await,
        "session_file_download" => tool_session_file_download(app, cfg, arguments).await,
        "session_file_transfer_status" => tool_session_file_transfer_status(app, arguments),
        "session_file_transfer_cancel" => tool_session_file_transfer_cancel(app, arguments),
        "session_file_transfer_pause" => tool_session_file_transfer_pause(app, arguments),
        "session_file_transfer_resume" => tool_session_file_transfer_resume(app, arguments),
        "session_file_transfer_list" => tool_session_file_transfer_list(app, arguments),
        other => Err(ToolError::invalid(format!("unknown tool: {other}"))),
    };
    let ms = t0.elapsed().as_millis();
    match &out {
        Ok(_) => ops_log::log(
            "MCP",
            &format!("tool={name} ok ms={ms}"),
        ),
        Err(e) => ops_log::log(
            "MCP",
            &format!("tool={name} err={} ms={ms}", e.code),
        ),
    }
    out
}

fn state(app: &AppHandle) -> Result<tauri::State<'_, AppState>, ToolError> {
    app.try_state::<AppState>()
        .ok_or_else(|| ToolError::internal("AppState unavailable"))
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
    })
}

fn snapshot_to_json(snap: &crate::app_state::SessionSnapshot) -> Value {
    json!({
        "session_id": snap.session_id,
        "state": snap.state,
        "host": snap.host,
        "username": snap.username,
        "cwd": snap.cwd,
        "message": snap.message,
        "attempt": snap.attempt,
        "mcp_inflight": snap.mcp_inflight,
    })
}

fn tool_sessions_list(
    app: &AppHandle,
    cfg: &McpConfig,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let st = state(app)?;
    let connected_only = arg_bool(arguments, "connected_only");
    let mut sessions: Vec<Value> = st
        .list_snapshots()
        .into_iter()
        .filter(|s| {
            if let Some(ref allow) = cfg.allowed_session_ids {
                if !allow.iter().any(|a| a == &s.session_id) {
                    return false;
                }
            }
            if connected_only && s.state != SessionState::Connected {
                return false;
            }
            true
        })
        .map(|s| snapshot_to_json(&s))
        .collect();
    // Stable-ish order: host then id
    sessions.sort_by(|a, b| {
        let ha = a.get("host").and_then(|v| v.as_str()).unwrap_or("");
        let hb = b.get("host").and_then(|v| v.as_str()).unwrap_or("");
        ha.cmp(hb).then_with(|| {
            let ia = a.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
            let ib = b.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
            ia.cmp(ib)
        })
    });
    Ok(json!({ "sessions": sessions, "count": sessions.len() }))
}

fn tool_session_get(
    app: &AppHandle,
    cfg: &McpConfig,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let sid = resolve_session_id(arg_str(arguments, "session_id"), cfg)?;
    check_session_allowed(&sid, cfg)?;
    let st = state(app)?;
    let rt = st
        .get_runtime(&sid)
        .map_err(|_| ToolError::not_found(format!("session not found: {sid}")))?;
    Ok(snapshot_to_json(&rt.snapshot()))
}

async fn tool_session_exec(
    app: &AppHandle,
    cfg: &McpConfig,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let sid = resolve_session_id(arg_str(arguments, "session_id"), cfg)?;
    check_session_allowed(&sid, cfg)?;
    let command = arg_str(arguments, "command")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("command is required"))?;
    let cwd = arg_str(arguments, "cwd");
    let timeout_secs = arg_u64(arguments, "timeout_secs");

    let st = state(app)?;
    let rt = st
        .get_runtime(&sid)
        .map_err(|_| ToolError::not_found(format!("session not found: {sid}")))?;

    let result = session_exec(&rt, cfg, command, cwd, timeout_secs).await?;
    Ok(serde_json::to_value(result).unwrap_or_else(|e| json!({"error": e.to_string()})))
}

fn tool_session_read_output(
    app: &AppHandle,
    cfg: &McpConfig,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let sid = resolve_session_id(arg_str(arguments, "session_id"), cfg)?;
    check_session_allowed(&sid, cfg)?;
    let max_bytes = arg_u64(arguments, "max_bytes")
        .unwrap_or(16_384)
        .clamp(256, 128 * 1024) as usize;

    let st = state(app)?;
    let rt = st
        .get_runtime(&sid)
        .map_err(|_| ToolError::not_found(format!("session not found: {sid}")))?;

    let snap = rt
        .output_ring
        .lock()
        .map_err(|_| ToolError::internal("output_ring lock poisoned"))?
        .snapshot(max_bytes);

    ops_log::log(
        "MCP",
        &format!(
            "tool=session_read_output sid={} returned={} avail={} trunc={}",
            &sid[..sid.len().min(8)],
            snap.returned_bytes,
            snap.available_bytes,
            snap.truncated
        ),
    );

    Ok(json!({
        "session_id": sid,
        "text": snap.text,
        "truncated": snap.truncated,
        "available_bytes": snap.available_bytes,
        "returned_bytes": snap.returned_bytes,
    }))
}

fn transfer_jobs(app: &AppHandle) -> Result<transfer::JobRegistry, ToolError> {
    let st = state(app)?;
    let g = st
        .mcp
        .lock()
        .map_err(|_| ToolError::internal("MCP 状态锁损坏"))?;
    Ok(g.transfer_jobs.clone())
}

async fn tool_session_file_upload(
    app: &AppHandle,
    cfg: &McpConfig,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let sid = resolve_session_id(arg_str(arguments, "session_id"), cfg)?;
    check_session_allowed(&sid, cfg)?;
    let remote_path = arg_str(arguments, "remote_path")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("remote_path is required"))?;

    let content = arguments.get("content").and_then(|v| v.as_str());
    let local_path = arg_str(arguments, "local_path");
    let encoding = arg_str(arguments, "encoding");
    let create_dirs = transfer::arg_bool(arguments, "create_dirs", false);
    let overwrite = transfer::arg_bool(arguments, "overwrite", true);
    let timeout_secs = arg_u64(arguments, "timeout_secs");
    let max_bytes = arg_u64(arguments, "max_bytes");
    let async_mode = arguments.get("async").and_then(|v| v.as_bool());
    let verify = transfer::parse_verify_mode(
        arg_str(arguments, "verify").or(Some(cfg.transfer_default_verify.as_str())),
    )?;
    let cleanup_on_fail = transfer::arg_bool(arguments, "cleanup_on_fail", true);
    let recursive = transfer::arg_bool(arguments, "recursive", false);
    let prefer_rsync = arguments.get("prefer_rsync").and_then(|v| v.as_bool());
    let resume_job_id = arguments.get("resume_job_id").and_then(|v| v.as_str());

    let st = state(app)?;
    let rt = st
        .get_runtime(&sid)
        .map_err(|_| ToolError::not_found(format!("session not found: {sid}")))?;
    let registry = transfer_jobs(app)?;

    transfer::session_file_upload(
        rt,
        cfg,
        &registry,
        transfer::FileTransferArgs {
            remote_path,
            content,
            encoding,
            local_path,
            create_dirs,
            overwrite,
            timeout_secs,
            max_bytes,
            async_mode,
            verify,
            cleanup_on_fail,
            recursive,
            prefer_rsync,
            resume_job_id,
        },
    )
    .await
}

async fn tool_session_file_download(
    app: &AppHandle,
    cfg: &McpConfig,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let sid = resolve_session_id(arg_str(arguments, "session_id"), cfg)?;
    check_session_allowed(&sid, cfg)?;
    let remote_path = arg_str(arguments, "remote_path")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("remote_path is required"))?;
    let local_path = arg_str(arguments, "local_path");
    let encoding = arg_str(arguments, "encoding");
    let timeout_secs = arg_u64(arguments, "timeout_secs");
    let max_bytes = arg_u64(arguments, "max_bytes");
    let async_mode = arguments.get("async").and_then(|v| v.as_bool());
    let verify = transfer::parse_verify_mode(
        arg_str(arguments, "verify").or(Some(cfg.transfer_default_verify.as_str())),
    )?;
    let cleanup_on_fail = transfer::arg_bool(arguments, "cleanup_on_fail", true);
    let overwrite = transfer::arg_bool(arguments, "overwrite", true);
    let recursive = transfer::arg_bool(arguments, "recursive", false);
    let prefer_rsync = arguments.get("prefer_rsync").and_then(|v| v.as_bool());
    let resume_job_id = arguments.get("resume_job_id").and_then(|v| v.as_str());

    let st = state(app)?;
    let rt = st
        .get_runtime(&sid)
        .map_err(|_| ToolError::not_found(format!("session not found: {sid}")))?;
    let registry = transfer_jobs(app)?;

    transfer::session_file_download(
        rt,
        cfg,
        &registry,
        transfer::FileTransferArgs {
            remote_path,
            content: None,
            encoding,
            local_path,
            create_dirs: false,
            overwrite,
            timeout_secs,
            max_bytes,
            async_mode,
            verify,
            cleanup_on_fail,
            recursive,
            prefer_rsync,
            resume_job_id,
        },
    )
    .await
}

fn tool_session_file_transfer_status(
    app: &AppHandle,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let job_id = arg_str(arguments, "job_id")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("job_id is required"))?;
    let registry = transfer_jobs(app)?;
    transfer::transfer_status(&registry, job_id)
}

fn tool_session_file_transfer_cancel(
    app: &AppHandle,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let job_id = arg_str(arguments, "job_id")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("job_id is required"))?;
    let registry = transfer_jobs(app)?;
    transfer::transfer_cancel(&registry, job_id)
}

fn tool_session_file_transfer_pause(
    app: &AppHandle,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let job_id = arg_str(arguments, "job_id")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("job_id is required"))?;
    let registry = transfer_jobs(app)?;
    transfer::transfer_pause(&registry, job_id)
}

fn tool_session_file_transfer_resume(
    app: &AppHandle,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let job_id = arg_str(arguments, "job_id")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("job_id is required"))?;
    let registry = transfer_jobs(app)?;
    transfer::transfer_resume(&registry, job_id)
}

fn tool_session_file_transfer_list(
    app: &AppHandle,
    arguments: &Value,
) -> Result<Value, ToolError> {
    let sid = arg_str(arguments, "session_id");
    let registry = transfer_jobs(app)?;
    transfer::transfer_list(&registry, sid)
}

/// Format tool result as MCP `content` array text.
pub fn tool_result_mcp_content(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| value.to_string());
    json!({
        "content": [
            { "type": "text", "text": text }
        ],
        "isError": is_error
    })
}
