//! Test-only loopback HTTP server that stands in for a forge's REST API.
//!
//! [`MockForge::start`] binds `127.0.0.1:0` (an OS-assigned free port, so
//! parallel tests never collide), runs `handler` on its own thread, and hands
//! it a [`MockForgeServer`] to pull requests from. The handler is either a
//! script (`server.recv()` once per expected call, asserting on each) or a
//! dispatcher (`for req in server.requests()`).
//!
//! Shutdown is drop-based: [`MockForge::finish`] or dropping the [`MockForge`]
//! unblocks the server and joins the handler thread. `tiny_http` queues the
//! unblock *behind* any requests already received, so the handler always
//! drains every request the client sent before it sees the shutdown. After
//! that, [`MockForgeServer::requests`] simply ends, and
//! [`MockForgeServer::recv`] panics naming the request that never arrived --
//! a scripted handler waiting on a call the client didn't make fails the test
//! instead of hanging it.
//!
//! Once the handler returns or panics, its thread keeps answering every
//! further request with a `500` until shutdown. Dropping the server would not
//! be enough: `tiny_http` keeps reading a client's keep-alive connection after
//! the `Server` is gone and queues its next request where nobody pops it, so
//! the client would sit out its whole read timeout. A request the handler was
//! holding when it panicked is answered `500` by `tiny_http` itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::forge::{ForgeClient, ForgeKind};

/// The handler thread's view of a [`MockForge`]: where it pulls requests from.
pub(crate) struct MockForgeServer {
    server: Arc<tiny_http::Server>,
}

impl MockForgeServer {
    /// The next request the client sent.
    ///
    /// # Panics
    ///
    /// If the [`MockForge`] shut down first, i.e. the code under test
    /// returned without sending the request this handler expected.
    pub(crate) fn recv(&self) -> tiny_http::Request {
        match self.server.recv() {
            Ok(req) => req,
            Err(err) => panic!(
                "mock forge: the handler expected another request, but the code under \
                 test finished without sending it ({err})"
            ),
        }
    }

    /// Every request the client sends, ending when the [`MockForge`] shuts
    /// down.
    pub(crate) fn requests(&self) -> impl Iterator<Item = tiny_http::Request> + '_ {
        std::iter::from_fn(|| self.server.recv().ok())
    }
}

/// A running mock forge. Shuts down (unblock + join) on [`Self::finish`] or
/// drop.
pub(crate) struct MockForge<T: Send + 'static = ()> {
    server: Arc<tiny_http::Server>,
    shutdown: Arc<AtomicBool>,
    addr: String,
    handle: Option<JoinHandle<T>>,
}

impl<T: Send + 'static> MockForge<T> {
    /// Bind a loopback server and run `handler` against it on a new thread.
    pub(crate) fn start(handler: impl FnOnce(&MockForgeServer) -> T + Send + 'static) -> Self {
        let server =
            Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("mock forge: bind 127.0.0.1:0"));
        let addr = server.server_addr().to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler_server = MockForgeServer {
            server: Arc::clone(&server),
        };
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&handler_server)));
            // `shutdown` is set before the unblock is queued, so whether the
            // handler consumed that unblock or not, this loop sees the flag
            // or the unblock and exits.
            while !thread_shutdown.load(Ordering::SeqCst) {
                if let Ok(req) = handler_server.server.recv() {
                    let body = format!(
                        "mock forge: unexpected {} {} after the handler exited",
                        req.method(),
                        req.url()
                    );
                    let _ =
                        req.respond(tiny_http::Response::from_string(body).with_status_code(500));
                }
            }
            match result {
                Ok(value) => value,
                Err(payload) => std::panic::resume_unwind(payload),
            }
        });
        Self {
            server,
            shutdown,
            addr,
            handle: Some(handle),
        }
    }

    /// `host:port` the server listens on.
    pub(crate) fn addr(&self) -> &str {
        &self.addr
    }

    /// `http://host:port`, the `api_base` a [`ForgeClient`] should use.
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A token-authenticated [`ForgeClient`] pointed at this server.
    pub(crate) fn client(&self, kind: ForgeKind, repo: &str) -> ForgeClient {
        ForgeClient::new(
            kind,
            self.base_url(),
            repo.to_string(),
            Some("tok".to_string()),
        )
    }

    fn shut_down(&mut self) -> Option<std::thread::Result<T>> {
        let handle = self.handle.take()?;
        self.shutdown.store(true, Ordering::SeqCst);
        self.server.unblock();
        Some(handle.join())
    }

    /// Shut the server down and return the handler's result, re-raising the
    /// handler's own panic (with its original message) if it failed.
    pub(crate) fn finish(mut self) -> T {
        match self.shut_down() {
            Some(Ok(value)) => value,
            Some(Err(payload)) => std::panic::resume_unwind(payload),
            None => unreachable!("mock forge: the handler thread is joined only once"),
        }
    }
}

