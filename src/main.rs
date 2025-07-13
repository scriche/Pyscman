use actix_files::Files;
use actix_web::{
    delete, get, post, put, web, App, HttpResponse, HttpServer, Responder,
};
use chrono::{DateTime, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};
use uuid::Uuid;

// Task scheduling types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum ScheduleType {
    Once,
    Interval(u64), // seconds
    Daily { hour: u32, minute: u32 },
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
    script_content: String,
    schedule: ScheduleType,
    last_run: Option<SystemTime>,
    status: TaskStatus,
    output: Option<String>,
}

// App state
struct AppState {
    tasks: Arc<Mutex<HashMap<String, ScriptTask>>>,
}

// Create task request
#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    name: String,
    script_content: String,
    schedule: ScheduleType,
}

// Update task request
#[derive(Debug, Deserialize)]
struct UpdateTaskRequest {
    name: Option<String>,
    script_content: Option<String>,
    schedule: Option<ScheduleType>,
}

// API endpoints
#[post("/tasks")]
async fn create_task(
    data: web::Data<AppState>,
    task_req: web::Json<CreateTaskRequest>,
) -> impl Responder {
    let id = Uuid::new_v4().to_string();
    let new_task = ScriptTask {
        id: id.clone(),
        name: task_req.name.clone(),
        script_content: task_req.script_content.clone(),
        schedule: task_req.schedule.clone(),
        last_run: None,
        status: TaskStatus::Pending,
        output: None,
    };

    data.tasks.lock().unwrap().insert(id.clone(), new_task);

    HttpResponse::Created().json(id)
}

#[get("/tasks")]
async fn get_tasks(data: web::Data<AppState>) -> impl Responder {
    let tasks = data.tasks.lock().unwrap();
    let task_list: Vec<ScriptTask> = tasks.values().cloned().collect();
    HttpResponse::Ok().json(task_list)
}

#[get("/tasks/{id}")]
async fn get_task(data: web::Data<AppState>, id: web::Path<String>) -> impl Responder {
    let tasks = data.tasks.lock().unwrap();
    match tasks.get(&*id) {
        Some(task) => HttpResponse::Ok().json(task),
        None => HttpResponse::NotFound().body("Task not found"),
    }
}

#[put("/tasks/{id}")]
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
        if let Some(content) = &update_req.script_content {
            task.script_content = content.clone();
        }
        if let Some(schedule) = &update_req.schedule {
            task.schedule = schedule.clone();
        }
        HttpResponse::Ok().json(task.clone())
    } else {
        HttpResponse::NotFound().body("Task not found")
    }
}

#[delete("/tasks/{id}")]
async fn delete_task(data: web::Data<AppState>, id: web::Path<String>) -> impl Responder {
    let mut tasks = data.tasks.lock().unwrap();
    let task_id = id.into_inner();
    
    if tasks.remove(&task_id).is_some() {
        HttpResponse::Ok().body("Task deleted")
    } else {
        HttpResponse::NotFound().body("Task not found")
    }
}

// Run Python script and capture output
fn run_python_script(content: &str, task_id: &str) -> String {
    // Create a temporary directory for scripts
    let scripts_dir = "scripts";
    if !Path::new(scripts_dir).exists() {
        fs::create_dir_all(scripts_dir).expect("Failed to create scripts directory");
    }
    
    let script_path = format!("{}/{}.py", scripts_dir, task_id);
    fs::write(&script_path, content).expect("Failed to write script file");
    
    match Command::new("python").arg(&script_path).output() {
        Ok(output) => {
            if output.status.success() {
                String::from_utf8_lossy(&output.stdout).to_string()
            } else {
                format!(
                    "ERROR: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            }
        }
        Err(e) => format!("EXECUTION FAILED: {}", e),
    }
}

// Scheduler thread
fn start_scheduler(data: Arc<AppState>) {
    thread::spawn(move || {
        loop {
            let now = Local::now();
            let mut tasks = data.tasks.lock().unwrap();

            for (id, task) in tasks.iter_mut() {
                let should_run = match task.schedule {
                    ScheduleType::Once => task.last_run.is_none(),
                    ScheduleType::Interval(secs) => task
                        .last_run
                        .map_or(true, |t| t.elapsed().unwrap() >= Duration::from_secs(secs)),
                    ScheduleType::Daily { hour, minute } => {
                        let scheduled_time = (now.hour() == hour && now.minute() == minute);
                        let never_run = task.last_run.is_none();
                        let new_day = task.last_run.map_or(false, |t| {
                            let last_run: DateTime<Local> = t.into();
                            last_run.date_naive() < now.date_naive()
                        });

                        scheduled_time && (never_run || new_day)
                    }
                };

                if should_run && task.status != TaskStatus::Running {
                    task.status = TaskStatus::Running;
                    let task_content = task.script_content.clone();
                    let task_id = id.clone();
                    let data_clone = data.clone();

                    // Run script in separate thread
                    thread::spawn(move || {
                        let output = run_python_script(&task_content, &task_id);
                        
                        // Update task status and output
                        let mut tasks = data_clone.tasks.lock().unwrap();
                        if let Some(task) = tasks.get_mut(&task_id) {
                            task.last_run = Some(SystemTime::now());
                            task.status = if output.contains("ERROR") {
                                TaskStatus::Failed
                            } else {
                                TaskStatus::Completed
                            };
                            task.output = Some(output);
                        }
                    });
                }
            }

            // Release lock before sleeping
            drop(tasks);
            thread::sleep(Duration::from_secs(1));
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
    // Create scripts directory if it doesn't exist
    if !Path::new("scripts").exists() {
        fs::create_dir("scripts").expect("Failed to create scripts directory");
    }
    
    // Initialize app state
    let app_state = Arc::new(AppState {
        tasks: Arc::new(Mutex::new(HashMap::new())),
    });

    // Start scheduler thread
    start_scheduler(app_state.clone());

    println!("Starting server at http://localhost:8080");

    // Start web server
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .service(create_task)
            .service(get_tasks)
            .service(get_task)
            .service(update_task)
            .service(delete_task)
            .route("/", web::get().to(index))
            .service(Files::new("/static", "static").show_files_listing())
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}