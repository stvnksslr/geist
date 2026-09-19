//! The single-instance pipe end to end on a real named pipe: bind, serve,
//! forward a request, answer it from the "UI" side, and refuse a second
//! server under the same name. Uses a throwaway pipe name via
//! `$GIEST_IPC_PIPE`, so it never touches a running giest.

#![cfg(windows)]

use giest::ipc::{self, Request, Response, SendError};

#[test]
fn a_request_round_trips_over_a_real_named_pipe() {
    // SAFETY (edition 2024 `set_var`): this test binary has one test, so no
    // other thread reads the environment concurrently.
    unsafe {
        std::env::set_var("GIEST_IPC_PIPE", format!("giest-test-{}", std::process::id()));
    }
    let name = ipc::pipe_name();

    // Nobody listening yet: the client must say so, which is what makes a
    // launch fall back to starting its own instance.
    assert!(matches!(ipc::send(&name, &Request::List), Err(SendError::NoServer)));

    assert!(ipc::start_server(), "first bind claims the name");
    // FILE_FLAG_FIRST_PIPE_INSTANCE: a second server can't slip in.
    assert!(!ipc::start_server(), "second bind must fail");

    let rx = ipc::take_receiver().expect("the server's channel");
    let ui = std::thread::spawn(move || {
        for _ in 0..2 {
            let inc = rx.recv().unwrap();
            let reply = match &inc.request {
                Request::NewTab { cwd, .. } => Response {
                    window: Some(0),
                    tab: Some(cwd.as_deref().map_or(1, |c| c.len() as u64)),
                    ..Response::ok()
                },
                _ => Response::err("unexpected"),
            };
            inc.respond(reply);
        }
    });

    let r = ipc::send(
        &name,
        &Request::NewTab {
            cwd: Some("abc".into()),
            command: None,
            window: None,
        },
    )
    .unwrap();
    assert!(r.ok);
    assert_eq!(r.tab, Some(3));

    let r = ipc::send(&name, &Request::List).unwrap();
    assert!(!r.ok);
    assert_eq!(r.error.as_deref(), Some("unexpected"));
    ui.join().unwrap();
}
