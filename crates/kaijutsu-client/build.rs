fn main() {
    // The schema lives OUTSIDE this package, so cargo's default "rerun when a
    // file in the package changes" never fires for it. Without this line the
    // generated code goes stale on every schema edit, and the client keeps
    // calling the ordinals it was built against.
    //
    // That was survivable while schema changes were additive — a stale client
    // simply could not see a new method, and in practice its own sources
    // changed alongside, which retriggered the build. It stops being
    // survivable the moment ordinals are renumbered: the 2026-08-15 flag day
    // compacted them, the server regenerated, this crate did not, and the
    // handshake failed with "Message contains non-list pointer where data was
    // expected" — a client asking for one method and being answered by
    // another. The server's build script has always had this line.
    println!("cargo::rerun-if-changed=../../kaijutsu.capnp");
    capnpc::CompilerCommand::new()
        .src_prefix("../../")
        .file("../../kaijutsu.capnp")
        .run()
        .expect("Failed to compile Cap'n Proto schema");
}
