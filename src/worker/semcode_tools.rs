use crate::ai::AiTool;
use crate::settings::SemcodeSettings;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

const SEMCODE_DB_DIR: &str = ".semcode.db";
const SEMCODE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Marker recording the git range the worktree database has been indexed over.
/// Every review child of a series asks for the same range, so the first one to
/// take the setup lock does the work and the rest match this and skip.
const SEMCODE_RANGE_MARKER: &str = ".semcode.db.range";

/// A LanceDB table that only exists once a database directory is fully written.
/// semcode itself uses the presence of this table to tell a database directory
/// from an arbitrary one (`database_utils.rs`), so a directory without it is a
/// torn copy rather than something to reuse.
const SEMCODE_DB_SENTINEL: &str = "functions.lance";

/// Backoff for retrying a call that semcode-mcp refused because it was indexing.
/// `semcode-mcp` starts a background pass over `HEAD^..HEAD` on every launch and
/// refuses every query while it runs, so the wait is for a single commit against
/// a database we already indexed.
///
/// Sized off measured passes rather than a guess. On a worktree whose HEAD touches
/// only documentation — the case that defeats semcode's skip check — the pass took
/// 36 s the first time a freshly reflinked 2.7 GB database was read, and about 1 s
/// once those pages were in page cache. So the cost is dominated by cold reads, not
/// by the single commit, and the worst case is a cold cache with up to
/// `review.concurrency` of these running at once. The budget is several times the
/// worst measurement. Still bounded: a pass that is not short degrades to a logged
/// error rather than stalling the review.
const SEMCODE_INDEX_WAIT_BACKOFF: [Duration; 11] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(25),
    Duration::from_secs(25),
    Duration::from_secs(25),
    Duration::from_secs(25),
    Duration::from_secs(25),
    Duration::from_secs(25),
];

/// How long to wait for another review child to finish copying and indexing the
/// shared worktree database. Generous because it is waiting on a real index pass,
/// which the journal shows taking one to three minutes.
const SEMCODE_SETUP_LOCK_TIMEOUT: Duration = Duration::from_secs(900);

/// Why semcode-mcp declined to answer.
///
/// `check_database_status` reports these as prose inside a *successful* `content`
/// payload, so they have to be recognized by text or they reach the model looking
/// like an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// A background index pass is running. Clears on its own, so worth waiting for.
    Indexing,
    /// Terminal for the life of this semcode-mcp process. The `Failed` branch of
    /// `check_database_status` returns before it looks at the table contents, so
    /// one background error makes a fully populated database refuse every
    /// subsequent query; waiting cannot clear it.
    Terminal,
}

pub struct SemcodeToolBox {
    child: Mutex<Child>,
    stdin: Mutex<tokio::process::ChildStdin>,
    stdout: Mutex<BufReader<tokio::process::ChildStdout>>,
    request: Mutex<()>,
    next_id: AtomicU64,
    /// Set once a call has waited out the whole backoff without the index pass
    /// finishing. The background pass runs once per process, so if it outlasts the
    /// budget on one call it will outlast it on all of them — and a review makes up
    /// to ~95 calls, which would add up to hours of sleeping and turn a degraded
    /// review into a timed-out one. Later calls fail fast instead.
    index_wait_exhausted: AtomicBool,
    /// Calls that got a refusal instead of an answer. Setup succeeding says
    /// nothing about the tools working — a stale index passes setup and then
    /// refuses every read — so the review row needs this too, not just
    /// whether the copy and the index pass went through.
    refusals: AtomicU64,
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

