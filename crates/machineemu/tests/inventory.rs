use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
};

fn api_command(command: &str, response: serde_json::Value) -> String {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = listener.local_addr().unwrap().to_string();
    fs::write(
        root.path().join("machineemu.yaml"),
        format!("client:\n  endpoint: {endpoint}\n  token: test-token\n  workspace: nonexistent\n"),
    )
    .unwrap();
    let name = command.to_owned();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with(&format!("GET /api/v2/{name} HTTP/1.1\r\n")));
        assert!(request.contains("Authorization: Bearer test-token\r\n"));
        let body = response.to_string();
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_machineemu"))
        .current_dir(root.path())
        .args([command, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn inventory_queries_the_api() {
    let images: serde_json::Value = serde_json::from_str(&api_command(
        "images",
        serde_json::json!([{
            "image_id":"remote-image", "engine_track":"qemu-system", "target":"x86_64-softmmu",
            "disk_sha256":"a".repeat(64)
        }]),
    ))
    .unwrap();
    assert_eq!(images[0]["image_id"], "remote-image");
    let profiles: serde_json::Value = serde_json::from_str(&api_command(
        "profiles",
        serde_json::json!([{
            "schema_version":2, "id":"remote-profile", "name":"Remote", "target":"x86_64-softmmu"
        }]),
    ))
    .unwrap();
    assert_eq!(profiles[0]["id"], "remote-profile");
}
