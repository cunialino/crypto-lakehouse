fn main() {
    prost_build::compile_protos(&["proto/trade_event.proto"], &["proto"])
        .unwrap();
}