        // Drain stderr. semcode-mcp reports its background indexing pass there,
        // and an undrained pipe fills at 64 KiB and blocks the process on write,
        // which surfaces as every later request timing out.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("semcode-mcp stderr: {}", line);
                }
            });
        }

        let toolbox = Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            request: Mutex::new(()),
            next_id: AtomicU64::new(1),
            index_wait_exhausted: AtomicBool::new(false),
            refusals: AtomicU64::new(0),
        };

        toolbox.initialize().await?;
        Ok(toolbox)
    }

    /// How many tool calls got a refusal instead of an answer.
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
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
        tokio::time::timeout(
            SEMCODE_REQUEST_TIMEOUT,
            self.send_request_inner(method, params),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "semcode-mcp request '{}' timed out after {:?}",
                method,
                SEMCODE_REQUEST_TIMEOUT
            )
        })?
    }

    async fn send_request_inner(&self, method: &str, params: Value) -> Result<Value> {
        // semcode-mcp uses one stdin/stdout pair. Keep the complete
        // write/read transaction serialized so another caller cannot consume
        // this request's response while stages or tool calls run in parallel.
        let _request = self.request.lock().await;
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

    /// Issue a tool call, waiting out a background index pass if semcode-mcp
    /// refuses because of one.
    ///
    /// The wait belongs here rather than at startup: the background task leaves
    /// the status at `NotStarted` both before it begins and when it decides to
    /// skip, so a readiness check at startup cannot tell "will not index" from
    /// "has not started indexing yet" and would race the flip to `InProgress`.
    /// Retrying the real call reacts to the condition itself.
    async fn call_mcp(&self, tool: &str, arguments: &Value) -> Result<Value> {
        let params = json!({ "name": tool, "arguments": arguments });
        let mut backoff = SEMCODE_INDEX_WAIT_BACKOFF.iter();
        let mut waited = Duration::ZERO;

        loop {
            let result = self.send_request("tools/call", params.clone()).await?;

            let Some((refusal, message)) = Self::classify_refusal(&result) else {
                if !waited.is_zero() {
                    info!(
                        "semcode-mcp answered '{}' after waiting {:?} for indexing",
                        tool, waited
                    );
                }
                return Ok(result);
            };

            if refusal == Refusal::Terminal {
                warn!("semcode-mcp refused '{}': {}", tool, message);
                self.refusals.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow!("{}", message));
            }

            if self.index_wait_exhausted.load(Ordering::Relaxed) {
                debug!(
                    "semcode-mcp still indexing, not waiting again for '{}'",
                    tool
                );
                self.refusals.fetch_add(1, Ordering::Relaxed);
                return Err(anyhow!("{}", message));
            }

            let Some(delay) = backoff.next() else {
                self.index_wait_exhausted.store(true, Ordering::Relaxed);
                self.refusals.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "semcode-mcp still indexing after {:?}, giving up on '{}' and on \
                     waiting for the rest of this review: {}",
                    waited, tool, message
                );
                return Err(anyhow!("{}", message));
            };
            debug!(
                "semcode-mcp is indexing, retrying '{}' in {:?}",
                tool, delay
            );
            tokio::time::sleep(*delay).await;
            waited += *delay;
        }
    }

    /// The single text segment of an MCP result, when there is exactly one. Both
    /// the miss sentinels and the status refusals arrive in that shape.
    fn single_text_segment(result: &Value) -> Option<&str> {
        let content = result.get("content")?.as_array()?;
        if content.len() != 1 {
            return None;
        }
        content[0].get("text")?.as_str()
    }

    /// Recognize a status message that semcode-mcp returned in place of an answer.
    ///
    /// These arrive as ordinary successful `content` payloads, so without this
    /// they reach the model as though they were findings about the code.
    fn classify_refusal(result: &Value) -> Option<(Refusal, String)> {
        let text = Self::single_text_segment(result);

        // The stale-index refusal marks itself with these, but carries `content`
        // too, so it has to be recognized before the text sentinels.
        let flagged = ["isError", "index_stale"]
            .iter()
            .any(|key| result.get(*key).and_then(Value::as_bool).unwrap_or(false));
        if flagged {
            return Some((
                Refusal::Terminal,
                text.unwrap_or("semcode-mcp reported an error").to_string(),
            ));
        }

        let trimmed = text?.trim();
        if trimmed.starts_with("Database is currently being indexed") {
            return Some((Refusal::Indexing, trimmed.to_string()));
        }
        // "Database is empty" is terminal rather than transient here: we hand
        // semcode-mcp a database we indexed ourselves, so an empty one means the
        // copy is broken, which no amount of waiting fixes.
        if trimmed.starts_with("Database indexing failed:")
            || trimmed.starts_with("Database is empty.")
        {
            return Some((Refusal::Terminal, trimmed.to_string()));
        }
        None
    }

    /// semcode-mcp signals a logical miss with a single text segment like
    /// "Function 'X' not found at git SHA …" or "Type or typedef 'X' not
    /// found …". Match those exact sentinel shapes so we don't mistake a
    /// real function body containing the words "not found" for a miss.
    fn mcp_result_is_miss(result: &Value) -> bool {
        let Some(text) = Self::single_text_segment(result) else {
            return false;
        };
        let trimmed = text.trim();
        (trimmed.starts_with("Function '") || trimmed.starts_with("Type or typedef '"))
            && trimmed.contains("' not found")
    }

    fn format_mcp_result(result: Value) -> Value {
        // Checked before `content`: a refusal carries both, and reporting it as a
        // result would strip exactly the fields that say it is not one.
        if let Some((_, message)) = Self::classify_refusal(&result) {
            return json!({"error": message});
        }

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

/// A semcode database directory is only usable once its `functions` table is
/// present. semcode itself uses that table to tell a database directory from an
/// arbitrary one, so a directory without it is a torn copy, not something to reuse.
fn db_is_complete(path: &Path) -> bool {
    path.join(SEMCODE_DB_SENTINEL).exists()
}

/// Advisory lock over semcode setup in a worktree.
///
/// The reviewer hands one worktree to every review child of a series
/// (`reviewer.rs`), and each child copies and indexes the same
/// `<worktree>/.semcode.db`. Without serializing them they collide inside LanceDB
/// with "Table 'X' already exists" and the review silently loses semcode. flock
/// rather than a sentinel file, because the kernel releases it if a holder dies
/// mid-setup.
struct SetupLock {
    // Closing the file releases the lock, so the guard needs to own it and
    // nothing else needs to happen on drop.
    _file: std::fs::File,
}

impl SetupLock {
    async fn acquire(worktree_path: &Path, timeout: Duration) -> Result<Self> {
        let path = worktree_path.join(format!("{}.lock", SEMCODE_DB_DIR));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| anyhow!("Failed to open semcode setup lock {:?}: {}", path, e))?;

        let deadline = Instant::now() + timeout;
        loop {
            // Non-blocking, then sleep: a blocking flock would pin a tokio worker
            // thread for the minutes an index pass can take.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { _file: file });
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(anyhow!("Failed to lock {:?}: {}", path, error));
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "Timed out after {:?} waiting for another review to finish semcode setup in {:?}",
                    timeout,
                    worktree_path
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Prepare the semcode database in a worktree: copy the repository's index in, then
/// bring it up to `git_range`.
///
/// Serialized across processes and idempotent, because every review child of a
/// series runs this against the same worktree with the same range. The first child
/// to take the lock does the work; the rest match the range marker and skip.
pub async fn setup_worktree_db(
    settings: &SemcodeSettings,
    repo_path: &Path,
    worktree_path: &Path,
    git_range: &str,
) -> Result<()> {
    let _lock = SetupLock::acquire(worktree_path, SEMCODE_SETUP_LOCK_TIMEOUT).await?;

    copy_semcode_db(repo_path, worktree_path).await?;

    let marker = worktree_path.join(SEMCODE_RANGE_MARKER);
    if std::fs::read_to_string(&marker).is_ok_and(|indexed| indexed.trim() == git_range) {
        debug!(
            "semcode DB in {:?} is already indexed over {}",
            worktree_path, git_range
        );
        return Ok(());
    }

    run_semcode_index(settings, worktree_path, git_range).await?;

    if let Err(error) = std::fs::write(&marker, git_range) {
        // Costs a redundant index pass for the next child, nothing worse.
        warn!(
            "Failed to record semcode index range in {:?}: {}",
            marker, error
        );
    }

    Ok(())
}

/// Copy the semcode database from the main repo into a worktree using reflink
/// (copy-on-write). On XFS/btrfs this is near-instant regardless of DB size.
/// Falls back to a regular copy on filesystems without reflink support.
///
/// The copy lands in a temporary directory and is renamed into place, so an
/// interrupted copy cannot leave a half-written database behind for a later run to
/// mistake for a complete one.
pub async fn copy_semcode_db(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    let src = repo_path.join(SEMCODE_DB_DIR);
    if !db_is_complete(&src) {
        return Err(anyhow!(
            "No semcode database at {:?} — run semcode-index on the main repo first",
            src
        ));
    }

    let dst = worktree_path.join(SEMCODE_DB_DIR);
    if db_is_complete(&dst) {
        return Ok(());
    }
    if dst.exists() {
        warn!("Discarding incomplete semcode DB at {:?}", dst);
        std::fs::remove_dir_all(&dst)
            .map_err(|e| anyhow!("Failed to remove incomplete semcode DB {:?}: {}", dst, e))?;
    }

    let tmp = worktree_path.join(format!("{}.tmp-{}", SEMCODE_DB_DIR, std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);

    info!("Copying semcode DB {:?} -> {:?} (reflink)", src, dst);
    let output = Command::new("cp")
        .arg("-r")
        .arg("--reflink=auto")
        .arg(&src)
        .arg(&tmp)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(anyhow!("Failed to copy semcode DB: {}", stderr));
    }

    if let Err(error) = std::fs::rename(&tmp, &dst) {
        let _ = std::fs::remove_dir_all(&tmp);
        // Only an error if nobody else got a complete database there.
        if db_is_complete(&dst) {
            return Ok(());
        }
        return Err(anyhow!(
            "Failed to move semcode DB into {:?}: {}",
            dst,
            error
        ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// One text segment, the shape semcode-mcp uses for both answers and refusals.
    fn text_result(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn indexing_refusal_is_transient_not_an_answer() {
        let result = text_result(
            "Database is currently being indexed (Analyzing files). Please try again shortly.",
        );
        assert_eq!(
            SemcodeToolBox::classify_refusal(&result).map(|(kind, _)| kind),
            Some(Refusal::Indexing)
        );
        assert!(SemcodeToolBox::format_mcp_result(result)["error"].is_string());
    }

    #[test]
    fn failed_and_empty_database_are_terminal() {
        for text in [
            "Database indexing failed: no space left on device. The database may be incomplete or empty.",
            "Database is empty. Run semcode-index first.",
        ] {
            let result = text_result(text);
            assert_eq!(
                SemcodeToolBox::classify_refusal(&result).map(|(kind, _)| kind),
                Some(Refusal::Terminal),
                "{text}"
            );
            assert!(SemcodeToolBox::format_mcp_result(result)["error"].is_string());
        }
    }

    #[test]
    fn stale_index_flags_win_over_the_content_it_ships_with() {
        // The refusal carries a readable `content` payload alongside the flags, so
        // reading `content` first would hand the model an answer and drop the flags.
        for flag in ["isError", "index_stale"] {
            let mut result = text_result("Index is stale relative to the checked-out tree.");
            result[flag] = json!(true);
            assert_eq!(
                SemcodeToolBox::classify_refusal(&result).map(|(kind, _)| kind),
                Some(Refusal::Terminal),
                "{flag}"
            );
            assert_eq!(
                SemcodeToolBox::format_mcp_result(result)["error"],
                json!("Index is stale relative to the checked-out tree.")
            );
        }
    }

    #[test]
    fn a_real_answer_is_still_an_answer() {
        let result = text_result("int foo(void)\n{\n\treturn 0;\n}");
        assert!(SemcodeToolBox::classify_refusal(&result).is_none());
        assert_eq!(
            SemcodeToolBox::format_mcp_result(result)["result"],
            json!("int foo(void)\n{\n\treturn 0;\n}")
        );
    }

    #[test]
    fn miss_sentinels_are_matched_by_shape_not_by_the_words() {
        assert!(SemcodeToolBox::mcp_result_is_miss(&text_result(
            "Function 'foo' not found at git SHA abc123"
        )));
        assert!(SemcodeToolBox::mcp_result_is_miss(&text_result(
            "Type or typedef 'struct foo' not found at git SHA abc123"
        )));
        // A function body that happens to talk about something not being found is
        // an answer; treating it as a miss would fire the kind=any fallback.
        assert!(!SemcodeToolBox::mcp_result_is_miss(&text_result(
            "static int foo(void)\n{\n\t/* not found in the cache */\n\treturn -ENOENT;\n}"
        )));
    }

    #[tokio::test]
    async fn setup_lock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let held = SetupLock::acquire(dir.path(), Duration::from_secs(5))
            .await
            .expect("first acquire");

        // flock is per open file description, so a second open in this same
        // process contends exactly as another review child would.
        assert!(
            SetupLock::acquire(dir.path(), Duration::from_millis(300))
                .await
                .is_err(),
            "second acquire must not get the lock"
        );

        drop(held);
        SetupLock::acquire(dir.path(), Duration::from_secs(5))
            .await
            .expect("acquire after release");
    }

    #[test]
    fn a_torn_copy_is_not_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(SEMCODE_DB_DIR);
        std::fs::create_dir(&db).unwrap();
        assert!(!db_is_complete(&db), "an empty directory is not a database");

        std::fs::create_dir(db.join(SEMCODE_DB_SENTINEL)).unwrap();
        assert!(db_is_complete(&db));
    }

    /// Reproduces the reviewer fan-out: N children, one worktree, one range.
    /// Before the lock and the marker, the losers collided in LanceDB
    /// `create_table` and the review silently continued with no semcode at all.
    #[tokio::test]
    async fn concurrent_children_index_the_worktree_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(repo.join(SEMCODE_DB_DIR).join(SEMCODE_DB_SENTINEL)).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();

        // Stub semcode-index: logs one line per invocation and takes long enough
        // that a second child would overlap it if the lock were not held.
        let log = dir.path().join("index.log");
        let stub = dir.path().join("semcode-index");
        std::fs::write(
            &stub,
            format!("#!/bin/sh\necho \"$@\" >> {:?}\nsleep 0.3\n", log),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let settings = SemcodeSettings {
            index_binary: Some(stub.to_string_lossy().into_owned()),
            enabled: true,
            ..Default::default()
        };

        let range = "base..tip";
        let (first, second, third) = tokio::join!(
            setup_worktree_db(&settings, &repo, &worktree, range),
            setup_worktree_db(&settings, &repo, &worktree, range),
            setup_worktree_db(&settings, &repo, &worktree, range),
        );
        first.expect("first child");
        second.expect("second child");
        third.expect("third child");

        let runs = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            runs.lines().count(),
            1,
            "semcode-index should run once per worktree, got: {runs}"
        );
        assert!(db_is_complete(&worktree.join(SEMCODE_DB_DIR)));
        assert_eq!(
            std::fs::read_to_string(worktree.join(SEMCODE_RANGE_MARKER)).unwrap(),
            range
        );
    }

    /// The indexing wait against a real semcode-mcp, not a synthetic payload.
    ///
    /// Ignored because it needs the semcode binaries on `PATH` and a prepared
    /// worktree. To prepare one: check out a commit whose diff touches no file with
    /// a supported extension (documentation only, say) — that is what defeats
    /// semcode's own skip check and makes it run a full background pass — then
    /// `cp -r --reflink=auto <repo>/.semcode.db <worktree>/`. Run with
    /// `SASHIKO_SEMCODE_TEST_WORKTREE=<worktree> cargo test -- --ignored real_background`.
    ///
    /// How long the pass takes depends on whether the database's pages are in page
    /// cache: 36 s on a cold 2.7 GB database, about 1 s warm, and warm enough it may
    /// finish before the first call and never refuse at all. The test reports which
    /// of those happened rather than requiring one, because it cannot control it;
    /// what it always checks is that the call ends in an answer and not in a refusal
    /// dressed as one.
    #[tokio::test]
    #[ignore = "needs semcode-mcp and a prepared worktree; see the doc comment"]
    async fn the_wait_rides_out_a_real_background_index_pass() {
        let worktree = std::env::var("SASHIKO_SEMCODE_TEST_WORKTREE")
            .expect("set SASHIKO_SEMCODE_TEST_WORKTREE to a prepared worktree");
        let symbol = std::env::var("SASHIKO_SEMCODE_TEST_SYMBOL")
            .unwrap_or_else(|_| "netdev_run_todo".to_string());

        let settings = SemcodeSettings {
            enabled: true,
            ..Default::default()
        };
        let toolbox = SemcodeToolBox::start(&settings, Path::new(&worktree))
            .await
            .expect("start semcode-mcp");

        let started = Instant::now();
        let answer = toolbox
            .call("sc_find_definition", json!({"name": symbol}))
            .await
            .expect("the wait should end in an answer, not an error");
        let waited = started.elapsed();

        if waited > Duration::from_secs(1) {
            eprintln!("waited out a background index pass: {waited:?}");
        } else {
            eprintln!(
                "answered in {waited:?}: the background pass finished first, so this \
                 run exercised the answer path but not the wait"
            );
        }

        let text = answer["result"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a result, got {answer}"));
        assert!(
            !text.starts_with("Database is currently being indexed"),
            "refusal reached the caller as an answer after {waited:?}"
        );
        assert!(
            text.contains(&symbol),
            "expected a definition of {symbol} after {waited:?}, got: {text}"
        );

        toolbox.shutdown().await;
    }
}
