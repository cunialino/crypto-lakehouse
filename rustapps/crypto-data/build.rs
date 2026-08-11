fn main() {
    prost_build::compile_protos(
        &["../../deploy/base/arroyo/proto/trade_event.proto"],
        &["../../deploy/base/arroyo/proto"],
    )
    .unwrap();
}