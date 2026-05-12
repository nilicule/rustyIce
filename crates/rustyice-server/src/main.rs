#![warn(clippy::pedantic)]

mod bus;
mod config_reload;
mod shutdown;
mod source_layer;
mod state;
mod stream_router;

fn main() {
    println!("rustyice starting");
}
