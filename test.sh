# cargo build --release
cargo test
./target/release/ruvm -M riscv64 -demo hello
./target/release/ruvm -M aarch64 -demo hello
./target/release/ruvm -M riscv64 -kernel prog.elf