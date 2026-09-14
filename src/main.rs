#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod collect;
mod config;
mod hooks;
mod merge;
mod model;

fn main() {
    println!("tracon");
}
