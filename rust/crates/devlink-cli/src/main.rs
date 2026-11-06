mod cli;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run(cli::parse()).await {
        if let Some(exit) = error.downcast_ref::<devlink_cli::setup::terminal::SetupExit>() {
            std::process::exit(i32::from(exit.0));
        }
        eprintln!("DevLink error: {error}");
        if let Some(port) = error.downcast_ref::<devlink_broker::broker::PortFailure>() {
            std::process::exit(port.child_exit_code());
        }
        std::process::exit(1);
    }
}
