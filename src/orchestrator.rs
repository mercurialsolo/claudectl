use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// A task definition from the tasks file.
#[derive(Debug, Deserialize, Clone)]
pub struct TaskDef {
    pub name: String,
    pub cwd: Option<String>,
    pub prompt: String,
    #[allow(dead_code)]
    pub budget: Option<f64>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub resume: Option<String>,
}

/// Task file containing a list of tasks.
#[derive(Debug, Deserialize)]
pub struct TaskFile {
    pub tasks: Vec<TaskDef>,
    #[allow(dead_code)]
    pub budget: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
enum TaskState {
    Pending,
    Running,
    Completed,
    Failed(String),
}

struct TaskRun {
    def: TaskDef,
    state: TaskState,
    pid: Option<u32>,
    start_time: Option<Instant>,
}

/// Load tasks from a JSON file.
pub fn load_tasks(path: &str) -> io::Result<TaskFile> {
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Run tasks with dependency resolution and parallel execution.
pub fn run_tasks(task_file: TaskFile, parallel: bool) -> io::Result<()> {
    let mut tasks: Vec<TaskRun> = task_file
        .tasks
        .into_iter()
        .map(|def| TaskRun {
            def,
            state: TaskState::Pending,
            pid: None,
            start_time: None,
        })
        .collect();

    // Validate dependencies exist
    let names: Vec<String> = tasks.iter().map(|t| t.def.name.clone()).collect();
    for task in &tasks {
        for dep in &task.def.depends_on {
            if !names.contains(dep) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Task '{}' depends on '{}' which doesn't exist",
                        task.def.name, dep
                    ),
                ));
            }
        }
    }

    // Check for cycles (simple: if task count > 0, we must make progress each iteration)
    let total = tasks.len();
    println!("Running {total} tasks...");
    println!();

    let poll_interval = Duration::from_secs(2);

    loop {
        let completed: Vec<String> = tasks
            .iter()
            .filter(|t| t.state == TaskState::Completed)
            .map(|t| t.def.name.clone())
            .collect();

        let failed: Vec<String> = tasks
            .iter()
            .filter(|t| matches!(t.state, TaskState::Failed(_)))
            .map(|t| t.def.name.clone())
            .collect();

        let running_count = tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count();
        let pending_count = tasks
            .iter()
            .filter(|t| t.state == TaskState::Pending)
            .count();

        // Print status
        print_status(&tasks);

        // Check if all done
        if completed.len() + failed.len() == total {
            println!();
            if failed.is_empty() {
                println!("All {total} tasks completed successfully.");
            } else {
                println!("{} completed, {} failed.", completed.len(), failed.len());
                for task in &tasks {
                    if let TaskState::Failed(ref msg) = task.state {
                        println!("  FAILED: {} — {}", task.def.name, msg);
                    }
                }
            }

            // Fire notification
            #[cfg(target_os = "macos")]
            {
                let msg = if failed.is_empty() {
                    format!("All {total} tasks completed")
                } else {
                    format!("{} completed, {} failed", completed.len(), failed.len())
                };
                let _ = Command::new("osascript")
                    .args([
                        "-e",
                        &format!("display notification \"{msg}\" with title \"claudectl run\""),
                    ])
                    .spawn();
            }

            return if failed.is_empty() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("{} tasks failed", failed.len()),
                ))
            };
        }

        // Launch eligible tasks
        for task in &mut tasks {
            if task.state != TaskState::Pending {
                continue;
            }

            // Check dependencies
            let deps_met = task
                .def
                .depends_on
                .iter()
                .all(|dep| completed.contains(dep));
            let deps_failed = task.def.depends_on.iter().any(|dep| failed.contains(dep));

            if deps_failed {
                task.state = TaskState::Failed("dependency failed".into());
                continue;
            }

            if !deps_met {
                continue;
            }

            // Don't launch more than one at a time unless parallel
            if !parallel && running_count > 0 {
                break;
            }

            // Launch the task
            match launch_claude_session(&task.def) {
                Ok(pid) => {
                    println!("  Started: {} (PID {})", task.def.name, pid);
                    task.pid = Some(pid);
                    task.state = TaskState::Running;
                    task.start_time = Some(Instant::now());
                }
                Err(e) => {
                    task.state = TaskState::Failed(format!("launch error: {e}"));
                }
            }
        }

        // Check running tasks
        for task in &mut tasks {
            if task.state != TaskState::Running {
                continue;
            }
            if let Some(pid) = task.pid {
                if !pid_alive(pid) {
                    let elapsed = task.start_time.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                    println!("  Finished: {} ({}s)", task.def.name, elapsed);
                    task.state = TaskState::Completed;
                }
            }
        }

        // Check for deadlock (all pending but nothing can run)
        if pending_count > 0 && running_count == 0 {
            let launchable = tasks.iter().any(|t| {
                t.state == TaskState::Pending
                    && t.def.depends_on.iter().all(|dep| completed.contains(dep))
                    && !t.def.depends_on.iter().any(|dep| failed.contains(dep))
            });
            if !launchable {
                // All remaining tasks have unmet dependencies
                for task in &mut tasks {
                    if task.state == TaskState::Pending {
                        task.state = TaskState::Failed("unresolvable dependency".into());
                    }
                }
                continue;
            }
        }

        std::thread::sleep(poll_interval);
    }
}

fn launch_claude_session(task: &TaskDef) -> io::Result<u32> {
    let cwd = task.cwd.as_deref().unwrap_or(".");

    let mut args = vec!["--print".to_string()];

    if let Some(ref resume) = task.resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }

    args.push(task.prompt.clone());

    let child = Command::new("claude")
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(child.id())
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn print_status(tasks: &[TaskRun]) {
    let total = tasks.len();
    let completed = tasks
        .iter()
        .filter(|t| t.state == TaskState::Completed)
        .count();
    let running = tasks
        .iter()
        .filter(|t| t.state == TaskState::Running)
        .count();
    let failed = tasks
        .iter()
        .filter(|t| matches!(t.state, TaskState::Failed(_)))
        .count();
    let pending = tasks
        .iter()
        .filter(|t| t.state == TaskState::Pending)
        .count();

    eprint!("\r  [{completed}/{total}] {running} running, {pending} pending, {failed} failed    ");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tasks_json() {
        let json = r#"{
            "tasks": [
                {
                    "name": "task1",
                    "prompt": "Do something",
                    "cwd": "./src"
                },
                {
                    "name": "task2",
                    "prompt": "Do something else",
                    "depends_on": ["task1"],
                    "budget": 2.0
                }
            ],
            "budget": 10.0
        }"#;

        let task_file: TaskFile = serde_json::from_str(json).unwrap();
        assert_eq!(task_file.tasks.len(), 2);
        assert_eq!(task_file.tasks[0].name, "task1");
        assert_eq!(task_file.tasks[0].cwd, Some("./src".into()));
        assert_eq!(task_file.tasks[1].depends_on, vec!["task1"]);
        assert_eq!(task_file.tasks[1].budget, Some(2.0));
        assert_eq!(task_file.budget, Some(10.0));
    }

    #[test]
    fn test_dependency_validation() {
        let task_file = TaskFile {
            tasks: vec![TaskDef {
                name: "task1".into(),
                prompt: "test".into(),
                cwd: None,
                budget: None,
                depends_on: vec!["nonexistent".into()],
                resume: None,
            }],
            budget: None,
        };

        let result = run_tasks(task_file, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }
}
