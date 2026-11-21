//! A local HTTP server for a scenario: it answers as the scenario says (a 403 here, a moved
//! endpoint there) and keeps a record of every request, which is how a check knows what the
//! agent really sent. It listens on the loopback address only; no scenario touches a network.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

/// One request the server took.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: String,
}

/// How a scenario answers a request: a status and a body.
pub type Respond = fn(&Hit) -> (u16, String);

pub struct FaultServer {
    pub address: SocketAddr,
    hits: Arc<Mutex<Vec<Hit>>>,
    serving: tokio::task::JoinHandle<()>,
}

impl FaultServer {
    pub async fn start(respond: Respond) -> std::io::Result<FaultServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let log = hits.clone();
        let serving = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let log = log.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let log = log.clone();
                        async move {
                            let (parts, body) = request.into_parts();
                            let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                            let hit = Hit {
                                method: parts.method.as_str().to_string(),
                                path: parts.uri.path().to_string(),
                                query: parts.uri.query().unwrap_or_default().to_string(),
                                body: String::from_utf8_lossy(&body).into_owned(),
                            };
                            let (status, text) = respond(&hit);
                            log.lock().unwrap().push(hit);
                            let response = Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(text)))
                                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Ok(FaultServer { address, hits, serving })
    }

    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub fn base(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }
}

impl Drop for FaultServer {
    fn drop(&mut self) {
        self.serving.abort();
    }
}
