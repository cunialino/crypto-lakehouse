fn main() {
    prost_build::compile_protos(&["src/data/trade_event.proto"], &["src/data"])
        .unwrap();
}
