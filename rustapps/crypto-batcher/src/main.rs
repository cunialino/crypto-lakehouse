use crypto_data::data::binance_web_socket::TradeEventBinance;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, BufRead};

fn main() -> anyhow::Result<()> {
    // let file = File::open("BTCUSDT-trades-2026-04-29.csv")?;
    let file = File::open("test.csv")?;
    let buf_lines = io::BufReader::new(file).lines();
    for line in buf_lines.map_while(Result::ok) {
        println!("{}", line);
    }
    Ok(())
}
