use anyhow::anyhow;
use clap::Parser;
use hyper::service::make_service_fn;
use hyper::Server;
use logging::init_logging;
use proxy::ProxyService;
use std::{convert::Infallible, net::SocketAddr};

mod logging;
mod monitor;
mod proxy;


#[derive(Parser)]
struct Opts {
    #[clap(default_value = "localhost:8080")]
    upstream: String,

    #[clap(default_value = "9090")]
    serve_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let opts = Opts::parse();
    let addr = SocketAddr::from(([0, 0, 0, 0], opts.serve_port));

    tracing::info!(%opts.upstream, %opts.serve_port, "Proxy started.");

    let proxy = ProxyService::new(&opts.upstream)
        .map_err(|e| anyhow!("Couldn't construct proxy to {}. Error: {:?}", opts.upstream, e))?;

    let make_svc = make_service_fn(|_conn| {
        let proxy = proxy.clone();
        async move { Ok::<_, Infallible>(proxy) }
    });

    let server = Server::bind(&addr).serve(make_svc);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }

    Ok(())
}
