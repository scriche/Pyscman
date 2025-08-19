use actix_multipart::Multipart;
use futures_util::TryStreamExt as _;
#[post("/api/tasks/{id}/upload")]
async fn upload_file(
    data: web::Data<AppState>,
    id: web::Path<String>,
    mut payload: Multipart,
) -> impl Responder {
    let task_id = id.into_inner();
    // Ensure the task exists
    let tasks = data.tasks.lock().unwrap();
    if !tasks.contains_key(&task_id) {
        return HttpResponse::NotFound().body("Task not found");
    }
    drop(tasks);

    let task_dir = format!("{}/{}", SCRIPTS_DIR, task_id);
    if !std::path::Path::new(&task_dir).exists() {
        if let Err(e) = std::fs::create_dir_all(&task_dir) {
            return HttpResponse::InternalServerError().body(format!("Failed to create task dir: {}", e));
        }
    }

    while let Ok(Some(mut field)) = payload.try_next().await {
        let content_disposition = field.content_disposition();
        let filename = if let Some(cd) = content_disposition {
            if let Some(fname) = cd.get_filename() {
                fname
            } else {
                return HttpResponse::BadRequest().body("No filename provided");
            }
        } else {
            return HttpResponse::BadRequest().body("No content disposition");
        };
        let filepath = format!("{}/{}", task_dir, sanitize_filename::sanitize(&filename));
        let mut f = match std::fs::File::create(&filepath) {
            Ok(file) => file,
            Err(e) => return HttpResponse::InternalServerError().body(format!("Failed to create file: {}", e)),
        };
        while let Some(chunk) = field.try_next().await.unwrap_or(None) {
            if let Err(e) = std::io::Write::write_all(&mut f, &chunk) {
                return HttpResponse::InternalServerError().body(format!("Failed to write file: {}", e));
            }
        }
    }
    HttpResponse::Ok().body("File uploaded")
}

// =========================
// Imports
// =========================
use actix_files::Files;
use actix_web::{
    delete, get, post, put, web, App, HttpResponse, HttpServer, Responder, Error, HttpRequest
};
use actix_ws::{Message, CloseReason};
use futures_util::stream::StreamExt;
use chrono::{DateTime, Local, Timelike, Duration, Datelike};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap, fs, io::{BufRead, BufReader}, path::Path, process::{Command, Stdio}, sync::{Arc, Mutex}
};
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver, unbounded_channel};
use std::time::Duration as StdDuration;
use uuid::Uuid;
use threadpool::ThreadPool;
use std::env;
use log::{info, error, warn};

// =========================
// WebSocket Helpers (async broadcast)
// =========================
fn broadcast_task_status(data: &web::Data<AppState>, task: &ScriptTask) {
    let msg = serde_json::json!({
        "type": "status",
        "task_id": task.id,
        "status": &task.status,
        "last_run": task.last_run,
        "schedule": &task.schedule,
        "output": &task.output,
    }).to_string();
    let clients = data.ws_clients.lock().unwrap();
    for sender in clients.iter() {
        let _ = sender.send(msg.clone());
    }
}

fn broadcast_task_output(data: &web::Data<AppState>, task_id: &str, line: &str) {
    let msg = serde_json::json!({
        "type": "output",
        "task_id": task_id,
        "line": line,
    }).to_string();
    let clients = data.ws_clients.lock().unwrap();
    for sender in clients.iter() {
        let _ = sender.send(msg.clone());
    }
}

// =========================
// WebSocket Handler (actix-ws)
// =========================

async fn ws_index(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, stream)?;

    // Register this client for broadcast using async channel
    let (tx, mut rx): (UnboundedSender<String>, UnboundedReceiver<String>) = unbounded_channel();
    data.ws_clients.lock().unwrap().push(tx.clone());

    // Spawn a task to forward broadcast messages to this websocket session
    let mut session_clone = session.clone();
    actix_web::rt::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let _ = session_clone.text(msg).await;
        }
    });

    // Handle incoming websocket messages
    let data_clone = data.clone();
    actix_web::rt::spawn(async move {
        while let Some(Ok(msg)) = msg_stream.next().await {
            match msg {
                Message::Ping(bytes) => {
                    let _ = session.pong(&bytes).await;
                }
                Message::Text(_) => {
                    // Ignore text messages from client
                }
                Message::Binary(_) => {}
                Message::Close(_) => {
                    // Remove sender from ws_clients
                    let mut clients = data_clone.ws_clients.lock().unwrap();
                    clients.retain(|s| !s.same_channel(&tx));
                    let _ = session.close(Some(CloseReason { code: actix_ws::CloseCode::Normal, description: None })).await;
                    break;
                }
                _ => {}
            }
        }
    });

    Ok(response)
}

