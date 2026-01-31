fn main() {
    println!("cargo::rerun-if-changed=../../kaijutsu.capnp");
    capnpc::CompilerCommand::new()
        .src_prefix("../../")
        .file("../../kaijutsu.capnp")
        .run()
        .expect("Failed to compile Cap'n Proto schema");
}
