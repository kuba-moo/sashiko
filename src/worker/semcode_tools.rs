use crate::ai::AiTool;
use crate::settings::SemcodeSettings;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

const SEMCODE_DB_DIR: &str = ".semcode.db";

pub struct SemcodeToolBox {
    child: Mutex<Child>,
    stdin: Mutex<tokio::process::ChildStdin>,
    stdout: Mutex<BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
}

impl SemcodeToolBox {
    pub async fn start(settings: &SemcodeSettings, worktree_path: &Path) -> Result<Self> {
        let mcp_binary = settings.mcp_binary.as_deref().unwrap_or("semcode-mcp");

        let db_path = worktree_path.join(SEMCODE_DB_DIR);
        if !db_path.exists() {
            return Err(anyhow!("No semcode database at {:?}", db_path));
        }

        let mut cmd = Command::new(mcp_binary);
        cmd.arg("--database")
            .arg(&db_path)
            .arg("--git-repo")
            .arg(worktree_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        info!("Starting semcode-mcp: {:?}", cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to start semcode-mcp ({}): {}", mcp_binary, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("No stdin on semcode-mcp"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("No stdout on semcode-mcp"))?;

        let toolbox = Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            next_id: AtomicU64::new(1),
        };

        toolbox.initialize().await?;
        Ok(toolbox)
    }

    async fn initialize(&self) -> Result<()> {
        let resp = self
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "sashiko", "version": "0.1.0"}
                }),
            )
            .await?;
        debug!("semcode-mcp initialized: {:?}", resp.get("serverInfo"));
        Ok(())
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        let mut response_line = String::new();
        {
            let mut stdout = self.stdout.lock().await;
            loop {
                response_line.clear();
                let n = stdout.read_line(&mut response_line).await?;
                if n == 0 {
                    return Err(anyhow!("semcode-mcp closed stdout unexpectedly"));
                }
                let trimmed = response_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                    if val.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                    }
                    debug!("semcode-mcp notification: {}", trimmed);
                    continue;
                }
                debug!("semcode-mcp non-JSON line: {}", trimmed);
            }
        }
    }

    pub fn get_declarations(&self) -> Vec<AiTool> {
        vec![
            AiTool {
                name: "sc_find_function".to_string(),
                description: "Find a function or macro by exact name. Returns the full definition body, file location, parameters, return type, and caller/callee counts.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The exact name of the function or macro to find." },
                        "git_sha": { "type": "string", "description": "Git commit SHA to search at (defaults to current HEAD)." }
                    },
                    "required": ["name"]
                }),
            },
            AiTool {
                name: "sc_find_type".to_string(),
                description: "Find a type, struct, union, or typedef by exact name. Returns the full definition with fields/members.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Type name without struct/enum/typedef prefix (e.g. 'task_struct')." },
                        "git_sha": { "type": "string", "description": "Git commit SHA to search at (defaults to current HEAD)." }
                    },
                    "required": ["name"]
                }),
            },
            AiTool {
                name: "sc_find_callers".to_string(),
                description: "Find all functions that call a specific function. Returns each caller's name, file, and line range.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The name of the function to find callers for." },
                        "git_sha": { "type": "string", "description": "Git commit SHA to search at (defaults to current HEAD)." }
                    },
                    "required": ["name"]
                }),
            },
            AiTool {
                name: "sc_find_calls".to_string(),
                description: "Find all functions called by a specific function. Returns each callee's name, file, and line range.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The name of the function to find calls for." },
                        "git_sha": { "type": "string", "description": "Git commit SHA to search at (defaults to current HEAD)." }
                    },
                    "required": ["name"]
                }),
            },
            AiTool {
                name: "sc_find_callchain".to_string(),
                description: "Show the complete call chain for a function: callers (up) and callees (down) to configurable depth.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The name of the function to analyze." },
                        "git_sha": { "type": "string", "description": "Git commit SHA to search at (defaults to current HEAD)." },
                        "up_levels": { "type": "integer", "description": "Caller levels to show (default: 2)." },
                        "down_levels": { "type": "integer", "description": "Callee levels to show (default: 3)." },
                        "calls_limit": { "type": "integer", "description": "Max calls per level (default: 15)." }
                    },
                    "required": ["name"]
                }),
            },
            AiTool {
                name: "sc_grep_functions".to_string(),
                description: "Search function bodies using regex patterns. Returns matching lines from indexed functions, optionally with full bodies.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern to search for in function bodies." },
                        "verbose": { "type": "boolean", "description": "Show full function bodies instead of just matching lines (default: false)." },
                        "git_sha": { "type": "string", "description": "Git commit SHA to search at (defaults to current HEAD)." },
                        "path_pattern": { "type": "string", "description": "Regex pattern to filter results by file path." },
                        "limit": { "type": "integer", "description": "Maximum results to return (default: 100)." }
                    },
                    "required": ["pattern"]
                }),
            },
        ]
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let mcp_tool_name = match name {
            "sc_find_function" => "find_function",
            "sc_find_type" => "find_type",
            "sc_find_callers" => "find_callers",
            "sc_find_calls" => "find_calls",
            "sc_find_callchain" => "find_callchain",
            "sc_grep_functions" => "grep_functions",
            _ => return Err(anyhow!("Unknown semcode tool: {}", name)),
        };

        let result = self
            .send_request(
                "tools/call",
                json!({
                    "name": mcp_tool_name,
                    "arguments": args,
                }),
            )
            .await?;

        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            let text: Vec<&str> = content
                .iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect();
            if !text.is_empty() {
                return Ok(json!({"result": text.join("\n")}));
            }
        }

        if let Some(err) = result.get("error") {
            return Ok(json!({"error": err}));
        }

        Ok(json!({"result": result.to_string()}))
    }

    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        if let Err(e) = child.kill().await {
            warn!("Failed to kill semcode-mcp: {}", e);
        }
    }
}