// Task scheduling types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum ScheduleType {
    Once,
    Interval(u64), // seconds
    Cron {
        minute: Option<u32>,   // 0-59, None = any
        hour: Option<u32>,     // 0-23, None = any
        day: Option<u32>,      // 1-31, None = any
        month: Option<u32>,    // 1-12, None = any
        weekday: Option<u32>,  // 0-6 (Sun=0), None = any
    },
}

// Task status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

// Task structure
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScriptTask {
    id: String,
    name: String,
    description: String,
    script_content: String,
    schedule: ScheduleType,
    last_run: Option<DateTime<Local>>,
    status: TaskStatus,
    output: Option<String>,
    enabled: bool,
}

// App state
struct AppState {
    tasks: Mutex<HashMap<String, ScriptTask>>,
    ws_clients: Mutex<Vec<UnboundedSender<String>>>,
    pool: ThreadPool,
}

// Create task request
#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    name: String,
    description: String,
    script_content: String,
    schedule: ScheduleType,
}

// Update task request
#[derive(Debug, Deserialize)]
struct UpdateTaskRequest {
    name: Option<String>,
    description: Option<String>,
    script_content: Option<String>,
    schedule: Option<ScheduleType>,
}

const TASKS_FILE: &str = "data/tasks.json";
const SCRIPTS_DIR: &str = "data/scripts";

// API endpoints
#[post("/api/tasks")]
async fn create_task(
    data: web::Data<AppState>,
    task_req: web::Json<CreateTaskRequest>,
) -> impl Responder {
    let id = Uuid::new_v4().to_string();


    let new_task = ScriptTask {
        id: id.clone(),
        name: task_req.name.clone(),
        description: task_req.description.clone(),
        script_content: task_req.script_content.clone(),
        schedule: task_req.schedule.clone(),
        last_run: None,
        status: TaskStatus::Pending,
        output: None,
        enabled: true, // default enabled
    };

    data.tasks.lock().unwrap().insert(id.clone(), new_task);

    // Save the new task to file
    save_tasks_to_file(&data.tasks.lock().unwrap());

    HttpResponse::Created().json(id)
}

#[get("/api/tasks")]
async fn get_tasks(data: web::Data<AppState>) -> impl Responder {
    let tasks = match data.tasks.lock() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to lock tasks: {e}");
            return HttpResponse::InternalServerError().body("Lock poisoned");
        }
    };
    let task_list: Vec<ScriptTask> = tasks.values().cloned().collect();
    HttpResponse::Ok().json(task_list)
}

#[get("/api/tasks/{id}")]
async fn get_task(data: web::Data<AppState>, id: web::Path<String>) -> impl Responder {
    let tasks = data.tasks.lock().unwrap();
    match tasks.get(&*id) {
        Some(task) => HttpResponse::Ok().json(task),
        None => HttpResponse::NotFound().body("Task not found"),
    }
}

#[put("/api/tasks/{id}")]
async fn update_task(
    data: web::Data<AppState>,
    id: web::Path<String>,
    update_req: web::Json<UpdateTaskRequest>,
) -> impl Responder {
    let mut tasks = data.tasks.lock().unwrap();
    let task_id = id.into_inner();

    if let Some(task) = tasks.get_mut(&task_id) {
        if let Some(name) = &update_req.name {
            task.name = name.clone();
        }
        if let Some(description) = &update_req.description {
            task.description = description.clone();
        }
        if let Some(content) = &update_req.script_content {
            task.script_content = content.clone();
        }
        if let Some(schedule) = &update_req.schedule {
            task.schedule = schedule.clone();
        }

        let updated_task = task.clone();
        let _ = task; // Ends the mutable borrow explicitly

        // Save the updated task
        save_tasks_to_file(&tasks);

        HttpResponse::Ok().json(updated_task)
    } else {
        HttpResponse::NotFound().body("Task not found")
    }
}

