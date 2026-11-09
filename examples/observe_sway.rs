#[path = "support/observation.rs"]
mod observation;

use std::env;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use observation::Observer;
use swayipc::Event;

fn main() -> ExitCode {
    match observe() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("observe_sway: {error}");
            ExitCode::FAILURE
        }
    }
}

fn observe() -> Result<(), String> {
    let socket = env::var_os("SWAYSOCK")
        .filter(|path| !path.is_empty())
        .ok_or("connect: SWAYSOCK is unset or empty")?;
    let connect =
        || UnixStream::connect(&socket).map_err(|error| format!("connect to SWAYSOCK: {error}"));

    let mut observer = Observer::subscribe(connect()?, Instant::now() + Duration::from_secs(2))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = observer.snapshot(connect()?, deadline)?;
        println!(
            "Workspace id={}: {} tree nodes, {} events through snapshot fence",
            snapshot.target,
            snapshot.tree.iter().count(),
            snapshot.events.len()
        );
        for event in snapshot.events {
            print_event(event);
        }

        let poll = deadline.min(Instant::now() + Duration::from_millis(250));
        while let Some(event) = observer.next_until(poll)? {
            print_event(event);
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
    }
}

fn print_event(event: Event) {
    match event {
        Event::Workspace(event) => println!("workspace::{:?}", event.change),
        Event::Window(event) => {
            println!("window::{:?} id={}", event.change, event.container.id);
        }
        _ => {}
    }
}
