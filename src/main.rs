use bytes::Bytes;
use clap::Parser;
use futures::future::Either::{Left as Tcp, Right as Unix};
use futures::{FutureExt, TryFutureExt};
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
        let state = Arc::new(State {
            client: hyper_util::client::legacy::Client::builder(
                hyper_util::rt::TokioExecutor::new(),
            )
            .build_http(),
            allow: regex::RegexSet::new(args.allow.iter().map(|allow| allow.as_str())).unwrap(),
        });

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
                    let service = service(&state);
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
                    let service = service(&state);
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

struct State {
    client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        hyper::body::Incoming,
    >,
    allow: regex::RegexSet,
}

fn service(
    state: &Arc<State>,
) -> impl hyper::service::Service<
    http::Request<hyper::body::Incoming>,
    Response = http::Response<impl http_body::Body<Data = Bytes, Error = hyper::Error> + 'static>,
    Error = Infallible,
    Future: Send,
> + 'static {
    fn status(
        status: http::StatusCode,
    ) -> http::Response<http_body_util::combinators::UnsyncBoxBody<Bytes, hyper::Error>> {
        http::Response::builder()
            .status(status)
            .body(http_body_util::combinators::UnsyncBoxBody::default())
            .unwrap()
    }

    let state = state.clone();
    let f = move |request: http::Request<hyper::body::Incoming>| {
        let state = state.clone();
        async move {
            if request.method() == http::Method::CONNECT {
                let (Some(host), Some(port)) = (request.uri().host(), request.uri().port_u16())
                else {
                    return status(http::StatusCode::BAD_REQUEST);
                };
                if !state.allow.is_match(&format!("{host}:{port}")) {
                    return status(http::StatusCode::FORBIDDEN);
                }
                match tokio::net::TcpStream::connect((host, port)).await {
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
                                            Err(e) => {
                                                tracing::error!(error = e.to_string())
                                            }
                                        }
                                    }
                                    Err(e) => tracing::error!(error = e.to_string()),
                                }
                            }
                            .instrument(tracing::Span::current()),
                        );
                        status(http::StatusCode::OK)
                    }
                    Err(e) => {
                        tracing::error!(error = e.to_string());
                        status(http::StatusCode::BAD_GATEWAY)
                    }
                }
            } else {
                let Some(host) = request.uri().host() else {
                    return status(http::StatusCode::BAD_REQUEST);
                };
                let port = match (request.uri().scheme(), request.uri().port_u16()) {
                    (_, Some(port)) => port,
                    (Some(scheme), _) if *scheme == http::uri::Scheme::HTTP => 80,
                    (Some(scheme), _) if *scheme == http::uri::Scheme::HTTPS => 443,
                    (Some(_), _) => return status(http::StatusCode::BAD_REQUEST),
                    _ => 80,
                };
                if !state.allow.is_match(&format!("{host}:{port}")) {
                    return status(http::StatusCode::FORBIDDEN);
                }
                match state.client.request(request).await {
                    Ok(response) => response.map(http_body_util::BodyExt::boxed_unsync),
                    Err(e) => {
                        tracing::error!(error = e.to_string());
                        status(http::StatusCode::BAD_GATEWAY)
                    }
                }
            }
        }
        .map(Ok)
    };
    let service = tower::ServiceBuilder::new()
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .service_fn(f);
    hyper_util::service::TowerToHyperService::new(service)
}