// Enable/disable task
#[derive(Debug, Deserialize)]
struct EnableTaskRequest {
    enabled: bool,
}

#[actix_web::patch("/api/tasks/{id}/enabled")]
async fn set_task_enabled(
    data: web::Data<AppState>,
    id: web::Path<String>,
    req: web::Json<EnableTaskRequest>,
) -> impl Responder {
    let mut tasks = data.tasks.lock().unwrap();
    let task_id = id.into_inner();
    if let Some(task) = tasks.get_mut(&task_id) {
        task.enabled = req.enabled;
        let response_task = task.clone();
        save_tasks_to_file(&tasks);
        HttpResponse::Ok().json(response_task)
    } else {
        HttpResponse::NotFound().body("Task not found")
    }
}
#[delete("/api/tasks/{id}")]
async fn delete_task(data: web::Data<AppState>, id: web::Path<String>) -> impl Responder {
    let mut tasks = data.tasks.lock().unwrap();
    let task_id = id.into_inner();

    // Remove script folder if it exists
    let script_dir = format!("{}/{}", SCRIPTS_DIR, task_id);
    if Path::new(&script_dir).exists() {
        if let Err(e) = fs::remove_dir_all(&script_dir) {
            eprintln!("Failed to delete script directory: {}", e);
        }
    }

    if tasks.remove(&task_id).is_some() {
        save_tasks_to_file(&tasks);
        HttpResponse::Ok().body("Task deleted")
    } else {
        HttpResponse::NotFound().body("Task not found")
    }
}
// Run a task immediately
#[post("/api/tasks/{id}/run")]
async fn run_task(data: web::Data<AppState>, id: web::Path<String>) -> impl Responder {
    let task_id = id.into_inner();
    let mut tasks = match data.tasks.lock() {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to lock tasks: {e}");
            return HttpResponse::InternalServerError().body("Lock poisoned");
        }
    };

    if let Some(task) = tasks.get_mut(&task_id) {
        if task.status == TaskStatus::Running {
            return HttpResponse::Conflict().body("Task is already running");
        }

        task.status = TaskStatus::Running;
        task.last_run = Some(Local::now());
        broadcast_task_status(&data, task);

        let task_content = task.script_content.clone();
        let task_id_clone = task_id.clone();
        let data_clone = data.clone();
        let pool = data.pool.clone();

        pool.execute(move || {
            let output = run_python_script_stream(&task_content, &task_id_clone, &data_clone);
            let mut tasks = match data_clone.tasks.lock() {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to lock tasks: {e}");
                    return;
                }
            };
            if let Some(task) = tasks.get_mut(&task_id_clone) {
                task.output = Some(output.clone());
                let new_status = if output.contains("ERROR") {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Completed
                };
                task.status = new_status.clone();
                broadcast_task_status(&data_clone, task);
                save_tasks_to_file(&tasks);
            }
        });

        HttpResponse::Accepted().body("Task is running")
    } else {
        HttpResponse::NotFound().body("Task not found")
    }
}

fn save_tasks_to_file(tasks: &HashMap<String, ScriptTask>) {
    match serde_json::to_string(tasks) {
        Ok(json) => {
            if let Err(e) = fs::write(TASKS_FILE, json) {
                error!("Failed to write tasks to file: {}", e);
            }
        }
        Err(e) => error!("Failed to serialize tasks: {}", e),
    }
}

fn load_tasks_from_file() -> HashMap<String, ScriptTask> {
    match fs::read_to_string(TASKS_FILE) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(tasks) => tasks,
            Err(e) => {
                error!("Failed to parse tasks file: {}", e);
                HashMap::new()
            }
        },
        Err(e) => {
            warn!("Could not read tasks file: {}", e);
            HashMap::new()
        }
    }
}

