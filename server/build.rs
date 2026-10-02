fn main() {
    // Vendor a prebuilt protoc so this builds on a fresh machine (Mac dev,
    // Linux GCP VM) without requiring `brew/apt install protobuf` first.
    let protoc_path = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc not available for this platform");
    std::env::set_var("PROTOC", protoc_path);

    tonic_build::configure()
        .compile(&["proto/shard.proto"], &["proto"])
        .expect("failed to compile proto/shard.proto");
}
