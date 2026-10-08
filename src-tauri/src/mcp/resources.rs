//! PR-M6: MCP Resources Support (2025-11-25)
//!
//! Provides predefined resources and handles `resources/list`, `resources/templates/list`, `resources/read`.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::fs;
use std::env;
use crate::error::AppError;

pub struct ResourceDescriptor {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
    pub file_rel_path: Option<&'static str>,
    pub is_binary: bool,
    pub builtin_text: Option<&'static str>,
}

pub const STATIC_RESOURCES: &[ResourceDescriptor] = &[
    ResourceDescriptor {
        uri: "anchorterm://tools/at-redis/linux-amd64",
        name: "at-redis (linux-amd64)",
        description: "AnchorTerm 定制极简纯静态 Redis 客户端。从标准输入读取一行命令，连接参数从权限 600 文件读取，支持 --read-only。适用于 x86_64 Linux 远端 Redis 诊断。请注意：优先将其上传到 /tmp 等临时目录执行，切勿直接上传到业务程序的运行目录。",
        mime_type: "application/octet-stream",
        file_rel_path: Some("tools/at-redis-linux-amd64"),
        is_binary: true,
        builtin_text: None,
    },
    ResourceDescriptor {
        uri: "anchorterm://tools/at-redis/linux-arm64",
        name: "at-redis (linux-arm64)",
        description: "AnchorTerm 定制极简纯静态 Redis 客户端。从标准输入读取一行命令，连接参数从权限 600 文件读取，支持 --read-only。适用于 aarch64/arm64 Linux 远端 Redis 诊断。请注意：优先将其上传到 /tmp 等临时目录执行，切勿直接上传到业务程序的运行目录。",
        mime_type: "application/octet-stream",
        file_rel_path: Some("tools/at-redis-linux-arm64"),
        is_binary: true,
        builtin_text: None,
    },
    ResourceDescriptor {
        uri: "anchorterm://tools/at-mysql/linux-amd64",
        name: "at-mysql (linux-amd64)",
        description: "AnchorTerm 定制极简纯静态 MySQL 客户端（仅链接 go-sql-driver/mysql）。从标准输入读取一条 SQL，连接参数从权限 600 文件读取，支持 --read-only 与 --max-rows。适用于 x86_64 Linux 远端数据库诊断。请注意：优先将其上传到 /tmp 等临时目录执行，切勿直接上传到业务程序的运行目录。",
        mime_type: "application/octet-stream",
        file_rel_path: Some("tools/at-mysql-linux-amd64"),
        is_binary: true,
        builtin_text: None,
    },
    ResourceDescriptor {
        uri: "anchorterm://tools/at-mysql/linux-arm64",
        name: "at-mysql (linux-arm64)",
        description: "AnchorTerm 定制极简纯静态 MySQL 客户端（仅链接 go-sql-driver/mysql）。从标准输入读取一条 SQL，连接参数从权限 600 文件读取，支持 --read-only 与 --max-rows。适用于 aarch64/arm64 Linux 远端数据库诊断。请注意：优先将其上传到 /tmp 等临时目录执行，切勿直接上传到业务程序的运行目录。",
        mime_type: "application/octet-stream",
        file_rel_path: Some("tools/at-mysql-linux-arm64"),
        is_binary: true,
        builtin_text: None,
    },
    ResourceDescriptor {
        uri: "anchorterm://tools/spring-connection/linux-amd64",
        name: "spring-connection (linux-amd64)",
        description: "从 Java 进程工作目录按规范顺序提取 MySQL 与 Redis 连接信息。支持外部 yml、Fat Jar 内嵌 yml（zip 流式只读）、profile 及环境变量覆盖。凭据写入远端 600 文件，标准输出脱敏，无口令泄露。请注意：优先将其上传到 /tmp 等临时目录执行，切勿直接上传到业务程序的运行目录。",
        mime_type: "application/octet-stream",
        file_rel_path: Some("tools/spring-connection-linux-amd64"),
        is_binary: true,
        builtin_text: None,
    },
    ResourceDescriptor {
        uri: "anchorterm://tools/spring-connection/linux-arm64",
        name: "spring-connection (linux-arm64)",
        description: "从 Java 进程工作目录按规范顺序提取 MySQL 与 Redis 连接信息。支持外部 yml、Fat Jar 内嵌 yml（zip 流式只读）、profile 及环境变量覆盖。凭据写入远端 600 文件，标准输出脱敏，无口令泄露。请注意：优先将其上传到 /tmp 等临时目录执行，切勿直接上传到业务程序的运行目录。",
        mime_type: "application/octet-stream",
        file_rel_path: Some("tools/spring-connection-linux-arm64"),
        is_binary: true,
        builtin_text: None,
    }
];

