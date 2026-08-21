//! LAN book sharing: mDNS discovery (`_weland-share._tcp.local.`) of other
//! weland instances on the network, plus a tiny embedded HTTP server so a
//! discovered peer can pull any book this device has explicitly marked
//! `shared` (see `LibraryEntry::shared` in `persistence.rs`).
//!
//! Deliberately whole-`.wld`-file transfer only -- annotations already live
//! inside that same SQLite file (see `schema.rs`'s `user_annotations`
//! table), so they ride along for free. Merging annotations into a book a
//! peer already owns from an *independently compiled* copy is a much harder,
//! separate problem (their `ast_nodes`/`user_annotations` ids aren't
//! globally unique) and isn't attempted here.
//!
//! Off by default (`Settings.lan_sharing_enabled`), pull model: once a book
//! is marked shared, anyone who can mDNS-discover this device on the LAN can
//! pull it -- there's no per-request prompt on the sender side (the HTTP
//! thread can't block on a GTK dialog). The receiving side always shows an
//! explicit accept dialog before anything touches disk (see `library.rs`),
//! and every received file is validated exactly like a local import before
//! being trusted.
//!
//! No async runtime anywhere in this app (see `library.rs`'s import code):
//! `mdns-sd` runs its own daemon thread and hands events back over a plain
//! channel, polled here via `glib::timeout_add_local` exactly like every
//! other background-thread result in this codebase; `tiny_http`'s blocking
//! server loop runs in one plain `std::thread::spawn`.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use gtk4::glib;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, TryRecvError};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::persistence;

pub const SERVICE_TYPE: &str = "_weland-share._tcp.local.";

// Generous but bounded -- a compiled .wld with embedded images is rarely
// more than a couple hundred MB; this just stops a misbehaving/malicious
// peer from streaming an unbounded response into memory.
const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub name: String,
    pub addr: SocketAddr,
    fullname: String,
}

/// One book as advertised by a peer's `GET /books` -- deliberately not
/// `persistence::LibraryEntry` (that has local-only fields like `path`/
/// `shared` that mean nothing on the wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedBook {
    pub title: String,
    pub author: Option<String>,
    pub content_hash: String,
    pub size: u64,
}

pub struct ShareService {
    daemon: ServiceDaemon,
    http_server: Arc<tiny_http::Server>,
    peers: Rc<RefCell<Vec<PeerInfo>>>,
    stopped: Rc<RefCell<bool>>,
}

impl ShareService {
    /// Starts advertising this device and browsing for peers. Cheap to call
    /// repeatedly (e.g. from a settings toggle) as long as the previous
    /// instance's `stop()` was called first -- the caller (`library.rs`)
    /// owns exactly one live instance at a time in an `Option`. Only serves
    /// `GET /books`/`GET /books/{hash}`; the corresponding fetch/import
    /// functions below take their own `config_dir`/`books_dir` from the
    /// caller rather than this struct storing a second copy.
    pub fn start(config_dir: PathBuf, device_name: String) -> Result<Rc<Self>> {
        let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;

        let http_server = tiny_http::Server::http("0.0.0.0:0")
            .map_err(|e| anyhow!("failed to start LAN-sharing HTTP server: {e}"))?;
        let http_port = http_server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
        let http_server = Arc::new(http_server);

        let instance_name = sanitize_label(&device_name, "weland-device");
        let host_name = format!("{instance_name}.local.");
        let properties = [("name", device_name.as_str())];
        let service_info = ServiceInfo::new(SERVICE_TYPE, &instance_name, &host_name, "", http_port, &properties[..])
            .context("failed to build mDNS service info")?
            .enable_addr_auto();
        // mDNS browsing sees every instance of this service type on the
        // network, including the one this process itself just registered --
        // captured here so the discovery loop below can filter it out of the
        // peer list (a device offering books to itself isn't a real peer).
        let own_fullname = service_info.get_fullname().to_string();
        daemon.register(service_info).context("failed to register mDNS service")?;

        let browse_rx = daemon.browse(SERVICE_TYPE).context("failed to browse for LAN peers")?;
        let peers: Rc<RefCell<Vec<PeerInfo>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let peers = peers.clone();
            glib::timeout_add_local(Duration::from_millis(500), move || {
                loop {
                    match browse_rx.try_recv() {
                        Ok(ServiceEvent::ServiceResolved(resolved)) if resolved.get_fullname() == own_fullname => continue,
                        Ok(ServiceEvent::ServiceResolved(resolved)) => {
                            let Some(ip) = resolved
                                .get_addresses_v4()
                                .into_iter()
                                .next()
                                .map(std::net::IpAddr::V4)
                                .or_else(|| resolved.get_addresses().iter().next().map(|a| a.to_ip_addr()))
                            else {
                                continue;
                            };
                            let name =
                                resolved.get_property_val_str("name").unwrap_or_else(|| resolved.get_hostname()).to_string();
                            let fullname = resolved.get_fullname().to_string();
                            let addr = SocketAddr::new(ip, resolved.get_port());

                            let mut peers = peers.borrow_mut();
                            if let Some(existing) = peers.iter_mut().find(|p| p.fullname == fullname) {
                                existing.addr = addr;
                                existing.name = name;
                            } else {
                                peers.push(PeerInfo { name, addr, fullname });
                            }
                        }
                        Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                            peers.borrow_mut().retain(|p| p.fullname != fullname);
                        }
                        Ok(_) => continue,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return glib::ControlFlow::Break,
                    }
                }
                glib::ControlFlow::Continue
            });
        }

        {
            let http_server = http_server.clone();
            let config_dir = config_dir.clone();
            std::thread::spawn(move || {
                for request in http_server.incoming_requests() {
                    if let Err(e) = handle_request(request, &config_dir) {
                        eprintln!("[sharing] request error: {e}");
                    }
                }
            });
        }

        Ok(Rc::new(Self { daemon, http_server, peers, stopped: Rc::new(RefCell::new(false)) }))
    }

    /// Unregisters from mDNS and unblocks the HTTP accept loop so its thread
    /// exits. Safe to call more than once.
    pub fn stop(&self) {
        if *self.stopped.borrow() {
            return;
        }
        *self.stopped.borrow_mut() = true;
        let _ = self.daemon.shutdown();
        self.http_server.unblock();
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        self.peers.borrow().clone()
    }
}

