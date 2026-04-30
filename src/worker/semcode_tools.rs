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
                name: "sc_find_definition".to_string(),
                description: "Find a definition by exact name — a function, macro, type, struct, union, or typedef. Returns the full definition body with file location, and for functions also parameters, return type, and caller/callee counts. Use `kind` when you already know which category you want; the default searches both and is the right choice when unsure.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The exact name of the symbol to find, without struct/enum/typedef prefix (e.g. 'task_struct', not 'struct task_struct')." },
                        "kind": {
                            "type": "string",
                            "enum": ["function", "type", "any"],
                            "description": "Which category to search. 'function' covers functions and macros. 'type' covers struct/union/typedef. 'any' (default) tries function first, then type if nothing is found."
                        },
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
        if name == "sc_find_definition" {
            return self.call_find_definition(args).await;
        }

        let mcp_tool_name = match name {
            "sc_find_callers" => "find_callers",
            "sc_find_calls" => "find_calls",
            "sc_find_callchain" => "find_callchain",
            "sc_grep_functions" => "grep_functions",
            _ => return Err(anyhow!("Unknown semcode tool: {}", name)),
        };

        let result = self.call_mcp(mcp_tool_name, &args).await?;
        Ok(Self::format_mcp_result(result))
    }

    /// Route sc_find_definition to find_function / find_type based on `kind`.
    async fn call_find_definition(&self, args: Value) -> Result<Value> {
        let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("any");

        // Strip `kind` before forwarding — the MCP side doesn't know it.
        let mut forwarded = args.clone();
        if let Some(obj) = forwarded.as_object_mut() {
            obj.remove("kind");
        }

        let (first, fallback) = match kind {
            "function" => ("find_function", None),
            "type" => ("find_type", None),
            _ => ("find_function", Some("find_type")),
        };

        let first_result = self.call_mcp(first, &forwarded).await?;
        if let Some(fb) = fallback
            && Self::mcp_result_is_miss(&first_result)
        {
            let fb_result = self.call_mcp(fb, &forwarded).await?;
            return Ok(Self::format_mcp_result(fb_result));
        }
        Ok(Self::format_mcp_result(first_result))
    }

    async fn call_mcp(&self, tool: &str, arguments: &Value) -> Result<Value> {
        self.send_request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
        .await
    }

    /// semcode-mcp signals a logical miss with a single text segment like
    /// "Function 'X' not found at git SHA …" or "Type or typedef 'X' not
    /// found …". Match those exact sentinel shapes so we don't mistake a
    /// real function body containing the words "not found" for a miss.
    fn mcp_result_is_miss(result: &Value) -> bool {
        let Some(content) = result.get("content").and_then(|c| c.as_array()) else {
            return false;
        };
        if content.len() != 1 {
            return false;
        }
        let Some(text) = content[0].get("text").and_then(|t| t.as_str()) else {
            return false;
        };
        let trimmed = text.trim();
        (trimmed.starts_with("Function '") || trimmed.starts_with("Type or typedef '"))
            && trimmed.contains("' not found")
    }

    fn format_mcp_result(result: Value) -> Value {
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            let text: Vec<&str> = content
                .iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect();
            if !text.is_empty() {
                return json!({"result": text.join("\n")});
            }
        }

        if let Some(err) = result.get("error") {
            return json!({"error": err.clone()});
        }

        json!({"result": result.to_string()})
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
