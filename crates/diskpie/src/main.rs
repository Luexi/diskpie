//! DiskPie desktop application entry point.

#![forbid(unsafe_code)]

fn main() {
    println!("{} {}", diskpie_app::PRODUCT_NAME, env!("CARGO_PKG_VERSION"));
}
