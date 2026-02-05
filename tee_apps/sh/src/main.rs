use std::process::{Child, Command, Stdio};

fn spawn_app(path: &str) -> Option<Child> {
    match Command::new(path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => {
            println!("tee_init: spawned {path}");
            Some(child)
        }
        Err(err) => {
            println!("tee_init: failed to spawn {path}: {err}");
            None
        }
    }
}

fn main() {
    println!("tee_init: starting foreground init");

    let apps_env = option_env!("TEE_INIT_APPS").unwrap_or("");
    let apps: Vec<String> = apps_env
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let children: Vec<(String, Child)> = apps
        .iter()
        .filter_map(|path| spawn_app(path).map(|child| (path.to_string(), child)))
        .collect();

    if children.is_empty() {
        println!("tee_init: no apps to spawn, exiting");
        return;
    }

    for (path, mut child) in children {
        match child.wait() {
            Ok(status) => {
                println!("tee_init: {path} exited with {status}");
            }
            Err(err) => {
                println!("tee_init: failed to wait {path}: {err}");
            }
        }
    }
}
