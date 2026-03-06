/// Health check for proxy container healthcheck.
use anyhow::Result;

pub async fn run(_proxy_only: bool) -> Result<()> {
    tokio::net::TcpStream::connect("127.0.0.1:3128")
        .await
        .map_err(|_| anyhow::anyhow!("cannot connect to proxy on port 3128"))?;
    Ok(())
}