// Run Python script and stream output line-by-line
fn run_python_script_stream(
    content: &str,
    task_id: &str,
    data: &web::Data<AppState>,
) -> String {
    let task_dir = format!("{}/{}", SCRIPTS_DIR, task_id);
    if !Path::new(&task_dir).exists() {
        if let Err(e) = fs::create_dir_all(&task_dir) {
            error!("Failed to create task script directory: {}", e);
            return format!("ERROR: Failed to create script directory: {}", e);
        }
    }
    let script_path = format!("{}/{}.py", task_dir, task_id);
    if let Err(e) = fs::write(&script_path, content) {
        error!("Failed to write script file: {}", e);
        return format!("ERROR: Failed to write script file: {}", e);
    }

    // Step 1: Detect imports
    let imports = extract_imports(content);

    // Step 2: Install missing packages
    for module in imports {
        if !is_module_installed(&module) {
            if let Err(e) = install_python_package(&module) {
                warn!("Failed to install Python package {}: {}", module, e);
            }
        }
    }

    // Step 3: Run script and stream output
    let mut output_accum = String::new();
    let python_bin = if Path::new("venv/bin/python3").exists() {
        "venv/bin/python3"
    } else {
        "python3"
    };
    let mut cmd = match Command::new(python_bin)
        .arg("-u")
        .arg(&script_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn() {
        Ok(child) => child,
        Err(e) => {
            error!("Failed to spawn python process: {}", e);
            return format!("EXECUTION FAILED: {}", e);
        }
    };

    // Helper to update in-memory output for the running task
    let update_task_output = |line: &str| {
        if let Ok(mut tasks) = data.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                if let Some(ref mut out) = task.output {
                    out.push_str(line);
                    out.push('\n');
                } else {
                    task.output = Some(format!("{}\n", line));
                }
            }
        }
    };

    // Stream stdout
    if let Some(stdout) = cmd.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    output_accum.push_str(&l);
                    output_accum.push('\n');
                    broadcast_task_output(data, task_id, &l);
                    update_task_output(&l);
                }
                Err(_) => break,
            }
        }
    }
    // Stream stderr
    if let Some(stderr) = cmd.stderr.take() {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    output_accum.push_str(&l);
                    output_accum.push('\n');
                    broadcast_task_output(data, task_id, &l);
                    update_task_output(&l);
                }
                Err(_) => break,
            }
        }
    }
    let status = match cmd.wait() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to wait for python process: {}", e);
            return format!("ERROR: Failed to wait for python process: {}", e);
        }
    };
    if !status.success() {
        output_accum = format!("ERROR: {}", output_accum);
    }
    output_accum
}

fn extract_imports(script: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ") {
            let parts: Vec<&str> = trimmed.strip_prefix("import ").unwrap().split(',').collect();
            for part in parts {
                let name = part.trim().split_whitespace().next().unwrap_or("").to_string();
                if !name.is_empty() {
                    modules.push(name);
                }
            }
        } else if trimmed.starts_with("from ") {
            if let Some(part) = trimmed.strip_prefix("from ") {
                let name = part.split_whitespace().next().unwrap_or("").to_string();
                if !name.is_empty() {
                    modules.push(name);
                }
            }
        }
    }
    modules.sort();
    modules.dedup();
    modules
}

