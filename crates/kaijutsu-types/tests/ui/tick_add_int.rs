// A bare integer is not a duration — `Tick + i64` must not compile.
// (Offsets must go through `TickDelta` so intent is explicit.)
use kaijutsu_types::Tick;

fn main() {
    let _ = Tick::new(1) + 2_i64;
}
