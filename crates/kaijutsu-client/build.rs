fn main() {
    capnpc::CompilerCommand::new()
        .file("../../kaijutsu.capnp")
        .run()
        .expect("Failed to compile Cap'n Proto schema");
}
