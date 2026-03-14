use bytes::Bytes;
use clap::Parser;
use futures::TryFutureExt;
use futures::future::Either::{Left as Tcp, Right as Unix};
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use tracing::Instrument;

#[derive(Parser)]
struct Args {
    #[clap(long)]
    bind: Vec<SocketAddr>,
    #[clap(long)]
    allow: Vec<regex::Regex>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let allow =
        Arc::new(regex::RegexSet::new(args.allow.iter().map(|allow| allow.as_str())).unwrap());

    let mut listeners = futures::future::try_join_all(
        args.bind
            .into_iter()
            .map(|bind| tokio::net::TcpListener::bind(bind).map_ok(Tcp)),
    )
    .await?;
    let mut listenfd = listenfd::ListenFd::from_env();
    for i in 0..listenfd.len() {
        if let Ok(Some(listener)) = listenfd.take_tcp_listener(i) {
            listener.set_nonblocking(true)?;
            listeners.push(Tcp(tokio::net::TcpListener::from_std(listener)?));
        } else if let Ok(Some(listener)) = listenfd.take_unix_listener(i) {
            listener.set_nonblocking(true)?;
            listeners.push(Unix(tokio::net::UnixListener::from_std(listener)?));
        }
    }
    for listener in &listeners {
        match listener {
            Tcp(listener) => tracing::info!(bind = ?listener.local_addr()),
            Unix(listener) => tracing::info!(bind = ?listener.local_addr()),
        }
    }

    if !listeners.is_empty() {
        loop {
            let accept = futures::future::poll_fn(|cx| {
                for listener in &listeners {
                    match listener {
                        Tcp(listener) => {
                            if let Poll::Ready(output) = listener.poll_accept(cx) {
                                return Poll::Ready(output.map(Tcp));
                            }
                        }
                        Unix(listener) => {
                            if let Poll::Ready(output) = listener.poll_accept(cx) {
                                return Poll::Ready(output.map(Unix));
                            }
                        }
                    }
                }
                Poll::Pending
            });
            match accept.await? {
                Tcp((stream, _)) => {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let service = service(allow.clone());
                    tokio::spawn(async move {
                        hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection_with_upgrades(io, service)
                        .await
                    });
                }
                Unix((stream, _)) => {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let service = service(allow.clone());
                    tokio::spawn(async move {
                        hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection_with_upgrades(io, service)
                        .await
                    });
                }
            }
        }
    }

    Ok(())
}

fn service(
    allow: Arc<regex::RegexSet>,
) -> impl hyper::service::Service<
    http::Request<hyper::body::Incoming>,
    Response = http::Response<impl http_body::Body<Data = Bytes, Error = Infallible>>,
    Error = Infallible,
    Future: Send,
> {
    let f = move |request: http::Request<hyper::body::Incoming>| {
        let addr = if request.method() == http::Method::CONNECT {
            if let (Some(host), Some(port)) = (request.uri().host(), request.uri().port_u16()) {
                let addr = format!("{host}:{port}");
                if allow.is_match(&addr) {
                    Ok(addr)
                } else {
                    Err(http::StatusCode::FORBIDDEN)
                }
            } else {
                Err(http::StatusCode::BAD_REQUEST)
            }
        } else {
            Err(http::StatusCode::NOT_FOUND)
        };
        async move {
            let status = match addr {
                Ok(addr) => match tokio::net::TcpStream::connect(addr).await {
                    Ok(mut stream) => {
                        tokio::spawn(
                            async move {
                                match hyper::upgrade::on(request).await {
                                    Ok(upgraded) => {
                                        let mut upgraded = hyper_util::rt::TokioIo::new(upgraded);
                                        match tokio::io::copy_bidirectional(
                                            &mut upgraded,
                                            &mut stream,
                                        )
                                        .await
                                        {
                                            Ok((tx, rx)) => tracing::info!(tx, rx),
                                            Err(e) => tracing::error!(error = e.to_string()),
                                        }
                                    }
                                    Err(e) => tracing::error!(error = e.to_string()),
                                }
                            }
                            .instrument(tracing::Span::current()),
                        );
                        http::StatusCode::OK
                    }
                    Err(e) => {
                        tracing::error!(error = e.to_string());
                        http::StatusCode::BAD_GATEWAY
                    }
                },
                Err(status) => status,
            };
            Ok(http::Response::builder()
                .status(status)
                .body(http_body_util::Empty::new())
                .unwrap())
        }
    };
    let service = tower::ServiceBuilder::new()
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .service_fn(f);
    hyper_util::service::TowerToHyperService::new(service)
}