fn sanitize_label(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn handle_request(request: tiny_http::Request, config_dir: &Path) -> std::io::Result<()> {
    let url = request.url().to_string();
    if url == "/books" {
        respond_books_list(request, config_dir)
    } else if let Some(hash) = url.strip_prefix("/books/") {
        respond_book_file(request, config_dir, hash)
    } else {
        request.respond(tiny_http::Response::from_string("not found").with_status_code(404))
    }
}

fn respond_books_list(request: tiny_http::Request, config_dir: &Path) -> std::io::Result<()> {
    let entries = persistence::read_library(config_dir).unwrap_or_default();
    let books: Vec<SharedBook> = entries
        .iter()
        .filter(|e| e.shared == Some(true))
        .filter_map(|e| {
            let content_hash = e.content_hash.clone()?;
            let size = std::fs::metadata(&e.path).map(|m| m.len()).unwrap_or(0);
            Some(SharedBook { title: e.title.clone(), author: e.author.clone(), content_hash, size })
        })
        .collect();

    let body = serde_json::to_string(&books).unwrap_or_else(|_| "[]".to_string());
    let response = tiny_http::Response::from_string(body)
        .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
    request.respond(response)
}

fn respond_book_file(request: tiny_http::Request, config_dir: &Path, hash: &str) -> std::io::Result<()> {
    let entries = persistence::read_library(config_dir).unwrap_or_default();
    let found = entries.into_iter().find(|e| e.shared == Some(true) && e.content_hash.as_deref() == Some(hash));

    match found.and_then(|e| std::fs::File::open(&e.path).ok()) {
        Some(file) => {
            let response = tiny_http::Response::from_file(file)
                .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..]).unwrap());
            request.respond(response)
        }
        None => request.respond(tiny_http::Response::from_string("not found").with_status_code(404)),
    }
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder().timeout_global(Some(HTTP_TIMEOUT)).build();
    config.into()
}

/// Blocking GET of a peer's shared-book list. Call off the GTK main thread.
pub fn fetch_peer_books(addr: SocketAddr) -> Result<Vec<SharedBook>> {
    let url = format!("http://{addr}/books");
    let body = http_agent()
        .get(&url)
        .call()
        .with_context(|| format!("request to {addr} failed"))?
        .body_mut()
        .read_to_string()
        .context("invalid response from peer")?;
    serde_json::from_str(&body).context("peer sent a malformed book list")
}

