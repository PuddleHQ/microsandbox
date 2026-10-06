// SPDX-License-Identifier: Apache-2.0
//! `RegistryBuilder::proxy` replaces the process environment: with `HTTPS_PROXY`/`HTTP_PROXY`/
//! `ALL_PROXY` pointing at a decoy, every request still goes to the explicit proxy and the decoy
//! sees nothing. One test per binary: it changes the process environment before any thread starts.

use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use microsandbox_image::{GlobalCache, Platform, PullOptions, Reference, Registry};

/// Accepts connections on its own thread and records the first line of each request.
fn recording_listener() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let sink = sink.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 2048];
                if let Ok(n) = stream.read(&mut buf) {
                    let head = String::from_utf8_lossy(&buf[..n]);
                    sink.lock()
                        .unwrap()
                        .push(head.lines().next().unwrap_or_default().to_owned());
                }
                // Dropping the stream closes it: the request fails, which is all the test needs.
            });
        }
    });
    (addr, seen)
}

#[test]
fn the_explicit_proxy_wins_over_the_environment() {
    let (decoy, decoy_seen) = recording_listener();
    let (explicit, explicit_seen) = recording_listener();
    let decoy_url = format!("http://{decoy}");
    // SAFETY: the only test in this binary, and no thread exists yet.
    unsafe {
        for name in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(name, &decoy_url);
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (reference, insecure) in [
            ("secure.invalid/ns/app:1", vec![]),
            (
                "plain.invalid:5000/ns/app:1",
                vec!["plain.invalid:5000".to_owned()],
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let cache = GlobalCache::new(temp.path()).unwrap();
            let registry = Registry::builder(Platform::default(), cache)
                .add_insecure_registries(insecure)
                .proxy(format!("http://puddle:tok3n@{explicit}"))
                .build()
                .unwrap();
            let reference: Reference = reference.parse().unwrap();
            let options = PullOptions::default();
            let pull = registry.pull(&reference, &options);
            let result = tokio::time::timeout(Duration::from_secs(20), pull).await;
            assert!(result.expect("pull timed out").is_err());
        }
    });

    let explicit_seen = explicit_seen.lock().unwrap();
    assert!(
        explicit_seen
            .iter()
            .any(|l| l.starts_with("CONNECT secure.invalid:443 ")),
        "{explicit_seen:?}"
    );
    assert!(
        explicit_seen
            .iter()
            .any(|l| l.starts_with("GET http://plain.invalid:5000/v2/")),
        "{explicit_seen:?}"
    );
    assert_eq!(
        *decoy_seen.lock().unwrap(),
        Vec::<String>::new(),
        "the environment proxy was consulted"
    );
}