fn is_module_installed(module: &str) -> bool {
    let python_bin = "venv/bin/python"; // adjust path if needed
    Command::new(python_bin)
        .args(["-c", &format!("import {}", module)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn install_python_package(module: &str) -> std::io::Result<()> {
    let python_bin = "venv/bin/python"; // adjust path if needed

    match Command::new(python_bin)
        .args(["-m", "pip", "install", module])
        .status()
    {
        Ok(status) => {
            println!("Package {} installation status: {}", module, status);
            if !status.success() {
                eprintln!("Failed to install Python package: {}", module);
            }
        }
        Err(e) => {
            eprintln!("Failed to run {}: {}", python_bin, e);
        }
    }

    Ok(())
}

// Scheduler thread
fn start_scheduler(data: Arc<web::Data<AppState>>) {
    let pool = data.pool.clone();
    std::thread::spawn(move || {
        loop {
            let now = Local::now();
            let mut tasks = match data.tasks.lock() {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to lock tasks: {e}");
                    std::thread::sleep(StdDuration::from_secs(1));
                    continue;
                }
            };

            for (id, task) in tasks.iter_mut() {
                if !task.enabled {
                    continue;
                }
                let should_run = match &task.schedule {
                    ScheduleType::Once => task.last_run.is_none(),
                    ScheduleType::Interval(secs) => task
                        .last_run
                        .map_or(true, |t| Local::now().signed_duration_since(t) >= Duration::seconds(*secs as i64)),
                    ScheduleType::Cron { minute, hour, day, month, weekday } => {
                        let m = now.minute();
                        let h = now.hour();
                        let d = now.day();
                        let mo = now.month();
                        let wd = now.weekday().num_days_from_sunday();

                        let minute_match = match minute { Some(val) => m == *val, None => true };
                        let hour_match = match hour { Some(val) => h == *val, None => true };
                        let day_match = match day { Some(val) => d == *val, None => true };
                        let month_match = match month { Some(val) => mo == *val, None => true };
                        let weekday_match = match weekday { Some(val) => wd == *val, None => true };

                        let scheduled_time = minute_match && hour_match && day_match && month_match && weekday_match;
                        let never_run = task.last_run.is_none();
                        let new_period = task.last_run.map_or(false, |t| {
                            // Only run once per scheduled time
                            let last = t;
                            last.minute() != m || last.hour() != h || last.day() != d || last.month() != mo || last.weekday().num_days_from_sunday() != wd
                        });
                        scheduled_time && (never_run || new_period)
                    }
                };

                if should_run && task.status != TaskStatus::Running {
                    task.status = TaskStatus::Running;
                    task.last_run = Some(Local::now());
                    task.output = None;
                    broadcast_task_status(&data, task);
                    let task_content = task.script_content.clone();
                    let task_id = id.clone();
                    let data_clone = data.clone();
                    let pool = pool.clone();
                    pool.execute(move || {
                        let output = run_python_script_stream(&task_content, &task_id, &data_clone);
                        let mut tasks = match data_clone.tasks.lock() {
                            Ok(t) => t,
                            Err(e) => {
                                error!("Failed to lock tasks: {e}");
                                return;
                            }
                        };
                        if let Some(task) = tasks.get_mut(&task_id) {
                            let new_status = if output.contains("ERROR") {
                                TaskStatus::Failed
                            } else {
                                TaskStatus::Completed
                            };
                            task.status = new_status.clone();
                            task.output = Some(output);
                            broadcast_task_status(&data_clone, task);
                            save_tasks_to_file(&tasks);
                        }
                    });
                }
            }
            drop(tasks);
            std::thread::sleep(StdDuration::from_secs(1));
        }
    });
}

// Serve HTML interface
async fn index() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(include_str!("index.html"))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    // Create scripts directory if it doesn't exist
    if !Path::new(SCRIPTS_DIR).exists() {
        if let Err(e) = fs::create_dir_all(SCRIPTS_DIR) {
            eprintln!("Failed to create scripts directory: {}", e);
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Failed to create scripts directory"));
        }
    }

    // load a venv if it exists
    if !Path::new("venv").exists() {
        if let Err(e) = Command::new("python3")
            .args(["-m", "venv", "venv"])
            .status()
        {            eprintln!("Failed to create virtual environment: {}", e);
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Failed to create virtual environment"));
        }
    }     

    // Load tasks and reset any stuck in Running state
    let mut loaded_tasks = load_tasks_from_file();
    for task in loaded_tasks.values_mut() {
        if task.status == TaskStatus::Running {
            task.status = TaskStatus::Pending;
        }
    }
    // Initialize app state with thread pool
    let pool = ThreadPool::new(8); // 8 threads, adjust as needed
    let app_state = web::Data::new(AppState {
        tasks: Mutex::new(loaded_tasks),
        ws_clients: Mutex::new(Vec::new()),
        pool,
    });

    // Start scheduler thread
    let scheduler_state = Arc::new(app_state.clone());
    start_scheduler(scheduler_state);

    // Configurable bind address
    let bind_addr = env::var("PYSCMAN_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    info!("Starting server at http://{}", bind_addr);

    // Start web server
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .service(create_task)
            .service(get_tasks)
            .service(get_task)
            .service(update_task)
            .service(delete_task)
            .service(run_task)
            .service(set_task_enabled)
            .service(upload_file)
            .route("/", web::get().to(index))
            .route("/ws/", web::get().to(ws_index))
            .service(Files::new("/", ".").index_file("index.html"))
    })
    .bind(bind_addr)?
    .run()
    .await
}