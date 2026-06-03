mod push;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use minimonitor_core::snapshot::{Sampler, SortMode};

fn main() {
    let once = std::env::args().any(|a| a == "--once");
    let mut sampler = Sampler::new();

    if once {
        let snap = sampler.sample(SortMode::Cpu);
        println!("{}", serde_json::to_string_pretty(&snap).unwrap());
        return;
    }

    let first = serde_json::to_string(&sampler.sample(SortMode::Cpu)).unwrap();
    let latest = Arc::new(Mutex::new(first));

    {
        let latest = latest.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(1));
            let snap = sampler.sample(SortMode::Cpu);
            if let Ok(json) = serde_json::to_string(&snap) {
                *latest.lock().unwrap() = json;
            }
        });
    }

    let addr = "127.0.0.1:9909";
    let server = tiny_http::Server::http(addr).expect("agent failed to bind 127.0.0.1:9909");
    eprintln!("minimonitor-agent serving http://{addr}/snapshot");

    for request in server.incoming_requests() {
        let (body, content_type) = match request.url() {
            "/healthz" => ("ok".to_owned(), "text/plain"),
            _ => (latest.lock().unwrap().clone(), "application/json"),
        };
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
            .unwrap();
        let _ = request.respond(tiny_http::Response::from_string(body).with_header(header));
    }
}
