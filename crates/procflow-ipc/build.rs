fn main() -> std::io::Result<()> {
    prost_build::compile_protos(&["proto/procflow/v1/procflow.proto"], &["proto"])
}
