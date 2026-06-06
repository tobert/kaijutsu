// Adding two positions is meaningless (like `Instant + Instant`) — must not compile.
use kaijutsu_types::Tick;

fn main() {
    let _ = Tick::new(1) + Tick::new(2);
}
