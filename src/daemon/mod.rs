pub mod agent;
pub mod ops;
pub mod protocol;
pub mod refresh;
pub mod server;
pub mod state;

use anyhow::Result;

pub fn run() -> Result<()> {
    server::run()
}
