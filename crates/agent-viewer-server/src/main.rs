#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8477").await?;
    axum::serve(listener, agent_viewer_server::router()).await?;
    Ok(())
}