impl<T: Send + 'static> Drop for MockForge<T> {
    fn drop(&mut self) {
        // The handler's panic (if any) was already printed by the panic hook;
        // re-raising it here could abort an unwinding test.
        let _ = self.shut_down();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(forge: &MockForge<impl Send + 'static>, path: &str) -> String {
        ureq::get(&format!("{}{path}", forge.base_url()))
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    }

    /// One agent, so successive calls reuse a keep-alive connection, with a
    /// read timeout far longer than any assertion below allows.
    fn keep_alive_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_secs(60))
            .build()
    }

    fn status_of(result: Result<ureq::Response, ureq::Error>) -> u16 {
        match result {
            Ok(resp) => resp.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(err) => panic!("expected an HTTP status, got {err}"),
        }
    }

    #[test]
    fn scripted_handler_returns_its_value_from_finish() {
        let forge = MockForge::start(|server| {
            let req = server.recv();
            let url = req.url().to_string();
            req.respond(tiny_http::Response::from_string("one"))
                .unwrap();
            url
        });
        assert_eq!(get(&forge, "/a"), "one");
        assert_eq!(forge.finish(), "/a");
    }

    #[test]
    fn dispatcher_ends_when_the_forge_finishes() {
        let forge = MockForge::start(|server| {
            let mut seen = 0;
            for req in server.requests() {
                seen += 1;
                req.respond(tiny_http::Response::from_string("ok")).unwrap();
            }
            seen
        });
        get(&forge, "/a");
        get(&forge, "/b");
        assert_eq!(forge.finish(), 2);
    }

    #[test]
    #[should_panic(expected = "finished without sending it")]
    fn finish_fails_instead_of_hanging_on_a_request_that_never_came() {
        let forge = MockForge::start(|server| {
            server.recv();
        });
        forge.finish();
    }

    #[test]
    fn requests_after_the_handler_returns_get_a_prompt_500() {
        let forge = MockForge::start(|server| {
            server
                .recv()
                .respond(tiny_http::Response::from_string("one"))
                .unwrap();
        });
        let agent = keep_alive_agent();
        let url = format!("{}/x", forge.base_url());
        let started = std::time::Instant::now();
        assert_eq!(status_of(agent.get(&url).call()), 200);
        assert_eq!(status_of(agent.get(&url).call()), 500);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        forge.finish();
    }

    #[test]
    fn a_panicked_handler_fails_the_client_fast_and_finish_reraises_it() {
        let forge = MockForge::start(|server| {
            server
                .recv()
                .respond(tiny_http::Response::from_string("one"))
                .unwrap();
            let _held = server.recv();
            panic!("handler assertion failed");
        });
        let agent = keep_alive_agent();
        let url = format!("{}/x", forge.base_url());
        let started = std::time::Instant::now();
        assert_eq!(status_of(agent.get(&url).call()), 200);
        assert_eq!(status_of(agent.get(&url).call()), 500, "the held request");
        assert_eq!(status_of(agent.get(&url).call()), 500, "after the panic");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
        let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| forge.finish()));
        let payload = finished.expect_err("finish re-raises the handler's panic");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"handler assertion failed")
        );
    }

    #[test]
    fn drop_shuts_down_a_handler_still_waiting_for_requests() {
        let forge = MockForge::start(|server| server.requests().count());
        drop(forge);
    }
}