fn resolve_resource_path(rel_path: &str) -> Option<PathBuf> {
    // 1. Try relative to current exe
    if let Ok(exe_path) = env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let p1 = parent.join("resources").join(rel_path);
            if p1.exists() {
                return Some(p1);
            }
        }
    }
    // 2. Try relative to CWD (for development `cargo run`)
    if let Ok(cwd) = env::current_dir() {
        let p2 = cwd.join("resources").join(rel_path);
        if p2.exists() {
            return Some(p2);
        }
    }
    None
}

pub fn resources_list_payload() -> Value {
    let mut items = Vec::new();
    for res in STATIC_RESOURCES {
        let mut size = None;
        if res.is_binary {
            if let Some(rel) = res.file_rel_path {
                if let Some(path) = resolve_resource_path(rel) {
                    if let Ok(meta) = fs::metadata(&path) {
                        size = Some(meta.len());
                    }
                }
            }
        } else if let Some(txt) = res.builtin_text {
            size = Some(txt.len() as u64);
        }

        let mut item = json!({
            "uri": res.uri,
            "name": res.name,
            "description": res.description,
            "mimeType": res.mime_type
        });
        
        if let Some(s) = size {
            item["size"] = json!(s);
        }
        items.push(item);
    }
    json!({ "resources": items })
}

pub fn resource_templates_list_payload() -> Value {
    json!({
        "resourceTemplates": [
            {
                "uriTemplate": "anchorterm://tools/{name}/{os}-{arch}",
                "name": "AnchorTerm 预编译静态运维工具集",
                "description": "提供无需动态依赖的 Linux 全静态单文件命令行工具。name 取值为 at-redis 或 at-mysql；os 取值为 linux；arch 取值为 amd64 或 arm64。当目标 Linux 服务器缺少对应的客户端时，按目标架构获取二进制资源。",
                "mimeType": "application/octet-stream"
            }
        ]
    })
}

pub fn read_resource_payload(uri: &str) -> Result<Value, AppError> {
    let res = STATIC_RESOURCES.iter().find(|r| r.uri == uri)
        .ok_or_else(|| AppError::Message(format!("Resource not found: {}", uri)))?;

    if res.is_binary {
        let rel = res.file_rel_path.unwrap_or("");
        let path = resolve_resource_path(rel)
            .ok_or_else(|| AppError::Message(format!("Binary resource file not found locally: {}", rel)))?;
        
        let data = fs::read(&path).map_err(|e| AppError::Message(format!("Failed to read resource: {}", e)))?;
        use base64::{Engine as _, engine::general_purpose};
        let b64 = general_purpose::STANDARD.encode(&data);
        
        // Return standard blob format + inject local_path in _meta for client optimization
        Ok(json!({
            "contents": [
                {
                    "uri": res.uri,
                    "mimeType": res.mime_type,
                    "blob": b64
                }
            ],
            "_meta": {
                "local_path": path.to_string_lossy().into_owned()
            }
        }))
    } else {
        let txt = res.builtin_text.unwrap_or("");
        Ok(json!({
            "contents": [
                {
                    "uri": res.uri,
                    "mimeType": res.mime_type,
                    "text": txt
                }
            ]
        }))
    }
}
