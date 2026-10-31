//! Actual CLI processes, private files and credential-checked Unix HTTP.
//! Peer metadata is synthetic; no live Tailscale or remote desktop is claimed.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn metadata_command(body: String, args: &[&str]) -> (Output, usize) {
    let socket = path("saved-peer-api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let serving = thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(4);
        let mut calls = 0;
        while !stopped.load(Ordering::Acquire) && Instant::now() < until {
            let (mut stream, _) = match listener.accept() {
                Ok(stream) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("fixture accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while request.len() < 2048 && !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) != 1 {
                    break;
                }
                request.push(byte[0]);
            }
            if request.is_empty() {
                continue;
            }
            assert!(request.starts_with(b"GET /localapi/v0/status?peers=true "));
            calls += 1;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        calls
    });
    let mut command = command(args);
    command.args(["--socket", socket.to_str().unwrap(), "--json"]);
    let output = wait(command.spawn().unwrap());
    stop.store(true, Ordering::Release);
    (output, serving.join().unwrap())
}
#[test]
fn saved_host_cli_persists_selection_and_refuses_a_renamed_peer() {
    let directory = path("saved-host-state");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = directory.join("hosts.json");
    let file = file.to_str().unwrap();
    let (saved, calls) = metadata_command(
        fixture(),
        &[
            "hosts",
            "save",
            "office",
            "n-peer",
            "--port",
            "9443",
            "--hosts-file",
            file,
        ],
    );
    let is_root = asupersync::net::unix::UnixStream::pair()
        .unwrap()
        .0
        .peer_cred()
        .unwrap()
        .uid
        == 0;
    if !is_root {
        assert_eq!(calls, 0);
        json(&saved, "assert x['error']['code']=='untrusted_localapi'");
        assert!(!std::path::Path::new(file).exists());
        return;
    }
    assert!(saved.status.success());
    assert_eq!(calls, 2);
    json(
        &saved,
        "assert x['selection_only'] and x['fresh_lookup_required']\np=x['hosts'][0]\nassert p=={'alias':'office','stable_id':'n-peer','certificate_name':'remote.fixture.ts.net','port':9443}",
    );
    let listed = run_cli(&["hosts", "saved", "--hosts-file", file, "--json"]);
    assert!(listed.status.success());
    json(
        &listed,
        "assert x['changed'] is False and len(x['hosts'])==1",
    );
    let renamed = fixture().replace("remote.fixture.ts.net.", "renamed.fixture.ts.net.");
    let (refused, calls) = metadata_command(
        renamed,
        &[
            "displays",
            "office",
            "--saved",
            "--hosts-file",
            file,
            "--experimental-native",
        ],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        calls, 1,
        "pin refuses during lookup, before the native dial"
    );
    json(
        &refused,
        "assert x['error']['code']=='tailnet_identity_refused'",
    );
    let forgotten = run_cli(&["hosts", "forget", "office", "--hosts-file", file, "--json"]);
    assert!(forgotten.status.success());
    json(&forgotten, "assert x['changed'] is True and x['hosts']==[]");
    let missing = run_cli(&[
        "displays",
        "office",
        "--saved",
        "--hosts-file",
        file,
        "--experimental-native",
        "--json",
    ]);
    json(
        &missing,
        "assert x['error']['code']=='saved_host_not_found'",
    );
}
#[test]
fn saved_list_does_not_create_directories_or_require_localapi() {
    let file = path("absent-saved-host-directory").join("hosts.json");
    let output = run_cli(&[
        "hosts",
        "saved",
        "--hosts-file",
        file.to_str().unwrap(),
        "--json",
    ]);
    assert!(output.status.success());
    assert!(!file.parent().unwrap().exists());
    json(&output, "assert x['hosts']==[] and x['changed'] is False");
}
