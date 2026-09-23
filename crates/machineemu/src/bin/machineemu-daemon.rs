#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    machineemu::api::serve().await
}
