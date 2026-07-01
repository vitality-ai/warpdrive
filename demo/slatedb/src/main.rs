mod sanity;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("====================================================");
    println!(" SlateDB + Warpdrive demo");
    println!("====================================================\n");

    sanity::run().await?;

    Ok(())
}
