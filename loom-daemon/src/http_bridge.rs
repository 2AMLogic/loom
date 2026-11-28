//! The daemon's one bridge from synchronous code to async HTTP (#15661): a
//! small runtime that the synchronous callers of its outbound HTTP clients
//! (fleet dispatch, the forge mirror) spawn their requests onto, and the
//! class those clients log a `reqwest` transport error under.
//!
//! The runtime is only ever *built* and spawned onto, never entered with
//! `block_on`, so first use is safe from any thread, inside a tokio runtime
//! or not. It is never dropped, so the clients' connections live as long as
//! the process. It has one worker: everything spawned onto it is async I/O,
//! and nothing spawned onto it may block.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

/// The shared runtime, or `None` when it could not be built.
///
/// A caller whose future owns something it must hand back when nothing runs
/// (a `done` callback) asks this first: a future owns what it captures as
/// soon as it is built, so it cannot be built "to be dropped unrun".
#[must_use]
pub(crate) fn runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("loom-http-bridge")
            .enable_all()
            .build()
            .map_err(|e| log::warn!("http_bridge: no runtime for the blocking HTTP callers ({e})"))
            .ok()
    })
    .as_ref()
}

/// Why a bridged call produced no answer. Its `Display` is the text each
/// client carries in its own transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BridgeError {
    /// The shared runtime could not be built.
    NoRuntime,
    /// No answer within the bound.
    Deadline,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoRuntime => "no runtime",
            Self::Deadline => "deadline",
        })
    }
}

/// Run `fut` on the shared runtime and wait for its answer, for at most
/// `deadline` whatever `fut` does. Past the bound `fut` keeps running and its
/// answer is dropped.
///
/// # Errors
///
/// [`BridgeError::NoRuntime`] when the runtime could not be built;
/// [`BridgeError::Deadline`] when no answer arrived within the bound.
pub(crate) fn run_blocking<T, F>(deadline: Duration, fut: F) -> Result<T, BridgeError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let Some(rt) = runtime() else {
        return Err(BridgeError::NoRuntime);
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    rt.spawn(async move {
        let _ = tx.send(fut.await);
    });
    wait(&rx, deadline).ok_or(BridgeError::Deadline)
}

/// Wait for `rx`, telling a multi-thread tokio runtime the current worker is
/// about to block (so its other tasks move off it). A current-thread runtime
/// cannot hand its only worker over; there the wait simply blocks, bounded
/// by `deadline`.
fn wait<T>(rx: &std::sync::mpsc::Receiver<T>, deadline: Duration) -> Option<T> {
    let multi = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi {
        tokio::task::block_in_place(|| rx.recv_timeout(deadline).ok())
    } else {
        rx.recv_timeout(deadline).ok()
    }
}

/// A transport error's class: its kind, never its URL (which carries no key,
/// but may carry repo names an operator did not expect in a log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportClass {
    /// `reqwest::Error::is_timeout`.
    Timeout,
    /// `reqwest::Error::is_connect`.
    Connect,
    /// `reqwest::Error::is_decode`: a body that could not be read as asked.
    Decode,
    /// `reqwest::Error::is_body`.
    Body,
    /// None of the above.
    Request,
}

impl std::fmt::Display for TransportClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Decode => "decode",
            Self::Body => "body",
            Self::Request => "request",
        })
    }
}

/// Classify a transport error.
#[must_use]
pub(crate) fn classify_reqwest_error(e: &reqwest::Error) -> TransportClass {
    if e.is_timeout() {
        TransportClass::Timeout
    } else if e.is_connect() {
        TransportClass::Connect
    } else if e.is_decode() {
        TransportClass::Decode
    } else if e.is_body() {
        TransportClass::Body
    } else {
        TransportClass::Request
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::{Duration, Instant};

    use super::*;

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    /// Answer one request with `raw` bytes, then close the connection.
    fn answer_once(raw: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            stream.write_all(raw.as_bytes()).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        });
        format!("http://{addr}")
    }

    #[test]
    fn a_class_displays_as_its_log_word() {
        for (class, word) in [
            (TransportClass::Timeout, "timeout"),
            (TransportClass::Connect, "connect"),
            (TransportClass::Decode, "decode"),
            (TransportClass::Body, "body"),
            (TransportClass::Request, "request"),
        ] {
            assert_eq!(class.to_string(), word);
        }
        assert_eq!(BridgeError::NoRuntime.to_string(), "no runtime");
        assert_eq!(BridgeError::Deadline.to_string(), "deadline");
    }

    #[tokio::test]
    async fn a_refused_connection_is_a_connect_error() {
        let dead = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let e = client()
            .get(format!("http://{dead}"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(classify_reqwest_error(&e), TransportClass::Connect);
    }

    #[tokio::test]
    async fn a_silent_server_is_a_timeout_error() {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", silent.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in silent.incoming().flatten() {
                held.push(stream);
            }
        });
        let e = client()
            .get(url)
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .unwrap_err();
        assert_eq!(classify_reqwest_error(&e), TransportClass::Timeout);
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_a_decode_error() {
        let url =
            answer_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nnope!");
        let res = client().get(url).send().await.unwrap();
        let e = res.json::<serde_json::Value>().await.unwrap_err();
        assert_eq!(classify_reqwest_error(&e), TransportClass::Decode);
    }

    /// reqwest 0.13 reads a body through `Response::bytes`, which maps every
    /// body error onto a decode error: a read cut short is `Decode`, never
    /// `Body`.
    #[tokio::test]
    async fn a_body_cut_short_is_a_decode_error() {
        let url = answer_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"a\":1}",
        );
        let res = client().get(url).send().await.unwrap();
        let e = res.bytes().await.unwrap_err();
        assert!(e.is_decode(), "{e:?}");
        assert_eq!(classify_reqwest_error(&e), TransportClass::Decode);
    }

    #[tokio::test]
    async fn an_unbuildable_request_is_a_request_error() {
        let e = client().get("relative").send().await.unwrap_err();
        assert_eq!(classify_reqwest_error(&e), TransportClass::Request);
    }

    #[test]
    fn a_bridged_call_answers_a_plain_thread() {
        assert_eq!(run_blocking(Duration::from_secs(5), async { 7 }).unwrap(), 7);
        let (first, again) = (runtime().unwrap(), runtime().unwrap());
        assert!(std::ptr::eq(first, again), "one runtime serves every caller");
        let thread = run_blocking(Duration::from_secs(5), async {
            std::thread::current().name().map(str::to_string)
        })
        .unwrap();
        assert_eq!(thread.as_deref(), Some("loom-http-bridge"));
    }

    #[test]
    fn a_bridged_call_is_bounded_whatever_the_future_does() {
        let started = Instant::now();
        let e = run_blocking(Duration::from_millis(100), std::future::pending::<()>()).unwrap_err();
        assert_eq!(e, BridgeError::Deadline);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_bridged_call_answers_inside_a_current_thread_runtime() {
        assert_eq!(run_blocking(Duration::from_secs(5), async { 7 }).unwrap(), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bridged_call_answers_inside_a_multi_thread_runtime() {
        assert_eq!(run_blocking(Duration::from_secs(5), async { 7 }).unwrap(), 7);
    }
}
