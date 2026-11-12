pub fn probe(stage: &str) {
    use std::io::Write;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open("/run/user/1000/omamail-probe.log") {
        let _ = writeln!(file, "{now} {stage}");
    }
}
pub mod account;
// Detached assistant workers currently rely on Linux pidfds and private storage.
#[cfg(all(feature = "agent", unix))]
pub mod agent;
pub mod attachment;
pub mod auth;
pub mod backend;
pub mod cache;
pub mod calendar;
pub mod cli;
pub mod compose;
pub mod contacts;
pub mod credentials;
pub mod mail;
pub mod message;
pub mod outbox;
pub mod platform;
pub mod process;
pub mod providers;
pub mod public_http;
pub mod sync;
pub mod tls;
