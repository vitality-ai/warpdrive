use tidesdb::S3Config;
use std::process::Command;

mod sanity;
mod replication;

const ENDPOINT: &str = "localhost:9710";
const BUCKET: &str = "tidesdb-sstables";
const ACCESS_KEY: &str = "adminkey";
const SECRET_KEY: &str = "adminsecretkey123456";

fn aws(args: &[&str]) -> std::process::Output {
    Command::new("aws")
        .args(args)
        .args(["--endpoint-url", &format!("http://{}", ENDPOINT)])
        .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .expect("aws cli not found — install with: pip install awscli")
}

fn s3_config() -> S3Config {
    S3Config::new(ENDPOINT, BUCKET, ACCESS_KEY, SECRET_KEY)
        .region("us-east-1")
        .use_path_style(true)  // Warpdrive uses path-style: host/bucket/key
        .use_ssl(false)        // Warpdrive is plain HTTP
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("====================================================");
    println!(" TidesDB + Warpdrive demo");
    println!("====================================================\n");

    println!("== Sanity: basic object store (write / flush / read) ==\n");
    sanity::run()?;

    println!("\n== Replication: primary -> Warpdrive -> replica -> promote ==\n");
    replication::run()?;

    Ok(())
}