/// Blocking fetch-validate-and-register of one book from a peer. Call off
/// the GTK main thread; safe to call repeatedly (a book already present by
/// content hash is a no-op, not a re-download).
pub fn import_from_peer(config_dir: &Path, books_dir: &Path, addr: SocketAddr, book: &SharedBook) -> Result<()> {
    let entries = persistence::read_library(config_dir)?;
    if entries.iter().any(|e| e.content_hash.as_deref() == Some(book.content_hash.as_str())) {
        return Ok(());
    }

    let url = format!("http://{addr}/books/{}", book.content_hash);
    let bytes = http_agent()
        .get(&url)
        .call()
        .with_context(|| format!("request to {addr} failed"))?
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .read_to_vec()
        .context("failed reading peer's response")?;

    std::fs::create_dir_all(books_dir)?;
    let tmp_path = books_dir.join(format!(".incoming-{}.wld", book.content_hash));
    std::fs::write(&tmp_path, &bytes).context("failed to write received book")?;

    // Validate exactly like a local import would (`import_one` in
    // `library.rs`): open read-only, make sure it actually parses as a
    // .wld. A malformed/truncated transfer is discarded here, never
    // partially registered.
    let validated = (|| -> Result<(String, Option<String>)> {
        let conn = Connection::open_with_flags(&tmp_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let metadata = weland::db::load_metadata(&conn)?;
        let title = metadata.get("title").cloned().unwrap_or_else(|| book.title.clone());
        Ok((title, metadata.get("source_epub_sha256").cloned()))
    })();

    let (title, verified_hash) = match validated {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.context("received file failed validation, discarded"));
        }
    };

    // The sender could be buggy (or actively lying) about which hash a file
    // is served under -- trust only what the file itself claims to be.
    if verified_hash.as_deref() != Some(book.content_hash.as_str()) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow!("received book's content hash didn't match what the peer advertised, discarded"));
    }

    let final_path = persistence::received_wld_output_path(books_dir, &book.content_hash, &title);
    std::fs::rename(&tmp_path, &final_path)?;

    persistence::upsert_library_entry(
        config_dir,
        &final_path.to_string_lossy(),
        &title,
        book.author.as_deref(),
        None,
        Some(&book.content_hash),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but real `.wld`: real schema, real `metadata` rows -- not a
    /// full EPUB compile (irrelevant to this module, which never looks past
    /// the `metadata` table), but everything `import_from_peer`'s validation
    /// actually reads.
    fn make_test_wld(path: &std::path::Path, title: &str, hash: &str) {
        let conn = Connection::open(path).unwrap();
        weland::schema::init_db(&conn).unwrap();
        conn.execute("INSERT INTO metadata (key, value) VALUES ('title', ?1)", rusqlite::params![title]).unwrap();
        conn.execute("INSERT INTO metadata (key, value) VALUES ('source_epub_sha256', ?1)", rusqlite::params![hash]).unwrap();
    }

    /// Exercises the exact functions the "Import" button calls
    /// (`fetch_peer_books`, `import_from_peer`) against a real `tiny_http`
    /// server running the real `handle_request` -- no GUI, no mDNS, so any
    /// bug in the fetch/validate/register pipeline itself (as opposed to a
    /// UI-layer bug) shows up here directly.
    #[test]
    fn fetch_and_import_end_to_end() {
        let sender_dir = tempfile::tempdir().unwrap();
        let sender_config = sender_dir.path().join("config");
        let sender_books = sender_dir.path().join("books");
        std::fs::create_dir_all(&sender_books).unwrap();

        let wld_path = sender_books.join("test.wld");
        make_test_wld(&wld_path, "Test Book", "abc123hash");
        persistence::upsert_library_entry(&sender_config, &wld_path.to_string_lossy(), "Test Book", None, None, Some("abc123hash")).unwrap();
        persistence::set_library_entry_shared(&sender_config, &wld_path.to_string_lossy(), true).unwrap();

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let server = Arc::new(server);
        {
            let server = server.clone();
            let sender_config = sender_config.clone();
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    handle_request(request, &sender_config).unwrap();
                }
            });
        }

        let books = fetch_peer_books(addr).expect("fetch_peer_books failed");
        assert_eq!(books.len(), 1, "the shared book must appear in the peer's list");
        assert_eq!(books[0].content_hash, "abc123hash");

        let receiver_config = tempfile::tempdir().unwrap();
        let receiver_books = tempfile::tempdir().unwrap();
        import_from_peer(receiver_config.path(), receiver_books.path(), addr, &books[0]).expect("import_from_peer failed");

        let entries = persistence::read_library(receiver_config.path()).unwrap();
        assert_eq!(entries.len(), 1, "the imported book must be registered in the receiver's library");
        assert_eq!(entries[0].title, "Test Book");
        assert_eq!(entries[0].content_hash.as_deref(), Some("abc123hash"));
        assert!(std::path::Path::new(&entries[0].path).exists());

        server.unblock();
    }
}
