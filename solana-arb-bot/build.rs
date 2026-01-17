use std::io::Result;

fn main() -> Result<()> {
    tonic_build::configure()
        .build_server(false) // We only need client
        .compile(
            &[
                "protos/auth.proto",
                "protos/bundle.proto",
                "protos/packet.proto",
                "protos/searcher.proto",
                "protos/shared.proto",
            ],
            &["protos"],
        )?;
    Ok(())
}