impl Drop for SemcodeToolBox {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

/// Copy the semcode database from the main repo into a worktree using reflink
/// (copy-on-write). On XFS/btrfs this is near-instant regardless of DB size.
/// Falls back to a regular copy on filesystems without reflink support.
pub async fn copy_semcode_db(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    let src = repo_path.join(SEMCODE_DB_DIR);
    if !src.exists() {
        return Err(anyhow!(
            "No semcode database at {:?} — run semcode-index on the main repo first",
            src
        ));
    }

    let dst = worktree_path.join(SEMCODE_DB_DIR);
    if dst.exists() {
        return Ok(());
    }

    info!("Copying semcode DB {:?} -> {:?} (reflink)", src, dst);
    let output = Command::new("cp")
        .arg("-r")
        .arg("--reflink=auto")
        .arg(&src)
        .arg(&dst)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to copy semcode DB: {}", stderr));
    }

    Ok(())
}

/// Run semcode-index on a worktree to incrementally index a git range.
pub async fn run_semcode_index(
    settings: &SemcodeSettings,
    worktree_path: &Path,
    git_range: &str,
) -> Result<()> {
    let index_binary = settings.index_binary.as_deref().unwrap_or("semcode-index");

    let db_path = worktree_path.join(SEMCODE_DB_DIR);

    let mut cmd = Command::new(index_binary);
    cmd.arg("--source")
        .arg(worktree_path)
        .arg("--database")
        .arg(&db_path)
        .arg("--git")
        .arg(git_range);

    info!("Running semcode-index: {:?}", cmd);
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow!("Failed to run semcode-index ({}): {}", index_binary, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("semcode-index failed: {}", stderr);
        return Err(anyhow!(
            "semcode-index exited with {}: {}",
            output.status,
            stderr
        ));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    if !stdout_str.is_empty() {
        info!("semcode-index: {}", stdout_str.trim());
    }
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !stderr_str.is_empty() {
        debug!("semcode-index stderr: {}", stderr_str.trim());
    }

    Ok(())
}
