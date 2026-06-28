mod sanity;
mod replication;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("====================================================");
    println!(" TidesDB + Warpdrive demo");
    println!("====================================================\n");

    println!("== Sanity: basic object store (write / flush / read) ==\n");
    sanity::run()?;

    println!("\n== Replication: primary + replica in parallel, fault injection ==\n");
    replication::run()?;

    Ok(())
}
