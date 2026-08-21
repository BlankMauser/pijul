use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;

use bytes::Bytes;
use clap::{Parser, ValueHint};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use tokio::select;
use tokio::sync::mpsc::channel;

#[derive(Parser, Debug)]
pub struct Client {
    /// Url to authenticate to.
    #[clap(value_name = "URL", value_hint = ValueHint::Url)]
    url: String,
}

async fn handle_request(
    req: Request<Incoming>,
    tx: tokio::sync::mpsc::Sender<String>,
) -> Response<Full<Bytes>> {
    let token = req.uri().query().and_then(|q| {
        let prefix = "token=";
        q.starts_with(prefix)
            .then(|| q.split_at(prefix.len()).1.to_string())
    });
    if let Some(token) = token {
        let _ = tx.send(token).await;
        Response::builder()
            .header("Content-Type", "text/html")
            .body(Full::new(Bytes::from(include_str!("client.html"))))
            .unwrap()
    } else {
        Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("Not found")))
            .unwrap()
    }
}

impl Client {
    pub fn repository_path(&self) -> Option<&Path> {
        None
    }

    pub async fn run(self) -> Result<(), anyhow::Error> {
        let url = url::Url::parse(&self.url)?;

        let mut cache_path = None;
        if let Ok(mut cached) = pijul_config::global_config_directory() {
            cached.push("cache");
            if let Some(host) = url.host_str() {
                cached.push(host);
                if let Ok(token) = std::fs::read_to_string(&cached) {
                    println!("Bearer {}", token);
                    return Ok(());
                } else {
                    cache_path = Some(cached);
                }
            }
        }

        let (tx, mut rx) = channel::<String>(1);

        let mut port = 3000u16;
        let listener = loop {
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => break l,
                Err(_) if port < u16::MAX => port += 1,
                Err(_) => anyhow::bail!("No available port found"),
            }
        };

        let mut url = url::Url::parse(&self.url)?;
        url.query_pairs_mut().append_pair("port", &port.to_string());
        open::that(&url.to_string()).unwrap_or(());
        eprintln!(
            "If the URL doesn't open automatically, please visit {}",
            url
        );

        let accept_loop = {
            let tx = tx.clone();
            async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            let tx = tx.clone();
                            let io = TokioIo::new(stream);
                            tokio::spawn(async move {
                                let _ = ServerBuilder::new(TokioExecutor::new())
                                    .serve_connection(
                                        io,
                                        service_fn(move |req| {
                                            let tx = tx.clone();
                                            async move {
                                                Ok::<_, Infallible>(handle_request(req, tx).await)
                                            }
                                        }),
                                    )
                                    .await;
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
        };

        select! {
            _ = accept_loop => {}
            x = rx.recv() => {
                if let Some(x) = x {
                    if let Some(cache_path) = cache_path {
                        if let Some(c) = cache_path.parent() {
                            std::fs::create_dir_all(c)?
                        }
                        if let Err(e) = std::fs::write(&cache_path, &x) {
                            log::debug!("Error while writing file {:?}: {:?}", cache_path, e)
                        }
                    }
                    println!("Bearer {}", x);
                }
            }
        }
        Ok(())
    }
}
