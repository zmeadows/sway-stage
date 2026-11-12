#[path = "../src/observation.rs"]
#[allow(
    dead_code,
    reason = "the example uses only the observer and SWAYSOCK helpers"
)]
mod observation;

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
    let socket = observation::sway_socket()?;
    let connect = || observation::connect(&socket);

    let mut observer = Observer::subscribe(connect()?, Instant::now() + Duration::from_secs(2))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = observer.snapshot(connect()?, deadline)?;
        println!(
            "Workspace id={}: {} tree nodes, {} events through snapshot fence",
            snapshot.target_workspace_id,
            snapshot.tree.iter().count(),
            snapshot.events.len()
        );
        for event in snapshot.events {
            print_event(event);
        }

        let event_poll_deadline = deadline.min(Instant::now() + Duration::from_millis(250));
        while let Some(event) = observer.next_until(event_poll_deadline)? {
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
